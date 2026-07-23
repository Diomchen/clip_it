use std::path::PathBuf;

use anyhow::Result;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use anyhow::bail;

use crate::config::AppConfig;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop {
    use std::{
        collections::{HashSet, VecDeque},
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result};
    use clipboard_rs::{
        Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
        ContentFormat, RustImageData, common::RustImage,
    };
    use tokio::sync::mpsc;
    use uuid::Uuid;

    use crate::{
        config::AppConfig,
        discovery::Discovery,
        protocol::{MAX_CLIPBOARD_IMAGE_BYTES, MAX_CLIPBOARD_IMAGE_PIXELS},
        transfer::{send_clipboard_image, send_clipboard_text, send_paths_with_intent},
    };

    use super::ClipboardBridge;

    const DUPLICATE_WINDOW: Duration = Duration::from_millis(800);
    const REMOTE_SUPPRESSION_WINDOW: Duration = Duration::from_secs(5);
    const MAX_RECENT_FINGERPRINTS: usize = 128;

    #[derive(Default)]
    pub(super) struct ClipboardState {
        recent_local: VecDeque<([u8; 32], Instant)>,
        recent_remote: VecDeque<([u8; 32], Instant)>,
    }

    #[derive(Clone, Debug)]
    enum LocalClipboardEvent {
        Text {
            event_id: Uuid,
            text: String,
        },
        Image {
            event_id: Uuid,
            png: Arc<[u8]>,
            width: u32,
            height: u32,
        },
        Files {
            paths: Vec<PathBuf>,
        },
    }

    struct Handler {
        context: ClipboardContext,
        state: Arc<Mutex<ClipboardState>>,
        sender: mpsc::UnboundedSender<LocalClipboardEvent>,
    }

    impl ClipboardHandler for Handler {
        fn on_clipboard_change(&mut self) {
            if self.context.has(ContentFormat::Files) {
                let paths = self
                    .context
                    .get_files()
                    .unwrap_or_default()
                    .into_iter()
                    .map(PathBuf::from)
                    .filter(|path| path.exists())
                    .collect::<Vec<_>>();
                if paths.is_empty() {
                    return;
                }
                let fingerprint = files_fingerprint(&paths);
                if should_skip(&self.state, fingerprint) {
                    return;
                }
                let _ = self.sender.send(LocalClipboardEvent::Files { paths });
                return;
            }

            if let Ok(image) = self.context.get_image() {
                let (width, height) = image.get_size();
                let pixels = u64::from(width).saturating_mul(u64::from(height));
                if width == 0 || height == 0 || pixels > MAX_CLIPBOARD_IMAGE_PIXELS {
                    eprintln!("剪贴板图片尺寸无效或超过限制: {width}x{height}");
                    return;
                }
                let Ok(rgba) = image.to_rgba8() else {
                    eprintln!("读取剪贴板图片像素失败");
                    return;
                };
                let fingerprint = image_fingerprint(width, height, rgba.as_raw());
                if should_skip(&self.state, fingerprint) {
                    return;
                }
                let Ok(png) = image.to_png() else {
                    eprintln!("将剪贴板图片编码为 PNG 失败");
                    return;
                };
                if png.get_bytes().len() > MAX_CLIPBOARD_IMAGE_BYTES {
                    eprintln!(
                        "剪贴板图片超过 {} MiB，已跳过同步",
                        MAX_CLIPBOARD_IMAGE_BYTES / 1024 / 1024
                    );
                    return;
                }
                let _ = self.sender.send(LocalClipboardEvent::Image {
                    event_id: Uuid::new_v4(),
                    png: Arc::from(png.get_bytes()),
                    width,
                    height,
                });
                return;
            }

            if self.context.has(ContentFormat::Text)
                && let Ok(text) = self.context.get_text()
                && !text.is_empty()
            {
                let fingerprint = text_fingerprint(&text);
                if should_skip(&self.state, fingerprint) {
                    return;
                }
                let _ = self.sender.send(LocalClipboardEvent::Text {
                    event_id: Uuid::new_v4(),
                    text,
                });
            }
        }
    }

    pub(super) fn start(config: AppConfig, bridge: ClipboardBridge) -> Result<()> {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let handler = Handler {
            context: ClipboardContext::new()
                .map_err(|error| anyhow::anyhow!("打开系统剪贴板失败: {error}"))?,
            state: Arc::clone(&bridge.state),
            sender,
        };
        let mut watcher = ClipboardWatcherContext::new()
            .map_err(|error| anyhow::anyhow!("创建剪贴板监听器失败: {error}"))?;
        watcher.add_handler(handler);
        std::thread::Builder::new()
            .name("clip-it-clipboard".into())
            .spawn(move || watcher.start_watch())
            .context("启动剪贴板监听线程失败")?;

        tokio::spawn(async move {
            while let Some(mut event) = receiver.recv().await {
                while let Ok(newer) = receiver.try_recv() {
                    event = newer;
                }
                let peers = match Discovery::listen(Duration::from_millis(1400)).await {
                    Ok(peers) => peers,
                    Err(error) => {
                        eprintln!("剪贴板同步发现设备失败: {error:#}");
                        continue;
                    }
                };
                let paired_ids = match config.paired_devices.list() {
                    Ok(devices) => devices
                        .into_iter()
                        .map(|device| device.id)
                        .collect::<HashSet<_>>(),
                    Err(error) => {
                        eprintln!("读取已连接设备列表失败: {error:#}");
                        continue;
                    }
                };
                let peers = peers
                    .into_iter()
                    .filter(|peer| peer.id != config.identity.id && paired_ids.contains(&peer.id))
                    .collect::<Vec<_>>();
                if peers.is_empty() {
                    eprintln!("剪贴板已变化，但没有已连接且在线的 ClipIt 设备");
                    continue;
                }

                for peer in peers {
                    let identity = config.identity.clone();
                    let event = event.clone();
                    tokio::spawn(async move {
                        let result = match event {
                            LocalClipboardEvent::Text { event_id, text } => {
                                send_clipboard_text(peer.addr, &text, event_id, &identity).await
                            }
                            LocalClipboardEvent::Image {
                                event_id,
                                png,
                                width,
                                height,
                            } => {
                                send_clipboard_image(
                                    peer.addr,
                                    png.as_ref(),
                                    width,
                                    height,
                                    event_id,
                                    &identity,
                                )
                                .await
                            }
                            LocalClipboardEvent::Files { paths } => send_paths_with_intent(
                                peer.addr,
                                &paths,
                                &identity,
                                crate::protocol::TransferIntent::Clipboard,
                            )
                            .await
                            .map(|_| ()),
                        };
                        if let Err(error) = result {
                            eprintln!("自动同步剪贴板到 {} 失败: {error:#}", peer.name);
                        }
                    });
                }
            }
        });
        Ok(())
    }

    pub(super) fn apply_text(bridge: &ClipboardBridge, text: &str) -> Result<()> {
        mark_remote(&bridge.state, text_fingerprint(text));
        ClipboardContext::new()
            .map_err(|error| anyhow::anyhow!("打开系统剪贴板失败: {error}"))?
            .set_text(text.to_owned())
            .map_err(|error| anyhow::anyhow!("写入远端文本剪贴板失败: {error}"))
    }

    pub(super) fn apply_files(bridge: &ClipboardBridge, paths: &[PathBuf]) -> Result<()> {
        mark_remote(&bridge.state, files_fingerprint(paths));
        ClipboardContext::new()
            .map_err(|error| anyhow::anyhow!("打开系统剪贴板失败: {error}"))?
            .set_files(
                paths
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect(),
            )
            .map_err(|error| anyhow::anyhow!("写入远端文件剪贴板失败: {error}"))
    }

    pub(super) fn apply_image(
        bridge: &ClipboardBridge,
        png: &[u8],
        expected_width: u32,
        expected_height: u32,
    ) -> Result<()> {
        let image = RustImageData::from_bytes(png)
            .map_err(|error| anyhow::anyhow!("解析远端剪贴板图片失败: {error}"))?;
        let (width, height) = image.get_size();
        if (width, height) != (expected_width, expected_height) {
            anyhow::bail!("远端剪贴板图片尺寸与声明不一致");
        }
        let rgba = image
            .to_rgba8()
            .map_err(|error| anyhow::anyhow!("读取远端剪贴板图片像素失败: {error}"))?;
        mark_remote(
            &bridge.state,
            image_fingerprint(width, height, rgba.as_raw()),
        );
        ClipboardContext::new()
            .map_err(|error| anyhow::anyhow!("打开系统剪贴板失败: {error}"))?
            .set_image(image)
            .map_err(|error| anyhow::anyhow!("写入远端图片剪贴板失败: {error}"))
    }

    fn should_skip(state: &Arc<Mutex<ClipboardState>>, fingerprint: [u8; 32]) -> bool {
        let Ok(mut state) = state.lock() else {
            return true;
        };
        let now = Instant::now();
        prune_expired(&mut state.recent_remote, now, REMOTE_SUPPRESSION_WINDOW);
        if state
            .recent_remote
            .iter()
            .any(|(value, _)| *value == fingerprint)
        {
            return true;
        }
        prune_expired(&mut state.recent_local, now, DUPLICATE_WINDOW);
        if state
            .recent_local
            .iter()
            .any(|(value, _)| *value == fingerprint)
        {
            return true;
        }
        remember(&mut state.recent_local, fingerprint, now);
        false
    }

    fn mark_remote(state: &Arc<Mutex<ClipboardState>>, fingerprint: [u8; 32]) {
        if let Ok(mut state) = state.lock() {
            let now = Instant::now();
            prune_expired(&mut state.recent_remote, now, REMOTE_SUPPRESSION_WINDOW);
            remember(&mut state.recent_remote, fingerprint, now);
        }
    }

    fn prune_expired(entries: &mut VecDeque<([u8; 32], Instant)>, now: Instant, window: Duration) {
        entries.retain(|(_, at)| now.duration_since(*at) < window);
    }

    fn remember(entries: &mut VecDeque<([u8; 32], Instant)>, fingerprint: [u8; 32], now: Instant) {
        if entries.len() == MAX_RECENT_FINGERPRINTS {
            entries.pop_front();
        }
        entries.push_back((fingerprint, now));
    }

    fn text_fingerprint(text: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"text\0");
        hasher.update(text.as_bytes());
        *hasher.finalize().as_bytes()
    }

    fn files_fingerprint(paths: &[PathBuf]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"files\0");
        for path in paths {
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(b"\0");
        }
        *hasher.finalize().as_bytes()
    }

    fn image_fingerprint(width: u32, height: u32, rgba: &[u8]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"image\0");
        hasher.update(&width.to_le_bytes());
        hasher.update(&height.to_le_bytes());
        hasher.update(rgba);
        *hasher.finalize().as_bytes()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fingerprints_include_clipboard_kind_and_paths() {
            assert_ne!(text_fingerprint("a"), text_fingerprint("b"));
            assert_ne!(
                text_fingerprint("/tmp/a"),
                files_fingerprint(&[PathBuf::from("/tmp/a")])
            );
        }

        #[test]
        fn suppresses_multiple_remote_echoes() {
            let state = Arc::new(Mutex::new(ClipboardState::default()));
            let first = text_fingerprint("first remote value");
            let second = text_fingerprint("second remote value");
            mark_remote(&state, first);
            mark_remote(&state, second);
            assert!(should_skip(&state, first));
            assert!(should_skip(&state, second));
        }

        #[test]
        fn image_fingerprints_include_dimensions_and_pixels() {
            assert_ne!(
                image_fingerprint(1, 1, &[0; 4]),
                image_fingerprint(2, 1, &[0; 8])
            );
            assert_ne!(
                image_fingerprint(1, 1, &[0; 4]),
                image_fingerprint(1, 1, &[1; 4])
            );
        }
    }
}

#[derive(Clone, Default)]
pub struct ClipboardBridge {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    state: std::sync::Arc<std::sync::Mutex<desktop::ClipboardState>>,
}

impl ClipboardBridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_text(&self, text: &str) -> Result<()> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        return desktop::apply_text(self, text);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            let _ = text;
            bail!("当前平台不支持剪贴板同步")
        }
    }

    pub fn apply_files(&self, paths: &[PathBuf]) -> Result<()> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        return desktop::apply_files(self, paths);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            let _ = paths;
            bail!("当前平台不支持剪贴板同步")
        }
    }

    pub fn apply_image(&self, png: &[u8], width: u32, height: u32) -> Result<()> {
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        return desktop::apply_image(self, png, width, height);
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            let _ = (png, width, height);
            bail!("当前平台不支持剪贴板同步")
        }
    }
}

pub fn start(config: AppConfig, bridge: ClipboardBridge) -> Result<()> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    return desktop::start(config, bridge);
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (config, bridge);
        bail!("当前平台不支持剪贴板同步")
    }
}
