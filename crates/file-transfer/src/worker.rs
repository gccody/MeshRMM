use anyhow::{Context, ensure};
use meshrmm_protocol::{DisplayId, FileDestination, FileMessage};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    AckWindow, Admission, CACHE_SWEEP_INTERVAL, Command, Incoming, KEPT_CLIPBOARD_BATCHES,
    OutgoingFiles, PEER_PICK_ID, PasteShortcut, Placement, Refusal, Role, SEND_WINDOW,
    answers_peer_pick, commit_documents, end_transfer, id, message_id, native,
    pick::{OpenPick, PickFor, Picker},
    receive::{CreateFile, move_to_documents},
    remove_batch, remove_batch_folder,
    send::Answer,
    send_paths, sweep_cache, sweep_transfer_folders,
};

/// The session's outgoing channel closed, which ends the worker.
type Closed = mpsc::SendError<FileMessage>;
/// Local files to send and where the peer puts them.
type Job = (Vec<PathBuf>, FileDestination);
/// Hands files to the app under a point of a display; true when it took them.
pub(super) type DropFiles = Box<dyn Fn(&[PathBuf], DisplayId, u16, u16) -> anyhow::Result<bool>>;
/// Runs work that would hold up the worker on a thread of its own.
pub(super) type Spawn = Box<dyn Fn(Box<dyn FnOnce() + Send>)>;
/// Puts files on this computer's clipboard.
pub(super) type SetClipboardFiles = Box<dyn Fn(&[PathBuf]) -> anyhow::Result<()>>;

/// What the worker asks of this computer's desktop. Tests replace it.
pub(super) struct Desktop {
    pub(super) documents: Box<dyn Fn() -> anyhow::Result<PathBuf>>,
    pub(super) cache: Box<dyn Fn() -> anyhow::Result<PathBuf>>,
    /// `None` where the platform cannot drop files on an app (macOS and
    /// Linux), which then receives a drop straight into Documents.
    pub(super) drop_files: Option<DropFiles>,
    pub(super) pick: Picker,
    pub(super) spawn: Spawn,
    pub(super) create_file: Option<CreateFile>,
    /// The files on this computer's clipboard.
    pub(super) clipboard_files: Box<dyn Fn() -> anyhow::Result<Vec<PathBuf>>>,
    pub(super) set_clipboard_files: SetClipboardFiles,
    /// The clipboard's change counter.
    pub(super) clipboard_sequence: Box<dyn Fn() -> u64>,
    /// Shows a progress window for incoming transfers and follows the clipboard.
    pub(super) interactive: bool,
}
impl Desktop {
    pub(super) fn native() -> Self {
        Self {
            documents: Box::new(native::documents),
            cache: Box::new(native::cache),
            #[cfg(windows)]
            drop_files: Some(Box::new(native::drop_files)),
            #[cfg(not(windows))]
            drop_files: None,
            // Each thread sets up native file operations (on Windows, the
            // signed-in user's token and OLE) for itself.
            pick: Arc::new(|cancel: &crate::PickCancel| {
                let _native = native::initialize()?;
                native::pick(cancel)
            }),
            spawn: Box::new(|work| {
                std::thread::spawn(move || {
                    let _native = native::initialize();
                    work()
                });
            }),
            create_file: None,
            clipboard_files: Box::new(native::clipboard_files),
            set_clipboard_files: Box::new(native::set_clipboard_files),
            clipboard_sequence: Box::new(native::clipboard_sequence),
            interactive: true,
        }
    }
}

pub(super) fn run(
    role: Role,
    commands: mpsc::Receiver<Command>,
    out: OutgoingFiles,
    status: Arc<Mutex<String>>,
    clipboard_enabled: fn() -> bool,
    paste: Option<PasteShortcut>,
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
    let worker = Worker::new(
        role,
        out,
        status,
        clipboard_enabled,
        paste,
        Desktop::native(),
    );
    serve(commands, worker);
}

