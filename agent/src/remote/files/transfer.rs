//! A transfer's socket: the Agent connects
//! `/v1/agents/{id}/file-transfers/{transfer_id}` when the server asks, and
//! sends a download's bytes or takes an upload's.
//!
//! File work runs on a thread of its own, as the transfer's account, joined
//! to the socket by a channel of a few pieces, so a slow browser holds the
//! file's reading where it is instead of filling memory: the socket's send
//! waits on TCP, the channel fills, and the reading thread waits on the
//! channel. A download's SHA-256 is worked out as it's read, and sent last
//! for the server to check against what it relayed. An upload's pieces are
//! written as they arrive; a socket that drops leaves the partial file to
//! go on from.
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use meshrmm_protocol::{
    FILE_STREAM_CHUNK_BYTES, FileDownloadKind, FileError, FileErrorCode, FileStreamControl,
    FileStreamEvent, FileTransfer, FileTransferRequest, FileVersion, RunAs,
};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::uploads::{self, hex};
use super::{Account, archive, io_error, paths, unix_ms};
use crate::remote::config::{Config, ExecutionMode};

/// Transfers past this many at once are refused.
const MAX_TRANSFERS: usize = 8;
/// A peer that takes this long to take a piece, or to send one, is gone.
/// Long, since a browser may pause a download.
const STALL: Duration = Duration::from_secs(10 * 60);

static TRANSFERS: AtomicUsize = AtomicUsize::new(0);

/// A place among the running transfers, given back on drop.
struct Slot;

