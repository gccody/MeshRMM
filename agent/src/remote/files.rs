//! The website's file browser on this computer. See docs/files.md.
//!
//! The server asks for listings and changes over the control connection
//! ([`FileRequest`]), and the Agent answers there ([`Files::next`]). Each
//! runs on a blocking thread, as SYSTEM or root, or as the user signed in to
//! the computer: on a thread of its own that takes on the user's identity
//! (Windows impersonation; per-thread credentials on macOS and Linux) and
//! ends with the operation, so nothing else ever runs as them. File contents
//! go over a socket of the transfer's own ([`transfer`]), never the control
//! connection.
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{FileError, FileErrorCode, FileOperation, FileReply, FileRequest};
use tokio::sync::{Semaphore, mpsc};

use super::config::ExecutionMode;

mod account;
mod archive;
mod listing;
mod operations;
mod paths;
mod roots;
pub(super) mod transfer;
#[cfg(unix)]
mod unix;
mod uploads;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use self::unix as platform;
#[cfg(windows)]
use self::windows as platform;

use self::account::Account;

/// Operations that run at once; more wait their turn.
const CONCURRENT_OPERATIONS: usize = 4;
/// How often abandoned uploads are looked for.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The Coordinator's file browser: runs the server's requests and queues
/// their answers. It outlives a connection, so an answer that arrives after
/// a reconnect is still sent; the server ignores answers it gave up on.
pub(super) struct Files {
    replies_tx: mpsc::UnboundedSender<(String, FileReply)>,
    replies: mpsc::UnboundedReceiver<(String, FileReply)>,
    running: Arc<Semaphore>,
    listings: Arc<listing::Cache>,
    sweep: tokio::time::Interval,
}

impl Files {
    pub(super) fn new() -> Self {
        let (replies_tx, replies) = mpsc::unbounded_channel();
        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            replies_tx,
            replies,
            running: Arc::new(Semaphore::new(CONCURRENT_OPERATIONS)),
            listings: Arc::new(listing::Cache::default()),
            sweep,
        }
    }

    /// Runs `request` in the background; its answer comes from [`Self::next`].
    pub(super) fn request(&self, mode: ExecutionMode, request: FileRequest) {
        let replies = self.replies_tx.clone();
        let running = Arc::clone(&self.running);
        let listings = Arc::clone(&self.listings);
        tokio::spawn(async move {
            let Ok(_permit) = running.acquire_owned().await else {
                return;
            };
            if let Some(text) = request.notification.clone() {
                super::terminal::show_notice(mode, text);
            }
            let request_id = request.request_id.clone();
            let operation = request.operation.name();
            let reply = tokio::task::spawn_blocking(move || operate(mode, &listings, request))
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, operation, "a file operation's task failed");
                    FileReply::failed(FileErrorCode::Failed, "The Agent could not finish it.")
                });
            if let FileReply::Failed { error } = &reply {
                tracing::info!(%request_id, operation, code = error.code.as_str(), message = error.message, "a file operation failed");
            }
            let _ = replies.send((request_id, reply));
        });
    }

    /// The next answer to send, and every hour, a look for uploads
    /// abandoned for a day.
    pub(super) async fn next(&mut self) -> (String, FileReply) {
        loop {
            tokio::select! {
                reply = self.replies.recv() => {
                    if let Some(reply) = reply {
                        return reply;
                    }
                }
                _ = self.sweep.tick() => {
                    tokio::task::spawn_blocking(uploads::sweep);
                }
            }
        }
    }
}

