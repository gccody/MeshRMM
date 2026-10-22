//! The website's file browser: a device's folders and files, reached
//! without a remote session.
//!
//! Listings and changes are small, so they go over the control connection:
//! the server sends [`crate::AgentCommand::FileRequest`] and the Agent
//! answers with [`crate::AgentStatusMessage::FileReply`]. File contents never
//! do. For a download or an upload the server sends
//! [`crate::AgentCommand::FileTransfer`], and the Agent connects a WebSocket
//! of its own for it, `/v1/agents/{id}/file-transfers/{transfer_id}`, which
//! the server relays to the browser's HTTP request. Binary messages carry the
//! file's bytes, in pieces of at most [`FILE_STREAM_CHUNK_BYTES`]; text messages
//! are JSON, [`FileStreamEvent`] from the Agent and [`FileStreamControl`]
//! towards it. TCP's flow control on both legs paces the Agent to the
//! browser, so the server holds one piece at a time.
use serde::{Deserialize, Serialize};

use crate::RunAs;

/// The most entries one listing page carries.
pub const MAX_FILE_LIST_PAGE: u32 = 1000;
/// The most paths a move, delete or download names at once.
pub const MAX_FILE_BATCH: usize = 1000;
/// The longest name filter a listing takes, in characters.
pub const MAX_FILE_FILTER_CHARS: usize = 255;
/// The longest path either side accepts, in bytes.
pub const MAX_FILE_PATH_BYTES: usize = 32 * 1024;
/// The size of the pieces a transfer's bytes travel in.
pub const FILE_STREAM_CHUNK_BYTES: usize = 256 * 1024;
/// The largest message either side sends on a transfer socket.
pub const MAX_FILE_STREAM_MESSAGE_BYTES: usize = 1024 * 1024;
/// The default for the largest file a transfer may carry, in MiB.
pub const DEFAULT_FILE_TRANSFER_LIMIT_MB: u32 = 4096;
/// The range an operator may set the transfer limit in, in MiB.
pub const MAX_FILE_TRANSFER_LIMIT_MB: u32 = 1024 * 1024;

/// Whether `megabytes` is a transfer limit an operator may choose.
pub fn valid_file_transfer_limit_mb(megabytes: u32) -> bool {
    (1..=MAX_FILE_TRANSFER_LIMIT_MB).contains(&megabytes)
}

/// What the server asks the Agent to do with the device's files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRequest {
    pub request_id: String,
    /// The account the operation runs as: the Agent's own (SYSTEM or root),
    /// or the user signed in to the computer. Unlike a script, it never
    /// falls back to the Agent's account when nobody is signed in.
    pub run_as: RunAs,
    /// The company's connection notification, rendered, for the computer's
    /// user to see; the server sends it with a technician's first operation
    /// on the device in a while, when company policy shows one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<String>,
    pub operation: FileOperation,
}

/// How a listing is ordered. Folders always come first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSort {
    /// Natural order, ignoring case: `file2` before `file10`.
    #[default]
    Name,
    Size,
    Modified,
    /// By extension, then name.
    Type,
}

