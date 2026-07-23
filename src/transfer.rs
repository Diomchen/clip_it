use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex as AsyncMutex,
    time,
};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    clipboard::ClipboardBridge,
    config::{
        AppConfig, Identity, PairedDevice, PairedDevices, ReceivePolicy, TrustedDevice,
        TrustedDevices,
    },
    confirmation::{self, Decision, IncomingSummary},
    protocol::{
        BenchmarkRequest, ChunkRange, ClipboardImage, ClipboardText, CompleteTransfer,
        ConnectionUpdate, EntryKind, FileChunk, FileEntry, MAX_CLIPBOARD_IMAGE_BYTES,
        MAX_CLIPBOARD_IMAGE_PIXELS, MAX_CLIPBOARD_TEXT_BYTES, MAX_MANIFEST_BYTES, Manifest,
        PROTOCOL_VERSION, Ping, Request, Response, SenderIdentity, TransferIntent,
    },
};

const BUFFER_SIZE: usize = 1024 * 1024;
const CHUNK_SIZE: u32 = 32 * 1024 * 1024;
const PARALLEL_STREAMS: usize = 4;
const MAX_BENCHMARK_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct TransferReceipt {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug)]
pub struct BenchmarkReceipt {
    pub bytes: u64,
    pub elapsed: Duration,
    pub gigabits_per_second: f64,
}

#[derive(Clone, Default)]
struct TransferRegistry {
    sessions: Arc<AsyncMutex<HashMap<Uuid, Arc<AsyncMutex<ReceiveSession>>>>>,
}

struct ReceiveSession {
    manifest: Manifest,
    safe_entries: Vec<PathBuf>,
    transfer_root: PathBuf,
    resume_path: PathBuf,
    fingerprint: String,
    completed: HashSet<ChunkRange>,
}

#[derive(Clone)]
struct DeviceStores {
    trusted: TrustedDevices,
    paired: PairedDevices,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResumeState {
    transfer_id: Uuid,
    fingerprint: String,
    completed: Vec<ChunkRange>,
}

pub async fn receive_loop(
    config: AppConfig,
    policy: ReceivePolicy,
    clipboard: ClipboardBridge,
) -> Result<()> {
    fs::create_dir_all(&config.download_dir)
        .await
        .context("创建接收目录失败")?;
    let listener = TcpListener::bind(config.listen_addr())
        .await
        .context("绑定文件传输端口失败")?;
    let download_dir = Arc::new(config.download_dir);
    let devices = DeviceStores {
        trusted: config.trusted_devices,
        paired: config.paired_devices,
    };
    let registry = TransferRegistry::default();

    loop {
        let (stream, peer) = listener.accept().await?;
        let download_dir = Arc::clone(&download_dir);
        let devices = devices.clone();
        let clipboard = clipboard.clone();
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(error) = receive_one(
                stream,
                peer,
                &download_dir,
                policy,
                &devices,
                &clipboard,
                &registry,
            )
            .await
            {
                eprintln!("来自 {peer} 的传输失败: {error:#}");
            }
        });
    }
}

async fn receive_one(
    mut stream: TcpStream,
    peer: SocketAddr,
    download_dir: &Path,
    policy: ReceivePolicy,
    devices: &DeviceStores,
    clipboard: &ClipboardBridge,
    registry: &TransferRegistry,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let request: Request = read_json(&mut stream).await?;
    match request {
        Request::FileTransfer(manifest) => {
            receive_files(
                stream,
                peer,
                download_dir,
                policy,
                &devices.trusted,
                registry,
                manifest,
            )
            .await
        }
        Request::ClipboardText(message) => {
            receive_clipboard_text(stream, peer, clipboard, message).await
        }
        Request::ClipboardImage(message) => {
            receive_clipboard_image(stream, peer, clipboard, message).await
        }
        Request::Ping(message) => receive_ping(stream, message).await,
        Request::Connection(message) => receive_connection(stream, &devices.paired, message).await,
        Request::FileChunk(chunk) => receive_file_chunk(stream, registry, chunk).await,
        Request::CompleteTransfer(message) => {
            complete_transfer(stream, registry, clipboard, message).await
        }
        Request::Benchmark(message) => receive_benchmark(stream, message).await,
    }
}

async fn receive_ping(mut stream: TcpStream, message: Ping) -> Result<()> {
    if message.version != PROTOCOL_VERSION {
        send_response(&mut stream, false, "协议版本不兼容", 0, 0).await?;
        bail!("unsupported protocol version {}", message.version);
    }
    validate_sender(&message.sender)?;
    send_response(&mut stream, true, "pong", 0, 0).await
}