impl Slot {
    fn take() -> Option<Self> {
        TRANSFERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |running| {
                (running < MAX_TRANSFERS).then_some(running + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        TRANSFERS.fetch_sub(1, Ordering::AcqRel);
    }
}

type Socket = meshrmm_signaling_client::Socket;

/// Runs `request`'s transfer on a task of its own, holding `activity` while
/// it runs.
pub(in crate::remote) fn start(
    config: &Config,
    mode: ExecutionMode,
    request: FileTransferRequest,
    activity: impl Send + 'static,
) {
    let config = config.clone();
    tokio::spawn(async move {
        let _activity = activity;
        run(config, mode, request).await;
    });
}

async fn run(config: Config, mode: ExecutionMode, request: FileTransferRequest) {
    let transfer_id = request.transfer_id.clone();
    if !uploads::valid_id(&transfer_id) {
        tracing::warn!(%transfer_id, "refused a transfer with an invalid ID");
        return;
    }
    let slot = Slot::take();
    let url = match meshrmm_signaling_client::endpoint_url(
        &config.server,
        &[
            "v1",
            "agents",
            &config.device_id,
            "file-transfers",
            &transfer_id,
        ],
        &[],
        true,
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(%transfer_id, error = %error, "could not build the transfer's address");
            return;
        }
    };
    let mut socket = match meshrmm_signaling_client::authenticated_websocket(
        url,
        &config.agent_token,
    )
    .await
    {
        Ok((socket, _)) => socket,
        Err(error) => {
            tracing::warn!(%transfer_id, error = %format!("{error:#}"), "could not connect the transfer's socket");
            return;
        }
    };
    if slot.is_none() {
        let error = FileError::new(
            FileErrorCode::Busy,
            format!(
                "The device is running {MAX_TRANSFERS} transfers already; try again when one finishes."
            ),
        );
        fail(&mut socket, error).await;
        return;
    }
    let started = Instant::now();
    let run_as = request.run_as;
    let result = match request.transfer {
        FileTransfer::Download {
            paths,
            kind,
            offset,
            length,
            expect,
            max_bytes,
        } => {
            let job = Download {
                paths,
                kind,
                offset,
                length,
                expect,
                max_bytes,
            };
            download(&mut socket, mode, run_as, job).await
        }
        FileTransfer::Upload {
            upload_id,
            path,
            offset,
            size,
        } => upload(&mut socket, mode, run_as, upload_id, path, offset, size).await,
    };
    let seconds = started.elapsed().as_secs();
    match result {
        Ok(Ended::Done(bytes)) => {
            tracing::info!(%transfer_id, run_as = run_as.as_str(), bytes, seconds, "finished a file transfer");
            let _ = tokio::time::timeout(Duration::from_secs(10), socket.close(None)).await;
        }
        Ok(Ended::Interrupted(bytes)) => {
            tracing::info!(%transfer_id, bytes, seconds, "a file transfer was interrupted");
        }
        Err(error) => {
            tracing::info!(%transfer_id, code = error.code.as_str(), message = error.message, "a file transfer failed");
            fail(&mut socket, error).await;
        }
    }
}

/// How a transfer ended, with the bytes it moved.
enum Ended {
    Done(u64),
    /// The server closed the socket, or it dropped.
    Interrupted(u64),
}

async fn send(socket: &mut Socket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(STALL, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn event(socket: &mut Socket, event: &FileStreamEvent) -> bool {
    let text = serde_json::to_string(event).expect("stream events serialize");
    send(socket, Message::Text(text.into())).await
}

async fn fail(socket: &mut Socket, error: FileError) {
    let _ = event(
        socket,
        &FileStreamEvent::Failed {
            error: error.sanitized(),
        },
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), socket.close(None)).await;
}

struct Download {
    paths: Vec<String>,
    kind: FileDownloadKind,
    offset: u64,
    length: Option<u64>,
    expect: Option<FileVersion>,
    max_bytes: u64,
}

/// What the reading thread hands the socket.
enum Piece {
    Begin {
        size: Option<u64>,
        modified: Option<i64>,
    },
    Data(Vec<u8>),
    Done {
        bytes: u64,
        sha256: String,
    },
}

async fn download(
    socket: &mut Socket,
    mode: ExecutionMode,
    run_as: RunAs,
    job: Download,
) -> Result<Ended, FileError> {
    let (pieces_tx, mut pieces) = mpsc::channel::<Piece>(4);
    let (failed_tx, mut failed) = oneshot::channel::<FileError>();
    std::thread::Builder::new()
        .name("meshrmm-file-download".into())
        .spawn(move || {
            if let Err(error) = read(mode, run_as, job, &pieces_tx) {
                let _ = failed_tx.send(error);
            }
        })
        .map_err(|_| {
            FileError::new(
                FileErrorCode::Failed,
                "The Agent could not start the transfer.",
            )
        })?;
    let mut sent = 0;
    loop {
        tokio::select! {
            piece = pieces.recv() => {
                let message = match piece {
                    Some(Piece::Begin { size, modified }) => {
                        let text = serde_json::to_string(&FileStreamEvent::Begin { size, modified }).expect("serializes");
                        Message::Text(text.into())
                    }
                    Some(Piece::Data(bytes)) => {
                        sent += bytes.len() as u64;
                        Message::Binary(bytes.into())
                    }
                    Some(Piece::Done { bytes, sha256 }) => {
                        if !event(socket, &FileStreamEvent::Done { bytes, sha256: Some(sha256) }).await {
                            return Ok(Ended::Interrupted(sent));
                        }
                        return Ok(Ended::Done(sent));
                    }
                    // The thread ended without finishing: it failed.
                    None => {
                        return Err(failed.try_recv().unwrap_or_else(|_| {
                            FileError::new(FileErrorCode::Failed, "The Agent stopped reading the file.")
                        }));
                    }
                };
                if !send(socket, message).await {
                    return Ok(Ended::Interrupted(sent));
                }
            }
            // The server closes the socket when the browser goes away.
            frame = socket.next() => match frame {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return Ok(Ended::Interrupted(sent)),
            },
        }
    }
}

/// Reads the download on this thread, as its account. Dropping `pieces`
/// without [`Piece::Done`] means it failed.
fn read(
    mode: ExecutionMode,
    run_as: RunAs,
    job: Download,
    pieces: &mpsc::Sender<Piece>,
) -> Result<(), FileError> {
    let account = Account::resolve(mode, run_as)?;
    let paths = job
        .paths
        .iter()
        .map(|text| paths::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    let gone = || FileError::new(FileErrorCode::Failed, "The transfer was closed.");
    match job.kind {
        FileDownloadKind::File => {
            let [path] = paths.as_slice() else {
                return Err(FileError::new(
                    FileErrorCode::Failed,
                    "A file download has one file.",
                ));
            };
            let (mut file, size, modified) = account.run(|| {
                let file = std::fs::File::open(path).map_err(|error| io_error(&error, path))?;
                let metadata = file.metadata().map_err(|error| io_error(&error, path))?;
                if metadata.is_dir() {
                    return Err(FileError::new(
                        FileErrorCode::IsADirectory,
                        format!("{} is a folder.", path.display()),
                    ));
                }
                Ok((
                    file,
                    metadata.len(),
                    metadata.modified().ok().and_then(unix_ms),
                ))
            })?;
            if let Some(expect) = job.expect
                && (expect.size != size || expect.modified != modified)
            {
                return Err(FileError::new(
                    FileErrorCode::Changed,
                    format!(
                        "{} changed since the download started; download it again.",
                        path.display()
                    ),
                ));
            }
            if size > job.max_bytes {
                return Err(FileError::new(
                    FileErrorCode::TooLarge,
                    format!("{} is over this server's transfer limit.", path.display()),
                ));
            }
            if job.offset > size {
                return Err(FileError::new(
                    FileErrorCode::Changed,
                    "The file is shorter than the part asked for.",
                ));
            }
            file.seek(SeekFrom::Start(job.offset))
                .map_err(|error| io_error(&error, path))?;
            let mut remaining = job.length.unwrap_or(u64::MAX).min(size - job.offset);
            pieces
                .blocking_send(Piece::Begin {
                    size: Some(remaining),
                    modified,
                })
                .map_err(|_| gone())?;
            let mut hasher = Sha256::new();
            let mut bytes = 0;
            while remaining > 0 {
                let mut piece = vec![0; FILE_STREAM_CHUNK_BYTES.min(remaining as usize)];
                let read = file
                    .read(&mut piece)
                    .map_err(|error| io_error(&error, path))?;
                if read == 0 {
                    return Err(FileError::new(
                        FileErrorCode::Changed,
                        format!(
                            "{} got shorter while it was sent; download it again.",
                            path.display()
                        ),
                    ));
                }
                piece.truncate(read);
                hasher.update(&piece);
                bytes += read as u64;
                remaining -= read as u64;
                pieces
                    .blocking_send(Piece::Data(piece))
                    .map_err(|_| gone())?;
            }
            pieces
                .blocking_send(Piece::Done {
                    bytes,
                    sha256: hex(&hasher.finalize()),
                })
                .map_err(|_| gone())
        }
        FileDownloadKind::Archive => {
            pieces
                .blocking_send(Piece::Begin {
                    size: None,
                    modified: None,
                })
                .map_err(|_| gone())?;
            // The whole archive is written as the account, since it opens
            // every file in it.
            let (summary, out) =
                account.run(|| archive::write(&paths, job.max_bytes, Pieces::new(pieces)))?;
            let (bytes, sha256) = out.finish().map_err(|_| gone())?;
            tracing::debug!(
                files = summary.files,
                skipped = summary.skipped.len(),
                "wrote an archive"
            );
            pieces
                .blocking_send(Piece::Done { bytes, sha256 })
                .map_err(|_| gone())
        }
    }
}

/// Collects an archive into pieces for the socket, hashing it on the way.
struct Pieces<'a> {
    pieces: &'a mpsc::Sender<Piece>,
    buffer: Vec<u8>,
    hasher: Sha256,
    bytes: u64,
}

impl<'a> Pieces<'a> {
    fn new(pieces: &'a mpsc::Sender<Piece>) -> Self {
        Self {
            pieces,
            buffer: Vec::with_capacity(FILE_STREAM_CHUNK_BYTES),
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn flush_piece(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let piece = std::mem::replace(
            &mut self.buffer,
            Vec::with_capacity(FILE_STREAM_CHUNK_BYTES),
        );
        self.hasher.update(&piece);
        self.bytes += piece.len() as u64;
        self.pieces
            .blocking_send(Piece::Data(piece))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }

    /// Sends what's left, and returns the archive's size and SHA-256.
    fn finish(mut self) -> std::io::Result<(u64, String)> {
        self.flush_piece()?;
        Ok((self.bytes, hex(&self.hasher.finalize())))
    }
}

impl Write for Pieces<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = FILE_STREAM_CHUNK_BYTES - self.buffer.len();
        let taken = room.min(data.len());
        self.buffer.extend_from_slice(&data[..taken]);
        if self.buffer.len() == FILE_STREAM_CHUNK_BYTES {
            self.flush_piece()?;
        }
        Ok(taken)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn upload(
    socket: &mut Socket,
    mode: ExecutionMode,
    run_as: RunAs,
    upload_id: String,
    path: String,
    offset: u64,
    size: u64,
) -> Result<Ended, FileError> {
    let opened = {
        let (upload_id, path) = (upload_id.clone(), path.clone());
        tokio::task::spawn_blocking(move || {
            let account = Account::resolve(mode, run_as)?;
            uploads::open(&account, &upload_id, &path, offset, size)
        })
        .await
        .map_err(|_| {
            FileError::new(
                FileErrorCode::Failed,
                "The Agent could not open the upload.",
            )
        })?
    };
    let file = opened?;
    let shown: PathBuf = paths::parse(&path)?;
    if !event(socket, &FileStreamEvent::Ready { received: offset }).await {
        return Ok(Ended::Interrupted(0));
    }
    // A short queue: waiting for room holds the socket, and so the
    // browser, to the disk's pace.
    let (pieces_tx, pieces) = mpsc::channel::<Vec<u8>>(4);
    let (written_tx, written) = oneshot::channel();
    std::thread::Builder::new()
        .name("meshrmm-file-upload".into())
        .spawn(move || {
            let _ = written_tx.send(write(file, pieces));
        })
        .map_err(|_| {
            FileError::new(
                FileErrorCode::Failed,
                "The Agent could not start the transfer.",
            )
        })?;
    let mut received = 0u64;
    let mut ended = false;
    loop {
        let frame = match tokio::time::timeout(STALL, socket.next()).await {
            Ok(frame) => frame,
            Err(_) => break,
        };
        match frame {
            Some(Ok(Message::Binary(bytes))) => {
                if offset + received + bytes.len() as u64 > size {
                    drop(pieces_tx);
                    let _ = written.await;
                    return Err(FileError::new(
                        FileErrorCode::TooLarge,
                        "More bytes arrived than the upload announced.",
                    ));
                }
                received += bytes.len() as u64;
                if pieces_tx.send(bytes.to_vec()).await.is_err() {
                    break;
                }
            }
            Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                Ok(FileStreamControl::End) => {
                    ended = true;
                    break;
                }
                Err(_) => break,
            },
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Close(_)) | Err(_)) | None => break,
        }
    }
    drop(pieces_tx);
    let total = written
        .await
        .map_err(|_| FileError::new(FileErrorCode::Failed, "The upload's writer stopped."))?
        .map_err(|error| io_error(&error, &shown))?;
    if !ended {
        return Ok(Ended::Interrupted(received));
    }
    if !event(
        socket,
        &FileStreamEvent::Done {
            bytes: total,
            sha256: None,
        },
    )
    .await
    {
        return Ok(Ended::Interrupted(received));
    }
    Ok(Ended::Done(received))
}

/// Writes pieces to the partial file until they stop, and returns its
/// length, with everything written on disk.
fn write(mut file: std::fs::File, mut pieces: mpsc::Receiver<Vec<u8>>) -> std::io::Result<u64> {
    while let Some(piece) = pieces.blocking_recv() {
        file.write_all(&piece)?;
    }
    file.flush()?;
    file.sync_data()?;
    Ok(file.metadata()?.len())
}