impl FileSort {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "name" => Some(Self::Name),
            "size" => Some(Self::Size),
            "modified" => Some(Self::Modified),
            "type" => Some(Self::Type),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileOperation {
    /// The places to start from: drives, or `/`, and the signed-in user's
    /// folders.
    Roots,
    /// A page of a folder's entries.
    List {
        path: String,
        offset: u32,
        limit: u32,
        #[serde(default)]
        sort: FileSort,
        #[serde(default)]
        descending: bool,
        /// Whether hidden and system entries are listed.
        #[serde(default)]
        hidden: bool,
        /// Only entries whose name contains this, ignoring case.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
        /// Reads the folder again instead of paging the Agent's recent
        /// reading of it.
        #[serde(default)]
        refresh: bool,
    },
    CreateFolder {
        path: String,
    },
    /// Renames `path` to `name` in the same folder, never replacing an
    /// existing entry.
    Rename {
        path: String,
        name: String,
    },
    /// Moves each of `paths` into the folder `destination`, never replacing
    /// an existing entry.
    Move {
        paths: Vec<String>,
        destination: String,
    },
    /// Deletes each of `paths`: into the account's Recycle Bin or Trash
    /// when `trash`, else permanently.
    Delete {
        paths: Vec<String>,
        #[serde(default)]
        trash: bool,
    },
    /// Checks that `paths` can be downloaded: one file as itself, or a
    /// folder or several entries as a ZIP archive of at most `max_bytes` of
    /// content.
    PrepareDownload {
        paths: Vec<String>,
        max_bytes: u64,
    },
    /// Starts an upload of `size` bytes to `path`, creating its missing
    /// parent folders. The bytes collect in a hidden partial file beside it.
    PrepareUpload {
        upload_id: String,
        path: String,
        size: u64,
        #[serde(default)]
        overwrite: bool,
    },
    /// How much of an upload the device has.
    UploadStatus {
        upload_id: String,
        path: String,
    },
    /// Checks the upload's SHA-256 and puts it in place.
    FinishUpload {
        upload_id: String,
        path: String,
        size: u64,
        sha256: String,
        #[serde(default)]
        overwrite: bool,
    },
    /// Removes an unfinished upload.
    CancelUpload {
        upload_id: String,
        path: String,
    },
}

impl FileOperation {
    /// The operation's name, for logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Roots => "roots",
            Self::List { .. } => "list",
            Self::CreateFolder { .. } => "create_folder",
            Self::Rename { .. } => "rename",
            Self::Move { .. } => "move",
            Self::Delete { .. } => "delete",
            Self::PrepareDownload { .. } => "prepare_download",
            Self::PrepareUpload { .. } => "prepare_upload",
            Self::UploadStatus { .. } => "upload_status",
            Self::FinishUpload { .. } => "finish_upload",
            Self::CancelUpload { .. } => "cancel_upload",
        }
    }
}

/// What a folder entry is. A link is described by what it points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    File,
    Directory,
    /// A device, socket, pipe, or a link whose target is gone.
    Other,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub kind: FileKind,
    /// A symbolic link, or a junction on Windows.
    #[serde(default, skip_serializing_if = "is_false")]
    pub link: bool,
    /// A file's size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// When it last changed, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    /// Hidden: a name starting with `.` on macOS and Linux, the hidden flag
    /// on macOS and Windows.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// Windows' system flag.
    #[serde(default, skip_serializing_if = "is_false")]
    pub system: bool,
    /// Windows' read-only flag, or no write permission for anyone on macOS
    /// and Linux.
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileListing {
    /// The folder, as the Agent spells it.
    pub path: String,
    /// The folder above, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// The account the folder was read as, such as `NT AUTHORITY\SYSTEM`.
    pub account: String,
    pub offset: u32,
    /// Entries in the whole listing, after the hidden and name filters.
    pub total: u32,
    pub entries: Vec<FileEntry>,
    /// When the folder was read, in milliseconds since the Unix epoch.
    pub read_at: i64,
    /// The folder has more entries than the Agent lists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRootKind {
    /// `/` on macOS and Linux.
    Root,
    Fixed,
    Removable,
    Network,
    Optical,
    Other,
}

/// A drive, `/`, or a mounted volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRoot {
    pub path: String,
    pub label: String,
    pub kind: FileRootKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePlaceKind {
    Home,
    Desktop,
    Documents,
    Downloads,
}

/// A user's folder: the signed-in user's, or the account's own home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePlace {
    pub path: String,
    pub kind: FilePlaceKind,
    /// Whose folder it is.
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRoots {
    pub account: String,
    pub roots: Vec<FileRoot>,
    pub places: Vec<FilePlace>,
}

