use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 3;
pub const DISCOVERY_MAGIC: &str = "CLIPIT_DISCOVERY_V3";
pub const DISCOVERY_GROUP: &str = "239.255.42.89";
pub const DISCOVERY_PORT: u16 = 42_489;
pub const TRANSFER_PORT: u16 = 42_490;
pub const TRAY_INSTANCE_PORT: u16 = 42_491;
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Request {
    FileTransfer(Manifest),
    ClipboardText(ClipboardText),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Announcement {
    pub magic: String,
    pub version: u16,
    pub id: Uuid,
    pub name: String,
    pub transfer_port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u16,
    pub sender: SenderIdentity,
    pub intent: TransferIntent,
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
pub struct SenderIdentity {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative_path: String,
    pub size: u64,
    pub kind: EntryKind,
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
}