async fn receive_connection(
    mut stream: TcpStream,
    paired_devices: &PairedDevices,
    message: ConnectionUpdate,
) -> Result<()> {
    if message.version != PROTOCOL_VERSION {
        send_response(&mut stream, false, "协议版本不兼容", 0, 0).await?;
        bail!("unsupported protocol version {}", message.version);
    }
    if let Err(error) =
        validate_sender(&message.sender).and_then(|_| validate_emoji(&message.emoji))
    {
        send_response(&mut stream, false, &error.to_string(), 0, 0).await?;
        return Err(error);
    }
    if message.connected {
        paired_devices.add(PairedDevice::new(
            message.sender.id,
            message.sender.name,
            message.emoji,
        ))?;
    } else {
        paired_devices.remove(message.sender.id)?;
    }
    send_response(&mut stream, true, "connection updated", 0, 0).await
}

async fn receive_files(
    mut stream: TcpStream,
    peer: SocketAddr,
    download_dir: &Path,
    policy: ReceivePolicy,
    trusted_devices: &TrustedDevices,
    registry: &TransferRegistry,
    manifest: Manifest,
) -> Result<()> {
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

    let total_bytes = manifest
        .files
        .iter()
        .try_fold(0_u64, |total, entry| total.checked_add(entry.size))
        .context("传输总大小溢出")?;
    let authorized = match if manifest.intent == TransferIntent::Clipboard {
        Ok(true)
    } else {
        authorize_transfer(
            policy,
            trusted_devices,
            &manifest.sender,
            peer,
            &manifest.files,
            total_bytes,
        )
        .await
    } {
        Ok(authorized) => authorized,
        Err(error) => {
            send_response(&mut stream, false, "无法完成接收确认", 0, 0).await?;
            return Err(error);
        }
    };
    if !authorized {
        send_response(&mut stream, false, "接收端未授权此次传输", 0, 0).await?;
        println!(
            "已拒绝来自 {} ({}, {}) 的传输",
            manifest.sender.name, manifest.sender.id, peer
        );
        return Ok(());
    }

    let session = prepare_receive_session(download_dir, manifest, safe_entries).await?;
    let missing_chunks = {
        let session = session.lock().await;
        expected_chunks(&session.manifest)
            .into_iter()
            .filter(|chunk| !session.completed.contains(chunk))
            .collect::<Vec<_>>()
    };
    let transfer_id = session.lock().await.manifest.transfer_id;
    registry
        .sessions
        .lock()
        .await
        .insert(transfer_id, Arc::clone(&session));
    write_json(
        &mut stream,
        &Response {
            ok: true,
            message: if missing_chunks.is_empty() {
                "ready to finalize".into()
            } else {
                "ready".into()
            },
            files: 0,
            bytes: 0,
            missing_chunks,
            elapsed_micros: 0,
        },
    )
    .await
}

async fn prepare_receive_session(
    download_dir: &Path,
    manifest: Manifest,
    safe_entries: Vec<PathBuf>,
) -> Result<Arc<AsyncMutex<ReceiveSession>>> {
    let transfer_root = download_dir.join(format!("Incoming-{}", manifest.transfer_id.simple()));
    fs::create_dir_all(&transfer_root).await?;
    let resume_path = transfer_root.join(".clipit-resume.json");
    let fingerprint = manifest_fingerprint(&manifest)?;
    let mut completed = load_resume(&resume_path, manifest.transfer_id, &fingerprint).await?;

    for (index, (entry, relative_path)) in manifest.files.iter().zip(&safe_entries).enumerate() {
        let destination = transfer_root.join(relative_path);
        match entry.kind {
            EntryKind::Directory => fs::create_dir_all(&destination).await?,
            EntryKind::File => {
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).await?;
                }
                if fs::metadata(&destination)
                    .await
                    .is_ok_and(|metadata| metadata.len() == entry.size)
                {
                    completed.extend(chunks_for_file(index as u32, entry, manifest.chunk_size));
                    continue;
                }
                let partial = partial_path(&destination)?;
                let reusable_partial = fs::metadata(&partial)
                    .await
                    .is_ok_and(|metadata| metadata.len() == entry.size);
                if !reusable_partial {
                    completed.retain(|chunk| chunk.file_index != index as u32);
                }
                let file = fs::OpenOptions::new()
                    .create(true)
                    .read(true)
                    .write(true)
                    .truncate(false)
                    .open(&partial)
                    .await?;
                file.set_len(entry.size).await?;
            }
        }
    }

    let expected = expected_chunks(&manifest)
        .into_iter()
        .collect::<HashSet<_>>();
    completed.retain(|chunk| expected.contains(chunk));
    let session = ReceiveSession {
        manifest,
        safe_entries,
        transfer_root,
        resume_path,
        fingerprint,
        completed,
    };
    persist_resume(&session).await?;
    Ok(Arc::new(AsyncMutex::new(session)))
}

async fn load_resume(
    path: &Path,
    transfer_id: Uuid,
    fingerprint: &str,
) -> Result<HashSet<ChunkRange>> {
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error.into()),
    };
    let state: ResumeState = serde_json::from_slice(&bytes).context("断点状态文件格式错误")?;
    if state.transfer_id != transfer_id || state.fingerprint != fingerprint {
        return Ok(HashSet::new());
    }
    Ok(state.completed.into_iter().collect())
}

