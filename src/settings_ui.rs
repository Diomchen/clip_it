use std::{
    collections::{HashMap, HashSet},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};
use uuid::Uuid;

use crate::{
    config::{AppConfig, PairedDevice, ReceivePolicy, Settings},
    discovery::{Discovery, Peer},
    transfer::set_peer_connection,
};

const ONLINE_WINDOW: Duration = Duration::from_secs(4);
const VISIBLE_WINDOW: Duration = Duration::from_secs(20);

#[derive(Default)]
struct NetworkState {
    peers: HashMap<Uuid, SeenPeer>,
    confirmed: HashSet<Uuid>,
}

struct SeenPeer {
    peer: Peer,
    last_seen: Instant,
}

#[derive(Serialize)]
struct NetworkSnapshot {
    local: LocalDevice,
    devices: Vec<DeviceView>,
}

#[derive(Serialize)]
struct LocalDevice {
    id: Uuid,
    name: String,
    emoji: String,
    port: u16,
}

#[derive(Serialize)]
struct DeviceView {
    id: Uuid,
    name: String,
    emoji: String,
    address: String,
    online: bool,
    connected: bool,
}

#[derive(Serialize)]
struct ApiMessage {
    ok: bool,
    message: String,
}

pub async fn run(config: AppConfig) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let token = Uuid::new_v4().simple().to_string();
    let url = format!("http://{}/?token={token}", listener.local_addr()?);
    let network = Arc::new(Mutex::new(NetworkState::default()));
    start_discovery(config.identity.id, Arc::clone(&network));
    println!("ClipIt 设置页面：{url}");
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
            respond_text(&mut stream, 403, "text/plain; charset=utf-8", "Forbidden").await?;
            continue;
        }

        if target.starts_with("/api/devices?") {
            let snapshot = network_snapshot(&config, &network)?;
            respond_json(&mut stream, 200, &snapshot).await?;
            continue;
        }
        if target.starts_with("/api/connect?") {
            let response = connect_device(target, &config, &network).await;
            match response {
                Ok(message) => respond_json(&mut stream, 200, &message).await?,
                Err(error) => {
                    respond_json(
                        &mut stream,
                        409,
                        &ApiMessage {
                            ok: false,
                            message: error.to_string(),
                        },
                    )
                    .await?;
                }
            }
            continue;
        }
        if target.starts_with("/api/disconnect?") {
            let response = disconnect_device(target, &config, &network).await;
            match response {
                Ok(message) => respond_json(&mut stream, 200, &message).await?,
                Err(error) => {
                    respond_json(
                        &mut stream,
                        409,
                        &ApiMessage {
                            ok: false,
                            message: error.to_string(),
                        },
                    )
                    .await?;
                }
            }
            continue;
        }

        if target.starts_with("/save?") {
            let settings = match parse_settings(target) {
                Ok(settings) => settings,
                Err(error) => {
                    respond_text(
                        &mut stream,
                        400,
                        "text/html; charset=utf-8",
                        &message_page("配置错误", &error.to_string()),
                    )
                    .await?;
                    continue;
                }
            };
            if let Err(error) = config.save_settings(&settings) {
                respond_text(
                    &mut stream,
                    400,
                    "text/html; charset=utf-8",
                    &message_page("配置错误", &error.to_string()),
                )
                .await?;
                continue;
            }
            respond_text(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                &message_page("设置已保存", "托盘服务将自动重启，可以关闭本页面。"),
            )
            .await?;
            return Ok(());
        }

        respond_text(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            &settings_page(&config.settings, &token),
        )
        .await?;
    }
}

fn start_discovery(own_id: Uuid, state: Arc<Mutex<NetworkState>>) {
    tokio::spawn(async move {
        loop {
            match Discovery::listen(Duration::from_millis(1_250)).await {
                Ok(peers) => {
                    let now = Instant::now();
                    if let Ok(mut state) = state.lock() {
                        for peer in peers.into_iter().filter(|peer| peer.id != own_id) {
                            state.peers.insert(
                                peer.id,
                                SeenPeer {
                                    peer,
                                    last_seen: now,
                                },
                            );
                        }
                        state
                            .peers
                            .retain(|_, seen| now.duration_since(seen.last_seen) < VISIBLE_WINDOW);
                        let online = state
                            .peers
                            .iter()
                            .filter(|(_, seen)| now.duration_since(seen.last_seen) < ONLINE_WINDOW)
                            .map(|(id, _)| *id)
                            .collect::<HashSet<_>>();
                        state.confirmed.retain(|id| online.contains(id));
                    }
                }
                Err(error) => eprintln!("设置页发现局域网设备失败: {error:#}"),
            }
            time::sleep(Duration::from_millis(100)).await;
        }
    });
}