/// Handles commands until the session closes. Dropping the worker then
/// closes a picker that is still open.
pub(super) fn serve(commands: mpsc::Receiver<Command>, mut worker: Worker) {
    loop {
        let command = match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => None,
        };
        if worker.step(command).is_err() {
            break;
        }
    }
}

pub(super) struct Worker {
    role: Role,
    out: OutgoingFiles,
    status: Arc<Mutex<String>>,
    clipboard_enabled: fn() -> bool,
    paste: Option<PasteShortcut>,
    desktop: Desktop,
    poll: Instant,
    last_cache_sweep: Instant,
    last_clipboard_sequence: u64,
    pub(super) available: bool,
    pending: VecDeque<Job>,
    sender_thread: Option<JoinHandle<()>>,
    sender: Option<(u64, mpsc::SyncSender<Answer>)>,
    progress_updated: Instant,
    progress: Option<native::Progress>,
    /// Transfers rejected or failed here, whose in-flight messages are dropped.
    ended: VecDeque<u64>,
    pub(super) incoming: Option<Incoming>,
    pub(super) admission: Admission,
    /// The file picker that is open, at most one.
    pick: Option<OpenPick>,
    /// Cache batches being copied into Documents, which the cache sweep keeps.
    copying: Arc<Mutex<Vec<PathBuf>>>,
    /// The cache batches this worker put on the clipboard, oldest first.
    clipboard_batches: VecDeque<PathBuf>,
}

impl Worker {
    pub(super) fn new(
        role: Role,
        out: OutgoingFiles,
        status: Arc<Mutex<String>>,
        clipboard_enabled: fn() -> bool,
        paste: Option<PasteShortcut>,
        desktop: Desktop,
    ) -> Self {
        Self {
            role,
            out,
            status,
            clipboard_enabled,
            paste,
            admission: Admission::new(role),
            incoming: None,
            ended: VecDeque::new(),
            progress: None,
            progress_updated: Instant::now(),
            sender: None,
            sender_thread: None,
            pending: VecDeque::new(),
            available: false,
            last_clipboard_sequence: (desktop.clipboard_sequence)(),
            desktop,
            // The sweep at startup covered the cache.
            last_cache_sweep: Instant::now(),
            poll: Instant::now(),
            pick: None,
            copying: Arc::default(),
            clipboard_batches: VecDeque::new(),
        }
    }

    fn set_status(&self, status: impl Into<String>) {
        *self.status.lock().unwrap() = status.into();
    }

    pub(super) fn step(&mut self, command: Option<Command>) -> Result<(), Closed> {
        let picked = self.picked()?;
        let command = match command {
            Some(Command::Peer(FileMessage::Begin { id, destination })) => {
                self.admit(id, destination)?
            }
            command => command,
        };
        let mut send = self.handle(command)?;
        if send.is_none()
            && self.desktop.interactive
            && self.available
            && self.poll.elapsed() >= Duration::from_millis(250)
        {
            send = self.poll_clipboard();
        }
        for job in [picked, send].into_iter().flatten() {
            if !job.0.is_empty() {
                self.queue(job)?;
            }
        }
        self.start_next_send();
        Ok(())
    }

    /// Passes the peer's new transfer on, or answers that it was rejected.
    fn admit(&mut self, id: u64, destination: FileDestination) -> Result<Option<Command>, Closed> {
        // Rejecting a new transfer leaves the one in progress intact.
        let admitted = if self.incoming.is_some() {
            if destination == FileDestination::Documents {
                self.admission.close_peer_pick();
            }
            Err("Another transfer is in progress".into())
        } else {
            self.admission
                .admit(&destination, (self.clipboard_enabled)(), Instant::now())
        };
        match admitted {
            Ok(()) => Ok(Some(Command::Peer(FileMessage::Begin { id, destination }))),
            Err(Refusal { reason, notice }) => {
                tracing::info!(id, ?destination, reason, "rejected file transfer");
                end_transfer(&mut self.ended, id);
                if let Some(notice) = notice {
                    self.set_status(notice);
                } else if self.role == Role::Viewer && destination != FileDestination::Clipboard {
                    self.set_status(reason.clone());
                }
                self.out.send(FileMessage::Error { id, reason })?;
                Ok(None)
            }
        }
    }