async fn persist_resume(session: &ReceiveSession) -> Result<()> {
    let state = ResumeState {
        transfer_id: session.manifest.transfer_id,
        fingerprint: session.fingerprint.clone(),
        completed: session.completed.iter().cloned().collect(),
    };
    let temporary = session.resume_path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(&state)?).await?;
    fs::rename(temporary, &session.resume_path).await?;
    Ok(())
}

async fn receive_file_chunk(
    mut stream: TcpStream,
    registry: &TransferRegistry,
    chunk: FileChunk,
) -> Result<()> {
    if chunk.version != PROTOCOL_VERSION {
        send_response(&mut stream, false, "协议版本不兼容", 0, 0).await?;
        bail!("unsupported protocol version {}", chunk.version);
    }
    validate_sender(&chunk.sender)?;
    let session = registry
        .sessions
        .lock()
        .await
        .get(&chunk.transfer_id)
        .cloned()
        .context("传输会话不存在，请重新开始传输")?;
    let range = ChunkRange {
        file_index: chunk.file_index,
        offset: chunk.offset,
        length: chunk.length,
    };
    let (partial, entry_size) = {
        let session = session.lock().await;
        if session.manifest.sender.id != chunk.sender.id {
            bail!("分块发送端与传输会话不匹配");
        }
        let entry = session
            .manifest
            .files
            .get(chunk.file_index as usize)
            .context("分块文件索引无效")?;
        if entry.kind != EntryKind::File
            || !chunks_for_file(chunk.file_index, entry, session.manifest.chunk_size)
                .contains(&range)
        {
            bail!("分块范围无效");
        }
        let destination = session
            .transfer_root
            .join(&session.safe_entries[chunk.file_index as usize]);
        (partial_path(&destination)?, entry.size)
    };
    if chunk.offset + u64::from(chunk.length) > entry_size {
        bail!("分块超出文件范围");
    }

    let mut output = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&partial)
        .await?;
    output.seek(std::io::SeekFrom::Start(chunk.offset)).await?;
    let mut remaining = u64::from(chunk.length);
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    let mut hasher = blake3::Hasher::new();
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(BUFFER_SIZE as u64))?;
        stream.read_exact(&mut buffer[..wanted]).await?;
        output.write_all(&buffer[..wanted]).await?;
        hasher.update(&buffer[..wanted]);
        remaining -= wanted as u64;
    }
    output.flush().await?;
    let mut expected_hash = [0_u8; 32];
    stream.read_exact(&mut expected_hash).await?;
    if hasher.finalize().as_bytes() != &expected_hash {
        send_response(&mut stream, false, "分块完整性校验失败", 0, 0).await?;
        bail!("分块完整性校验失败");
    }
    {
        let mut session = session.lock().await;
        session.completed.insert(range);
        persist_resume(&session).await?;
    }
    send_response(
        &mut stream,
        true,
        "chunk stored",
        0,
        u64::from(chunk.length),
    )
    .await
}

async fn complete_transfer(
    mut stream: TcpStream,
    registry: &TransferRegistry,
    clipboard: &ClipboardBridge,
    message: CompleteTransfer,
) -> Result<()> {
    if message.version != PROTOCOL_VERSION {
        send_response(&mut stream, false, "协议版本不兼容", 0, 0).await?;
        bail!("unsupported protocol version {}", message.version);
    }
    validate_sender(&message.sender)?;
    let session = registry
        .sessions
        .lock()
        .await
        .get(&message.transfer_id)
        .cloned()
        .context("传输会话不存在")?;
    let (files, bytes, transfer_root, clipboard_paths) = {
        let session = session.lock().await;
        if session.manifest.sender.id != message.sender.id {
            bail!("发送端与传输会话不匹配");
        }
        let expected = expected_chunks(&session.manifest);
        if expected
            .iter()
            .any(|chunk| !session.completed.contains(chunk))
        {
            send_response(&mut stream, false, "仍有分块未完成", 0, 0).await?;
            return Ok(());
        }
        let mut files = 0_u64;
        let mut bytes = 0_u64;
        for (entry, relative_path) in session.manifest.files.iter().zip(&session.safe_entries) {
            if entry.kind != EntryKind::File {
                continue;
            }
            let destination = session.transfer_root.join(relative_path);
            if !destination.exists() {
                fs::rename(partial_path(&destination)?, &destination).await?;
            }
            files += 1;
            bytes += entry.size;
        }
        let _ = fs::remove_file(&session.resume_path).await;
        let clipboard_paths = if session.manifest.intent == TransferIntent::Clipboard {
            clipboard_root_paths(&session.transfer_root, &session.safe_entries)
        } else {
            Vec::new()
        };
        (files, bytes, session.transfer_root.clone(), clipboard_paths)
    };
    registry.sessions.lock().await.remove(&message.transfer_id);
    if !clipboard_paths.is_empty() {
        if let Err(error) = clipboard.apply_files(&clipboard_paths) {
            eprintln!("文件已接收，但写入系统剪贴板失败: {error:#}");
        } else {
            println!("已将接收的文件写入系统剪贴板，可直接粘贴");
        }
    }
    send_response(
        &mut stream,
        true,
        transfer_root.to_string_lossy().as_ref(),
        files,
        bytes,
    )
    .await?;
    println!(
        "已完成断点传输 {}：{} 个文件（{} 字节），保存到 {}",
        message.transfer_id,
        files,
        bytes,
        transfer_root.display()
    );
    Ok(())
}