/// Runs one request as its account.
fn operate(mode: ExecutionMode, listings: &listing::Cache, request: FileRequest) -> FileReply {
    let account = match Account::resolve(mode, request.run_as) {
        Ok(account) => account,
        Err(error) => return FileReply::Failed { error },
    };
    let result = match request.operation {
        FileOperation::Roots => Ok(roots::roots(mode, &account)),
        FileOperation::List {
            path,
            offset,
            limit,
            sort,
            descending,
            hidden,
            filter,
            refresh,
        } => listing::list(
            &account,
            listings,
            &path,
            listing::Page {
                offset,
                limit,
                sort,
                descending,
                hidden,
                filter,
                refresh,
            },
        ),
        operation => {
            // Anything that changes files makes recent listings stale.
            if !matches!(
                operation,
                FileOperation::PrepareDownload { .. } | FileOperation::UploadStatus { .. }
            ) {
                listings.clear();
            }
            change(&account, operation)
        }
    };
    result.unwrap_or_else(|error| FileReply::Failed {
        error: error.sanitized(),
    })
}

fn change(account: &Account, operation: FileOperation) -> Result<FileReply, FileError> {
    match operation {
        FileOperation::Roots | FileOperation::List { .. } => {
            unreachable!("listings are handled by operate")
        }
        FileOperation::CreateFolder { path } => operations::create_folder(account, &path),
        FileOperation::Rename { path, name } => operations::rename(account, &path, &name),
        FileOperation::Move { paths, destination } => {
            operations::move_all(account, &paths, &destination).map(FileReply::Batch)
        }
        FileOperation::Delete { paths, trash } => {
            operations::delete_all(account, &paths, trash).map(FileReply::Batch)
        }
        FileOperation::PrepareDownload { paths, max_bytes } => {
            operations::prepare_download(account, &paths, max_bytes).map(FileReply::Download)
        }
        FileOperation::PrepareUpload {
            upload_id,
            path,
            size,
            overwrite,
        } => uploads::prepare(account, &upload_id, &path, size, overwrite),
        FileOperation::UploadStatus { upload_id, path } => {
            uploads::status(account, &upload_id, &path)
        }
        FileOperation::FinishUpload {
            upload_id,
            path,
            size,
            sha256,
            overwrite,
        } => uploads::finish(account, &upload_id, &path, size, &sha256, overwrite),
        FileOperation::CancelUpload { upload_id, path } => {
            uploads::cancel(account, &upload_id, &path)
        }
    }
}

/// `time` in milliseconds since the Unix epoch.
fn unix_ms(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

fn now_ms() -> i64 {
    unix_ms(SystemTime::now()).unwrap_or_default()
}

/// What went wrong with `path`, for the website.
fn io_error(error: &std::io::Error, path: &Path) -> FileError {
    use std::io::ErrorKind;
    let shown = path.display();
    let (code, message) = match error.kind() {
        ErrorKind::NotFound => (FileErrorCode::NotFound, format!("{shown} doesn't exist.")),
        ErrorKind::PermissionDenied => (FileErrorCode::AccessDenied, platform::denied(error, path)),
        ErrorKind::AlreadyExists => (
            FileErrorCode::AlreadyExists,
            format!("{shown} already exists."),
        ),
        ErrorKind::NotADirectory => (
            FileErrorCode::NotADirectory,
            format!("{shown} isn't a folder."),
        ),
        ErrorKind::IsADirectory => (FileErrorCode::IsADirectory, format!("{shown} is a folder.")),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => (
            FileErrorCode::NoSpace,
            format!("There isn't enough space for {shown}."),
        ),
        ErrorKind::InvalidFilename => (
            FileErrorCode::InvalidName,
            format!("{shown} isn't a name this computer allows."),
        ),
        ErrorKind::ReadOnlyFilesystem => (
            FileErrorCode::AccessDenied,
            format!("{shown} is on a read-only volume."),
        ),
        ErrorKind::DirectoryNotEmpty => (
            FileErrorCode::AlreadyExists,
            format!("{shown} is a folder that isn't empty."),
        ),
        _ => (FileErrorCode::Failed, format!("{shown}: {error}")),
    };
    FileError::new(code, message)
}

fn invalid_path(message: impl Into<String>) -> FileError {
    FileError::new(FileErrorCode::InvalidPath, message)
}