    /// Acts on one command and returns any local files it asks to send.
    fn handle(&mut self, command: Option<Command>) -> Result<Option<Job>, Closed> {
        let job = match command {
            Some(Command::Pick) => {
                if self.available {
                    self.open_picker(PickFor::Local)?;
                }
                None
            }
            Some(Command::RequestPeerPick) => {
                if !self.available {
                    self.set_status("Waiting for file-transfer support…");
                } else {
                    self.admission.request_peer_pick(Instant::now());
                    self.out.send(FileMessage::Pick)?;
                }
                None
            }
            Some(Command::Send(paths, destination)) => {
                self.available.then_some((paths, destination))
            }
            Some(Command::Peer(FileMessage::Available)) => {
                self.available = true;
                self.set_status("Ready");
                None
            }
            Some(Command::Peer(FileMessage::Pick)) if !self.admission.allows_peer_pick() => {
                tracing::warn!("ignored a request to pick local files for the remote device");
                None
            }
            Some(Command::Peer(FileMessage::Pick)) => {
                if self.available {
                    self.open_picker(PickFor::Peer)?;
                } else {
                    self.answer_peer_pick(Err("File transfers are not ready".into()))?;
                }
                None
            }
            Some(Command::Peer(FileMessage::Error { id, reason }))
                if id == PEER_PICK_ID && self.role == Role::Viewer =>
            {
                self.admission.close_peer_pick();
                self.set_status(format!("No files received: {reason}"));
                None
            }
            Some(Command::Peer(FileMessage::Ack { id })) => {
                self.settle_sender(id, Ok(()));
                None
            }
            Some(Command::Peer(FileMessage::Error { id, reason })) => {
                self.peer_failed(id, &reason);
                None
            }
            // A sender has a window of messages in flight when it learns its
            // transfer failed; answering each would only repeat the error.
            Some(Command::Peer(message)) if self.ended.contains(&message_id(&message)) => None,
            Some(Command::Peer(message)) => {
                self.receive(message)?;
                None
            }
            None => None,
        };
        Ok(job)
    }

    /// Opens the file picker on its own thread, so this one keeps handling
    /// the session while the dialog is open. A second pick while one is open
    /// is refused: the user would get a dialog behind the one they see.
    fn open_picker(&mut self, purpose: PickFor) -> Result<(), Closed> {
        if self.pick.is_some() {
            return match purpose {
                PickFor::Local => {
                    self.set_status("A file picker is already open");
                    Ok(())
                }
                PickFor::Peer => self
                    .answer_peer_pick(Err(
                        "A file picker is already open on the remote device".into()
                    ))
                    .map(drop),
            };
        }
        self.pick = Some(OpenPick::start(&self.desktop.pick, purpose));
        Ok(())
    }

    /// What the open picker chose, once it closed.
    fn picked(&mut self) -> Result<Option<Job>, Closed> {
        let Some(chosen) = self.pick.as_mut().and_then(OpenPick::chosen) else {
            return Ok(None);
        };
        let purpose = self.pick.take().map(|pick| pick.purpose);
        let chosen = chosen.map_err(|error| {
            tracing::warn!(error = %format!("{error:#}"), "file picker failed");
            let reason = format!("File picker failed: {error:#}");
            self.set_status(reason.clone());
            reason
        });
        match (purpose, chosen) {
            (Some(PickFor::Peer), chosen) => {
                let chosen = match chosen {
                    Ok(paths) if paths.is_empty() => Err("No files were chosen".to_owned()),
                    chosen => chosen,
                };
                self.answer_peer_pick(chosen)
            }
            (_, chosen) => Ok(chosen.ok().map(|paths| (paths, FileDestination::Documents))),
        }
    }