async fn receive_benchmark(mut stream: TcpStream, message: BenchmarkRequest) -> Result<()> {
    if message.version != PROTOCOL_VERSION || message.bytes > MAX_BENCHMARK_BYTES {
        send_response(&mut stream, false, "基准请求无效", 0, 0).await?;
        bail!("invalid benchmark request");
    }
    validate_sender(&message.sender)?;
    send_response(&mut stream, true, "ready", 0, 0).await?;
    let started = Instant::now();
    let mut remaining = message.bytes;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(BUFFER_SIZE as u64))?;
        stream.read_exact(&mut buffer[..wanted]).await?;
        remaining -= wanted as u64;
    }
    let elapsed_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    write_json(
        &mut stream,
        &Response {
            ok: true,
            message: "benchmark complete".into(),
            files: 0,
            bytes: message.bytes,
            missing_chunks: Vec::new(),
            elapsed_micros,
        },
    )
    .await
}

async fn receive_clipboard_text(
    mut stream: TcpStream,
    peer: SocketAddr,
    clipboard: &ClipboardBridge,
    message: ClipboardText,
) -> Result<()> {
    if message.version != PROTOCOL_VERSION {
        send_response(&mut stream, false, "协议版本不兼容", 0, 0).await?;
        bail!("unsupported protocol version {}", message.version);
    }
    if let Err(error) = validate_sender(&message.sender) {
        send_response(&mut stream, false, &error.to_string(), 0, 0).await?;
        return Err(error);
    }
    if message.event_id.is_nil() || message.text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        send_response(&mut stream, false, "剪贴板内容无效或过大", 0, 0).await?;
        bail!("invalid clipboard text message");
    }
    clipboard.apply_text(&message.text)?;
    send_response(
        &mut stream,
        true,
        "clipboard updated",
        0,
        message.text.len() as u64,
    )
    .await?;
    println!(
        "已同步来自 {} ({}) 的文本剪贴板（{} 字节）",
        message.sender.name,
        peer,
        message.text.len()
    );
    Ok(())
}

async fn receive_clipboard_image(
    mut stream: TcpStream,
    peer: SocketAddr,
    clipboard: &ClipboardBridge,
    message: ClipboardImage,
) -> Result<()> {
    if let Err(error) = validate_clipboard_image(&message) {
        send_response(&mut stream, false, &error.to_string(), 0, 0).await?;
        return Err(error);
    }
    let length = usize::try_from(message.length)?;
    let mut png = vec![0_u8; length];
    time::timeout(Duration::from_secs(30), stream.read_exact(&mut png))
        .await
        .context("接收剪贴板图片超时")??;
    if let Err(error) = validate_png_dimensions(&png, message.width, message.height) {
        send_response(&mut stream, false, &error.to_string(), 0, 0).await?;
        return Err(error);
    }
    if *blake3::hash(&png).as_bytes() != message.blake3 {
        send_response(&mut stream, false, "剪贴板图片校验失败", 0, 0).await?;
        bail!("clipboard image checksum mismatch");
    }
    if let Err(error) = clipboard.apply_image(&png, message.width, message.height) {
        send_response(&mut stream, false, &error.to_string(), 0, 0).await?;
        return Err(error);
    }
    send_response(
        &mut stream,
        true,
        "clipboard image updated",
        0,
        message.length,
    )
    .await?;
    println!(
        "已同步来自 {} ({}) 的图片剪贴板（{}x{}，{} 字节）",
        message.sender.name, peer, message.width, message.height, message.length
    );
    Ok(())
}

pub async fn send_paths(
    target: SocketAddr,
    paths: &[PathBuf],
    sender: &Identity,
) -> Result<TransferReceipt> {
    send_paths_with_intent(target, paths, sender, TransferIntent::Manual).await
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub async fn send_clipboard_image(
    target: SocketAddr,
    png: &[u8],
    width: u32,
    height: u32,
    event_id: Uuid,
    sender: &Identity,
) -> Result<()> {
    let length = u64::try_from(png.len())?;
    let message = ClipboardImage {
        version: PROTOCOL_VERSION,
        sender: sender_identity(sender),
        event_id,
        width,
        height,
        length,
        blake3: *blake3::hash(png).as_bytes(),
    };
    validate_clipboard_image(&message)?;
    validate_png_dimensions(png, width, height)?;
    let mut stream = TcpStream::connect(target)
        .await
        .with_context(|| format!("连接 {target} 失败"))?;
    stream.set_nodelay(true)?;
    write_json(&mut stream, &Request::ClipboardImage(message)).await?;
    stream.write_all(png).await?;
    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端拒绝剪贴板图片同步: {}", response.message);
    }
    Ok(())
}

