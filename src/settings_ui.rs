use std::{process::Command, time::Duration};

use anyhow::{Context, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};
use uuid::Uuid;

use crate::config::{AppConfig, ReceivePolicy, Settings};

pub async fn run(config: AppConfig) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = Uuid::new_v4().simple().to_string();
    let url = format!("http://{}/?token={token}", listener.local_addr()?);
    open_browser(&url)?;

    loop {
        let (mut stream, _) = time::timeout(Duration::from_secs(600), listener.accept())
            .await
            .context("设置页等待超时")??;
        let request = read_request(&mut stream).await?;
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        if query_value(target, "token") != Some(token.as_str()) {
            respond(&mut stream, 403, "Forbidden").await?;
            continue;
        }

        if target.starts_with("/save?") {
            let settings = match parse_settings(target) {
                Ok(settings) => settings,
                Err(error) => {
                    respond(
                        &mut stream,
                        400,
                        &page("配置错误", &format!("<h1>配置错误</h1><p>{error}</p>")),
                    )
                    .await?;
                    continue;
                }
            };
            if let Err(error) = config.save_settings(&settings) {
                respond(
                    &mut stream,
                    400,
                    &page("配置错误", &format!("<h1>配置错误</h1><p>{error}</p>")),
                )
                .await?;
                continue;
            }
            respond(
                &mut stream,
                200,
                &page(
                    "设置已保存",
                    "<h1>设置已保存</h1><p>托盘服务将自动重启，可以关闭本页面。</p>",
                ),
            )
            .await?;
            return Ok(());
        }

        respond(&mut stream, 200, &settings_page(&config.settings, &token)).await?;
    }
}

fn parse_settings(target: &str) -> Result<Settings> {
    let port = query_value(target, "port")
        .context("缺少传输端口")?
        .parse::<u16>()
        .context("端口必须是 1-65535 的整数")?;
    let receive_policy = match query_value(target, "policy") {
        Some("confirm") => ReceivePolicy::Confirm,
        Some("trusted-only") => ReceivePolicy::TrustedOnly,
        Some("accept-all") => ReceivePolicy::AcceptAll,
        _ => anyhow::bail!("接收策略无效"),
    };
    Ok(Settings {
        transfer_port: port,
        receive_policy,
        clipboard_sync: query_value(target, "clipboard") == Some("on"),
    })
}

fn settings_page(settings: &Settings, token: &str) -> String {
    let selected = |policy| {
        if settings.receive_policy == policy {
            " selected"
        } else {
            ""
        }
    };
    let checked = if settings.clipboard_sync {
        " checked"
    } else {
        ""
    };
    let content = format!(
        "<h1>ClipIt 设置</h1><form method=\"get\" action=\"/save\">\
         <input type=\"hidden\" name=\"token\" value=\"{token}\">\
         <label>文件传输端口<input name=\"port\" type=\"number\" min=\"1\" max=\"65535\" value=\"{}\" required></label>\
         <label>接收策略<select name=\"policy\">\
         <option value=\"confirm\"{}>未知设备需要确认</option>\
         <option value=\"trusted-only\"{}>仅可信设备</option>\
         <option value=\"accept-all\"{}>接受所有设备</option></select></label>\
         <label class=\"check\"><input name=\"clipboard\" type=\"checkbox\" value=\"on\"{checked}> 自动同步文本及复制的文件/目录到所有在线 ClipIt 设备</label>\
         <button type=\"submit\">保存并重启服务</button></form>\
         <p class=\"note\">修改端口后，其他设备会通过局域网发现自动获得新端口。文件类型不受限制，符号链接除外。</p>",
        settings.transfer_port,
        selected(ReceivePolicy::Confirm),
        selected(ReceivePolicy::TrustedOnly),
        selected(ReceivePolicy::AcceptAll),
    );
    page("ClipIt 设置", &content)
}

fn page(title: &str, content: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title}</title><style>body{{font:16px system-ui;background:#f4f6f8;color:#17202a;max-width:560px;margin:9vh auto;padding:24px}}h1{{font-size:28px}}form{{display:grid;gap:18px;background:white;padding:24px;border-radius:14px;box-shadow:0 3px 18px #0001}}label{{display:grid;gap:7px;color:#475467}}input,select,button{{font:inherit;padding:11px;border:1px solid #d0d5dd;border-radius:8px}}.check{{display:flex;align-items:flex-start;color:#17202a}}.check input{{margin-top:4px}}button{{border:0;background:#2563eb;color:white;cursor:pointer}}.note{{color:#667085;font-size:13px}}</style><body>{content}</body></html>"
    )
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut data = vec![0_u8; 16 * 1024];
    let length = stream.read(&mut data).await?;
    Ok(String::from_utf8_lossy(&data[..length]).into_owned())
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = if status == 200 {
        "OK"
    } else if status == 400 {
        "Bad Request"
    } else {
        "Forbidden"
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    Ok(())
}

fn query_value<'a>(target: &'a str, key: &str) -> Option<&'a str> {
    target.split_once('?')?.1.split('&').find_map(|part| {
        part.split_once('=')
            .filter(|(name, _)| *name == key)
            .map(|(_, value)| value)
    })
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    Command::new("rundll32")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn()
        .context("打开设置页失败")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    Command::new("open")
        .arg(url)
        .spawn()
        .context("打开设置页失败")?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_browser(url: &str) -> Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("打开设置页失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings_query() {
        let settings =
            parse_settings("/save?token=x&port=43123&policy=trusted-only&clipboard=on").unwrap();
        assert_eq!(settings.transfer_port, 43_123);
        assert_eq!(settings.receive_policy, ReceivePolicy::TrustedOnly);
        assert!(settings.clipboard_sync);
    }
}
