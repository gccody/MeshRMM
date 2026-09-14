use serde::{Deserialize, Serialize};

pub const FILE_CHUNK_BYTES: usize = 32 * 1024;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileDestination {
    Documents,
    Clipboard,
    ClipboardPaste {
        display_id: crate::DisplayId,
    },
    Drop {
        display_id: crate::DisplayId,
        x: u16,
        y: u16,
    },
}
/// Ordered, acknowledged chunks bound both memory and disk writes. Paths are
/// relative manifest entries, never remote-selected absolute filesystem paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileMessage {
    Available,
    Pick,
    Begin {
        id: u64,
        destination: FileDestination,
    },
    Entry {
        id: u64,
        path: String,
        size: Option<u64>,
    },
    Chunk {
        id: u64,
        data: Vec<u8>,
    },
    EndEntry {
        id: u64,
        sha256: Vec<u8>,
    },
    Finish {
        id: u64,
    },
    Ack {
        id: u64,
    },
    Error {
        id: u64,
        reason: String,
    },
}