/// Why an operation failed, for the website to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileErrorCode {
    NotFound,
    AccessDenied,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    InvalidPath,
    InvalidName,
    /// Nobody is signed in to act as.
    NoUser,
    TooLarge,
    NoSpace,
    ChecksumMismatch,
    /// The partial upload isn't where the request expected it.
    OffsetMismatch,
    /// The file changed since the download started.
    Changed,
    /// The entry can't go to a Recycle Bin or Trash; delete it permanently.
    TrashUnavailable,
    /// The Agent is busy with as many transfers as it runs at once.
    Busy,
    /// A folder can't go into itself, and the like.
    InvalidMove,
    Failed,
}

impl FileErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::AccessDenied => "access_denied",
            Self::AlreadyExists => "already_exists",
            Self::NotADirectory => "not_a_directory",
            Self::IsADirectory => "is_a_directory",
            Self::InvalidPath => "invalid_path",
            Self::InvalidName => "invalid_name",
            Self::NoUser => "no_user",
            Self::TooLarge => "too_large",
            Self::NoSpace => "no_space",
            Self::ChecksumMismatch => "checksum_mismatch",
            Self::OffsetMismatch => "offset_mismatch",
            Self::Changed => "changed",
            Self::TrashUnavailable => "trash_unavailable",
            Self::Busy => "busy",
            Self::InvalidMove => "invalid_move",
            Self::Failed => "failed",
        }
    }
}

/// The longest message a [`FileError`] carries, in characters.
pub const MAX_FILE_ERROR_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The error with its message cut to [`MAX_FILE_ERROR_CHARS`] and
    /// without control characters.
    pub fn sanitized(self) -> Self {
        Self {
            code: self.code,
            message: clean(&self.message, MAX_FILE_ERROR_CHARS),
        }
    }
}

fn clean(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .take(limit)
        .collect()
}

/// One entry of a move or delete that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFailure {
    pub path: String,
    pub error: FileError,
}

/// What a move or delete did with each entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBatch {
    /// The entries that were moved or deleted.
    pub done: Vec<String>,
    pub failed: Vec<FileFailure>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDownloadKind {
    /// One file, as it is.
    File,
    /// A ZIP archive the Agent writes as it sends it.
    Archive,
}

/// What a download will carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDownloadInfo {
    /// The file's name, or the archive's without `.zip`.
    pub name: String,
    pub kind: FileDownloadKind,
    /// The file's size, or the total size of the archive's files.
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    /// The files an archive holds.
    #[serde(default)]
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum FileReply {
    Roots(FileRoots),
    Listing(FileListing),
    /// The entry a folder creation, rename or finished upload made.
    Entry {
        path: String,
        entry: FileEntry,
    },
    Batch(FileBatch),
    Download(FileDownloadInfo),
    /// How many bytes of an upload the device has.
    Upload {
        received: u64,
    },
    Failed {
        error: FileError,
    },
}

impl FileReply {
    pub fn failed(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self::Failed {
            error: FileError::new(code, message),
        }
    }
}

/// What the server asks the Agent to stream, on a socket of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransferRequest {
    pub transfer_id: String,
    pub run_as: RunAs,
    pub transfer: FileTransfer,
}

/// A file's version, to resume a download only where it is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVersion {
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "snake_case")]
pub enum FileTransfer {
    /// Sends one file from `offset`, at most `length` bytes of it, or a ZIP
    /// archive of `paths`.
    Download {
        paths: Vec<String>,
        kind: FileDownloadKind,
        #[serde(default)]
        offset: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        length: Option<u64>,
        /// A resumed download goes on only from this version of the file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expect: Option<FileVersion>,
        /// The most file content the transfer may carry.
        max_bytes: u64,
    },
    /// Receives an upload's bytes from `offset`, into its partial file.
    Upload {
        upload_id: String,
        path: String,
        offset: u64,
        size: u64,
    },
}

/// What the Agent says on a transfer socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileStreamEvent {
    /// A download starts: `size` bytes follow, unknown for an archive.
    Begin {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modified: Option<i64>,
    },
    /// An upload's partial file is ready for bytes from `received`.
    Ready { received: u64 },
    /// A download sent `bytes`, whose SHA-256 is `sha256`; an upload's
    /// partial file now holds `received` bytes.
    Done {
        bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
    /// The transfer failed; the socket closes after.
    Failed { error: FileError },
}