pub async fn set_peer_connection(
    target: SocketAddr,
    sender: &Identity,
    connected: bool,
) -> Result<()> {
    let mut stream = time::timeout(Duration::from_secs(2), TcpStream::connect(target))
        .await
        .context("连接设备超时")?
        .with_context(|| format!("连接 {target} 失败"))?;
    stream.set_nodelay(true)?;
    write_json(
        &mut stream,
        &Request::Connection(ConnectionUpdate {
            version: PROTOCOL_VERSION,
            sender: sender_identity(sender),
            emoji: sender.emoji.clone(),
            connected,
        }),
    )
    .await?;
    let response: Response = time::timeout(Duration::from_secs(2), read_json(&mut stream))
        .await
        .context("等待设备响应超时")??;
    if !response.ok {
        bail!("设备连接状态更新失败: {}", response.message);
    }
    Ok(())
}

pub async fn send_paths_with_intent(
    target: SocketAddr,
    paths: &[PathBuf],
    sender: &Identity,
    intent: TransferIntent,
) -> Result<TransferReceipt> {
    let sources = Arc::new(build_sources(paths)?);
    let transfer_id = build_transfer_id(&sources, sender);
    let manifest = Manifest {
        version: PROTOCOL_VERSION,
        sender: sender_identity(sender),
        intent,
        transfer_id,
        chunk_size: CHUNK_SIZE,
        files: sources.iter().map(|source| source.entry.clone()).collect(),
    };
    let mut stream = TcpStream::connect(target)
        .await
        .with_context(|| format!("连接 {target} 失败"))?;
    stream.set_nodelay(true)?;
    write_json(&mut stream, &Request::FileTransfer(manifest)).await?;

    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端拒绝传输: {}", response.message);
    }
    drop(stream);

    send_missing_chunks(
        target,
        Arc::clone(&sources),
        sender.clone(),
        transfer_id,
        response.missing_chunks,
    )
    .await?;

    let mut stream = TcpStream::connect(target).await?;
    stream.set_nodelay(true)?;
    write_json(
        &mut stream,
        &Request::CompleteTransfer(CompleteTransfer {
            version: PROTOCOL_VERSION,
            sender: sender_identity(sender),
            transfer_id,
        }),
    )
    .await?;
    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端完成传输失败: {}", response.message);
    }
    Ok(TransferReceipt {
        files: response.files,
        bytes: response.bytes,
    })
}

async fn send_missing_chunks(
    target: SocketAddr,
    sources: Arc<Vec<SourceEntry>>,
    sender: Identity,
    transfer_id: Uuid,
    chunks: Vec<ChunkRange>,
) -> Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let queue = Arc::new(AsyncMutex::new(VecDeque::from(chunks)));
    let mut workers = Vec::new();
    for _ in 0..PARALLEL_STREAMS {
        let queue = Arc::clone(&queue);
        let sources = Arc::clone(&sources);
        let sender = sender.clone();
        workers.push(tokio::spawn(async move {
            loop {
                let Some(chunk) = queue.lock().await.pop_front() else {
                    return Ok::<(), anyhow::Error>(());
                };
                send_file_chunk(target, &sources, &sender, transfer_id, chunk).await?;
            }
        }));
    }
    for worker in workers {
        worker.await.context("分块传输任务异常退出")??;
    }
    Ok(())
}

async fn send_file_chunk(
    target: SocketAddr,
    sources: &[SourceEntry],
    sender: &Identity,
    transfer_id: Uuid,
    chunk: ChunkRange,
) -> Result<()> {
    let source = sources
        .get(chunk.file_index as usize)
        .context("接收端请求了无效文件索引")?;
    let mut stream = TcpStream::connect(target)
        .await
        .with_context(|| format!("连接 {target} 失败"))?;
    stream.set_nodelay(true)?;
    write_json(
        &mut stream,
        &Request::FileChunk(FileChunk {
            version: PROTOCOL_VERSION,
            sender: sender_identity(sender),
            transfer_id,
            file_index: chunk.file_index,
            offset: chunk.offset,
            length: chunk.length,
        }),
    )
    .await?;
    let mut input = fs::File::open(&source.source_path).await?;
    input.seek(std::io::SeekFrom::Start(chunk.offset)).await?;
    let mut remaining = u64::from(chunk.length);
    let mut buffer = vec![0_u8; BUFFER_SIZE];
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
    stream.flush().await?;
    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端拒绝分块: {}", response.message);
    }
    Ok(())
}

