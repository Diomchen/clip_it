use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 4;
pub const DISCOVERY_MAGIC: &str = "CLIPIT_DISCOVERY_V4";
pub const DISCOVERY_GROUP: &str = "239.255.42.89";
pub const DISCOVERY_PORT: u16 = 42_489;
pub const TRANSFER_PORT: u16 = 42_490;
pub const TRAY_INSTANCE_PORT: u16 = 42_491;
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CLIPBOARD_IMAGE_PIXELS: u64 = 100_000_000;

pub fn default_device_emoji() -> String {
    "📋".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Request {
    FileTransfer(Manifest),
    FileChunk(FileChunk),
    CompleteTransfer(CompleteTransfer),
    ClipboardText(ClipboardText),
    ClipboardImage(ClipboardImage),
    Ping(Ping),
    Connection(ConnectionUpdate),
    Benchmark(BenchmarkRequest),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Announcement {
    pub magic: String,
    pub version: u16,
    pub id: Uuid,
    pub name: String,
    #[serde(default = "default_device_emoji")]
    pub emoji: String,
    pub transfer_port: u16,
    #[serde(default)]
    pub connected_devices: Vec<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u16,
    pub sender: SenderIdentity,
    pub intent: TransferIntent,
    pub transfer_id: Uuid,
    pub chunk_size: u32,
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferIntent {
    Manual,
    Clipboard,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardText {
    pub version: u16,
    pub sender: SenderIdentity,
    pub event_id: Uuid,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardImage {
    pub version: u16,
    pub sender: SenderIdentity,
    pub event_id: Uuid,
    pub width: u32,
    pub height: u32,
    pub length: u64,
    pub blake3: [u8; 32],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ping {
    pub version: u16,
    pub sender: SenderIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionUpdate {
    pub version: u16,
    pub sender: SenderIdentity,
    #[serde(default = "default_device_emoji")]
    pub emoji: String,
    pub connected: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileChunk {
    pub version: u16,
    pub sender: SenderIdentity,
    pub transfer_id: Uuid,
    pub file_index: u32,
    pub offset: u64,
    pub length: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompleteTransfer {
    pub version: u16,
    pub sender: SenderIdentity,
    pub transfer_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkRequest {
    pub version: u16,
    pub sender: SenderIdentity,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SenderIdentity {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative_path: String,
    pub size: u64,
    pub modified_millis: u64,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ChunkRange {
    pub file_index: u32,
    pub offset: u64,
    pub length: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub missing_chunks: Vec<ChunkRange>,
    #[serde(default)]
    pub elapsed_micros: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_announcement_defaults_to_no_connections() {
        let announcement: Announcement = serde_json::from_str(
            r#"{"magic":"CLIPIT_DISCOVERY_V4","version":4,"id":"00000000-0000-4000-8000-000000000001","name":"旧设备","emoji":"📋","transfer_port":42490}"#,
        )
        .unwrap();
        assert!(announcement.connected_devices.is_empty());
    }
}
