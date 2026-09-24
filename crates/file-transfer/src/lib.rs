//! Native, session-scoped file copying. File data never passes through signaling.
use anyhow::{Context, bail, ensure};
use meshrmm_protocol::{FILE_CHUNK_BYTES, FileDestination, FileMessage};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
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

/// Native change counter, shared by text/image and file clipboard polling.
pub fn clipboard_sequence() -> u64 {
    native::clipboard_sequence()
}

pub fn clipboard_has_files() -> bool {
    native::clipboard_files().is_ok_and(|paths| !paths.is_empty())
}

/// Folder, under Documents or the cache, that receives transferred files.
const TRANSFER_FOLDER: &str = "MeshRMM Transferred Files";
/// Clipboard copies (automatic sync and explicit paste) stay small; larger
/// files go through Send/Receive files or a drop instead.
pub const CLIPBOARD_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
/// Upper bound for one Documents or drop transfer.
pub const TRANSFER_LIMIT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Free space a transfer must leave on the receiving volume.
const FREE_SPACE_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
/// A peer's files are accepted into Documents this long after asking for them.
const PEER_PICK_WINDOW: Duration = Duration::from_secs(15 * 60);
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
        .unwrap()
        .as_nanos() as u64;
    let previous = LAST
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            Some(now.max(last + 1))
        })
        .unwrap();
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
                            if state.destination != FileDestination::Documents {
                                sweep_cache(&state.base);
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
            sender_thread = Some(std::thread::spawn(move || {
                let _native = native::initialize();
                *status.lock().unwrap() = "Transferring files…".into();
                let mut window = AckWindow::new(&acknowledgements, SEND_WINDOW);
                let result = send_paths(transfer_id, paths, destination, |message| {
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
                    let _ = out.send(FileMessage::Error {
                        id: transfer_id,
                        reason: format!("{e:#}"),
                    });
                }
            }));
        }
    }
}

fn end_transfer(ended: &mut VecDeque<u64>, id: u64) {
    if ended.len() == 8 {
        ended.pop_front();
    }
    ended.push_back(id);
}

/// Keeps up to `limit` transfer messages unacknowledged. The receiver
/// acknowledges each message in order or answers with an error.
struct AckWindow<'a> {
    acknowledgements: &'a mpsc::Receiver<bool>,
    limit: usize,
    unacknowledged: usize,
}
impl<'a> AckWindow<'a> {
    fn new(acknowledgements: &'a mpsc::Receiver<bool>, limit: usize) -> Self {
        Self {
            acknowledgements,
            limit,
            unacknowledged: 0,
        }
    }
    /// A receiver rejects a transfer when it begins or learns its totals, so
    /// nothing follows those until they are accepted. A transfer is complete
    /// once its finish is acknowledged.
    fn settles(&self, message: &FileMessage) -> bool {
        matches!(
            message,
            FileMessage::Begin { .. } | FileMessage::Totals { .. } | FileMessage::Finish { .. }
        )
    }
    /// Records one sent message, then waits until another may be sent.
    fn sent(&mut self, settle: bool) -> anyhow::Result<()> {
        self.unacknowledged += 1;
        let limit = if settle { 1 } else { self.limit };
        while self.unacknowledged >= limit {
            ensure!(
                self.acknowledgements
                    .recv_timeout(Duration::from_secs(120))
                    .context("transfer acknowledgement timed out")?,
                "peer rejected transfer"
            );
            self.unacknowledged -= 1;
        }
        Ok(())
    }
}

/// Removes what earlier sessions left behind: interrupted transfers in both
/// transfer folders and clipboard/drop copies no app can still be reading.
fn sweep_transfer_folders() {
    if let Ok(documents) = native::documents() {
        sweep_partials(&documents.join(TRANSFER_FOLDER), SystemTime::now());
    }
    if let Ok(cache) = native::cache() {
        sweep_cache(&cache.join(TRANSFER_FOLDER));
    }
}

fn sweep_cache(base: &Path) {
    let keep = native::clipboard_files().unwrap_or_default();
    sweep_partials(base, SystemTime::now());
    sweep_batches(base, &keep, SystemTime::now());
}

/// Deletes `.partial-*` folders nothing has written to for [`STALE_PARTIAL_AGE`].
/// Another viewer may be receiving into the same folder, so recent ones stay.
fn sweep_partials(base: &Path, now: SystemTime) {
    sweep(base, ".partial-", STALE_PARTIAL_AGE, &[], now);
}