pub async fn run_benchmark(
    target: SocketAddr,
    total_bytes: u64,
    streams: usize,
    sender: &Identity,
) -> Result<BenchmarkReceipt> {
    if total_bytes == 0 || total_bytes > MAX_BENCHMARK_BYTES || !(1..=32).contains(&streams) {
        bail!("基准大小必须为 1 字节到 64 GiB，流数量必须为 1-32");
    }
    let base = total_bytes / streams as u64;
    let remainder = total_bytes % streams as u64;
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(streams);
    for index in 0..streams {
        let bytes = base + u64::from((index as u64) < remainder);
        let sender = sender.clone();
        tasks.push(tokio::spawn(async move {
            run_benchmark_stream(target, bytes, &sender).await
        }));
    }
    for task in tasks {
        task.await.context("基准流异常退出")??;
    }
    let elapsed = started.elapsed();
    let gigabits_per_second = total_bytes as f64 * 8.0 / elapsed.as_secs_f64() / 1_000_000_000.0;
    Ok(BenchmarkReceipt {
        bytes: total_bytes,
        elapsed,
        gigabits_per_second,
    })
}

async fn run_benchmark_stream(target: SocketAddr, bytes: u64, sender: &Identity) -> Result<()> {
    let mut stream = TcpStream::connect(target).await?;
    stream.set_nodelay(true)?;
    write_json(
        &mut stream,
        &Request::Benchmark(BenchmarkRequest {
            version: PROTOCOL_VERSION,
            sender: sender_identity(sender),
            bytes,
        }),
    )
    .await?;
    let ready: Response = read_json(&mut stream).await?;
    if !ready.ok {
        bail!("接收端拒绝基准测试: {}", ready.message);
    }
    let buffer = vec![0_u8; BUFFER_SIZE];
    let mut remaining = bytes;
    while remaining > 0 {
        let length = usize::try_from(remaining.min(BUFFER_SIZE as u64))?;
        stream.write_all(&buffer[..length]).await?;
        remaining -= length as u64;
    }
    stream.flush().await?;
    let complete: Response = read_json(&mut stream).await?;
    if !complete.ok || complete.bytes != bytes {
        bail!("接收端基准结果无效");
    }
    Ok(())
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub async fn send_clipboard_text(
    target: SocketAddr,
    text: &str,
    event_id: Uuid,
    sender: &Identity,
) -> Result<()> {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        bail!("文本剪贴板超过 1 MiB 限制");
    }
    let message = ClipboardText {
        version: PROTOCOL_VERSION,
        sender: sender_identity(sender),
        event_id,
        text: text.to_owned(),
    };
    let mut stream = TcpStream::connect(target)
        .await
        .with_context(|| format!("连接 {target} 失败"))?;
    stream.set_nodelay(true)?;
    write_json(&mut stream, &Request::ClipboardText(message)).await?;
    let response: Response = read_json(&mut stream).await?;
    if !response.ok {
        bail!("接收端拒绝剪贴板同步: {}", response.message);
    }
    Ok(())
}

fn sender_identity(sender: &Identity) -> SenderIdentity {
    SenderIdentity {
        id: sender.id,
        name: sender.name.clone(),
    }
}

async fn authorize_transfer(
    policy: ReceivePolicy,
    trusted_devices: &TrustedDevices,
    sender: &SenderIdentity,
    peer: SocketAddr,
    files: &[FileEntry],
    total_bytes: u64,
) -> Result<bool> {
    let trusted = trusted_devices.contains(sender.id)?;
    match policy {
        ReceivePolicy::AcceptAll => Ok(true),
        ReceivePolicy::TrustedOnly => Ok(trusted),
        ReceivePolicy::Confirm if trusted => Ok(true),
        ReceivePolicy::Confirm => {
            let paths = files
                .iter()
                .map(|entry| entry.relative_path.clone())
                .collect::<Vec<_>>();
            let decision = time::timeout(
                CONFIRMATION_TIMEOUT,
                confirmation::prompt(IncomingSummary {
                    sender,
                    peer,
                    files: files.len(),
                    bytes: total_bytes,
                    paths: &paths,
                }),
            )
            .await
            .context("接收确认超时")??;
            match decision {
                Decision::AcceptOnce => Ok(true),
                Decision::TrustAndAccept => {
                    trusted_devices.add(TrustedDevice {
                        id: sender.id,
                        name: sender.name.clone(),
                    })?;
                    println!("已将 {} ({}) 加入可信设备列表", sender.name, sender.id);
                    Ok(true)
                }
                Decision::Reject => Ok(false),
            }
        }
    }
}

#[derive(Clone, Debug)]
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
    let metadata = path.metadata()?;
    let size = if kind == EntryKind::File {
        metadata.len()
    } else {
        0
    };
    let modified_millis = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    Ok(SourceEntry {
        source_path: path.to_owned(),
        entry: FileEntry {
            relative_path,
            size,
            modified_millis,
            kind,
        },
    })
}