    /// The files for the peer's request, or tells the peer why none are coming.
    fn answer_peer_pick(
        &mut self,
        picked: Result<Vec<PathBuf>, String>,
    ) -> Result<Option<Job>, Closed> {
        match picked {
            Ok(paths) => Ok(Some((paths, FileDestination::Documents))),
            Err(reason) => {
                let reply = FileMessage::Error {
                    id: PEER_PICK_ID,
                    reason,
                };
                self.out.send(reply)?;
                Ok(None)
            }
        }
    }

    /// Tells the sending thread whether the peer took its message for transfer `id`.
    fn settle_sender(&self, id: u64, answer: Answer) {
        if let Some((active, tx)) = &self.sender
            && *active == id
        {
            let _ = tx.try_send(answer);
        }
    }

    fn peer_failed(&mut self, id: u64, reason: &str) {
        let sending = self
            .sender
            .as_ref()
            .is_some_and(|(active, _)| *active == id);
        let receiving = self.incoming.as_ref().is_some_and(|i| i.id == id);
        // Errors for transfers rejected before they began here are not ours to report.
        if sending || receiving {
            self.set_status(format!("Transfer failed: {reason}"));
        }
        self.settle_sender(id, Err(reason.into()));
        if receiving {
            self.incoming = None;
            self.progress = None;
        }
    }

    /// Applies one message of the peer's transfer and acknowledges it or reports its failure.
    fn receive(&mut self, message: FileMessage) -> Result<(), Closed> {
        let packet_id = message_id(&message);
        let result = match message {
            FileMessage::Begin { id, destination } => self.begin_incoming(id, destination),
            FileMessage::Finish { .. } => self.finish_incoming(packet_id),
            message => self.accept_incoming(packet_id, message),
        };
        let response = match result {
            Ok(()) => FileMessage::Ack { id: packet_id },
            Err(e) => {
                self.incoming = None;
                self.progress = None;
                end_transfer(&mut self.ended, packet_id);
                let reason = format!("{e:#}");
                tracing::warn!(id = packet_id, %reason, "file transfer failed");
                match e.downcast_ref::<Refusal>().and_then(|r| r.notice.clone()) {
                    Some(notice) => self.set_status(notice),
                    None => self.set_status(reason.clone()),
                }
                FileMessage::Error {
                    id: packet_id,
                    reason,
                }
            }
        };
        self.out.send(response)
    }

    /// Where a transfer's files go: Documents, or a cache batch that the
    /// clipboard or a drop target reads.
    fn placement(&self, destination: &FileDestination) -> Placement {
        match destination {
            FileDestination::Documents => Placement::Documents,
            FileDestination::Drop { .. } if self.desktop.drop_files.is_none() => {
                Placement::Documents
            }
            _ => Placement::Batch,
        }
    }

    fn begin_incoming(&mut self, id: u64, destination: FileDestination) -> anyhow::Result<()> {
        tracing::info!(id, ?destination, "receiving file transfer");
        let placement = self.placement(&destination);
        let storage = match placement {
            Placement::Documents => (self.desktop.documents)()?,
            Placement::Batch => (self.desktop.cache)()?,
        };
        let mut incoming = Incoming::new(id, destination, storage, placement)?;
        if let Some(create) = self.desktop.create_file {
            incoming = incoming.creating_files_with(create);
        }
        self.incoming = Some(incoming);
        if self.desktop.interactive {
            self.progress = native::Progress::new(id)
                .map_err(|error| {
                    tracing::warn!(%error, "could not show file transfer progress");
                })
                .ok();
        }
        Ok(())
    }

    fn accept_incoming(&mut self, packet_id: u64, message: FileMessage) -> anyhow::Result<()> {
        let state = active_incoming(&mut self.incoming, packet_id)?;
        if let FileMessage::Totals { bytes, .. } = message {
            // Refused here, the transfer ends before any file is opened.
            self.admission.admit_totals(
                &state.destination,
                bytes,
                || native::available_space(&state.stage).ok(),
                Instant::now(),
            )?;
        }
        let refresh = !matches!(message, FileMessage::Chunk { .. });
        state.accept(message)?;
        if refresh || self.progress_updated.elapsed() >= Duration::from_millis(100) {
            if let Some(progress) = &self.progress {
                progress.update(
                    state.received_bytes,
                    state.total_bytes,
                    &state.current_name,
                    state.entries,
                    state.total_entries,
                );
            }
            self.progress_updated = Instant::now();
        }
        Ok(())
    }

