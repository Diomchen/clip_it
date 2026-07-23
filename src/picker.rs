use std::{path::PathBuf, process::Command, time::Duration};

use crate::{config::Identity, discovery::Discovery, transfer::send_paths};
use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;

pub async fn run(paths: Vec<PathBuf>, identity: Identity) -> Result<()> {
    let mut peers = Discovery::listen(Duration::from_millis(2200)).await?;
    peers.retain(|peer| peer.id != identity.id);
    if peers.is_empty() {
        bail!("未发现设备；请先在接收设备运行 `clip-it serve`");
    }

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = Uuid::new_v4().simple().to_string();
    let url = format!("http://{}/?token={token}", listener.local_addr()?);
    open_browser(&url)?;

    loop {
        let (mut stream, _) = listener.accept().await?;
        let request = read_request(&mut stream).await?;
        let first_line = request.lines().next().unwrap_or_default();
        let request_target = first_line.split_whitespace().nth(1).unwrap_or("/");

        if !request_target.contains(&format!("token={token}")) {
            respond(&mut stream, 403, "text/plain; charset=utf-8", "Forbidden").await?;
            continue;
        }
        if request_target.starts_with("/send?") {
            let Some(index) =
                query_value(request_target, "peer").and_then(|value| value.parse::<usize>().ok())
            else {
                respond(&mut stream, 400, "text/plain; charset=utf-8", "Bad request").await?;
                continue;
            };
            let Some(peer) = peers.get(index) else {
                respond(&mut stream, 404, "text/plain; charset=utf-8", "Not found").await?;
                continue;
            };
            match send_paths(peer.addr, &paths, &identity).await {
                Ok(receipt) => {
                    let body = page(&format!(
                        "发送完成：{} 个文件，{} 字节 → {}",
                        receipt.files, receipt.bytes, peer.name
                    ));
                    respond(&mut stream, 200, "text/html; charset=utf-8", &body).await?;
                    return Ok(());
                }
                Err(error) => {
                    let body = page(&format!("发送失败：{error:#}"));
                    respond(&mut stream, 500, "text/html; charset=utf-8", &body).await?;
                    return Err(error);
                }
            }
        }

        let buttons = peers
            .iter()
            .enumerate()
            .map(|(index, peer)| {
                format!(
                    "<a class=\"device\" href=\"/send?token={token}&peer={index}\"><span class=\"label\"><b>{}</b>{}</span><small>{}</small></a>",
                    html_escape(&peer.emoji),
                    html_escape(&peer.name),
                    html_escape(&peer.addr.to_string())
                )
            })
            .collect::<String>();
        let body = page(&format!(
            "<h1>发送到</h1><div class=\"devices\">{buttons}</div>"
        ));
        respond(&mut stream, 200, "text/html; charset=utf-8", &body).await?;
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut data = vec![0_u8; 8192];
    let length = stream.read(&mut data).await?;
    Ok(String::from_utf8_lossy(&data[..length]).into_owned())
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n{body}",
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

fn page(content: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>ClipIt</title><style>body{{font:16px system-ui;background:#f4f6f8;color:#17202a;max-width:560px;margin:12vh auto;padding:24px}}h1{{font-size:26px}}.devices{{display:grid;gap:12px}}.device{{display:flex;justify-content:space-between;align-items:center;padding:18px 20px;border-radius:12px;background:white;color:inherit;text-decoration:none;box-shadow:0 3px 16px #0001}}.device:hover{{outline:2px solid #3b82f6}}.label{{display:flex;align-items:center;gap:10px}}.label b{{font-size:24px}}small{{color:#667085}}</style><body>{content}</body></html>"
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    Command::new("rundll32")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn()
        .context("打开默认浏览器失败")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    Command::new("open")
        .arg(url)
        .spawn()
        .context("打开默认浏览器失败")?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn open_browser(url: &str) -> Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("打开默认浏览器失败")?;
    Ok(())
}