fn network_snapshot(
    config: &AppConfig,
    network: &Arc<Mutex<NetworkState>>,
) -> Result<NetworkSnapshot> {
    let state = network
        .lock()
        .map_err(|_| anyhow::anyhow!("设备状态锁已损坏"))?;
    let now = Instant::now();
    let paired = config.paired_devices.list()?;
    let paired_ids = paired
        .iter()
        .map(|device| device.id)
        .collect::<HashSet<_>>();
    let mut devices = state
        .peers
        .iter()
        .map(|(id, seen)| {
            let online = now.duration_since(seen.last_seen) < ONLINE_WINDOW;
            DeviceView {
                id: *id,
                name: seen.peer.name.clone(),
                emoji: seen.peer.emoji.clone(),
                address: seen.peer.addr.to_string(),
                online,
                connected: online
                    && paired_ids.contains(id)
                    && (seen.peer.connected_devices.contains(&config.identity.id)
                        || state.confirmed.contains(id)),
            }
        })
        .collect::<Vec<_>>();
    for device in paired {
        if !state.peers.contains_key(&device.id) {
            devices.push(DeviceView {
                id: device.id,
                name: device.name,
                emoji: device.emoji,
                address: String::new(),
                online: false,
                connected: false,
            });
        }
    }
    devices.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.cmp(&b.name)));
    Ok(NetworkSnapshot {
        local: LocalDevice {
            id: config.identity.id,
            name: config.device_name.clone(),
            emoji: config.identity.emoji.clone(),
            port: config.settings.transfer_port,
        },
        devices,
    })
}

async fn connect_device(
    target: &str,
    config: &AppConfig,
    network: &Arc<Mutex<NetworkState>>,
) -> Result<ApiMessage> {
    let id = parse_device_id(target)?;
    let peer = {
        let state = network
            .lock()
            .map_err(|_| anyhow::anyhow!("设备状态锁已损坏"))?;
        let seen = state.peers.get(&id).context("设备已离线")?;
        if seen.last_seen.elapsed() >= ONLINE_WINDOW {
            bail!("设备已离线");
        }
        seen.peer.clone()
    };
    set_peer_connection(peer.addr, &config.identity, true).await?;
    config.paired_devices.add(PairedDevice::new(
        peer.id,
        peer.name.clone(),
        peer.emoji.clone(),
    ))?;
    network
        .lock()
        .map_err(|_| anyhow::anyhow!("设备状态锁已损坏"))?
        .confirmed
        .insert(id);
    Ok(ApiMessage {
        ok: true,
        message: format!("已连接 {}", peer.name),
    })
}

async fn disconnect_device(
    target: &str,
    config: &AppConfig,
    network: &Arc<Mutex<NetworkState>>,
) -> Result<ApiMessage> {
    let id = parse_device_id(target)?;
    let peer = {
        let state = network
            .lock()
            .map_err(|_| anyhow::anyhow!("设备状态锁已损坏"))?;
        let seen = state
            .peers
            .get(&id)
            .context("设备已离线，无法同步断开状态")?;
        if seen.last_seen.elapsed() >= ONLINE_WINDOW {
            bail!("设备已离线，无法同步断开状态");
        }
        seen.peer.clone()
    };
    set_peer_connection(peer.addr, &config.identity, false).await?;
    config.paired_devices.remove(id)?;
    network
        .lock()
        .map_err(|_| anyhow::anyhow!("设备状态锁已损坏"))?
        .confirmed
        .remove(&id);
    Ok(ApiMessage {
        ok: true,
        message: format!("已断开 {}", peer.name),
    })
}

fn parse_device_id(target: &str) -> Result<Uuid> {
    query_value(target, "id")
        .context("缺少设备 ID")?
        .parse()
        .context("设备 ID 无效")
}

fn parse_settings(target: &str) -> Result<Settings> {
    let device_name = query_value_decoded(target, "name")?.trim().to_owned();
    let device_emoji = query_value_decoded(target, "emoji")?.trim().to_owned();
    let port = query_value(target, "port")
        .context("缺少传输端口")?
        .parse::<u16>()
        .context("端口必须是 1-65535 的整数")?;
    let receive_policy = match query_value(target, "policy") {
        Some("confirm") => ReceivePolicy::Confirm,
        Some("trusted-only") => ReceivePolicy::TrustedOnly,
        Some("accept-all") => ReceivePolicy::AcceptAll,
        _ => bail!("接收策略无效"),
    };
    Ok(Settings {
        device_name,
        device_emoji,
        transfer_port: port,
        receive_policy,
        clipboard_sync: query_value(target, "clipboard") == Some("on"),
    })
}

