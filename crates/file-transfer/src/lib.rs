//! Native, session-scoped file copying. File data never passes through signaling.
use anyhow::{bail, ensure};
use meshrmm_protocol::{FileDestination, FileMessage};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "macos")]
use macos as native;
#[cfg(windows)]
use windows as native;

mod receive;
mod send;
mod sweep;
mod worker;

use receive::Incoming;
use send::{AckWindow, send_paths};
use sweep::{remove_batch, sweep_cache, sweep_transfer_folders};

/// Native change counter, shared by text/image and file clipboard polling.
pub fn clipboard_sequence() -> u64 {
    native::clipboard_sequence()
}

pub fn clipboard_has_files() -> bool {
    native::clipboard_files().is_ok_and(|paths| !paths.is_empty())
}

/// Folder, under Documents or the cache, that receives transferred files.
pub const TRANSFER_FOLDER: &str = "MeshRMM Transferred Files";
/// Clipboard copies (automatic sync and explicit paste) stay small; larger
/// files go through Send/Receive files or a drop instead.
pub const CLIPBOARD_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound for one Documents or drop transfer.
pub const TRANSFER_LIMIT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Free space a transfer must leave on the receiving volume.
const FREE_SPACE_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
/// A peer's files are accepted into Documents this long after asking for them.
const PEER_PICK_WINDOW: Duration = Duration::from_secs(15 * 60);
/// The ID of the agent's [`FileMessage::Error`] saying a requested pick sent nothing, which closes
/// the viewer's request. Transfer IDs are timestamps, so none is 0, and earlier viewers ignore
/// errors for transfers they do not know.
const PEER_PICK_ID: u64 = 0;
/// Transfer messages, mostly [`FILE_CHUNK_BYTES`] chunks, a sender keeps
/// unacknowledged. The file channel holds at most 64 KiB unacknowledged by
/// SCTP (see `meshrmm_session_transport::ServiceChannel::writable`), so this
/// covers that plus the receiver's queues. Earlier receivers queue 32
/// commands, which leaves room for their acknowledgements and local requests.
const SEND_WINDOW: usize = 16;
/// Commands a worker queues: a window of the peer's transfer, the
/// acknowledgements for its own, and local requests.
pub const COMMAND_QUEUE: usize = 64;
/// Interrupted transfers leave `.partial-*` folders; untouched ones are removed.
const STALE_PARTIAL_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Clipboard and drop copies live in the cache while an app may still read them.
const CACHE_BATCH_AGE: Duration = Duration::from_secs(60 * 60);
/// Receipts sweep the cache at most this often, since walking it can take a while and batches
/// only expire after [`CACHE_BATCH_AGE`].
const CACHE_SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Which end of a session a worker serves. The agent follows the viewer's
/// requests. The viewer never opens its own picker for the peer and saves only
/// the files it asked for, so a compromised endpoint cannot push files onto the
/// technician's machine or make it upload local files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Agent,
}

pub enum Command {
    Peer(FileMessage),
    Send(Vec<PathBuf>, FileDestination),
    /// Choose local files and send them to the peer's Documents folder.
    Pick,
    /// Ask the peer to choose files and send them to this side's Documents folder.
    RequestPeerPick,
}
#[derive(Clone)]
struct OutgoingFiles {
    sender: mpsc::SyncSender<FileMessage>,
    ready: Arc<tokio::sync::Notify>,
}
impl OutgoingFiles {
    fn send(&self, message: FileMessage) -> Result<(), mpsc::SendError<FileMessage>> {
        self.sender.send(message)?;
        self.ready.notify_one();
        Ok(())
    }
}