/// Deletes finished clipboard/drop batches older than [`CACHE_BATCH_AGE`]
/// unless they hold a file that is still on the clipboard.
fn sweep_batches(base: &Path, keep: &[PathBuf], now: SystemTime) {
    sweep(base, "transfer-", CACHE_BATCH_AGE, keep, now);
}

fn sweep(base: &Path, prefix: &str, age: Duration, keep: &[PathBuf], now: SystemTime) {
    let Ok(children) = fs::read_dir(base) else {
        return;
    };
    for child in children.flatten() {
        let path = child.path();
        let named = child
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix));
        let is_dir = child.file_type().is_ok_and(|kind| kind.is_dir());
        if !named || !is_dir || keep.iter().any(|kept| kept.starts_with(&path)) {
            continue;
        }
        let stale = latest_write(&path, 0)
            .is_ok_and(|written| now.duration_since(written).is_ok_and(|idle| idle >= age));
        if stale {
            match fs::remove_dir_all(&path) {
                Ok(()) => tracing::info!(path = %path.display(), "removed old transferred files"),
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "could not remove old transfer")
                }
            }
        }
    }
}

/// The most recent modification time in a folder tree, without following links.
fn latest_write(path: &Path, depth: usize) -> io::Result<SystemTime> {
    let metadata = fs::symlink_metadata(path)?;
    let mut latest = metadata.modified()?;
    if metadata.is_dir() && depth < 128 {
        for child in fs::read_dir(path)? {
            latest = latest.max(latest_write(&child?.path(), depth + 1)?);
        }
    }
    Ok(latest)
}

/// Deletes the cache batch that holds `paths` once they were copied elsewhere.
fn remove_batch(paths: &[PathBuf]) {
    let Some(batch) = paths.first().and_then(|path| path.parent()) else {
        return;
    };
    if batch
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("transfer-"))
        && let Err(error) = fs::remove_dir_all(batch)
    {
        tracing::debug!(%error, "could not remove a copied transfer batch");
    }
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

/// Tags a received file or folder the way browsers tag downloads (the
/// quarantine attribute on macOS, Mark of the Web on Windows), so opening a
/// received app or installer gets the same checks. Both the viewer and the
/// agent tag what they receive. A volume without extended attributes or
/// alternate streams keeps the file untagged.
fn mark_received(path: &Path) {
    if let Err(error) = native::mark_received(path) {
        tracing::warn!(%error, path = %path.display(), "could not mark a received file as downloaded");
    }
}

/// Moves `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`]
/// instead of replacing an existing file or folder.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match native::rename_no_replace(from, to) {
        // Some network volumes lack an atomic no-replace rename.
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            if to.try_exists()? {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        result => result,
    }
}

/// Moves `from` into `folder` as `name`, or under a name from `fallback`
/// while that name is taken, without ever replacing an existing file.
fn move_unique(
    from: &Path,
    folder: &Path,
    name: &str,
    fallback: impl Fn() -> String,
) -> anyhow::Result<PathBuf> {
    let mut target = folder.join(name);
    for _ in 0..8 {
        match rename_no_replace(from, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                target = folder.join(fallback());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not save {name:?}"));
            }
        }
    }
    bail!("could not find a free name for {name:?}")
}