    /// Puts a verified transfer in place; its acknowledgement follows.
    fn finish_incoming(&mut self, packet_id: u64) -> anyhow::Result<()> {
        let state = active_incoming(&mut self.incoming, packet_id)?;
        let paths = state.finish()?;
        let (destination, placement, base) = (
            state.destination.clone(),
            state.placement,
            state.base.clone(),
        );
        self.incoming = None;
        // Hide before native delivery so the progress window cannot
        // obscure the user's Explorer/browser drop target.
        self.progress = None;
        let clipboard = matches!(
            destination,
            FileDestination::Clipboard | FileDestination::ClipboardPaste { .. }
        );
        if clipboard
            && placement == Placement::Batch
            && let Some(batch) = paths.first().and_then(|path| path.parent())
        {
            self.clipboard_batches.push_back(batch.to_owned());
        }
        let delivered = self.deliver(packet_id, &paths, &destination, placement);
        if clipboard {
            self.release_clipboard_batches();
        }
        delivered?;
        if placement == Placement::Batch && self.last_cache_sweep.elapsed() >= CACHE_SWEEP_INTERVAL
        {
            self.last_cache_sweep = Instant::now();
            // Off this thread, which acknowledges the peer's transfers.
            let mut keep = (self.desktop.clipboard_files)().unwrap_or_default();
            keep.extend(self.copying.lock().unwrap().iter().cloned());
            std::thread::spawn(move || sweep_cache(&base, &keep));
        }
        tracing::info!(id = packet_id, "file transfer received and verified");
        self.set_status("Transfer complete");
        Ok(())
    }

    /// Removes, off this thread, the clipboard batches this worker received
    /// before the newest [`KEPT_CLIPBOARD_BATCHES`]. Each copy replaces the
    /// clipboard, so an older batch is still used only while the clipboard
    /// points into it (the user copied a received file again) or while it is
    /// being copied into Documents; those stay until a later copy. Nothing is
    /// removed while the clipboard cannot be read.
    fn release_clipboard_batches(&mut self) {
        let excess = self
            .clipboard_batches
            .len()
            .saturating_sub(KEPT_CLIPBOARD_BATCHES);
        if excess == 0 {
            return;
        }
        let Ok(mut keep) = (self.desktop.clipboard_files)() else {
            return;
        };
        keep.extend(self.copying.lock().unwrap().iter().cloned());
        let (held, released): (Vec<_>, Vec<_>) = self
            .clipboard_batches
            .drain(..excess)
            .partition(|batch| keep.iter().any(|kept| kept.starts_with(batch)));
        for batch in held.into_iter().rev() {
            self.clipboard_batches.push_front(batch);
        }
        if !released.is_empty() {
            (self.desktop.spawn)(Box::new(move || {
                for batch in released {
                    remove_batch_folder(&batch);
                }
            }));
        }
    }