fn build_transfer_id(sources: &[SourceEntry], sender: &Identity) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"clip-it-transfer-v4\0");
    hasher.update(sender.id.as_bytes());
    for source in sources {
        hasher.update(source.entry.relative_path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&source.entry.size.to_le_bytes());
        hasher.update(&source.entry.modified_millis.to_le_bytes());
        hasher.update(&[match source.entry.kind {
            EntryKind::Directory => 0,
            EntryKind::File => 1,
        }]);
    }
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn expected_chunks(manifest: &Manifest) -> Vec<ChunkRange> {
    manifest
        .files
        .iter()
        .enumerate()
        .flat_map(|(index, entry)| chunks_for_file(index as u32, entry, manifest.chunk_size))
        .collect()
}

fn chunks_for_file(file_index: u32, entry: &FileEntry, chunk_size: u32) -> Vec<ChunkRange> {
    if entry.kind != EntryKind::File || entry.size == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut offset = 0_u64;
    while offset < entry.size {
        let length = (entry.size - offset).min(u64::from(chunk_size)) as u32;
        chunks.push(ChunkRange {
            file_index,
            offset,
            length,
        });
        offset += u64::from(length);
    }
    chunks
}

fn manifest_fingerprint(manifest: &Manifest) -> Result<String> {
    let bytes = serde_json::to_vec(manifest)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn partial_path(destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .context("接收文件路径缺少文件名")?
        .to_string_lossy();
    Ok(destination.with_file_name(format!(".{name}.clipit-part")))
}

fn protocol_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_manifest(manifest: &Manifest) -> Result<Vec<PathBuf>> {
    validate_sender(&manifest.sender)?;
    if manifest.transfer_id.is_nil() {
        bail!("传输 ID 无效");
    }
    if !(1024 * 1024..=256 * 1024 * 1024).contains(&manifest.chunk_size) {
        bail!("分块大小无效");
    }
    if manifest.files.is_empty() {
        bail!("清单为空");
    }
    if manifest.files.len() > 100_000 {
        bail!("文件数量超过限制");
    }
    if manifest
        .files
        .iter()
        .any(|entry| entry.kind == EntryKind::Directory && entry.size != 0)
    {
        bail!("目录条目的大小必须为 0");
    }

    manifest
        .files
        .iter()
        .map(|entry| safe_protocol_path(&entry.relative_path))
        .collect()
}

fn validate_sender(sender: &SenderIdentity) -> Result<()> {
    let sender_name = sender.name.trim();
    if sender.id.is_nil()
        || sender_name.is_empty()
        || sender_name.chars().count() > 128
        || sender_name.chars().any(char::is_control)
    {
        bail!("发送端身份无效");
    }
    Ok(())
}

fn validate_clipboard_image(message: &ClipboardImage) -> Result<()> {
    validate_sender(&message.sender)?;
    let pixels = u64::from(message.width).saturating_mul(u64::from(message.height));
    if message.version != PROTOCOL_VERSION
        || message.event_id.is_nil()
        || message.length == 0
        || message.length > MAX_CLIPBOARD_IMAGE_BYTES as u64
        || pixels == 0
        || pixels > MAX_CLIPBOARD_IMAGE_PIXELS
    {
        bail!("剪贴板图片信息无效或超过限制");
    }
    Ok(())
}

fn validate_png_dimensions(png: &[u8], expected_width: u32, expected_height: u32) -> Result<()> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || &png[..8] != PNG_SIGNATURE || &png[12..16] != b"IHDR" {
        bail!("剪贴板图片不是有效的 PNG 数据");
    }
    let width = u32::from_be_bytes(png[16..20].try_into()?);
    let height = u32::from_be_bytes(png[20..24].try_into()?);
    if (width, height) != (expected_width, expected_height) {
        bail!("剪贴板 PNG 尺寸与声明不一致");
    }
    Ok(())
}

fn validate_emoji(emoji: &str) -> Result<()> {
    let emoji = emoji.trim();
    if emoji.is_empty() || emoji.chars().count() > 12 || emoji.chars().any(char::is_control) {
        bail!("设备图标无效");
    }
    Ok(())
}