#[derive(Clone)]
pub struct TransferSession {
    tx: mpsc::SyncSender<Command>,
    rx: Arc<Mutex<mpsc::Receiver<FileMessage>>>,
    status: Arc<Mutex<String>>,
    ready: Arc<tokio::sync::Notify>,
    clipboard_enabled: fn() -> bool,
}
impl TransferSession {
    /// The endpoint's worker, which answers the viewer's pick requests.
    pub fn agent() -> Self {
        Self::spawn(Role::Agent, || true)
    }
    /// The technician's worker. Clipboard file copies are skipped, not sent,
    /// and rejected on receipt while `clipboard_enabled` returns false; other
    /// transfers are unaffected.
    pub fn viewer(clipboard_enabled: fn() -> bool) -> Self {
        Self::spawn(Role::Viewer, clipboard_enabled)
    }
    fn spawn(role: Role, clipboard_enabled: fn() -> bool) -> Self {
        let (tx, commands) = mpsc::sync_channel(COMMAND_QUEUE);
        let (sender, rx) = mpsc::sync_channel(8);
        let ready = Arc::new(tokio::sync::Notify::new());
        let out = OutgoingFiles {
            sender,
            ready: ready.clone(),
        };
        let status = Arc::new(Mutex::new("Waiting for file-transfer support…".into()));
        let worker_status = status.clone();
        std::thread::spawn(move || {
            worker::run(role, commands, out, worker_status, clipboard_enabled)
        });
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            ready,
            status,
            clipboard_enabled,
        }
    }
    /// Run an agent worker's native OLE operations on a dedicated helper's main
    /// thread while its pipe transport runs separately and remains responsive
    /// to cancellation.
    pub fn run_on_current_thread<T: Send + 'static>(
        client: impl FnOnce(Self) -> T + Send + 'static,
    ) -> T {
        let (tx, commands) = mpsc::sync_channel(COMMAND_QUEUE);
        let (sender, rx) = mpsc::sync_channel(8);
        let ready = Arc::new(tokio::sync::Notify::new());
        let out = OutgoingFiles {
            sender,
            ready: ready.clone(),
        };
        let status = Arc::new(Mutex::new("Waiting for file-transfer support…".into()));
        let session = Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            status: status.clone(),
            ready,
            clipboard_enabled: || true,
        };
        let transport = std::thread::spawn(move || client(session));
        worker::run(Role::Agent, commands, out, status, || true);
        transport.join().expect("file transport thread panicked")
    }
    pub fn command(&self, command: Command) {
        if self.tx.try_send(command).is_err() {
            self.set_status("Transfer queue is busy");
        }
    }
    pub fn receive(&self, message: FileMessage) {
        self.command(Command::Peer(message));
    }
    pub fn send(&self, paths: Vec<PathBuf>, destination: FileDestination) {
        self.command(Command::Send(paths, destination));
    }
    pub fn paste_files(&self, display_id: meshrmm_protocol::DisplayId) -> bool {
        if !(self.clipboard_enabled)() {
            return false;
        }
        let paths = native::clipboard_files().unwrap_or_default();
        if paths.is_empty() {
            return false;
        }
        self.send(paths, FileDestination::ClipboardPaste { display_id });
        true
    }
    pub fn pick(&self) {
        self.command(Command::Pick);
    }
    pub fn request_peer_pick(&self) {
        self.command(Command::RequestPeerPick);
    }
    pub fn outgoing_ready(&self) -> Arc<tokio::sync::Notify> {
        self.ready.clone()
    }
    pub fn poll(&self) -> Option<FileMessage> {
        self.rx.lock().ok()?.try_recv().ok()
    }
    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }
    fn set_status(&self, s: &str) {
        *self.status.lock().unwrap() = s.into();
    }
}
fn id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let (Ok(previous) | Err(previous)) =
        LAST.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            Some(now.max(last + 1))
        });
    now.max(previous + 1)
}

/// Decides which of the peer's requests this side acts on.
struct Admission {
    role: Role,
    /// When this side asked the peer to pick files, oldest first.
    peer_picks: VecDeque<Instant>,
}
impl Admission {
    fn new(role: Role) -> Self {
        Self {
            role,
            peer_picks: VecDeque::new(),
        }
    }
    fn request_peer_pick(&mut self, now: Instant) {
        if self.peer_picks.len() == 4 {
            self.peer_picks.pop_front();
        }
        self.peer_picks.push_back(now);
    }
    /// The peer answered the oldest request without sending files, or with files this side
    /// rejected, so that request no longer admits a transfer.
    fn close_peer_pick(&mut self) {
        self.peer_picks.pop_front();
    }
    /// Only the agent opens a file picker for its peer.
    fn allows_peer_pick(&self) -> bool {
        self.role == Role::Agent
    }
    fn admit(
        &mut self,
        destination: &FileDestination,
        clipboard_enabled: bool,
        now: Instant,
    ) -> Result<(), &'static str> {
        let clipboard = matches!(
            destination,
            FileDestination::Clipboard | FileDestination::ClipboardPaste { .. }
        );
        if clipboard && !clipboard_enabled {
            return Err("Clipboard file transfers are disabled");
        }
        if self.role == Role::Agent {
            return Ok(());
        }
        match destination {
            FileDestination::Clipboard => Ok(()),
            FileDestination::Documents => {
                self.peer_picks
                    .retain(|requested| now.duration_since(*requested) < PEER_PICK_WINDOW);
                self.peer_picks
                    .pop_front()
                    .map(drop)
                    .ok_or("Files are accepted only after you choose Receive files")
            }
            FileDestination::ClipboardPaste { .. } | FileDestination::Drop { .. } => {
                Err("The viewer does not accept pasted or dropped files")
            }
        }
    }
}

/// Whether this job is the agent's answer to a viewer's request to pick files, the only way the
/// agent sends files to the viewer's Documents folder.
fn answers_peer_pick(role: Role, destination: &FileDestination) -> bool {
    role == Role::Agent && *destination == FileDestination::Documents
}