fn settings_page(settings: &Settings, token: &str) -> String {
    SETTINGS_PAGE
        .replace("__TOKEN__", token)
        .replace("__DEVICE_NAME__", &html_escape(&settings.device_name))
        .replace("__DEVICE_EMOJI__", &html_escape(&settings.device_emoji))
        .replace("__PORT__", &settings.transfer_port.to_string())
        .replace(
            "__CONFIRM_SELECTED__",
            selected(settings.receive_policy == ReceivePolicy::Confirm),
        )
        .replace(
            "__TRUSTED_SELECTED__",
            selected(settings.receive_policy == ReceivePolicy::TrustedOnly),
        )
        .replace(
            "__ALL_SELECTED__",
            selected(settings.receive_policy == ReceivePolicy::AcceptAll),
        )
        .replace(
            "__CLIPBOARD_CHECKED__",
            if settings.clipboard_sync {
                " checked"
            } else {
                ""
            },
        )
}

fn selected(value: bool) -> &'static str {
    if value { " selected" } else { "" }
}

fn message_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><style>body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;background:#f5f5f7;color:#1d1d1f;display:grid;place-items:center;min-height:100vh;margin:0}}main{{background:#fff;padding:36px;border-radius:24px;box-shadow:0 20px 60px #0002;max-width:480px}}h1{{margin-top:0}}p{{color:#6e6e73;line-height:1.6}}</style><main><h1>{}</h1><p>{}</p></main></html>",
        html_escape(title),
        html_escape(title),
        html_escape(message)
    )
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut data = vec![0_u8; 16 * 1024];
    let length = stream.read(&mut data).await?;
    Ok(String::from_utf8_lossy(&data[..length]).into_owned())
}

async fn respond_json<T: Serialize>(stream: &mut TcpStream, status: u16, value: &T) -> Result<()> {
    let body = serde_json::to_string(value)?;
    respond_text(stream, status, "application/json; charset=utf-8", &body).await
}

async fn respond_text(
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
        409 => "Conflict",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; connect-src 'self'; form-action 'self'\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\n\r\n{body}",
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

fn query_value_decoded(target: &str, key: &str) -> Result<String> {
    let encoded = query_value(target, key).with_context(|| format!("缺少参数 {key}"))?;
    let input = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'+' => decoded.push(b' '),
            b'%' if index + 2 < input.len() => {
                let high = hex_value(input[index + 1]).context("URL 编码无效")?;
                let low = hex_value(input[index + 2]).context("URL 编码无效")?;
                decoded.push((high << 4) | low);
                index += 2;
            }
            b'%' => bail!("URL 编码无效"),
            byte => decoded.push(byte),
        }
        index += 1;
    }
    String::from_utf8(decoded).context("设置必须使用 UTF-8 编码")
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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

const SETTINGS_PAGE: &str = include_str!("settings_page.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_settings_query() {
        let settings = parse_settings(
            "/save?token=x&name=%E5%AE%A2%E5%8E%85+Mac&emoji=%F0%9F%8D%8E&port=43123&policy=trusted-only&clipboard=on",
        )
        .unwrap();
        assert_eq!(settings.device_name, "客厅 Mac");
        assert_eq!(settings.device_emoji, "🍎");
        assert_eq!(settings.transfer_port, 43_123);
        assert_eq!(settings.receive_policy, ReceivePolicy::TrustedOnly);
        assert!(settings.clipboard_sync);
    }

    #[test]
    fn settings_page_contains_glitch_topology_without_placeholders() {
        let page = settings_page(&Settings::default(), "token123");
        assert!(page.contains("class=\"entity local\""));
        assert!(page.contains("/api/${kind}"));
        assert!(page.contains("id=\"asciiLayer\""));
        assert!(page.contains("id=\"signalPath\""));
        assert!(page.contains("requestAnimationFrame(animateDrag)"));
        assert!(page.contains("reactToPointer"));
        assert!(page.contains("transmitAscii"));
        assert!(page.contains("dematerializing"));
        assert!(page.contains("materializing"));
        assert!(!page.contains("__TOKEN__"));
        assert!(!page.contains("__DEVICE_NAME__"));
        assert!(!page.contains("__DEVICE_EMOJI__"));
    }
}