fn clipboard_root_paths(transfer_root: &Path, safe_entries: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut roots = Vec::new();
    for path in safe_entries {
        let Some(root) = path.components().next() else {
            continue;
        };
        let root = PathBuf::from(root.as_os_str());
        if seen.insert(root.clone()) {
            roots.push(transfer_root.join(root));
        }
    }
    roots
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
            missing_chunks: Vec::new(),
            elapsed_micros: 0,
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

    #[test]
    fn rejects_invalid_sender_identity() {
        let manifest = Manifest {
            version: PROTOCOL_VERSION,
            sender: SenderIdentity {
                id: Uuid::nil(),
                name: "".into(),
            },
            intent: TransferIntent::Manual,
            files: vec![FileEntry {
                relative_path: "ok.txt".into(),
                size: 1,
                modified_millis: 0,
                kind: EntryKind::File,
            }],
            transfer_id: Uuid::new_v4(),
            chunk_size: CHUNK_SIZE,
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn validates_clipboard_png_header_dimensions() {
        let mut png_header = Vec::from(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice());
        png_header.extend_from_slice(&1920_u32.to_be_bytes());
        png_header.extend_from_slice(&1080_u32.to_be_bytes());

        assert!(validate_png_dimensions(&png_header, 1920, 1080).is_ok());
        assert!(validate_png_dimensions(&png_header, 1080, 1920).is_err());
        assert!(validate_png_dimensions(b"not a png", 1, 1).is_err());
    }

    #[tokio::test]
    async fn connection_updates_are_persisted_by_the_receiver() {
        let root = std::env::temp_dir().join(format!("clip-it-link-test-{}", Uuid::new_v4()));
        let paired = PairedDevices::load(root.join("paired-devices.json")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver_store = paired.clone();
        let receiver = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: Request = read_json(&mut stream).await.unwrap();
                let Request::Connection(message) = request else {
                    panic!("expected connection update");
                };
                receive_connection(stream, &receiver_store, message)
                    .await
                    .unwrap();
            }
        });
        let sender = Identity {
            id: Uuid::new_v4(),
            name: "书房电脑".into(),
            emoji: "💻".into(),
            transfer_port: crate::protocol::TRANSFER_PORT,
        };

        set_peer_connection(address, &sender, true).await.unwrap();
        assert!(
            paired
                .list()
                .unwrap()
                .iter()
                .any(|device| device.id == sender.id)
        );
        set_peer_connection(address, &sender, false).await.unwrap();
        receiver.await.unwrap();
        assert!(
            !paired
                .list()
                .unwrap()
                .iter()
                .any(|device| device.id == sender.id)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[tokio::test]
    async fn clipboard_image_uses_binary_payload_after_header() {
        let mut png = Vec::from(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice());
        png.extend_from_slice(&2_u32.to_be_bytes());
        png.extend_from_slice(&3_u32.to_be_bytes());
        png.extend_from_slice(b"binary-payload");
        let expected = png.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Request = read_json(&mut stream).await.unwrap();
            let Request::ClipboardImage(message) = request else {
                panic!("expected clipboard image");
            };
            let mut payload = vec![0_u8; message.length as usize];
            stream.read_exact(&mut payload).await.unwrap();
            assert_eq!(payload, expected);
            assert_eq!((message.width, message.height), (2, 3));
            assert_eq!(*blake3::hash(&payload).as_bytes(), message.blake3);
            send_response(&mut stream, true, "ok", 0, message.length)
                .await
                .unwrap();
        });
        let sender = Identity {
            id: Uuid::new_v4(),
            name: "截图设备".into(),
            emoji: "📸".into(),
            transfer_port: crate::protocol::TRANSFER_PORT,
        };

        send_clipboard_image(address, &png, 2, 3, Uuid::new_v4(), &sender)
            .await
            .unwrap();
        receiver.await.unwrap();
    }

    #[test]
    fn chunks_cover_file_without_overlap() {
        let entry = FileEntry {
            relative_path: "large.bin".into(),
            size: u64::from(CHUNK_SIZE) * 2 + 17,
            modified_millis: 0,
            kind: EntryKind::File,
        };
        let chunks = chunks_for_file(3, &entry, CHUNK_SIZE);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[1].offset, u64::from(CHUNK_SIZE));
        assert_eq!(chunks[2].length, 17);
    }

    #[tokio::test]
    async fn resume_state_survives_new_session() {
        let root = std::env::temp_dir().join(format!("clip-it-resume-test-{}", Uuid::new_v4()));
        let transfer_id = Uuid::new_v4();
        let manifest = Manifest {
            version: PROTOCOL_VERSION,
            sender: SenderIdentity {
                id: Uuid::new_v4(),
                name: "sender".into(),
            },
            intent: TransferIntent::Manual,
            transfer_id,
            chunk_size: 1024 * 1024,
            files: vec![FileEntry {
                relative_path: "large.bin".into(),
                size: 1024 * 1024 + 9,
                modified_millis: 1,
                kind: EntryKind::File,
            }],
        };
        let first = ChunkRange {
            file_index: 0,
            offset: 0,
            length: 1024 * 1024,
        };
        let session =
            prepare_receive_session(&root, manifest.clone(), vec![PathBuf::from("large.bin")])
                .await
                .unwrap();
        {
            let mut session = session.lock().await;
            session.completed.insert(first.clone());
            persist_resume(&session).await.unwrap();
        }
        let resumed = prepare_receive_session(&root, manifest, vec![PathBuf::from("large.bin")])
            .await
            .unwrap();
        assert!(resumed.lock().await.completed.contains(&first));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clipboard_paths_include_each_selected_root_once() {
        let root = Path::new("/downloads/Incoming-test");
        let paths = vec![
            PathBuf::from("folder"),
            PathBuf::from("folder/a.txt"),
            PathBuf::from("single.bin"),
        ];
        assert_eq!(
            clipboard_root_paths(root, &paths),
            vec![root.join("folder"), root.join("single.bin")]
        );
    }
}
