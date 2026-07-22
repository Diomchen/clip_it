use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    config::AppConfig,
    protocol::{EntryKind, FileEntry, MAX_MANIFEST_BYTES, Manifest, PROTOCOL_VERSION, Response},
};

const BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Debug)]
pub struct TransferReceipt {
    pub files: u64,
    pub bytes: u64,
}

pub async fn receive_loop(config: AppConfig) -> Result<()> {
    fs::create_dir_all(&config.download_dir)
        .await
        .context("创建接收目录失败")?;
    let listener = TcpListener::bind(config.listen_addr())
        .await
        .context("绑定文件传输端口失败")?;
    let download_dir = Arc::new(config.download_dir);

    loop {
        let (stream, peer) = listener.accept().await?;
        let download_dir = Arc::clone(&download_dir);
        tokio::spawn(async move {
            if let Err(error) = receive_one(stream, &download_dir).await {
                eprintln!("来自 {peer} 的传输失败: {error:#}");
            }
        });
    }
}

async fn receive_one(mut stream: TcpStream, download_dir: &Path) -> Result<()> {
    stream.set_nodelay(true)?;
    let manifest: Manifest = read_json(&mut stream).await?;
    if manifest.version != PROTOCOL_VERSION {
        send_response(&mut stream, false, "协议版本不兼容", 0, 0).await?;
        bail!("unsupported protocol version {}", manifest.version);
    }

    let safe_entries = match validate_manifest(&manifest) {
        Ok(entries) => entries,
        Err(error) => {
            send_response(&mut stream, false, &error.to_string(), 0, 0).await?;
            return Err(error);
        }
    };

    let transfer_root = download_dir.join(format!("Incoming-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&transfer_root).await?;
    send_response(&mut stream, true, "ready", 0, 0).await?;

    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut received_files = 0_u64;
    let mut received_bytes = 0_u64;

    for (entry, relative_path) in manifest.files.iter().zip(safe_entries) {
        let destination = transfer_root.join(relative_path);
        match entry.kind {
            EntryKind::Directory => fs::create_dir_all(&destination).await?,
            EntryKind::File => {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).await?;
                }
                let temporary =
                    destination.with_extension(format!("{}.clipit-part", Uuid::new_v4().simple()));
                let mut output = fs::File::create(&temporary).await?;
                let mut hasher = blake3::Hasher::new();
                let mut remaining = entry.size;

                while remaining > 0 {
                    let wanted = usize::try_from(remaining.min(BUFFER_SIZE as u64))?;
                    stream.read_exact(&mut buffer[..wanted]).await?;
                    output.write_all(&buffer[..wanted]).await?;
                    hasher.update(&buffer[..wanted]);
                    remaining -= wanted as u64;
                }
                output.flush().await?;
                drop(output);

                let mut expected_hash = [0_u8; 32];
                stream.read_exact(&mut expected_hash).await?;
                if hasher.finalize().as_bytes() != &expected_hash {
                    let _ = fs::remove_file(&temporary).await;
                    bail!("文件完整性校验失败: {}", entry.relative_path);
                }
                fs::rename(&temporary, &destination).await?;
                received_files += 1;
                received_bytes += entry.size;
            }
        }
    }

    send_response(
        &mut stream,
        true,
        transfer_root.to_string_lossy().as_ref(),
        received_files,
        received_bytes,
    )
    .await?;
    println!(
        "已从 {} 接收 {} 个文件（{} 字节），保存到 {}",
        stream.peer_addr()?,
        received_files,
        received_bytes,
        transfer_root.display()
    );
    Ok(())
}

pub async fn send_paths(
    target: std::net::SocketAddr,
    paths: &[PathBuf],
) -> Result<TransferReceipt> {
    let sources = build_sources(paths)?;
    let manifest = Manifest {
        version: PROTOCOL_VERSION,
        files: sources.iter().map(|source| source.entry.clone()).collect(),
    };
    let mut stream = TcpStream::connect(target)
        .await
        .with_context(|| format!("连接 {target} 失败"))?;
    stream.set_nodelay(true)?;
    write_json(&mut stream, &manifest).await?;

    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端拒绝传输: {}", response.message);
    }

    let mut buffer = vec![0_u8; BUFFER_SIZE];
    for source in &sources {
        if source.entry.kind == EntryKind::Directory {
            continue;
        }
        let mut input = fs::File::open(&source.source_path).await?;
        let mut remaining = source.entry.size;
        let mut hasher = blake3::Hasher::new();
        while remaining > 0 {
            let wanted = usize::try_from(remaining.min(BUFFER_SIZE as u64))?;
            let read = input.read(&mut buffer[..wanted]).await?;
            if read == 0 {
                bail!("文件在传输期间被截短: {}", source.source_path.display());
            }
            stream.write_all(&buffer[..read]).await?;
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        stream.write_all(hasher.finalize().as_bytes()).await?;
    }
    stream.flush().await?;

    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端写入失败: {}", response.message);
    }
    Ok(TransferReceipt {
        files: response.files,
        bytes: response.bytes,
    })
}

