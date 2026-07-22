use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::TRANSFER_PORT;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub id: Uuid,
    pub name: String,
    pub transfer_port: u16,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub identity: Identity,
    pub device_name: String,
    pub download_dir: PathBuf,
}

impl AppConfig {
    pub fn load_or_create() -> Result<Self> {
        let config_dir = std::env::var_os("CLIP_IT_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|path| path.join("clip-it")))
            .context("无法确定用户配置目录")?;
        let identity_path = config_dir.join("identity.json");

        let identity = if identity_path.exists() {
            let bytes = fs::read(&identity_path).context("读取 ClipIt 身份配置失败")?;
            serde_json::from_slice(&bytes).context("ClipIt 身份配置格式错误")?
        } else {
            fs::create_dir_all(&config_dir).context("创建 ClipIt 配置目录失败")?;
            let name = hostname::get()
                .unwrap_or_default()
                .to_string_lossy()
                .trim()
                .to_owned();
            let identity = Identity {
                id: Uuid::new_v4(),
                name: if name.is_empty() {
                    "ClipIt Device".into()
                } else {
                    name
                },
                transfer_port: TRANSFER_PORT,
            };
            fs::write(&identity_path, serde_json::to_vec_pretty(&identity)?)
                .context("保存 ClipIt 身份配置失败")?;
            identity
        };

        let download_dir = std::env::var_os("CLIP_IT_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                dirs::download_dir()
                    .or_else(dirs::home_dir)
                    .map(|path| path.join("ClipIt"))
            })
            .context("无法确定接收目录")?;

        Ok(Self {
            device_name: identity.name.clone(),
            identity,
            download_dir,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            self.identity.transfer_port,
        )
    }
}