struct Incoming {
    id: u64,
    destination: FileDestination,
    stage: PathBuf,
    base: PathBuf,
    roots: Vec<String>,
    file: Option<(File, u64, Sha256)>,
    entries: usize,
    received_bytes: u64,
    /// Sum of the file sizes announced so far; never above `total_bytes`.
    declared_bytes: u64,
    total_bytes: u64,
    total_entries: u64,
    current_name: String,
}
impl Incoming {
    fn new(id: u64, destination: FileDestination, documents: PathBuf) -> anyhow::Result<Self> {
        let base = documents.join(TRANSFER_FOLDER);
        fs::create_dir_all(&base)?;
        ensure!(
            !fs::symlink_metadata(&base)?.file_type().is_symlink(),
            "transfer directory cannot be a link"
        );
        let stage = base.join(format!(".partial-{}-{}", id, crate::id()));
        fs::create_dir(&stage)?;
        Ok(Self {
            id,
            destination,
            stage,
            base,
            roots: Vec::new(),
            file: None,
            entries: 0,
            received_bytes: 0,
            declared_bytes: 0,
            total_bytes: 0,
            total_entries: 0,
            current_name: String::new(),
        })
    }
    fn accept(&mut self, message: FileMessage) -> anyhow::Result<()> {
        match message {
            FileMessage::Totals { bytes, entries, .. } => {
                ensure!(
                    self.total_entries == 0 && entries > 0 && entries <= 100_000,
                    "invalid transfer totals"
                );
                check_limit(&self.destination, bytes)?;
                let free = native::available_space(&self.stage)
                    .context("could not check free disk space")?;
                ensure!(
                    free >= bytes.saturating_add(FREE_SPACE_RESERVE_BYTES),
                    "Not enough disk space: the files need {} and {} is free",
                    mebibytes(bytes),
                    mebibytes(free)
                );
                self.total_bytes = bytes;
                self.total_entries = entries;
            }
            FileMessage::Entry { path, size, .. } => {
                ensure!(self.total_entries > 0, "transfer totals are missing");
                self.current_name = path.clone();
                ensure!(self.file.is_none(), "previous file is unfinished");
                ensure!(
                    (self.entries as u64) < self.total_entries,
                    "more entries than announced"
                );
                self.entries += 1;
                let relative = valid_path(&path)?;
                if let Some(size) = size {
                    self.declared_bytes = self
                        .declared_bytes
                        .checked_add(size)
                        .filter(|declared| *declared <= self.total_bytes)
                        .context("files are larger than announced")?;
                }
                let root = path.split('/').next().unwrap().to_owned();
                if !self.roots.contains(&root) {
                    self.roots.push(root);
                }
                let target = self.stage.join(relative);
                if let Some(parent) = target.parent() {
                    ensure!(parent.is_dir(), "parent directory missing");
                }
                if let Some(size) = size {
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)?;
                    mark_received(&target);
                    self.file = Some((file, size, Sha256::new()));
                } else {
                    fs::create_dir(&target)?;
                    mark_received(&target);
                }
            }
            FileMessage::Chunk { data, .. } => {
                ensure!(
                    !data.is_empty() && data.len() <= FILE_CHUNK_BYTES,
                    "invalid chunk size"
                );
                let (file, remaining, hash) = self.file.as_mut().context("no open file")?;
                ensure!(
                    data.len() as u64 <= *remaining,
                    "file exceeds declared size"
                );
                file.write_all(&data)?;
                hash.update(&data);
                *remaining -= data.len() as u64;
                self.received_bytes = self
                    .received_bytes
                    .checked_add(data.len() as u64)
                    .context("transfer size overflow")?;
            }
            FileMessage::EndEntry { sha256, .. } => {
                let (file, remaining, hash) = self.file.take().context("no open file")?;
                ensure!(
                    remaining == 0 && hash.finalize().as_slice() == sha256,
                    "file checksum or size mismatch"
                );
                file.sync_all()?;
            }
            _ => bail!("unexpected transfer packet"),
        }
        Ok(())
    }
    fn finish(&mut self) -> anyhow::Result<Vec<PathBuf>> {
        ensure!(
            self.file.is_none()
                && !self.roots.is_empty()
                && self.entries as u64 == self.total_entries
                && self.received_bytes == self.total_bytes,
            "incomplete transfer"
        );
        if self.destination != FileDestination::Documents {
            let batch_name = || format!("transfer-{}", crate::id());
            let batch = move_unique(&self.stage, &self.base, &batch_name(), batch_name)?;
            return Ok(self.roots.iter().map(|root| batch.join(root)).collect());
        }
        self.roots
            .iter()
            .map(|root| {
                move_unique(&self.stage.join(root), &self.base, root, || {
                    format!("{}-{root}", crate::id())
                })
            })
            .collect()
    }
}
impl Drop for Incoming {
    fn drop(&mut self) {
        self.file = None;
        let _ = fs::remove_dir_all(&self.stage);
    }
}
fn send_paths(
    id: u64,
    paths: Vec<PathBuf>,
    destination: FileDestination,
    mut send: impl FnMut(FileMessage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    fn visit(
        path: PathBuf,
        relative: String,
        entries: &mut Vec<(PathBuf, String, Option<u64>)>,
        depth: usize,
    ) -> anyhow::Result<()> {
        ensure!(
            depth < 128 && entries.len() < 100_000,
            "folder tree is too large"
        );
        valid_path(&relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic links are not transferred"
        );
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "only regular files and folders can be transferred"
        );
        entries.push((
            path.clone(),
            relative.clone(),
            metadata.is_file().then_some(metadata.len()),
        ));
        if metadata.is_dir() {
            for child in fs::read_dir(path)? {
                let child = child?;
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("file name is not Unicode"))?;
                visit(
                    child.path(),
                    format!("{relative}/{name}"),
                    entries,
                    depth + 1,
                )?;
            }
        }
        Ok(())
    }
    for path in paths {
        let name = path
            .file_name()
            .context("cannot transfer a filesystem root")?
            .to_str()
            .context("file name is not Unicode")?
            .to_owned();
        visit(path, name, &mut entries, 0)?;
    }
    let bytes = entries.iter().try_fold(0u64, |sum, (_, _, size)| {
        sum.checked_add(size.unwrap_or(0))
            .context("transfer size overflow")
    })?;
    check_limit(&destination, bytes)?;
    send(FileMessage::Begin { id, destination })?;
    send(FileMessage::Totals {
        id,
        bytes,
        entries: entries.len() as u64,
    })?;
    for (path, relative, size) in entries {
        send(FileMessage::Entry {
            id,
            path: relative,
            size,
        })?;
        if let Some(size) = size {
            send_file(id, &path, size, &mut send)?;
        }
    }
    send(FileMessage::Finish { id })
}

