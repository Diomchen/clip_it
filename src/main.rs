mod config;
mod discovery;
mod integration;
mod picker;
mod protocol;
mod transfer;

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    config::AppConfig,
    discovery::{Discovery, Peer},
    transfer::{receive_loop, send_paths},
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
    Serve,
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
}

#[derive(Debug, Subcommand)]
enum IntegrationAction {
    Install,
    Remove,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = AppConfig::load_or_create()?;

    match cli.command {
        None | Some(Command::Serve) => serve(config).await?,
        Some(Command::Devices { timeout }) => {
            let peers = discover(Duration::from_secs(timeout), config.identity.id).await?;
            if peers.is_empty() {
                println!("未发现设备；请确认对方正在运行 `clip-it serve`。");
            } else {
                for peer in peers {
                    println!("{}\t{}\t{}", peer.name, peer.addr, peer.id);
                }
            }
        }
        Some(Command::Send { to, device, paths }) => {
            let target = resolve_target(to, device.as_deref(), config.identity.id).await?;
            let receipt = send_paths(target, &paths).await?;
            println!(
                "已发送 {} 个文件，共 {} 字节到 {}",
                receipt.files, receipt.bytes, target
            );
        }
        Some(Command::Pick { paths }) => picker::run(paths, config.identity.id).await?,
        Some(Command::Integrate { action }) => match action {
            IntegrationAction::Install => integration::install()?,
            IntegrationAction::Remove => integration::remove()?,
        },
    }

    Ok(())
}

async fn serve(config: AppConfig) -> Result<()> {
    println!(
        "ClipIt {} 正在监听 {}",
        config.device_name,
        config.listen_addr()
    );
    let discovery = Discovery::new(config.identity.clone())?;
    tokio::try_join!(discovery.run_announcer(), receive_loop(config))?;
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
