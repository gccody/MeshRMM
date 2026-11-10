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
    /// The receiver applied the message. For a `Finish`: every file arrived,
    /// matched its checksum, was written (handed to the operating system, not
    /// flushed to the disk) and is in place, in Documents, on the clipboard or
    /// with the drop target. A drop no app took is moved into Documents,
    /// except across volumes: then it is verified in the cache, its copy to
    /// Documents follows, and an `Error` for the transfer reports a failure.
    Ack {
        id: u64,
    },
    Error {
        id: u64,
        reason: String,
    },
    /// Aggregate manifest sizes for receiver-side progress. Appended for stable tags.
    Totals {
        id: u64,
        bytes: u64,
        entries: u64,
    },
}
