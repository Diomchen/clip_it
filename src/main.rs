mod clipboard;
mod config;
mod confirmation;
mod discovery;
mod integration;
mod picker;
mod protocol;
mod settings_ui;
mod transfer;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod tray;

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    config::{AppConfig, ReceivePolicy, TrustedDevice},
    discovery::{Discovery, Peer},
    transfer::{receive_loop, run_benchmark, send_paths},
};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the receiver and advertise this device on the LAN.
    Serve {
        /// How incoming transfers from untrusted devices are handled.
        #[arg(long, value_enum)]
        receive_policy: Option<ReceivePolicy>,
    },
    /// List ClipIt devices visible on the LAN.
    Devices {
        #[arg(long, default_value_t = 2)]
        timeout: u64,
    },
    /// Send one or more files/directories to an address or discovered device.
    Send {
        #[arg(long, value_name = "IP:PORT", conflicts_with = "device")]
        to: Option<SocketAddr>,
        #[arg(long, value_name = "NAME", conflicts_with = "to")]
        device: Option<String>,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Measure memory-to-memory LAN throughput with parallel TCP streams.
    Benchmark {
        #[arg(long, value_name = "IP:PORT", conflicts_with = "device")]
        to: Option<SocketAddr>,
        #[arg(long, value_name = "NAME", conflicts_with = "to")]
        device: Option<String>,
        #[arg(long, default_value_t = 1, value_name = "GiB")]
        size_gib: u64,
        #[arg(long, default_value_t = 4)]
        streams: usize,
    },
    /// Open the browser-based device picker (used by the context menu).
    Pick {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Install or remove the native file-manager context-menu entry.
    Integrate {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    /// Manage devices that may send without an interactive confirmation.
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Open the loopback-only settings page.
    Configure,
    /// Manage login startup for the tray application.
    Startup {
        #[command(subcommand)]
        action: StartupAction,
    },
}

#[derive(Debug, Subcommand)]
enum IntegrationAction {
    Install,
    Remove,
}

#[derive(Debug, Subcommand)]
enum TrustAction {
    /// List trusted devices.
    List,
    /// Add or rename a trusted device by UUID.
    Add {
        #[arg(value_name = "DEVICE_ID")]
        id: uuid::Uuid,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Remove a trusted device by UUID.
    Remove {
        #[arg(value_name = "DEVICE_ID")]
        id: uuid::Uuid,
    },
    /// Remove every device from the trusted list.
    Clear,
}

#[derive(Debug, Subcommand)]
enum StartupAction {
    Install,
    Remove,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load_or_create()?;

    match cli.command {
        None => run_default(config).await?,
        Some(Command::Serve { receive_policy }) => {
            let policy = receive_policy.unwrap_or(config.settings.receive_policy);
            serve(config, policy).await?;
        }
        Some(Command::Devices { timeout }) => {
            let peers = discover(Duration::from_secs(timeout), config.identity.id).await?;
            if peers.is_empty() {
                println!("未发现设备；请确认对方正在运行 `clip-it serve`。");
            } else {
                for peer in peers {
                    println!("{} {}\t{}\t{}", peer.emoji, peer.name, peer.addr, peer.id);
                }
            }
        }
        Some(Command::Send { to, device, paths }) => {
            let target = resolve_target(to, device.as_deref(), config.identity.id).await?;
            let receipt = send_paths(target, &paths, &config.identity).await?;
            println!(
                "已发送 {} 个文件，共 {} 字节到 {}",
                receipt.files, receipt.bytes, target
            );
        }
        Some(Command::Benchmark {
            to,
            device,
            size_gib,
            streams,
        }) => {
            let target = resolve_target(to, device.as_deref(), config.identity.id).await?;
            let bytes = size_gib
                .checked_mul(1024 * 1024 * 1024)
                .context("基准大小溢出")?;
            let receipt = run_benchmark(target, bytes, streams, &config.identity).await?;
            println!(
                "已发送 {:.2} GiB，{} 条并发流，用时 {:.3} 秒，吞吐 {:.2} Gbit/s",
                receipt.bytes as f64 / 1024_f64.powi(3),
                streams,
                receipt.elapsed.as_secs_f64(),
                receipt.gigabits_per_second
            );
        }
        Some(Command::Pick { paths }) => picker::run(paths, config.identity).await?,
        Some(Command::Integrate { action }) => match action {
            IntegrationAction::Install => integration::install()?,
            IntegrationAction::Remove => integration::remove()?,
        },
        Some(Command::Trust { action }) => manage_trust(&config, action)?,
        Some(Command::Configure) => settings_ui::run(config).await?,
        Some(Command::Startup { action }) => manage_startup(action)?,
    }

    Ok(())
}

async fn serve(config: AppConfig, policy: ReceivePolicy) -> Result<()> {
    println!(
        "ClipIt {} 正在监听 {}",
        config.device_name,
        config.listen_addr()
    );
    let clipboard = clipboard::ClipboardBridge::new();
    if config.settings.clipboard_sync {
        clipboard::start(config.clone(), clipboard.clone())?;
        println!("文本及文件剪贴板自动同步已开启");
    }
    let discovery = Discovery::new(config.identity.clone())?;
    tokio::try_join!(
        discovery.run_announcer(),
        receive_loop(config, policy, clipboard)
    )?;
    Ok(())
}

async fn run_default(config: AppConfig) -> Result<()> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        if let Err(error) = integration::install() {
            eprintln!("安装文件右键菜单失败: {error:#}");
        }
        if let Err(error) = integration::startup_install() {
            eprintln!("启用登录启动失败: {error:#}");
        }
        // Integration prints status messages, so keep the console handles valid
        // until it finishes. Detaching earlier makes release builds abort when
        // println! writes to an invalid Windows console handle.
        #[cfg(target_os = "windows")]
        detach_windows_console();
        tray::run(config)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let policy = config.settings.receive_policy;
        serve(config, policy).await
    }
}

#[cfg(target_os = "windows")]
fn detach_windows_console() {
    if std::env::var_os("CLIP_IT_KEEP_CONSOLE").is_some() {
        return;
    }
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }
    unsafe {
        FreeConsole();
    }
}

