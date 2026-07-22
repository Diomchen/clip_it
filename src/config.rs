use std::{
    collections::BTreeMap,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
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
    pub trusted_devices: TrustedDevices,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReceivePolicy {
    /// Ask in a loopback-only browser page for devices that are not trusted.
    #[default]
    Confirm,
    /// Accept transfers only from devices in the trusted-device list.
    TrustedOnly,
    /// Accept every transfer without confirmation (legacy behavior).
    AcceptAll,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedDevice {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct TrustedDevices {
    path: Arc<PathBuf>,
    entries: Arc<Mutex<BTreeMap<Uuid, String>>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustedDevicesFile {
    devices: Vec<TrustedDevice>,
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
        let trusted_devices = TrustedDevices::load(config_dir.join("trusted-devices.json"))?;

        Ok(Self {
            device_name: identity.name.clone(),
            identity,
            download_dir,
            trusted_devices,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            self.identity.transfer_port,
        )
    }
}

impl TrustedDevices {
    fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            if fs::metadata(&path)?.len() > 1024 * 1024 {
                bail!("可信设备列表过大");
            }
            let bytes = fs::read(&path).context("读取可信设备列表失败")?;
            let stored: TrustedDevicesFile =
                serde_json::from_slice(&bytes).context("可信设备列表格式错误")?;
            stored
                .devices
                .into_iter()
                .map(|device| (device.id, device.name))
                .collect()
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path: Arc::new(path),
            entries: Arc::new(Mutex::new(entries)),
        })
    }

    pub fn contains(&self, id: Uuid) -> Result<bool> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("可信设备列表锁已损坏"))?
            .contains_key(&id))
    }

    pub fn list(&self) -> Result<Vec<TrustedDevice>> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("可信设备列表锁已损坏"))?
            .iter()
            .map(|(id, name)| TrustedDevice {
                id: *id,
                name: name.clone(),
            })
            .collect())
    }

    pub fn add(&self, device: TrustedDevice) -> Result<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("可信设备列表锁已损坏"))?;
        let mut updated = entries.clone();
        updated.insert(device.id, device.name);
        self.save(&updated)?;
        *entries = updated;
        Ok(())
    }

    pub fn remove(&self, id: Uuid) -> Result<bool> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("可信设备列表锁已损坏"))?;
        let mut updated = entries.clone();
        let removed = updated.remove(&id).is_some();
        if removed {
            self.save(&updated)?;
            *entries = updated;
        }
        Ok(removed)
    }

    pub fn clear(&self) -> Result<usize> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow::anyhow!("可信设备列表锁已损坏"))?;
        let count = entries.len();
        if count > 0 {
            self.save(&BTreeMap::new())?;
            entries.clear();
        }
        Ok(count)
    }

    fn save(&self, entries: &BTreeMap<Uuid, String>) -> Result<()> {
        let parent = self.path.parent().context("可信设备列表路径无效")?;
        fs::create_dir_all(parent).context("创建 ClipIt 配置目录失败")?;
        let stored = TrustedDevicesFile {
            devices: entries
                .iter()
                .map(|(id, name)| TrustedDevice {
                    id: *id,
                    name: name.clone(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&stored)?;
        if bytes.len() > 1024 * 1024 {
            bail!("可信设备列表过大");
        }
        fs::write(self.path.as_ref(), bytes).context("保存可信设备列表失败")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_devices_round_trip() {
        let root = std::env::temp_dir().join(format!("clip-it-trust-test-{}", Uuid::new_v4()));
        let path = root.join("trusted-devices.json");
        let store = TrustedDevices::load(path.clone()).unwrap();
        let id = Uuid::new_v4();

        assert!(!store.contains(id).unwrap());
        store
            .add(TrustedDevice {
                id,
                name: "测试设备".into(),
            })
            .unwrap();
        assert!(store.contains(id).unwrap());
        assert_eq!(store.list().unwrap()[0].name, "测试设备");

        let reloaded = TrustedDevices::load(path).unwrap();
        assert!(reloaded.contains(id).unwrap());
        assert!(reloaded.remove(id).unwrap());
        assert!(!reloaded.remove(id).unwrap());
        let _ = fs::remove_dir_all(root);
    }
}
