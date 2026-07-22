use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};
use uuid::Uuid;

use crate::protocol::SenderIdentity;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    AcceptOnce,
    TrustAndAccept,
    Reject,
}

pub struct IncomingSummary<'a> {
    pub sender: &'a SenderIdentity,
    pub peer: SocketAddr,
    pub files: usize,
    pub bytes: u64,
    pub paths: &'a [String],
}

pub async fn prompt(summary: IncomingSummary<'_>) -> Result<Decision> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("启动接收确认页失败")?;
    let token = Uuid::new_v4().simple().to_string();
    let url = format!("http://{}/?token={token}", listener.local_addr()?);
    open_browser(&url)?;

    loop {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let request = match time::timeout(REQUEST_TIMEOUT, read_request(&mut stream)).await {
            Ok(Ok(request)) => request,
            _ => continue,
        };
        let first_line = request.lines().next().unwrap_or_default();
        let mut request_parts = first_line.split_whitespace();
        let method = request_parts.next().unwrap_or_default();
        let target = request_parts.next().unwrap_or("/");

        if query_value(target, "token") != Some(token.as_str()) {
            respond(&mut stream, 403, "Forbidden").await?;
            continue;
        }

        if method == "POST" && target.starts_with("/decision?") {
            let decision = match query_value(target, "choice") {
                Some("once") => Decision::AcceptOnce,
                Some("always") => Decision::TrustAndAccept,
                Some("reject") => Decision::Reject,
                _ => {
                    respond(&mut stream, 400, "Bad request").await?;
                    continue;
                }
            };
            let message = if decision == Decision::Reject {
                "已拒绝此次传输，可以关闭本页面。"
            } else {
                "已允许此次传输，文件接收完成前请保持 ClipIt 运行。"
            };
            respond(&mut stream, 200, &page("接收确认", message)).await?;
            return Ok(decision);
        }

        respond(&mut stream, 200, &confirmation_page(&summary, &token)).await?;
    }
}

fn confirmation_page(summary: &IncomingSummary<'_>, token: &str) -> String {
    let paths = summary
        .paths
        .iter()
        .take(8)
        .map(|path| format!("<li>{}</li>", html_escape(path)))
        .collect::<String>();
    let more = if summary.paths.len() > 8 {
        format!("<li>以及另外 {} 项…</li>", summary.paths.len() - 8)
    } else {
        String::new()
    };
    let content = format!(
        "<h1>是否接收文件？</h1>\
         <dl><dt>设备</dt><dd>{}</dd><dt>设备 ID</dt><dd><code>{}</code></dd>\
         <dt>来源地址</dt><dd>{}</dd><dt>内容</dt><dd>{} 项，{}</dd></dl>\
         <ul>{paths}{more}</ul><div class=\"actions\">\
         {}{}{}\
         </div><p class=\"notice\">设备 ID 未经密码学认证；此确认功能用于避免可信局域网中的误发。</p>",
        html_escape(&summary.sender.name),
        summary.sender.id,
        html_escape(&summary.peer.to_string()),
        summary.files,
        format_bytes(summary.bytes),
        decision_form(token, "once", "仅本次允许", "primary"),
        decision_form(token, "always", "始终允许此设备", "secondary"),
        decision_form(token, "reject", "拒绝", "danger"),
    );
    page("接收确认", &content)
}

fn decision_form(token: &str, choice: &str, label: &str, class: &str) -> String {
    format!(
        "<form method=\"post\" action=\"/decision?token={token}&amp;choice={choice}\"><button class=\"{class}\" type=\"submit\">{label}</button></form>"
    )
}

fn page(title: &str, content: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{title} - ClipIt</title><style>body{{font:16px system-ui;background:#f4f6f8;color:#17202a;max-width:620px;margin:8vh auto;padding:24px}}h1{{font-size:28px}}dl{{display:grid;grid-template-columns:90px 1fr;gap:8px 16px;background:white;padding:20px;border-radius:12px}}dt{{color:#667085}}dd{{margin:0;overflow-wrap:anywhere}}ul{{background:white;padding:18px 18px 18px 38px;border-radius:12px}}.actions{{display:flex;gap:10px;flex-wrap:wrap}}form{{margin:0}}button{{border:0;border-radius:9px;padding:12px 16px;font:inherit;cursor:pointer}}.primary{{background:#2563eb;color:white}}.secondary{{background:#dbeafe;color:#1e3a8a}}.danger{{background:#fee2e2;color:#991b1b}}.notice{{color:#667085;font-size:13px}}code{{font-size:13px}}</style><body>{content}</body></html>"
    )
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut data = vec![0_u8; 8192];
    let length = stream.read(&mut data).await?;
    if length == data.len() {
        bail!("确认请求过大");
    }
    Ok(String::from_utf8_lossy(&data[..length]).into_owned())
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Internal Server Error",
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

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("rundll32")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn()
        .context("打开接收确认页失败")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .context("打开接收确认页失败")?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("打开接收确认页失败")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_untrusted_html() {
        assert_eq!(html_escape("<x a='b'>&"), "&lt;x a=&#39;b&#39;&gt;&amp;");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_bytes(12), "12 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
    }
}
