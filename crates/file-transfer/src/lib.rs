//! Native, session-scoped file copying. File data never passes through signaling.
use anyhow::{Context, bail, ensure};
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
        std::thread::spawn(move || worker(role, commands, out, worker_status, clipboard_enabled));
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
        worker(Role::Agent, commands, out, status, || true);
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

fn worker(
    role: Role,
    commands: mpsc::Receiver<Command>,
    out: OutgoingFiles,
    status: Arc<Mutex<String>>,
    clipboard_enabled: fn() -> bool,
) {
    let _native = match native::initialize() {
        Ok(v) => v,
        Err(e) => {
            *status.lock().unwrap() = format!("File transfers unavailable: {e:#}");
            return;
        }
    };
    sweep_transfer_folders();
    let _ = out.send(FileMessage::Available);
    let mut admission = Admission::new(role);
    let mut incoming: Option<Incoming> = None;
    // Transfers rejected or failed here, whose in-flight messages are dropped.
    let mut ended = VecDeque::new();
    let mut progress: Option<native::Progress> = None;
    let mut progress_updated = Instant::now();
    let mut sender: Option<(u64, mpsc::SyncSender<bool>)> = None;
    let mut sender_thread: Option<std::thread::JoinHandle<()>> = None;
    let mut pending = VecDeque::new();
    let mut available = false;
    let mut last_clipboard_sequence = native::clipboard_sequence();
    // The sweep at startup covered the cache.
    let mut last_cache_sweep = Instant::now();
    let mut poll = Instant::now();
    loop {
        let command = match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => None,
        };
        let command = match command {
            Some(Command::Peer(FileMessage::Begin { id, destination })) => {
                // Rejecting a new transfer leaves the one in progress intact.
                let admitted = if incoming.is_some() {
                    if destination == FileDestination::Documents {
                        admission.close_peer_pick();
                    }
                    Err("Another transfer is in progress")
                } else {
                    admission.admit(&destination, clipboard_enabled(), Instant::now())
                };
                match admitted {
                    Ok(()) => Some(Command::Peer(FileMessage::Begin { id, destination })),
                    Err(reason) => {
                        tracing::info!(id, ?destination, reason, "rejected file transfer");
                        end_transfer(&mut ended, id);
                        if role == Role::Viewer && destination != FileDestination::Clipboard {
                            *status.lock().unwrap() = reason.into();
                        }
                        let reason = reason.into();
                        if out.send(FileMessage::Error { id, reason }).is_err() {
                            break;
                        }
                        None
                    }
                }
            }
            command => command,
        };
        let mut send = None;
        match command {
            Some(Command::Pick) => {
                if available {
                    match native::pick() {
                        Ok(paths) => send = Some((paths, FileDestination::Documents)),
                        Err(error) => {
                            tracing::warn!(%error, "file picker failed");
                            *status.lock().unwrap() = format!("File picker failed: {error:#}");
                        }
                    }
                }
            }
            Some(Command::RequestPeerPick) => {
                if !available {
                    *status.lock().unwrap() = "Waiting for file-transfer support…".into();
                } else {
                    admission.request_peer_pick(Instant::now());
                    if out.send(FileMessage::Pick).is_err() {
                        break;
                    }
                }
            }
            Some(Command::Send(paths, destination)) => {
                if available {
                    send = Some((paths, destination));
                }
            }
            Some(Command::Peer(FileMessage::Available)) => {
                available = true;
                *status.lock().unwrap() = "Ready".into();
            }
            Some(Command::Peer(FileMessage::Pick)) if !admission.allows_peer_pick() => {
                tracing::warn!("ignored a request to pick local files for the remote device");
            }
            Some(Command::Peer(FileMessage::Pick)) => {
                let picked = if !available {
                    Err("File transfers are not ready".to_owned())
                } else {
                    match native::pick() {
                        Ok(paths) if paths.is_empty() => Err("No files were chosen".to_owned()),
                        Ok(paths) => Ok(paths),
                        Err(error) => {
                            tracing::warn!(%error, "file picker failed");
                            *status.lock().unwrap() = format!("File picker failed: {error:#}");
                            Err(format!("File picker failed: {error:#}"))
                        }
                    }
                };
                match picked {
                    Ok(paths) => send = Some((paths, FileDestination::Documents)),
                    Err(reason) => {
                        let reply = FileMessage::Error {
                            id: PEER_PICK_ID,
                            reason,
                        };
                        if out.send(reply).is_err() {
                            break;
                        }
                    }
                }
            }
            Some(Command::Peer(FileMessage::Error { id, reason }))
                if id == PEER_PICK_ID && role == Role::Viewer =>
            {
                admission.close_peer_pick();
                *status.lock().unwrap() = format!("No files received: {reason}");
            }
            Some(Command::Peer(FileMessage::Ack { id })) => {
                if let Some((active, tx)) = &sender
                    && *active == id
                {
                    let _ = tx.try_send(true);
                }
            }
            Some(Command::Peer(FileMessage::Error { id, reason })) => {
                // Errors for transfers rejected before they began here are not ours to report.
                if sender.as_ref().is_some_and(|(active, _)| *active == id)
                    || incoming.as_ref().is_some_and(|i| i.id == id)
                {
                    *status.lock().unwrap() = format!("Transfer failed: {reason}");
                }
                if let Some((active, tx)) = &sender
                    && *active == id
                {
                    let _ = tx.try_send(false);
                }
                if incoming.as_ref().is_some_and(|i| i.id == id) {
                    incoming = None;
                    progress = None;
                }
            }
            // A sender has a window of messages in flight when it learns its
            // transfer failed; answering each would only repeat the error.
            Some(Command::Peer(message)) if ended.contains(&message_id(&message)) => {}
            Some(Command::Peer(message)) => {
                let packet_id = message_id(&message);
                let result = (|| -> anyhow::Result<()> {
                    if let FileMessage::Begin { id, destination } = message {
                        tracing::info!(id, ?destination, "receiving file transfer");
                        let storage = if destination == FileDestination::Documents {
                            native::documents()?
                        } else {
                            native::cache()?
                        };
                        incoming = Some(Incoming::new(id, destination, storage)?);
                        progress = native::Progress::new(id)
                            .map_err(|error| {
                                tracing::warn!(%error, "could not show file transfer progress");
                            })
                            .ok();
                    } else {
                        let state = incoming.as_mut().context("transfer has not started")?;
                        ensure!(state.id == packet_id, "transfer ID mismatch");
                        if let FileMessage::Finish { .. } = message {
                            let paths = state.finish()?;
                            // Hide before native delivery so the progress window cannot
                            // obscure the user's Explorer/browser drop target.
                            progress = None;
                            match &state.destination {
                                FileDestination::Clipboard
                                | FileDestination::ClipboardPaste { .. } => {
                                    native::set_clipboard_files(&paths)?;
                                    last_clipboard_sequence = native::clipboard_sequence();
                                    if let FileDestination::ClipboardPaste { display_id } =
                                        state.destination
                                    {
                                        native::paste_files(display_id)?;
                                    }
                                }
                                FileDestination::Drop { display_id, x, y } => {
                                    let accepted = native::drop_files(&paths, *display_id, *x, *y)
                                        .unwrap_or_else(|error| {
                                            tracing::warn!(%error, "native drop unavailable; saving to Documents");
                                            false
                                        });
                                    if !accepted {
                                        commit_documents(paths.clone(), native::documents()?)?;
                                        remove_batch(&paths);
                                    }
                                }
                                FileDestination::Documents => {}
                            }
                            if state.destination != FileDestination::Documents
                                && last_cache_sweep.elapsed() >= CACHE_SWEEP_INTERVAL
                            {
                                last_cache_sweep = Instant::now();
                                // Off this thread, which acknowledges the peer's transfers.
                                let base = state.base.clone();
                                let keep = native::clipboard_files().unwrap_or_default();
                                std::thread::spawn(move || sweep_cache(&base, &keep));
                            }
                            tracing::info!(id = packet_id, "file transfer received and verified");
                            *status.lock().unwrap() = "Transfer complete".into();
                            incoming = None;
                        } else {
                            let refresh = !matches!(message, FileMessage::Chunk { .. });
                            state.accept(message)?;
                            if refresh || progress_updated.elapsed() >= Duration::from_millis(100) {
                                if let Some(progress) = &progress {
                                    progress.update(
                                        state.received_bytes,
                                        state.total_bytes,
                                        &state.current_name,
                                        state.entries,
                                        state.total_entries,
                                    );
                                }
                                progress_updated = Instant::now();
                            }
                        }
                    }
                    Ok(())
                })();
                let response = match result {
                    Ok(()) => FileMessage::Ack { id: packet_id },
                    Err(e) => {
                        incoming = None;
                        progress = None;
                        end_transfer(&mut ended, packet_id);
                        let reason = format!("{e:#}");
                        tracing::warn!(id = packet_id, %reason, "file transfer failed");
                        *status.lock().unwrap() = reason.clone();
                        FileMessage::Error {
                            id: packet_id,
                            reason,
                        }
                    }
                };
                if out.send(response).is_err() {
                    break;
                }
            }
            None => {}
        }
        if send.is_none() && available && poll.elapsed() >= Duration::from_millis(250) {
            poll = Instant::now();
            let sequence = native::clipboard_sequence();
            if sequence != last_clipboard_sequence && !clipboard_enabled() {
                // Copies made while clipboard sync is off stay local after re-enabling.
                last_clipboard_sequence = sequence;
            } else if sequence != last_clipboard_sequence
                && let Ok(paths) = native::clipboard_files()
            {
                tracing::info!(files = paths.len(), "native file clipboard changed");
                last_clipboard_sequence = sequence;
                if !paths.is_empty() {
                    send = Some((paths, FileDestination::Clipboard));
                }
            }
        }
        if let Some(job) = send
            && !job.0.is_empty()
        {
            if pending.len() < 8 {
                pending.push_back(job);
            } else {
                *status.lock().unwrap() =
                    "Transfer queue is full; try again after completion".into();
                if answers_peer_pick(role, &job.1) {
                    let reason = "The remote transfer queue is full".into();
                    let reply = FileMessage::Error {
                        id: PEER_PICK_ID,
                        reason,
                    };
                    if out.send(reply).is_err() {
                        break;
                    }
                }
            }
        }
        if sender_thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
            && let Some((paths, destination)) = pending.pop_front()
        {
            let transfer_id = id();
            let (ack, acknowledgements) = mpsc::sync_channel(SEND_WINDOW + 1);
            sender = Some((transfer_id, ack));
            let out = out.clone();
            let status = status.clone();
            let answers_pick = answers_peer_pick(role, &destination);
            sender_thread = Some(std::thread::spawn(move || {
                let _native = native::initialize();
                *status.lock().unwrap() = "Transferring files…".into();
                let mut window = AckWindow::new(&acknowledgements, SEND_WINDOW);
                let mut began = false;
                let result = send_paths(transfer_id, paths, destination, |message| {
                    began |= matches!(message, FileMessage::Begin { .. });
                    let settle = window.settles(&message);
                    out.send(message).context("session closed")?;
                    window.sent(settle)
                });
                *status.lock().unwrap() = match &result {
                    Ok(()) => "Transfer complete".into(),
                    Err(e) => format!("Transfer failed: {e:#}"),
                };
                if let Err(e) = result {
                    tracing::warn!(error = %format!("{e:#}"), "file transfer not sent");
                    // A pick that failed before its transfer began still has to answer the request.
                    let id = if answers_pick && !began {
                        PEER_PICK_ID
                    } else {
                        transfer_id
                    };
                    let _ = out.send(FileMessage::Error {
                        id,
                        reason: format!("{e:#}"),
                    });
                }
            }));
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