fn end_transfer(ended: &mut VecDeque<u64>, id: u64) {
    if ended.len() == 8 {
        ended.pop_front();
    }
    ended.push_back(id);
}

fn mebibytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024. * 1024. * 1024.))
    } else {
        format!("{:.0} MiB", (bytes as f64 / (1024. * 1024.)).ceil())
    }
}

/// The largest transfer accepted for `destination`.
fn transfer_limit(destination: &FileDestination) -> u64 {
    match destination {
        FileDestination::Clipboard | FileDestination::ClipboardPaste { .. } => {
            CLIPBOARD_LIMIT_BYTES
        }
        FileDestination::Documents | FileDestination::Drop { .. } => TRANSFER_LIMIT_BYTES,
    }
}

/// Checks a transfer's declared size against its destination's limit.
fn check_limit(destination: &FileDestination, bytes: u64) -> anyhow::Result<()> {
    let limit = transfer_limit(destination);
    if bytes <= limit {
        return Ok(());
    }
    if limit == CLIPBOARD_LIMIT_BYTES {
        bail!(
            "Copied files total {}, over the {} clipboard limit; use Send files instead",
            mebibytes(bytes),
            mebibytes(limit)
        );
    }
    bail!(
        "Files total {}, over the {} transfer limit",
        mebibytes(bytes),
        mebibytes(limit)
    )
}

fn progress_detail(bytes: u64, total: u64, entries: usize, total_entries: u64) -> String {
    let percent = if total == 0 {
        entries as f64 / total_entries.max(1) as f64
    } else {
        bytes as f64 / total as f64
    } * 100.;
    format!(
        "{:.0}% — {:.1} of {:.1} MiB · {entries}/{total_entries} items",
        percent.clamp(0., 100.),
        bytes as f64 / 1_048_576.,
        total as f64 / 1_048_576.
    )
}

fn message_id(m: &FileMessage) -> u64 {
    match m {
        FileMessage::Begin { id, .. }
        | FileMessage::Entry { id, .. }
        | FileMessage::Chunk { id, .. }
        | FileMessage::EndEntry { id, .. }
        | FileMessage::Finish { id }
        | FileMessage::Ack { id }
        | FileMessage::Error { id, .. }
        | FileMessage::Totals { id, .. } => *id,
        _ => 0,
    }
}
/// Device names Windows reserves in every folder, compared against a name's
/// part before the first dot with trailing spaces removed.
fn reserved_on_windows(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let Some(number) = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
    else {
        return false;
    };
    let mut digits = number.chars();
    matches!(
        (digits.next(), digits.next()),
        (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
    )
}

/// Accepts a relative `/`-separated manifest path that is safe to create on
/// both Windows and macOS.
fn valid_path(path: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "invalid path length"
    );
    let mut result = PathBuf::new();
    for component in path.split('/') {
        ensure!(
            !component.is_empty()
                && component.len() <= 255
                && component != "."
                && component != ".."
                && !component.chars().any(|c| {
                    c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*')
                })
                && !component.ends_with(['.', ' ']),
            "{component:?} is not a valid file name"
        );
        ensure!(
            !reserved_on_windows(component),
            "{component:?} is a reserved Windows file name"
        );
        result.push(component);
    }
    Ok(result)
}

fn commit_documents(paths: Vec<PathBuf>, documents: PathBuf) -> anyhow::Result<Vec<PathBuf>> {
    // Documents may be redirected to another volume or a network share. Copy
    // into a verified staging directory there instead of relying on rename.
    let transfer_id = id();
    let mut receiver = Incoming::new(transfer_id, FileDestination::Documents, documents)?;
    let mut result = Vec::new();
    send_paths(
        transfer_id,
        paths,
        FileDestination::Documents,
        |message| match message {
            FileMessage::Begin { .. } => Ok(()),
            FileMessage::Finish { .. } => {
                result = receiver.finish()?;
                Ok(())
            }
            message => receiver.accept(message),
        },
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod outgoing_tests {
    use super::*;
    #[tokio::test]
    async fn outgoing_notification_preserves_bounded_queue_and_order() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let ready = Arc::new(tokio::sync::Notify::new());
        let out = OutgoingFiles {
            sender,
            ready: ready.clone(),
        };
        out.send(FileMessage::Available).unwrap();
        tokio::time::timeout(Duration::from_secs(1), ready.notified())
            .await
            .unwrap();
        let (finished, mut done) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let result = out.send(FileMessage::Pick);
            let _ = finished.send(result);
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut done)
                .await
                .is_err()
        );
        assert!(matches!(
            receiver.try_recv().unwrap(),
            FileMessage::Available
        ));
        tokio::time::timeout(Duration::from_secs(1), ready.notified())
            .await
            .unwrap();
        done.await.unwrap().unwrap();
        assert!(matches!(receiver.try_recv().unwrap(), FileMessage::Pick));
    }
}
