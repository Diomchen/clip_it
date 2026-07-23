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
    #[serde(default = "crate::protocol::default_device_emoji")]
    pub emoji: String,
    pub transfer_port: u16,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub config_dir: PathBuf,
    pub identity: Identity,
    pub device_name: String,
    pub download_dir: PathBuf,
    pub settings: Settings,
    pub trusted_devices: TrustedDevices,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    #[serde(default)]
    pub device_name: String,
    #[serde(default = "crate::protocol::default_device_emoji")]
    pub device_emoji: String,
    pub transfer_port: u16,
    pub receive_policy: ReceivePolicy,
    pub clipboard_sync: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device_name: "ClipIt Device".into(),
            device_emoji: crate::protocol::default_device_emoji(),
            transfer_port: TRANSFER_PORT,
            receive_policy: ReceivePolicy::Confirm,
            clipboard_sync: true,
        }
    }
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

        let mut identity = if identity_path.exists() {
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
                emoji: crate::protocol::default_device_emoji(),
                transfer_port: TRANSFER_PORT,
            };
            fs::write(&identity_path, serde_json::to_vec_pretty(&identity)?)
                .context("保存 ClipIt 身份配置失败")?;
            identity
        };

        let settings_path = config_dir.join("settings.json");
        let mut settings = if settings_path.exists() {
            let bytes = fs::read(&settings_path).context("读取 ClipIt 设置失败")?;
            serde_json::from_slice::<Settings>(&bytes).context("ClipIt 设置格式错误")?
        } else {
            Settings {
                device_name: identity.name.clone(),
                device_emoji: identity.emoji.clone(),
                transfer_port: identity.transfer_port,
                ..Settings::default()
            }
        };
        if settings.device_name.trim().is_empty() {
            settings.device_name = identity.name.clone();
        }
        validate_settings(&settings)?;
        identity.name.clone_from(&settings.device_name);
        identity.emoji.clone_from(&settings.device_emoji);
        identity.transfer_port = settings.transfer_port;

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
            config_dir,
            device_name: identity.name.clone(),
            identity,
            download_dir,
            settings,
            trusted_devices,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            self.identity.transfer_port,
        )
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        validate_settings(settings)?;
        fs::create_dir_all(&self.config_dir).context("创建 ClipIt 配置目录失败")?;
        fs::write(
            self.config_dir.join("settings.json"),
            serde_json::to_vec_pretty(settings)?,
        )
        .context("保存 ClipIt 设置失败")
    }
}

fn validate_settings(settings: &Settings) -> Result<()> {
    let name = settings.device_name.trim();
    if name.is_empty() || name.chars().count() > 48 || name.chars().any(char::is_control) {
        bail!("设备名称不能为空、不能包含控制字符，且最多 48 个字符");
    }
    let emoji = settings.device_emoji.trim();
    if emoji.is_empty() || emoji.chars().count() > 12 || emoji.chars().any(char::is_control) {
        bail!("节点图标不能为空、不能包含控制字符，且最多 12 个字符");
    }
    if settings.transfer_port == 0
        || [
            crate::protocol::DISCOVERY_PORT,
            crate::protocol::TRAY_INSTANCE_PORT,
        ]
        .contains(&settings.transfer_port)
    {
        bail!("传输端口必须为 1-65535，且不能使用 ClipIt 保留端口");
    }
    Ok(())
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

    #[test]
    fn rejects_reserved_transfer_ports() {
        let settings = Settings {
            transfer_port: 0,
            ..Settings::default()
        };
        assert!(validate_settings(&settings).is_err());
        let settings = Settings {
            transfer_port: crate::protocol::DISCOVERY_PORT,
            ..Settings::default()
        };
        assert!(validate_settings(&settings).is_err());
        let settings = Settings {
            transfer_port: crate::protocol::TRAY_INSTANCE_PORT,
            ..Settings::default()
        };
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn validates_custom_node_identity() {
        let settings = Settings {
            device_name: "客厅 Mac".into(),
            device_emoji: "🍎".into(),
            ..Settings::default()
        };
        assert!(validate_settings(&settings).is_ok());

        let settings = Settings {
            device_name: " ".into(),
            ..Settings::default()
        };
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn old_settings_receive_profile_defaults() {
        let settings: Settings = serde_json::from_str(
            r#"{"transfer_port":42490,"receive_policy":"confirm","clipboard_sync":true}"#,
        )
        .unwrap();
        assert!(settings.device_name.is_empty());
        assert_eq!(settings.device_emoji, "📋");

        let identity: Identity = serde_json::from_str(
            r#"{"id":"00000000-0000-4000-8000-000000000001","name":"旧电脑","transfer_port":42490}"#,
        )
        .unwrap();
        assert_eq!(identity.name, "旧电脑");
        assert_eq!(identity.emoji, "📋");
    }
}