/// Sends exactly `size` bytes of `path` and its checksum. A file that grew
/// since it was listed is cut at `size`; one that shrank fails the transfer.
fn send_file(
    id: u64,
    path: &Path,
    size: u64,
    send: &mut impl FnMut(FileMessage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut file = File::open(path)?.take(size);
    let mut hash = Sha256::new();
    let mut buffer = vec![0; FILE_CHUNK_BYTES];
    let mut sent = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        sent += count as u64;
        hash.update(&buffer[..count]);
        send(FileMessage::Chunk {
            id,
            data: buffer[..count].to_vec(),
        })?;
    }
    ensure!(
        sent == size,
        "{} changed while it was being sent",
        path.display()
    );
    send(FileMessage::EndEntry {
        id,
        sha256: hash.finalize().to_vec(),
    })
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
mod tests {
    use super::*;
    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("meshrmm-transfer-test-{}", id()));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn rejected_drop_copies_to_documents_without_overwriting() {
        let source = Sandbox::new();
        let target = Sandbox::new();
        let folder = source.0.join("drop-folder");
        fs::create_dir_all(folder.join("empty")).unwrap();
        fs::write(folder.join("file.txt"), "drop payload").unwrap();
        let first = commit_documents(vec![folder.clone()], target.0.clone()).unwrap();
        let second = commit_documents(vec![folder.clone()], target.0.clone()).unwrap();
        assert_ne!(first, second);
        for path in first.into_iter().chain(second) {
            assert_eq!(
                fs::read_to_string(path.join("file.txt")).unwrap(),
                "drop payload"
            );
            assert!(path.join("empty").is_dir());
        }
        assert!(folder.is_dir());
    }
    #[test]
    fn transfers_nested_unicode_empty_files_and_folders_with_checksums() {
        let source = Sandbox::new();
        let target = Sandbox::new();
        let folder = source.0.join("Folder é");
        fs::create_dir_all(folder.join("empty folder")).unwrap();
        fs::write(folder.join("zero.txt"), []).unwrap();
        let data: Vec<u8> = (0..1_000_000).map(|i| (i % 251) as u8).collect();
        fs::write(folder.join("large.bin"), &data).unwrap();
        let mut receiver = Incoming::new(7, FileDestination::Documents, target.0.clone()).unwrap();
        let mut received = Vec::new();
        send_paths(7, vec![folder], FileDestination::Documents, |packet| {
            let bytes = meshrmm_protocol::SessionMessage::FileTransfer(packet.clone())
                .encode()
                .unwrap();
            assert!(bytes.len() < 65536);
            let meshrmm_protocol::SessionMessage::FileTransfer(decoded) =
                meshrmm_protocol::SessionMessage::decode(&bytes).unwrap()
            else {
                panic!()
            };
            match decoded {
                FileMessage::Begin { .. } => {}
                FileMessage::Finish { .. } => received = receiver.finish()?,
                message => receiver.accept(message)?,
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(receiver.received_bytes, data.len() as u64);
        assert_eq!(receiver.total_bytes, data.len() as u64);
        assert_eq!(receiver.total_entries, receiver.entries as u64);
        assert_eq!(received.len(), 1);
        assert_eq!(fs::read(received[0].join("large.bin")).unwrap(), data);
        assert!(received[0].join("empty folder").is_dir());
        assert_eq!(fs::metadata(received[0].join("zero.txt")).unwrap().len(), 0);
    }
    #[test]
    fn rejects_unsafe_paths_and_incomplete_or_corrupt_data() {
        for path in [
            "../escape",
            "/absolute",
            "a/../b",
            "C:/x",
            "a\\b",
            "a//b",
            "NUL.txt",
            "a.",
            "a/",
            "x\0y",
        ] {
            assert!(valid_path(path).is_err(), "{path}");
        }
        let sandbox = Sandbox::new();
        let mut receiver = Incoming::new(2, FileDestination::Documents, sandbox.0.clone()).unwrap();
        let stage = receiver.stage.clone();
        receiver
            .accept(FileMessage::Totals {
                id: 2,
                bytes: 2,
                entries: 1,
            })
            .unwrap();
        receiver
            .accept(FileMessage::Entry {
                id: 2,
                path: "test".into(),
                size: Some(2),
            })
            .unwrap();
        assert!(receiver.finish().is_err());
        assert!(
            receiver
                .accept(FileMessage::Chunk {
                    id: 2,
                    data: vec![1; 3]
                })
                .is_err()
        );
        receiver
            .accept(FileMessage::Chunk {
                id: 2,
                data: vec![1; 2],
            })
            .unwrap();
        assert!(
            receiver
                .accept(FileMessage::EndEntry {
                    id: 2,
                    sha256: vec![0; 32]
                })
                .is_err()
        );
        drop(receiver);
        assert!(!stage.exists());
    }
    fn totals(bytes: u64, entries: u64) -> FileMessage {
        FileMessage::Totals {
            id: 1,
            bytes,
            entries,
        }
    }
    fn entry(path: &str, size: Option<u64>) -> FileMessage {
        FileMessage::Entry {
            id: 1,
            path: path.into(),
            size,
        }
    }
    #[test]
    fn rejects_windows_reserved_and_invalid_names() {
        for path in [
            "COM0",
            "com1.txt",
            "LPT0.log",
            "LPT9",
            "COM\u{b9}",
            "lpt\u{b3}.txt",
            "CONIN$",
            "conout$.txt",
            "NUL .txt",
            "folder/AUX",
            "a<b",
            "a>b",
            "a:b",
            "a\"b",
            "a|b",
            "what?",
            "star*",
            "tab\tname",
            "bell\u{7}",
            &"x".repeat(256),
        ] {
            assert!(valid_path(path).is_err(), "{path:?}");
        }
        for path in [
            "COM10",
            "LPT",
            "CONSOLE.txt",
            "nul-file",
            "résumé.pdf",
            "folder/COM1x",
            &"x".repeat(255),
        ] {
            assert!(valid_path(path).is_ok(), "{path:?}");
        }
    }
    #[test]
    fn receiver_requires_totals_and_enforces_them() {
        let sandbox = Sandbox::new();
        let mut receiver = Incoming::new(1, FileDestination::Documents, sandbox.0.clone()).unwrap();
        assert!(receiver.accept(entry("early", Some(1))).is_err());
        receiver.accept(totals(4, 2)).unwrap();
        assert!(receiver.accept(totals(4, 2)).is_err(), "totals twice");
        assert!(
            receiver.accept(entry("big", Some(5))).is_err(),
            "file larger than the announced total"
        );

        let mut receiver = Incoming::new(1, FileDestination::Documents, sandbox.0.clone()).unwrap();
        receiver.accept(totals(4, 1)).unwrap();
        receiver.accept(entry("folder", None)).unwrap();
        assert!(
            receiver.accept(entry("folder/extra", None)).is_err(),
            "more entries than announced"
        );

        // A transfer that ends short of its announced bytes is incomplete.
        let mut receiver = Incoming::new(1, FileDestination::Documents, sandbox.0.clone()).unwrap();
        receiver.accept(totals(4, 1)).unwrap();
        receiver.accept(entry("short", Some(2))).unwrap();
        receiver
            .accept(FileMessage::Chunk {
                id: 1,
                data: vec![7; 2],
            })
            .unwrap();
        receiver
            .accept(FileMessage::EndEntry {
                id: 1,
                sha256: Sha256::digest([7, 7]).to_vec(),
            })
            .unwrap();
        assert!(receiver.finish().is_err());
    }
    #[test]
    fn receiver_enforces_per_destination_limits() {
        let sandbox = Sandbox::new();
        for destination in [
            FileDestination::Clipboard,
            FileDestination::ClipboardPaste {
                display_id: meshrmm_protocol::DisplayId(1),
            },
        ] {
            let mut receiver = Incoming::new(1, destination, sandbox.0.clone()).unwrap();
            let error = receiver
                .accept(totals(CLIPBOARD_LIMIT_BYTES + 1, 1))
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("clipboard limit"),
                "{error:#}"
            );
        }
        let mut receiver = Incoming::new(1, FileDestination::Documents, sandbox.0.clone()).unwrap();
        assert!(
            receiver
                .accept(totals(TRANSFER_LIMIT_BYTES + 1, 1))
                .is_err()
        );
        // Within the limit but beyond any disk this test runs on.
        let mut receiver = Incoming::new(1, FileDestination::Documents, sandbox.0.clone()).unwrap();
        let free = native::available_space(&sandbox.0).unwrap();
        assert!(free > 0);
        if free < TRANSFER_LIMIT_BYTES {
            let error = receiver
                .accept(totals(TRANSFER_LIMIT_BYTES, 1))
                .unwrap_err();
            assert!(format!("{error:#}").contains("disk space"), "{error:#}");
        }
    }
    #[test]
    fn sender_refuses_oversized_clipboard_copies_before_sending() {
        let source = Sandbox::new();
        let file = source.0.join("large.bin");
        File::create(&file)
            .unwrap()
            .set_len(CLIPBOARD_LIMIT_BYTES + 1)
            .unwrap();
        let mut sent = 0;
        let error = send_paths(1, vec![file], FileDestination::Clipboard, |_| {
            sent += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(sent, 0);
        assert!(format!("{error:#}").contains("Send files"), "{error:#}");
    }
    #[test]
    fn sender_sends_exactly_the_listed_size() {
        let source = Sandbox::new();
        let file = source.0.join("growing.txt");
        fs::write(&file, "0123456789").unwrap();
        let mut data = Vec::new();
        let mut checksum = Vec::new();
        send_file(1, &file, 4, &mut |message| {
            match message {
                FileMessage::Chunk { data: chunk, .. } => data.extend(chunk),
                FileMessage::EndEntry { sha256, .. } => checksum = sha256,
                _ => panic!("unexpected message"),
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(data, b"0123");
        assert_eq!(checksum, Sha256::digest(b"0123").to_vec());
        let error = send_file(1, &file, 11, &mut |_| Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("changed"), "{error:#}");
    }
    #[test]
    fn rename_never_replaces_an_existing_file_or_folder() {
        let sandbox = Sandbox::new();
        let (from, to) = (sandbox.0.join("from.txt"), sandbox.0.join("to.txt"));
        fs::write(&from, "new").unwrap();
        fs::write(&to, "original").unwrap();
        let error = rename_no_replace(&from, &to).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&to).unwrap(), "original");
        let (folder, taken) = (sandbox.0.join("folder"), sandbox.0.join("taken"));
        fs::create_dir(&folder).unwrap();
        fs::create_dir(&taken).unwrap();
        let error = rename_no_replace(&folder, &taken).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let moved = move_unique(&from, &sandbox.0, "to.txt", || "fallback.txt".into()).unwrap();
        assert_eq!(moved, sandbox.0.join("fallback.txt"));
        assert_eq!(fs::read_to_string(moved).unwrap(), "new");
        assert_eq!(fs::read_to_string(&to).unwrap(), "original");
    }
    #[test]
    fn sweeps_only_stale_partials_and_unused_cache_batches() {
        let sandbox = Sandbox::new();
        let base = &sandbox.0;
        for name in [
            ".partial-1",
            "transfer-1",
            "transfer-2",
            "kept.txt",
            "other",
        ] {
            fs::create_dir(base.join(name)).unwrap();
            fs::write(base.join(name).join("file"), "x").unwrap();
        }
        let now = SystemTime::now();
        sweep_partials(base, now);
        sweep_batches(base, &[], now);
        assert_eq!(
            fs::read_dir(base).unwrap().count(),
            5,
            "recent folders stay"
        );

        let clipboard = [base.join("transfer-2").join("file")];
        sweep_partials(base, now + STALE_PARTIAL_AGE);
        sweep_batches(base, &clipboard, now + CACHE_BATCH_AGE);
        assert!(!base.join(".partial-1").exists());
        assert!(!base.join("transfer-1").exists());
        assert!(base.join("transfer-2").exists(), "still on the clipboard");
        assert!(base.join("kept.txt").exists() && base.join("other").exists());
    }
    #[test]
    fn copied_drop_batch_is_removed() {
        let sandbox = Sandbox::new();
        let batch = sandbox.0.join("transfer-5");
        fs::create_dir(&batch).unwrap();
        fs::write(batch.join("a.txt"), "a").unwrap();
        remove_batch(&[batch.join("a.txt")]);
        assert!(!batch.exists());
        let other = sandbox.0.join("Documents");
        fs::create_dir(&other).unwrap();
        fs::write(other.join("a.txt"), "a").unwrap();
        remove_batch(&[other.join("a.txt")]);
        assert!(other.exists());
    }
    #[test]
    fn ack_window_pipelines_chunks_and_settles_the_rest() {
        let (acks, acknowledgements) = mpsc::sync_channel(SEND_WINDOW + 1);
        let mut window = AckWindow::new(&acknowledgements, 3);
        let chunk = FileMessage::Chunk {
            id: 1,
            data: vec![0],
        };
        assert!(!window.settles(&chunk));
        for message in [
            FileMessage::Begin {
                id: 1,
                destination: FileDestination::Documents,
            },
            totals(1, 1),
            FileMessage::Finish { id: 1 },
        ] {
            assert!(window.settles(&message));
        }
        // Two chunks go out without waiting; the third waits for one ack.
        window.sent(false).unwrap();
        window.sent(false).unwrap();
        acks.send(true).unwrap();
        window.sent(false).unwrap();
        assert_eq!(window.unacknowledged, 2);
        // A settling message waits for every earlier one too.
        for _ in 0..3 {
            acks.send(true).unwrap();
        }
        window.sent(true).unwrap();
        assert_eq!(window.unacknowledged, 0);
        window.sent(false).unwrap();
        window.sent(false).unwrap();
        acks.send(false).unwrap();
        let error = window.sent(false).unwrap_err();
        assert!(format!("{error:#}").contains("rejected"), "{error:#}");
    }
    #[test]
    fn windowed_transfer_keeps_a_window_in_flight() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let source = Sandbox::new();
        let target = Sandbox::new();
        let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 253) as u8).collect();
        let file = source.0.join("large.bin");
        fs::write(&file, &data).unwrap();
        let (wire, packets) = mpsc::sync_channel::<FileMessage>(SEND_WINDOW + 1);
        let (acks, acknowledgements) = mpsc::sync_channel(SEND_WINDOW + 1);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let receiver_in_flight = in_flight.clone();
        let documents = target.0.clone();
        let receiver = std::thread::spawn(move || {
            let mut incoming = Incoming::new(1, FileDestination::Documents, documents).unwrap();
            let mut received = Vec::new();
            for packet in packets {
                // A slow receiver lets the sender fill its window.
                std::thread::sleep(Duration::from_millis(3));
                let finished = matches!(packet, FileMessage::Finish { .. });
                match packet {
                    FileMessage::Begin { .. } => {}
                    FileMessage::Finish { .. } => received = incoming.finish().unwrap(),
                    packet => incoming.accept(packet).unwrap(),
                }
                receiver_in_flight.fetch_sub(1, Ordering::SeqCst);
                acks.send(true).unwrap();
                if finished {
                    break;
                }
            }
            received
        });
        let mut window = AckWindow::new(&acknowledgements, SEND_WINDOW);
        let mut peak = 0;
        send_paths(1, vec![file], FileDestination::Documents, |message| {
            let settle = window.settles(&message);
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            if matches!(
                message,
                FileMessage::Begin { .. } | FileMessage::Totals { .. }
            ) {
                assert_eq!(now, 1, "nothing follows a transfer until it is accepted");
            }
            peak = peak.max(now);
            wire.send(message).unwrap();
            window.sent(settle)
        })
        .unwrap();
        let received = receiver.join().unwrap();
        assert_eq!(in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(peak, SEND_WINDOW);
        assert_eq!(fs::read(&received[0]).unwrap(), data);
    }
    #[test]
    fn viewer_accepts_only_requested_documents_and_clipboard_copies() {
        let start = Instant::now();
        let paste = FileDestination::ClipboardPaste {
            display_id: meshrmm_protocol::DisplayId(1),
        };
        let drop = FileDestination::Drop {
            display_id: meshrmm_protocol::DisplayId(1),
            x: 0,
            y: 0,
        };
        let mut viewer = Admission::new(Role::Viewer);
        assert!(!viewer.allows_peer_pick());
        assert!(
            viewer
                .admit(&FileDestination::Clipboard, true, start)
                .is_ok()
        );
        assert!(
            viewer
                .admit(&FileDestination::Clipboard, false, start)
                .is_err()
        );
        assert!(viewer.admit(&paste, true, start).is_err());
        assert!(viewer.admit(&drop, true, start).is_err());
        assert!(
            viewer
                .admit(&FileDestination::Documents, true, start)
                .is_err()
        );
        viewer.request_peer_pick(start);
        assert!(
            viewer
                .admit(&FileDestination::Documents, true, start)
                .is_ok()
        );
        assert!(
            viewer
                .admit(&FileDestination::Documents, true, start)
                .is_err(),
            "one request admits one transfer"
        );
        viewer.request_peer_pick(start);
        assert!(
            viewer
                .admit(&FileDestination::Documents, true, start + PEER_PICK_WINDOW)
                .is_err(),
            "requests expire"
        );

        let mut agent = Admission::new(Role::Agent);
        assert!(agent.allows_peer_pick());
        for destination in [
            FileDestination::Documents,
            FileDestination::Clipboard,
            paste.clone(),
            drop,
        ] {
            assert!(agent.admit(&destination, true, start).is_ok());
        }
        assert!(agent.admit(&paste, false, start).is_err());
    }
    /// Reads the tag `mark_received` leaves, if any.
    #[cfg(target_os = "macos")]
    fn received_mark(path: &Path) -> Option<String> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = c"com.apple.quarantine";
        let mut value = [0u8; 256];
        // SAFETY: both strings are NUL-terminated and `value` is writable.
        let length = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        (length >= 0).then(|| String::from_utf8_lossy(&value[..length as usize]).into_owned())
    }
    #[cfg(windows)]
    fn received_mark(path: &Path) -> Option<String> {
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        fs::read_to_string(stream).ok()
    }
    #[test]
    fn received_files_are_marked_as_downloaded() {
        let source = Sandbox::new();
        let target = Sandbox::new();
        let folder = source.0.join("Tool.app");
        fs::create_dir_all(folder.join("Contents")).unwrap();
        fs::write(folder.join("Contents").join("setup.exe"), "MZ").unwrap();
        assert!(received_mark(&folder.join("Contents").join("setup.exe")).is_none());
        for destination in [FileDestination::Documents, FileDestination::Clipboard] {
            let mut receiver = Incoming::new(1, destination.clone(), target.0.clone()).unwrap();
            let mut received = Vec::new();
            send_paths(1, vec![folder.clone()], destination, |message| {
                match message {
                    FileMessage::Begin { .. } => {}
                    FileMessage::Finish { .. } => received = receiver.finish()?,
                    message => receiver.accept(message)?,
                }
                Ok(())
            })
            .unwrap();
            let file = received[0].join("Contents").join("setup.exe");
            let mark = received_mark(&file).expect("received file is marked");
            #[cfg(target_os = "macos")]
            {
                assert!(mark.contains(";MeshRMM;"), "{mark}");
                let folder_mark = received_mark(&received[0]).expect("received folder is marked");
                assert!(folder_mark.contains(";MeshRMM;"), "{folder_mark}");
            }
            #[cfg(windows)]
            assert!(mark.contains("ZoneId=3"), "{mark}");
        }
        assert!(received_mark(&folder.join("Contents").join("setup.exe")).is_none());
    }
    #[test]
    fn clipboard_batches_preserve_names_across_repeated_copies() {
        let source = Sandbox::new();
        let target = Sandbox::new();
        let file = source.0.join("clipboard.txt");
        fs::write(&file, "file contents").unwrap();
        let mut completed = Vec::new();
        for id in [10, 11] {
            let mut receiver =
                Incoming::new(id, FileDestination::Clipboard, target.0.clone()).unwrap();
            send_paths(id, vec![file.clone()], FileDestination::Clipboard, |m| {
                match m {
                    FileMessage::Begin { .. } => {}
                    FileMessage::Finish { .. } => completed.extend(receiver.finish()?),
                    m => receiver.accept(m)?,
                }
                Ok(())
            })
            .unwrap();
        }
        assert_ne!(completed[0], completed[1]);
        for file in completed {
            assert_eq!(file.file_name().unwrap(), "clipboard.txt");
            assert_eq!(fs::read_to_string(file).unwrap(), "file contents");
        }
    }
    #[test]
    fn preserves_existing_destination_and_cleans_cancelled_transfer() {
        let source = Sandbox::new();
        let target = Sandbox::new();
        let path = source.0.join("same.txt");
        fs::write(&path, "new").unwrap();
        let base = target.0.join("MeshRMM Transferred Files");
        fs::create_dir(&base).unwrap();
        fs::write(base.join("same.txt"), "original").unwrap();
        let mut receiver = Incoming::new(3, FileDestination::Documents, target.0.clone()).unwrap();
        send_paths(3, vec![path], FileDestination::Documents, |m| {
            match m {
                FileMessage::Begin { .. } => {}
                FileMessage::Finish { .. } => {
                    receiver.finish()?;
                }
                m => receiver.accept(m)?,
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(base.join("same.txt")).unwrap(),
            "original"
        );
        drop(receiver);
        assert_eq!(fs::read_dir(base).unwrap().count(), 2);
    }
}

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