    /// Hands received files to the clipboard or the drop target; files
    /// received into Documents are already in place.
    fn deliver(
        &mut self,
        id: u64,
        paths: &[PathBuf],
        destination: &FileDestination,
        placement: Placement,
    ) -> anyhow::Result<()> {
        match destination {
            FileDestination::Clipboard | FileDestination::ClipboardPaste { .. } => {
                (self.desktop.set_clipboard_files)(paths)?;
                self.last_clipboard_sequence = (self.desktop.clipboard_sequence)();
                if let FileDestination::ClipboardPaste { display_id } = destination {
                    self.paste
                        .as_ref()
                        .context("this side does not paste files")?(
                        *display_id
                    )?;
                }
            }
            FileDestination::Drop { .. } if placement == Placement::Documents => {}
            FileDestination::Drop { display_id, x, y } => {
                let accepted = match &self.desktop.drop_files {
                    Some(drop_files) => drop_files(paths, *display_id, *x, *y),
                    None => Ok(false),
                }
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "native drop unavailable; saving to Documents");
                    false
                });
                if !accepted {
                    self.save_refused_drop(id, paths)?;
                }
            }
            FileDestination::Documents => {}
        }
        Ok(())
    }

    /// Moves a drop no app took from its cache batch into Documents. When
    /// Documents is on another volume the files have to be copied, which for
    /// a large drop takes about as long as the transfer did: that copy runs
    /// after the acknowledgement, off this thread, and a failure is reported
    /// to the peer as an error for the transfer.
    fn save_refused_drop(&mut self, id: u64, paths: &[PathBuf]) -> anyhow::Result<()> {
        let documents = (self.desktop.documents)()?;
        let left = move_to_documents(paths, &documents)?;
        if left.is_empty() {
            remove_batch(paths);
            return Ok(());
        }
        let batch = left[0]
            .parent()
            .context("received file has no folder")?
            .to_owned();
        tracing::info!(
            id,
            "Documents is on another volume; copying the dropped files there"
        );
        self.copying.lock().unwrap().push(batch.clone());
        let (out, copying) = (self.out.clone(), self.copying.clone());
        (self.desktop.spawn)(Box::new(move || {
            let copied = commit_documents(left.clone(), documents);
            copying.lock().unwrap().retain(|kept| *kept != batch);
            match copied {
                Ok(_) => remove_batch(&left),
                Err(error) => {
                    let reason =
                        format!("The files arrived but could not be saved to Documents: {error:#}");
                    tracing::warn!(id, %reason, "dropped files not saved");
                    let _ = out.send(FileMessage::Error { id, reason });
                }
            }
        }));
        Ok(())
    }

    /// The local file clipboard to send, when it changed since the last poll.
    fn poll_clipboard(&mut self) -> Option<Job> {
        self.poll = Instant::now();
        let sequence = (self.desktop.clipboard_sequence)();
        if sequence != self.last_clipboard_sequence && !(self.clipboard_enabled)() {
            // Copies made while clipboard sync is off stay local after re-enabling.
            self.last_clipboard_sequence = sequence;
        } else if sequence != self.last_clipboard_sequence
            && let Ok(paths) = (self.desktop.clipboard_files)()
        {
            tracing::info!(files = paths.len(), "native file clipboard changed");
            self.last_clipboard_sequence = sequence;
            if !paths.is_empty() {
                return Some((paths, FileDestination::Clipboard));
            }
        }
        None
    }

    fn queue(&mut self, job: Job) -> Result<(), Closed> {
        if self.pending.len() < 8 {
            self.pending.push_back(job);
            return Ok(());
        }
        self.set_status("Transfer queue is full; try again after completion");
        if answers_peer_pick(self.role, &job.1) {
            let reason = "The remote transfer queue is full".into();
            let reply = FileMessage::Error {
                id: PEER_PICK_ID,
                reason,
            };
            self.out.send(reply)?;
        }
        Ok(())
    }

    /// Starts sending the next queued job once the previous one is done.
    fn start_next_send(&mut self) {
        if self
            .sender_thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
            && let Some((paths, destination)) = self.pending.pop_front()
        {
            let transfer_id = id();
            let (ack, acknowledgements) = mpsc::sync_channel(SEND_WINDOW + 1);
            self.sender = Some((transfer_id, ack));
            let out = self.out.clone();
            let status = self.status.clone();
            let answers_pick = answers_peer_pick(self.role, &destination);
            self.sender_thread = Some(std::thread::spawn(move || {
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

/// The transfer in progress, which `packet_id` has to belong to.
fn active_incoming(
    incoming: &mut Option<Incoming>,
    packet_id: u64,
) -> anyhow::Result<&mut Incoming> {
    let state = incoming.as_mut().context("transfer has not started")?;
    ensure!(state.id == packet_id, "transfer ID mismatch");
    Ok(state)
}
