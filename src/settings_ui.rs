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

const SETTINGS_PAGE: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>ClipIt 设置</title>
<style>
:root{color-scheme:light dark;--blue:#0a84ff;--cyan:#64d2ff;--violet:#5e5ce6;--ink:#1d1d1f;--muted:#6e6e73;--card:rgba(255,255,255,.72);--line:rgba(60,60,67,.12);--stage:rgba(244,247,252,.78)}
*{box-sizing:border-box}body{margin:0;min-height:100vh;font-family:-apple-system,BlinkMacSystemFont,"SF Pro Display","Segoe UI",sans-serif;color:var(--ink);background:radial-gradient(circle at 18% 4%,#dff5ff 0,transparent 36%),radial-gradient(circle at 88% 12%,#ebe8ff 0,transparent 34%),#f5f5f7;padding:42px 22px 70px}main{width:min(1120px,100%);margin:auto}.hero{display:flex;align-items:end;justify-content:space-between;margin:0 6px 22px}.hero h1{font-size:38px;letter-spacing:-1.5px;margin:0 0 5px}.hero p{margin:0;color:var(--muted)}.live{font-size:13px;color:#248a3d;background:#e7f8eb;padding:7px 12px;border-radius:99px;display:flex;align-items:center;gap:7px}.live:before{content:"";width:7px;height:7px;border-radius:50%;background:#30d158;box-shadow:0 0 0 5px #30d15820;animation:breathe 2s infinite}.layout{display:grid;grid-template-columns:minmax(0,1.7fr) minmax(300px,.85fr);gap:18px}.card{border:1px solid rgba(255,255,255,.68);background:var(--card);backdrop-filter:blur(28px) saturate(1.4);-webkit-backdrop-filter:blur(28px) saturate(1.4);border-radius:28px;box-shadow:0 18px 60px rgba(31,50,85,.11),inset 0 1px rgba(255,255,255,.7)}.network{padding:20px}.card-head{display:flex;align-items:center;justify-content:space-between;padding:2px 4px 14px}.card-head h2{font-size:18px;margin:0;letter-spacing:-.25px}.count{font-size:12px;color:var(--muted);background:rgba(118,118,128,.1);padding:6px 10px;border-radius:99px}.stage{position:relative;height:460px;overflow:hidden;border-radius:22px;background:linear-gradient(145deg,rgba(255,255,255,.65),rgba(232,241,252,.72));border:1px solid rgba(255,255,255,.82);isolation:isolate}.stage:before,.stage:after{content:"";position:absolute;border-radius:50%;filter:blur(2px);opacity:.52;animation:drift 13s ease-in-out infinite}.stage:before{width:290px;height:290px;left:-110px;top:-120px;background:radial-gradient(circle at 70% 70%,#8edaff70,transparent 68%)}.stage:after{width:330px;height:330px;right:-130px;bottom:-170px;background:radial-gradient(circle at 30% 30%,#928cff60,transparent 68%);animation-delay:-5s}.orbit{position:absolute;inset:10%;border:1px dashed rgba(10,132,255,.13);border-radius:50%;pointer-events:none}.orbit.two{inset:22%;border-style:solid;border-color:rgba(10,132,255,.08)}.bubble{--lean-x:0px;--lean-y:0px;--stretch-x:1;--stretch-y:1;position:absolute;left:50%;top:50%;width:96px;height:96px;border:0;border-radius:45% 55% 52% 48%/52% 46% 54% 48%;transform:translate(-50%,-50%) translate(var(--lean-x),var(--lean-y)) scale(var(--stretch-x),var(--stretch-y));display:grid;place-items:center;text-align:center;padding:12px;color:#063b64;background:radial-gradient(circle at 30% 24%,rgba(255,255,255,.96),rgba(100,210,255,.74) 38%,rgba(10,132,255,.32) 76%);box-shadow:inset -10px -12px 24px rgba(10,91,180,.12),inset 8px 8px 18px rgba(255,255,255,.8),0 16px 34px rgba(44,113,170,.16);cursor:grab;user-select:none;touch-action:none;transition:left .72s cubic-bezier(.2,.85,.2,1),top .72s cubic-bezier(.2,.85,.2,1),width .35s,height .35s,opacity .35s,filter .35s,box-shadow .35s,transform .2s cubic-bezier(.2,.9,.2,1),border-radius .2s;animation:float 5.5s ease-in-out infinite;z-index:4;will-change:left,top,transform,border-radius}.bubble.pointer-reacting{animation:none}.node-emoji{display:block;font-size:25px;line-height:1;margin-bottom:5px;filter:drop-shadow(0 2px 3px #16558c35)}.bubble strong{display:block;max-width:78px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px;line-height:1.2}.bubble small{display:block;font-size:9px;opacity:.58;margin-top:4px}.bubble.dragging{cursor:grabbing;transition:none;animation:none;z-index:12}.bubble.rebounding{transition:left .68s cubic-bezier(.16,1.15,.35,1),top .68s cubic-bezier(.16,1.15,.35,1),transform .52s cubic-bezier(.16,1.4,.35,1),border-radius .48s}.bubble.offline{filter:grayscale(.85);opacity:.46;background:radial-gradient(circle at 30% 24%,#fff,#d8d8dc 45%,#a8a8ae 90%)}.bubble.connected{width:88px;height:88px;color:#fff;background:radial-gradient(circle at 30% 22%,#bdf2ff 0,#2aa8ff 42%,#4a5ce8 100%);box-shadow:inset 8px 8px 18px #ffffff7a,inset -10px -12px 24px #2434b84d,0 12px 32px #147ee94f;animation:connectedFloat 4.6s ease-in-out infinite}.bubble.merging{animation:merge .8s cubic-bezier(.16,.85,.3,1)}.local{width:154px;height:154px;cursor:default;color:#fff;background:radial-gradient(circle at 30% 24%,#c9f5ff 0,#35adff 30%,#087cf1 62%,#5b55e8 100%);box-shadow:inset 12px 12px 26px #ffffff73,inset -16px -18px 36px #3430ae5e,0 24px 58px #1766c54a;z-index:3;animation:localFloat 6s ease-in-out infinite}.local .node-emoji{font-size:38px;margin-bottom:7px}.local strong{font-size:16px;max-width:122px}.local small{font-size:10px}.local:after{content:"本机";position:absolute;bottom:14px;font-size:9px;font-weight:700;letter-spacing:1px;opacity:.7}.tether-layer{position:absolute;inset:0;width:100%;height:100%;overflow:visible;pointer-events:none;z-index:2}.tether-layer path{fill:url(#tetherGradient);opacity:0;filter:url(#tetherBlur);transition:opacity .12s}.tether-layer.active path{opacity:.68}.tether-layer.snapped path{opacity:0;transition:opacity .18s}.tether-pulse{fill:#64d2ff;opacity:0}.tether-layer.active .tether-pulse{opacity:.42;animation:tetherPulse .85s ease-in-out infinite}.fusion-ring{position:absolute;width:248px;height:248px;left:50%;top:50%;transform:translate(-50%,-50%) scale(.68);border-radius:48% 52% 46% 54%/52% 44% 56% 48%;background:radial-gradient(circle,transparent 52%,rgba(100,210,255,.1) 54%,rgba(10,132,255,.17) 70%,transparent 72%);opacity:0;transition:opacity .45s,transform .7s cubic-bezier(.16,.85,.3,1);pointer-events:none;z-index:2}.stage.has-connection .fusion-ring{opacity:1;transform:translate(-50%,-50%) scale(1);animation:ringMorph 7s ease-in-out infinite}.empty{position:absolute;left:50%;bottom:22px;transform:translateX(-50%);color:var(--muted);font-size:12px;z-index:1;transition:opacity .3s}.hint{display:flex;gap:15px;align-items:center;color:var(--muted);font-size:12px;padding:14px 5px 2px}.legend{margin-left:auto;display:flex;gap:12px}.dot{display:inline-block;width:7px;height:7px;border-radius:50%;margin-right:5px;background:#64d2ff}.dot.connected-dot{background:#0a84ff}.settings{padding:24px}.settings h2{font-size:18px;margin:0 0 18px}.form{display:grid;gap:17px}.field{display:grid;gap:7px;font-size:12px;color:var(--muted)}.identity-grid{display:grid;grid-template-columns:74px 1fr;gap:10px}.emoji-input{text-align:center;font-size:23px}.emoji-presets{display:flex;gap:5px;flex-wrap:wrap;margin-top:-2px}.emoji-preset{width:32px;height:32px;padding:0;border:1px solid var(--line);border-radius:10px;background:rgba(118,118,128,.08);cursor:pointer;transition:.18s}.emoji-preset:hover{transform:scale(1.08);background:#0a84ff18}input,select,button{font:inherit}input[type=text],input[type=number],select{width:100%;border:1px solid var(--line);background:rgba(255,255,255,.76);color:var(--ink);padding:11px 12px;border-radius:12px;outline:none;transition:border-color .2s,box-shadow .2s}input:focus,select:focus{border-color:#0a84ff;box-shadow:0 0 0 4px #0a84ff20}.switch{display:flex;align-items:flex-start;gap:10px;color:var(--ink);font-size:13px;line-height:1.4}.switch input{appearance:none;width:38px;height:22px;flex:0 0 auto;border-radius:99px;background:#d1d1d6;position:relative;transition:.25s;margin:0}.switch input:after{content:"";position:absolute;width:18px;height:18px;left:2px;top:2px;border-radius:50%;background:#fff;box-shadow:0 1px 4px #0004;transition:.25s}.switch input:checked{background:#34c759}.switch input:checked:after{transform:translateX(16px)}.save{border:0;border-radius:13px;padding:12px;background:linear-gradient(180deg,#168cff,#0879ef);color:#fff;font-weight:650;box-shadow:0 8px 18px #0879ef35;cursor:pointer;transition:transform .18s,filter .18s}.save:hover{filter:brightness(1.04)}.save:active{transform:scale(.98)}.note{font-size:11px;color:var(--muted);line-height:1.55;margin:0}.toast{position:fixed;left:50%;bottom:28px;transform:translate(-50%,20px);background:rgba(28,28,30,.9);color:#fff;padding:10px 15px;border-radius:99px;font-size:12px;opacity:0;pointer-events:none;transition:.3s;backdrop-filter:blur(18px);z-index:30}.toast.show{opacity:1;transform:translate(-50%,0)}
.bubble,.bubble.offline,.bubble.connected,.bubble.local{background:transparent;box-shadow:none;isolation:isolate}.bubble::before{content:"";position:absolute;inset:0;z-index:-1;border-radius:46% 54% 51% 49%/53% 45% 55% 47%;background:radial-gradient(circle at 30% 24%,rgba(255,255,255,.96),rgba(100,210,255,.74) 38%,rgba(10,132,255,.32) 76%);box-shadow:inset -10px -12px 24px rgba(10,91,180,.12),inset 8px 8px 18px rgba(255,255,255,.8),0 16px 34px rgba(44,113,170,.16);animation:bubbleSurface 6.4s ease-in-out infinite;will-change:border-radius,transform}.bubble.offline::before{background:radial-gradient(circle at 30% 24%,#fff,#d8d8dc 45%,#a8a8ae 90%)}.bubble.connected::before{background:radial-gradient(circle at 30% 22%,#bdf2ff 0,#2aa8ff 42%,#4a5ce8 100%);box-shadow:inset 8px 8px 18px #ffffff7a,inset -10px -12px 24px #2434b84d,0 12px 32px #147ee94f;animation:connectedSurface 5.6s ease-in-out infinite}.bubble.local::before{background:radial-gradient(circle at 30% 24%,#c9f5ff 0,#35adff 30%,#087cf1 62%,#5b55e8 100%);box-shadow:inset 12px 12px 26px #ffffff73,inset -16px -18px 36px #3430ae5e,0 24px 58px #1766c54a;animation:localSurface 7.2s ease-in-out infinite}.bubble.pointer-reacting{animation:float 5.5s ease-in-out infinite}.bubble.connected.pointer-reacting{animation:connectedFloat 4.6s ease-in-out infinite}.bubble.local,.bubble.local.pointer-reacting{animation:localDrift 6s ease-in-out infinite}.bubble.merging::before{animation:connectedSurface 5.6s ease-in-out infinite,mergeSurface .8s cubic-bezier(.16,.85,.3,1)}
@keyframes bubbleSurface{0%,100%{border-radius:46% 54% 51% 49%/53% 45% 55% 47%;transform:rotate(-1deg) scale(1)}23%{border-radius:53% 47% 44% 56%/47% 55% 45% 53%;transform:rotate(.8deg) scale(1.012,.99)}51%{border-radius:43% 57% 54% 46%/57% 46% 54% 43%;transform:rotate(-.4deg) scale(.992,1.014)}76%{border-radius:56% 44% 48% 52%/45% 52% 48% 55%;transform:rotate(1.1deg) scale(1.008,.996)}}
@keyframes localSurface{0%,100%{border-radius:45% 55% 52% 48%/52% 46% 54% 48%;transform:rotate(-.6deg) scale(1)}27%{border-radius:54% 46% 44% 56%/47% 56% 44% 53%;transform:rotate(.5deg) scale(1.01,.992)}58%{border-radius:48% 52% 58% 42%/56% 43% 57% 44%;transform:rotate(-.3deg) scale(.995,1.012)}82%{border-radius:57% 43% 49% 51%/45% 51% 49% 55%;transform:rotate(.8deg) scale(1.006,.996)}}
@keyframes connectedSurface{0%,100%{border-radius:48% 52% 45% 55%/54% 46% 54% 46%;transform:rotate(-1deg) scale(1)}32%{border-radius:56% 44% 53% 47%/45% 57% 43% 55%;transform:rotate(.9deg) scale(1.014,.99)}67%{border-radius:43% 57% 48% 52%/58% 44% 56% 42%;transform:rotate(-.6deg) scale(.992,1.015)}}
@keyframes localDrift{0%,100%{margin-top:0}50%{margin-top:-6px}}
@keyframes mergeSurface{0%,100%{filter:brightness(1)}45%{filter:brightness(1.2);box-shadow:inset 8px 8px 20px #ffffff9c,0 0 0 18px #0a84ff12,0 18px 40px #147ee966}}
.bubble{--phase:-.6s}.bubble.local{--phase:-2.1s}.bubble::before{background-size:145% 145%;animation:autonomousBubble 4.2s cubic-bezier(.45,.04,.28,1) infinite,liquidLight 7.4s ease-in-out infinite;animation-delay:var(--phase),var(--phase)}.bubble.connected::before{animation:autonomousConnected 3.9s cubic-bezier(.45,.04,.28,1) infinite,liquidLight 6.6s ease-in-out infinite;animation-delay:var(--phase),var(--phase)}.bubble.local::before{animation:autonomousLocal 4.8s cubic-bezier(.45,.04,.28,1) infinite,liquidLight 8.2s ease-in-out infinite;animation-delay:var(--phase),var(--phase)}.bubble.merging::before{animation:autonomousConnected 3.9s cubic-bezier(.45,.04,.28,1) infinite,liquidLight 6.6s ease-in-out infinite,mergeSurface .8s cubic-bezier(.16,.85,.3,1);animation-delay:var(--phase),var(--phase),0s}
@keyframes autonomousBubble{0%,100%{border-radius:38% 62% 47% 53%/58% 42% 58% 42%;transform:rotate(-1.8deg) scale(1.025,.975)}18%{border-radius:55% 45% 61% 39%/43% 57% 46% 54%;transform:rotate(1.4deg) scale(.98,1.03)}41%{border-radius:44% 56% 39% 61%/62% 45% 55% 38%;transform:rotate(-.9deg) scale(1.035,.97)}66%{border-radius:63% 37% 52% 48%/39% 61% 43% 57%;transform:rotate(1.9deg) scale(.97,1.035)}84%{border-radius:47% 53% 64% 36%/54% 38% 62% 46%;transform:rotate(-1.2deg) scale(1.02,.982)}}
@keyframes autonomousLocal{0%,100%{border-radius:40% 60% 48% 52%/57% 43% 58% 42%;transform:rotate(-1.2deg) scale(1.018,.982)}22%{border-radius:58% 42% 62% 38%/44% 61% 39% 56%;transform:rotate(1deg) scale(.984,1.024)}47%{border-radius:43% 57% 38% 62%/63% 46% 54% 37%;transform:rotate(-.7deg) scale(1.028,.978)}71%{border-radius:61% 39% 55% 45%/40% 58% 42% 60%;transform:rotate(1.3deg) scale(.978,1.027)}88%{border-radius:48% 52% 63% 37%/55% 39% 61% 45%;transform:rotate(-.8deg) scale(1.015,.986)}}
@keyframes autonomousConnected{0%,100%{border-radius:39% 61% 46% 54%/59% 41% 57% 43%;transform:rotate(-1.6deg) scale(1.022,.978)}25%{border-radius:60% 40% 58% 42%/42% 62% 38% 58%;transform:rotate(1.5deg) scale(.976,1.032)}52%{border-radius:42% 58% 37% 63%/64% 44% 56% 36%;transform:rotate(-1deg) scale(1.034,.974)}78%{border-radius:64% 36% 53% 47%/38% 57% 43% 62%;transform:rotate(1.8deg) scale(.972,1.034)}}
@keyframes liquidLight{0%,100%{background-position:18% 12%}28%{background-position:78% 24%}55%{background-position:66% 82%}78%{background-position:20% 68%}}
@keyframes breathe{50%{box-shadow:0 0 0 9px #30d15808}}@keyframes float{0%,100%{margin-top:0}50%{margin-top:-8px}}@keyframes localFloat{0%,100%{border-radius:45% 55% 52% 48%/52% 46% 54% 48%}50%{border-radius:53% 47% 45% 55%/46% 54% 48% 52%}}@keyframes connectedFloat{0%,100%{margin-top:0}50%{margin-top:-5px}}@keyframes merge{0%,100%{filter:brightness(1)}45%{filter:brightness(1.18);box-shadow:inset 8px 8px 20px #ffffff9c,0 0 0 18px #0a84ff12,0 18px 40px #147ee966}}@keyframes tetherPulse{50%{opacity:.16;r:24px}}@keyframes ringMorph{0%,100%{border-radius:48% 52% 46% 54%/52% 44% 56% 48%}50%{border-radius:55% 45% 52% 48%/44% 55% 45% 56%}}@keyframes drift{50%{transform:translate(35px,25px) scale(1.08)}}
@media(max-width:820px){body{padding:24px 12px 50px}.layout{grid-template-columns:1fr}.hero{align-items:flex-start}.hero h1{font-size:31px}.stage{height:420px}}@media(prefers-reduced-motion:reduce){*{animation:none!important;transition-duration:.01ms!important}}
@media(prefers-color-scheme:dark){:root{--ink:#f5f5f7;--muted:#a1a1a6;--card:rgba(35,35,38,.76);--line:rgba(235,235,245,.14);--stage:rgba(24,29,39,.82)}body{background:radial-gradient(circle at 18% 4%,#12344d 0,transparent 36%),radial-gradient(circle at 88% 12%,#29234d 0,transparent 34%),#101012}.card{border-color:#ffffff12;box-shadow:0 18px 60px #0005,inset 0 1px #ffffff12}.stage{background:linear-gradient(145deg,#222b38cc,#171a24d9);border-color:#ffffff12}.live{background:#153b20;color:#7ee787}input[type=text],input[type=number],select{background:#2c2c2ecc}.bubble{color:#dff7ff}}
</style>
</head>
<body>
<main>
  <header class="hero"><div><h1>ClipIt</h1><p>局域网剪贴板与文件流</p></div><span class="live">实时发现</span></header>
  <div class="layout">
    <section class="card network">
      <div class="card-head"><h2>附近设备</h2><span class="count" id="count">正在扫描…</span></div>
      <div class="stage" id="stage">
        <div class="orbit"></div><div class="orbit two"></div><div class="fusion-ring"></div>
        <svg class="tether-layer" id="tetherLayer" aria-hidden="true">
          <defs>
            <linearGradient id="tetherGradient" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#64d2ff" stop-opacity=".72"/><stop offset="1" stop-color="#5e5ce6" stop-opacity=".5"/></linearGradient>
            <filter id="tetherBlur"><feGaussianBlur stdDeviation="1.4"/></filter>
          </defs>
          <path id="tetherPath"></path><circle class="tether-pulse" id="tetherPulse" r="14"></circle>
        </svg>
        <div class="bubble local" id="local"><div><span class="node-emoji">📋</span><strong>本机</strong><small>ClipIt</small></div></div>
        <div class="empty" id="empty">同一局域网中的 ClipIt 设备会浮现在这里</div>
      </div>
      <div class="hint"><span>拖动设备水泡到本机水泡完成连接；向外拖动即可断开</span><span class="legend"><span><i class="dot"></i>可连接</span><span><i class="dot connected-dot"></i>已连接</span></span></div>
    </section>
    <aside class="card settings">
      <h2>运行设置</h2>
      <form class="form" method="get" action="/save">
        <input type="hidden" name="token" value="__TOKEN__">
        <div class="identity-grid">
          <label class="field">节点图标<input class="emoji-input" id="emojiInput" name="emoji" type="text" maxlength="24" value="__DEVICE_EMOJI__" aria-label="节点 Emoji" required></label>
          <label class="field">显示名称<input name="name" type="text" maxlength="48" value="__DEVICE_NAME__" autocomplete="off" required></label>
        </div>
        <div class="emoji-presets" aria-label="常用节点图标">
          <button class="emoji-preset" type="button" data-emoji="📋">📋</button><button class="emoji-preset" type="button" data-emoji="💻">💻</button><button class="emoji-preset" type="button" data-emoji="🖥️">🖥️</button><button class="emoji-preset" type="button" data-emoji="🍎">🍎</button><button class="emoji-preset" type="button" data-emoji="🚀">🚀</button><button class="emoji-preset" type="button" data-emoji="🐳">🐳</button><button class="emoji-preset" type="button" data-emoji="🏠">🏠</button><button class="emoji-preset" type="button" data-emoji="✨">✨</button>
        </div>
        <label class="field">文件传输端口<input name="port" type="number" min="1" max="65535" value="__PORT__" required></label>
        <label class="field">接收策略<select name="policy"><option value="confirm"__CONFIRM_SELECTED__>未知设备需要确认</option><option value="trusted-only"__TRUSTED_SELECTED__>仅可信设备</option><option value="accept-all"__ALL_SELECTED__>接受所有设备</option></select></label>
        <label class="switch"><input name="clipboard" type="checkbox" value="on"__CLIPBOARD_CHECKED__><span>自动同步文本、截图以及复制的文件和目录</span></label>
        <button class="save" type="submit">保存并重启服务</button>
        <p class="note">连接关系会在双方设备中保存 7 天，期间离线后再次上线会自动连接；到期后需重新拖动连接。文件类型不受限制；符号链接和特殊设备文件除外。</p>
      </form>
    </aside>
  </div>
</main>
<div class="toast" id="toast"></div>
<script>
const token="__TOKEN__",stage=document.getElementById("stage"),local=document.getElementById("local"),empty=document.getElementById("empty"),count=document.getElementById("count"),toast=document.getElementById("toast"),emojiInput=document.getElementById("emojiInput"),tetherLayer=document.getElementById("tetherLayer"),tetherPath=document.getElementById("tetherPath"),tetherPulse=document.getElementById("tetherPulse");
const bubbles=new Map(),reduceMotion=matchMedia("(prefers-reduced-motion: reduce)").matches;let snapshot=null,drag=null,toastTimer;
function hash(value){let h=2166136261;for(const c of value){h^=c.charCodeAt(0);h=Math.imul(h,16777619)}return h>>>0}
function positionFor(device,index){const rect=stage.getBoundingClientRect(),angle=((hash(device.id)%360)+index*37)*Math.PI/180;if(device.connected){const radius=112;return{x:rect.width/2+Math.cos(angle)*radius,y:rect.height/2+Math.sin(angle)*radius}}const rx=Math.max(135,rect.width*.38),ry=Math.max(118,rect.height*.36);return{x:rect.width/2+Math.cos(angle)*rx,y:rect.height/2+Math.sin(angle)*ry}}
function notify(message){toast.textContent=message;toast.classList.add("show");clearTimeout(toastTimer);toastTimer=setTimeout(()=>toast.classList.remove("show"),2200)}
async function action(kind,id){const response=await fetch(`/api/${kind}?token=${token}&id=${id}`,{cache:"no-store"});const data=await response.json();if(!response.ok)throw new Error(data.message||"操作失败");notify(data.message);await refresh(kind==="connect"?id:null)}
function createBubble(device){const el=document.createElement("button");el.type="button";el.className="bubble";el.dataset.id=device.id;el.style.setProperty("--phase",`${-(hash(device.id)%3600)/1000}s`);el.innerHTML="<div><span class=\"node-emoji\"></span><strong></strong><small></small></div>";el.addEventListener("pointerdown",startDrag);stage.appendChild(el);bubbles.set(device.id,el);return el}
function resetDeformation(el){if(drag&&drag.el===el)return;el.classList.remove("pointer-reacting");el.style.setProperty("--lean-x","0px");el.style.setProperty("--lean-y","0px");el.style.setProperty("--stretch-x","1");el.style.setProperty("--stretch-y","1")}
function render(mergeId){if(!snapshot)return;local.querySelector(".node-emoji").textContent=snapshot.local.emoji;local.querySelector("strong").textContent=snapshot.local.name;local.querySelector("small").textContent=`端口 ${snapshot.local.port}`;const active=new Set();let connected=0;snapshot.devices.forEach((device,index)=>{active.add(device.id);const el=bubbles.get(device.id)||createBubble(device);el.querySelector(".node-emoji").textContent=device.emoji;el.querySelector("strong").textContent=device.name;el.querySelector("small").textContent=device.online?(device.connected?"已连接":device.address):"已离线";el.classList.toggle("connected",device.connected);el.classList.toggle("offline",!device.online);el.disabled=!device.online;if(device.connected)connected++;if(!drag||drag.el!==el){const p=positionFor(device,index);el.style.left=`${p.x}px`;el.style.top=`${p.y}px`}if(device.id===mergeId){el.classList.add("merging");setTimeout(()=>el.classList.remove("merging"),850)}});for(const[id,el]of bubbles){if(!active.has(id)){el.remove();bubbles.delete(id)}}empty.style.opacity=snapshot.devices.length?0:1;count.textContent=`${snapshot.devices.filter(x=>x.online).length} 台在线 · ${connected} 台已连接`;stage.classList.toggle("has-connection",connected>0)}
async function refresh(mergeId=null){try{const response=await fetch(`/api/devices?token=${token}`,{cache:"no-store"});snapshot=await response.json();render(mergeId)}catch(error){count.textContent="扫描暂时中断"}}
function bubbleCenter(el,stageRect){const rect=el.getBoundingClientRect();return{x:rect.left-stageRect.left+rect.width/2,y:rect.top-stageRect.top+rect.height/2}}
function reactToPointer(event){if(drag||reduceMotion)return;const rect=stage.getBoundingClientRect(),px=event.clientX-rect.left,py=event.clientY-rect.top;for(const el of [local,...bubbles.values()]){if(el.disabled)continue;const center=bubbleCenter(el,rect),dx=px-center.x,dy=py-center.y,distance=Math.hypot(dx,dy),radius=el===local?185:145;if(distance>=radius){resetDeformation(el);continue}const force=1-distance/radius,nx=dx/(distance||1),ny=dy/(distance||1),lean=force*(el===local?9:12),stretch=force*(el===local?.055:.1);el.classList.add("pointer-reacting");el.style.setProperty("--lean-x",`${(nx*lean).toFixed(2)}px`);el.style.setProperty("--lean-y",`${(ny*lean).toFixed(2)}px`);el.style.setProperty("--stretch-x",(1+Math.abs(nx)*stretch-Math.abs(ny)*stretch*.24).toFixed(3));el.style.setProperty("--stretch-y",(1+Math.abs(ny)*stretch-Math.abs(nx)*stretch*.24).toFixed(3))}}
function clearPointerReaction(){if(drag)return;for(const el of [local,...bubbles.values()])resetDeformation(el)}
function startDrag(event){const el=event.currentTarget;if(el.disabled)return;event.preventDefault();const rect=stage.getBoundingClientRect(),center=bubbleCenter(el,rect),pointerX=event.clientX-rect.left,pointerY=event.clientY-rect.top;clearPointerReaction();drag={el,id:el.dataset.id,rect,anchorX:center.x,anchorY:center.y,currentX:center.x,currentY:center.y,targetX:center.x,targetY:center.y,grabX:center.x-pointerX,grabY:center.y-pointerY,frame:0,snapped:false};el.classList.remove("rebounding","pointer-reacting");el.classList.add("dragging");el.setPointerCapture(event.pointerId);window.addEventListener("pointermove",moveDrag);window.addEventListener("pointerup",endDrag,{once:true});window.addEventListener("pointercancel",endDrag,{once:true});moveDrag(event)}
function moveDrag(event){if(!drag)return;const margin=44,x=event.clientX-drag.rect.left+drag.grabX,y=event.clientY-drag.rect.top+drag.grabY;drag.targetX=Math.max(margin,Math.min(drag.rect.width-margin,x));drag.targetY=Math.max(margin,Math.min(drag.rect.height-margin,y));if(!drag.frame)drag.frame=requestAnimationFrame(animateDrag)}
function animateDrag(){if(!drag)return;drag.frame=0;const follow=reduceMotion?1:.34;drag.currentX+=(drag.targetX-drag.currentX)*follow;drag.currentY+=(drag.targetY-drag.currentY)*follow;const dx=drag.currentX-drag.anchorX,dy=drag.currentY-drag.anchorY,distance=Math.hypot(dx,dy),nx=dx/(distance||1),ny=dy/(distance||1),lag=Math.hypot(drag.targetX-drag.currentX,drag.targetY-drag.currentY),stretch=reduceMotion?0:Math.min(.19,distance*.00072+lag*.0032);drag.el.style.left=`${drag.currentX}px`;drag.el.style.top=`${drag.currentY}px`;drag.el.style.setProperty("--stretch-x",(1+Math.abs(nx)*stretch-Math.abs(ny)*stretch*.28).toFixed(3));drag.el.style.setProperty("--stretch-y",(1+Math.abs(ny)*stretch-Math.abs(nx)*stretch*.28).toFixed(3));updateTether(distance,nx,ny);if(lag>.45)drag.frame=requestAnimationFrame(animateDrag)}
function updateTether(distance,nx,ny){if(!drag||reduceMotion||distance<8){tetherLayer.classList.remove("active");return}if(distance>215){drag.snapped=true;tetherLayer.classList.remove("active");tetherLayer.classList.add("snapped");return}if(drag.snapped&&distance>185)return;drag.snapped=false;tetherLayer.classList.remove("snapped");tetherLayer.classList.add("active");const neck=Math.max(5,22-distance*.075),bx=drag.currentX,by=drag.currentY,ax=drag.anchorX,ay=drag.anchorY,px=-ny,py=nx,a1x=ax+px*neck,a1y=ay+py*neck,a2x=ax-px*neck,a2y=ay-py*neck,b1x=bx+px*neck*.72,b1y=by+py*neck*.72,b2x=bx-px*neck*.72,b2y=by-py*neck*.72,c1x=ax+(bx-ax)*.42,c1y=ay+(by-ay)*.42,c2x=ax+(bx-ax)*.62,c2y=ay+(by-ay)*.62;tetherPath.setAttribute("d",`M ${a1x} ${a1y} C ${c1x+px*neck} ${c1y+py*neck},${c2x+px*neck*.72} ${c2y+py*neck*.72},${b1x} ${b1y} L ${b2x} ${b2y} C ${c2x-px*neck*.72} ${c2y-py*neck*.72},${c1x-px*neck} ${c1y-py*neck},${a2x} ${a2y} Z`);tetherPulse.setAttribute("cx",ax);tetherPulse.setAttribute("cy",ay)}
function hideTether(){tetherLayer.classList.remove("active","snapped");tetherPath.removeAttribute("d")}
async function endDrag(){if(!drag)return;const current=drag,device=snapshot.devices.find(x=>x.id===current.id);if(current.frame)cancelAnimationFrame(current.frame);current.el.classList.remove("dragging");current.el.classList.add("rebounding");window.removeEventListener("pointermove",moveDrag);window.removeEventListener("pointerup",endDrag);window.removeEventListener("pointercancel",endDrag);const localCenter=bubbleCenter(local,current.rect),distanceToLocal=Math.hypot(current.targetX-localCenter.x,current.targetY-localCenter.y);drag=null;hideTether();resetDeformation(current.el);setTimeout(()=>current.el.classList.remove("rebounding"),720);try{if(device&&!device.connected&&distanceToLocal<145)await action("connect",device.id);else if(device&&device.connected&&distanceToLocal>185)await action("disconnect",device.id);else render()}catch(error){notify(error.message);render()}}
stage.addEventListener("pointermove",reactToPointer);stage.addEventListener("pointerleave",clearPointerReaction);document.querySelectorAll(".emoji-preset").forEach(button=>button.addEventListener("click",()=>{emojiInput.value=button.dataset.emoji;emojiInput.focus()}));
refresh();setInterval(()=>{if(!drag)refresh()},1400);addEventListener("resize",()=>{clearPointerReaction();render()});
</script>
</body></html>"##;

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
    fn settings_page_contains_bubble_controls_without_placeholders() {
        let page = settings_page(&Settings::default(), "token123");
        assert!(page.contains("class=\"bubble local\""));
        assert!(page.contains("/api/${kind}"));
        assert!(page.contains("id=\"tetherPath\""));
        assert!(page.contains("requestAnimationFrame(animateDrag)"));
        assert!(page.contains("reactToPointer"));
        assert!(!page.contains("__TOKEN__"));
        assert!(!page.contains("__DEVICE_NAME__"));
        assert!(!page.contains("__DEVICE_EMOJI__"));
    }
}