/// What the server says on an upload's transfer socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileStreamControl {
    /// The request's body ended; every byte has been sent.
    End,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_have_their_wire_form() {
        let request = FileRequest {
            request_id: "r1".into(),
            run_as: RunAs::System,
            notification: None,
            operation: FileOperation::List {
                path: "C:\\Users".into(),
                offset: 0,
                limit: 500,
                sort: FileSort::Modified,
                descending: true,
                hidden: false,
                filter: None,
                refresh: false,
            },
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["operation"]["op"], "list");
        assert_eq!(json["operation"]["sort"], "modified");
        assert!(json.get("notification").is_none());
        assert_eq!(
            serde_json::from_value::<FileRequest>(json).unwrap(),
            request
        );
        let roots: FileRequest = serde_json::from_str(
            r#"{"request_id":"r2","run_as":"user","operation":{"op":"roots"}}"#,
        )
        .unwrap();
        assert_eq!(roots.operation, FileOperation::Roots);
    }

    #[test]
    fn replies_have_their_wire_form() {
        let reply = FileReply::Entry {
            path: "/tmp/a".into(),
            entry: FileEntry {
                name: "a".into(),
                kind: FileKind::Directory,
                link: false,
                size: None,
                modified: Some(1),
                hidden: false,
                system: false,
                read_only: false,
            },
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert_eq!(json["reply"], "entry");
        assert_eq!(json["entry"]["kind"], "directory");
        assert!(json["entry"].get("hidden").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<FileReply>(json).unwrap(), reply);
        let failed =
            serde_json::to_value(FileReply::failed(FileErrorCode::AccessDenied, "no")).unwrap();
        assert_eq!(
            failed,
            serde_json::json!({"reply":"failed","error":{"code":"access_denied","message":"no"}})
        );
        let batch = serde_json::to_value(FileReply::Batch(FileBatch::default())).unwrap();
        assert_eq!(batch["reply"], "batch");
    }

    #[test]
    fn stream_messages_have_their_wire_form() {
        assert_eq!(
            serde_json::to_string(&FileStreamEvent::Begin {
                size: Some(5),
                modified: None
            })
            .unwrap(),
            r#"{"type":"begin","size":5}"#
        );
        assert_eq!(
            serde_json::from_str::<FileStreamControl>(r#"{"type":"end"}"#).unwrap(),
            FileStreamControl::End
        );
        let transfer = FileTransferRequest {
            transfer_id: "t".into(),
            run_as: RunAs::User,
            transfer: FileTransfer::Upload {
                upload_id: "u".into(),
                path: "/tmp/x".into(),
                offset: 3,
                size: 10,
            },
        };
        let json = serde_json::to_value(&transfer).unwrap();
        assert_eq!(json["transfer"]["direction"], "upload");
        assert_eq!(
            serde_json::from_value::<FileTransferRequest>(json).unwrap(),
            transfer
        );
    }

    #[test]
    fn errors_are_cleaned_and_bounded() {
        let error = FileError::new(
            FileErrorCode::Failed,
            format!("a\u{1b}b{}", "x".repeat(900)),
        )
        .sanitized();
        assert!(error.message.starts_with("ab"));
        assert_eq!(error.message.chars().count(), MAX_FILE_ERROR_CHARS);
    }

    #[test]
    fn limits_and_sorts_are_checked() {
        assert!(valid_file_transfer_limit_mb(DEFAULT_FILE_TRANSFER_LIMIT_MB));
        assert!(!valid_file_transfer_limit_mb(0));
        assert!(!valid_file_transfer_limit_mb(
            MAX_FILE_TRANSFER_LIMIT_MB + 1
        ));
        assert_eq!(FileSort::parse("type"), Some(FileSort::Type));
        assert_eq!(FileSort::parse("owner"), None);
    }
}