#[derive(Debug)]
struct SourceEntry {
    source_path: PathBuf,
    entry: FileEntry,
}

fn build_sources(paths: &[PathBuf]) -> Result<Vec<SourceEntry>> {
    let mut sources = Vec::new();
    let mut roots = HashSet::new();

    for selected in paths {
        let canonical = selected
            .canonicalize()
            .with_context(|| format!("无法读取 {}", selected.display()))?;
        let root_name = canonical
            .file_name()
            .context("不能发送文件系统根目录")?
            .to_string_lossy()
            .into_owned();
        if !roots.insert(root_name.clone()) {
            bail!("选择项存在同名根目录或文件: {root_name}");
        }

        if canonical.is_file() {
            sources.push(source_entry(&canonical, root_name, EntryKind::File)?);
            continue;
        }
        if !canonical.is_dir() {
            bail!("不支持的文件类型: {}", selected.display());
        }

        for item in WalkDir::new(&canonical).follow_links(false) {
            let item = item?;
            if item.file_type().is_symlink() {
                continue;
            }
            let suffix = item.path().strip_prefix(&canonical)?;
            let relative = if suffix.as_os_str().is_empty() {
                PathBuf::from(&root_name)
            } else {
                PathBuf::from(&root_name).join(suffix)
            };
            let kind = if item.file_type().is_dir() {
                EntryKind::Directory
            } else if item.file_type().is_file() {
                EntryKind::File
            } else {
                continue;
            };
            sources.push(source_entry(item.path(), protocol_path(&relative), kind)?);
        }
    }
    Ok(sources)
}

fn source_entry(path: &Path, relative_path: String, kind: EntryKind) -> Result<SourceEntry> {
    let size = if kind == EntryKind::File {
        path.metadata()?.len()
    } else {
        0
    };
    Ok(SourceEntry {
        source_path: path.to_owned(),
        entry: FileEntry {
            relative_path,
            size,
            kind,
        },
    })
}

fn protocol_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_manifest(manifest: &Manifest) -> Result<Vec<PathBuf>> {
    if manifest.files.is_empty() {
        bail!("清单为空");
    }
    if manifest.files.len() > 100_000 {
        bail!("文件数量超过限制");
    }

    manifest
        .files
        .iter()
        .map(|entry| safe_protocol_path(&entry.relative_path))
        .collect()
}

fn safe_protocol_path(value: &str) -> Result<PathBuf> {
    let mut output = PathBuf::new();
    if value.is_empty() || value.starts_with('/') || value.contains('\\') {
        bail!("不安全的相对路径");
    }
    for segment in value.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains(':') {
            bail!("不安全的路径片段: {segment}");
        }
        output.push(segment);
    }
    Ok(output)
}

async fn write_json<T: serde::Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        bail!("协议消息过大");
    }
    stream.write_u32(bytes.len().try_into()?).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn read_json<T: serde::de::DeserializeOwned>(stream: &mut TcpStream) -> Result<T> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_MANIFEST_BYTES {
        bail!("协议消息过大");
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn send_response(
    stream: &mut TcpStream,
    ok: bool,
    message: &str,
    files: u64,
    bytes: u64,
) -> Result<()> {
    write_json(
        stream,
        &Response {
            ok,
            message: message.into(),
            files,
            bytes,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal() {
        assert!(safe_protocol_path("../secret").is_err());
        assert!(safe_protocol_path("ok/../../secret").is_err());
        assert!(safe_protocol_path("C:/secret").is_err());
        assert!(safe_protocol_path("/absolute").is_err());
        assert!(safe_protocol_path("ok\\secret").is_err());
    }

    #[test]
    fn accepts_nested_relative_path() {
        assert_eq!(
            safe_protocol_path("photos/2026/pic.jpg").unwrap(),
            PathBuf::from("photos").join("2026").join("pic.jpg")
        );
    }
}