fn manage_startup(action: StartupAction) -> Result<()> {
    match action {
        StartupAction::Install => {
            integration::startup_install()?;
            println!("已启用登录时自动启动 ClipIt");
        }
        StartupAction::Remove => {
            integration::startup_remove()?;
            println!("已关闭登录时自动启动 ClipIt");
        }
        StartupAction::Status => println!(
            "登录启动：{}",
            if integration::startup_enabled() {
                "已启用"
            } else {
                "未启用"
            }
        ),
    }
    Ok(())
}

fn manage_trust(config: &AppConfig, action: TrustAction) -> Result<()> {
    match action {
        TrustAction::List => {
            let devices = config.trusted_devices.list()?;
            if devices.is_empty() {
                println!("可信设备列表为空。");
            } else {
                for device in devices {
                    println!("{}\t{}", device.name, device.id);
                }
            }
        }
        TrustAction::Add { id, name } => {
            if id == config.identity.id {
                anyhow::bail!("不能把本机加入可信设备列表");
            }
            let name = name.unwrap_or_else(|| id.to_string());
            let name = name.trim();
            if name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control) {
                anyhow::bail!("设备名称必须为 1 到 128 个可见字符");
            }
            config.trusted_devices.add(TrustedDevice {
                id,
                name: name.into(),
            })?;
            println!("已信任 {name} ({id})");
        }
        TrustAction::Remove { id } => {
            if config.trusted_devices.remove(id)? {
                println!("已移除可信设备 {id}");
            } else {
                println!("可信设备列表中没有 {id}");
            }
        }
        TrustAction::Clear => {
            let count = config.trusted_devices.clear()?;
            println!("已清空可信设备列表（移除 {count} 项）");
        }
    }
    Ok(())
}

async fn discover(timeout: Duration, own_id: uuid::Uuid) -> Result<Vec<Peer>> {
    let mut peers = Discovery::listen(timeout).await?;
    peers.retain(|peer| peer.id != own_id);
    Ok(peers)
}

async fn resolve_target(
    to: Option<SocketAddr>,
    device: Option<&str>,
    own_id: uuid::Uuid,
) -> Result<SocketAddr> {
    if let Some(addr) = to {
        return Ok(addr);
    }

    let peers = discover(Duration::from_secs(3), own_id).await?;
    match device {
        Some(name) => peers
            .into_iter()
            .find(|peer| peer.name.eq_ignore_ascii_case(name) || peer.id.to_string() == name)
            .map(|peer| peer.addr)
            .with_context(|| format!("未发现设备 {name}")),
        None if peers.len() == 1 => Ok(peers[0].addr),
        None if peers.is_empty() => anyhow::bail!("未发现 ClipIt 设备"),
        None => anyhow::bail!("发现多个设备，请使用 --device NAME 或 --to IP:PORT"),
    }
}
