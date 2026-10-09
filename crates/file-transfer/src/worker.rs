use anyhow::{Context, ensure};
use meshrmm_protocol::{FileDestination, FileMessage};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{
    AckWindow, Admission, CACHE_SWEEP_INTERVAL, Command, Incoming, OutgoingFiles, PEER_PICK_ID,
    Role, SEND_WINDOW, answers_peer_pick, commit_documents, end_transfer, id, message_id, native,
    remove_batch, send_paths, sweep_cache, sweep_transfer_folders,
};

/// The session's outgoing channel closed, which ends the worker.
type Closed = mpsc::SendError<FileMessage>;
/// Local files to send and where the peer puts them.
type Job = (Vec<PathBuf>, FileDestination);

pub(super) fn run(
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
    let mut worker = Worker::new(role, out, status, clipboard_enabled);
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

struct Worker {
    role: Role,
    out: OutgoingFiles,
    status: Arc<Mutex<String>>,
    clipboard_enabled: fn() -> bool,
    poll: Instant,
    last_cache_sweep: Instant,
    last_clipboard_sequence: u64,
    available: bool,
    pending: VecDeque<Job>,
    sender_thread: Option<JoinHandle<()>>,
    sender: Option<(u64, mpsc::SyncSender<bool>)>,
    progress_updated: Instant,
    progress: Option<native::Progress>,
    /// Transfers rejected or failed here, whose in-flight messages are dropped.
    ended: VecDeque<u64>,
    incoming: Option<Incoming>,
    admission: Admission,
}

impl Worker {
    fn new(
        role: Role,
        out: OutgoingFiles,
        status: Arc<Mutex<String>>,
        clipboard_enabled: fn() -> bool,
    ) -> Self {
        Self {
            role,
            out,
            status,
            clipboard_enabled,
            admission: Admission::new(role),
            incoming: None,
            ended: VecDeque::new(),
            progress: None,
            progress_updated: Instant::now(),
            sender: None,
            sender_thread: None,
            pending: VecDeque::new(),
            available: false,
            last_clipboard_sequence: native::clipboard_sequence(),
            // The sweep at startup covered the cache.
            last_cache_sweep: Instant::now(),
            poll: Instant::now(),
        }
    }

    fn set_status(&self, status: impl Into<String>) {
        *self.status.lock().unwrap() = status.into();
    }

    fn step(&mut self, command: Option<Command>) -> Result<(), Closed> {
        let command = match command {
            Some(Command::Peer(FileMessage::Begin { id, destination })) => {
                self.admit(id, destination)?
            }
            command => command,
        };
        let mut send = self.handle(command)?;
        if send.is_none() && self.available && self.poll.elapsed() >= Duration::from_millis(250) {
            send = self.poll_clipboard();
        }
        if let Some(job) = send
            && !job.0.is_empty()
        {
            self.queue(job)?;
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
            Err("Another transfer is in progress")
        } else {
            self.admission
                .admit(&destination, (self.clipboard_enabled)(), Instant::now())
        };
        match admitted {
            Ok(()) => Ok(Some(Command::Peer(FileMessage::Begin { id, destination }))),
            Err(reason) => {
                tracing::info!(id, ?destination, reason, "rejected file transfer");
                end_transfer(&mut self.ended, id);
                if self.role == Role::Viewer && destination != FileDestination::Clipboard {
                    self.set_status(reason);
                }
                let reason = reason.into();
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
                    self.pick_files()
                        .ok()
                        .map(|paths| (paths, FileDestination::Documents))
                } else {
                    None
                }
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
            Some(Command::Peer(FileMessage::Pick)) => self.answer_peer_pick()?,
            Some(Command::Peer(FileMessage::Error { id, reason }))
                if id == PEER_PICK_ID && self.role == Role::Viewer =>
            {
                self.admission.close_peer_pick();
                self.set_status(format!("No files received: {reason}"));
                None
            }
            Some(Command::Peer(FileMessage::Ack { id })) => {
                self.settle_sender(id, true);
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

    fn pick_files(&self) -> Result<Vec<PathBuf>, String> {
        native::pick().map_err(|error| {
            tracing::warn!(%error, "file picker failed");
            self.set_status(format!("File picker failed: {error:#}"));
            format!("File picker failed: {error:#}")
        })
    }

    /// Picks files for the peer's request, or tells the peer why none are coming.
    fn answer_peer_pick(&mut self) -> Result<Option<Job>, Closed> {
        let picked = if !self.available {
            Err("File transfers are not ready".to_owned())
        } else {
            match self.pick_files() {
                Ok(paths) if paths.is_empty() => Err("No files were chosen".to_owned()),
                picked => picked,
            }
        };
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
    fn settle_sender(&self, id: u64, accepted: bool) {
        if let Some((active, tx)) = &self.sender
            && *active == id
        {
            let _ = tx.try_send(accepted);
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
        self.settle_sender(id, false);
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
                self.set_status(reason.clone());
                FileMessage::Error {
                    id: packet_id,
                    reason,
                }
            }
        };
        self.out.send(response)
    }

    fn begin_incoming(&mut self, id: u64, destination: FileDestination) -> anyhow::Result<()> {
        tracing::info!(id, ?destination, "receiving file transfer");
        let storage = if destination == FileDestination::Documents {
            native::documents()?
        } else {
            native::cache()?
        };
        self.incoming = Some(Incoming::new(id, destination, storage)?);
        self.progress = native::Progress::new(id)
            .map_err(|error| {
                tracing::warn!(%error, "could not show file transfer progress");
            })
            .ok();
        Ok(())
    }

    fn accept_incoming(&mut self, packet_id: u64, message: FileMessage) -> anyhow::Result<()> {
        let state = active_incoming(&mut self.incoming, packet_id)?;
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

    fn finish_incoming(&mut self, packet_id: u64) -> anyhow::Result<()> {
        let state = active_incoming(&mut self.incoming, packet_id)?;
        let paths = state.finish()?;
        // Hide before native delivery so the progress window cannot
        // obscure the user's Explorer/browser drop target.
        self.progress = None;
        deliver(
            &paths,
            &state.destination,
            &mut self.last_clipboard_sequence,
        )?;
        if state.destination != FileDestination::Documents
            && self.last_cache_sweep.elapsed() >= CACHE_SWEEP_INTERVAL
        {
            self.last_cache_sweep = Instant::now();
            // Off this thread, which acknowledges the peer's transfers.
            let base = state.base.clone();
            let keep = native::clipboard_files().unwrap_or_default();
            std::thread::spawn(move || sweep_cache(&base, &keep));
        }
        tracing::info!(id = packet_id, "file transfer received and verified");
        self.set_status("Transfer complete");
        self.incoming = None;
        Ok(())
    }

    /// The local file clipboard to send, when it changed since the last poll.
    fn poll_clipboard(&mut self) -> Option<Job> {
        self.poll = Instant::now();
        let sequence = native::clipboard_sequence();
        if sequence != self.last_clipboard_sequence && !(self.clipboard_enabled)() {
            // Copies made while clipboard sync is off stay local after re-enabling.
            self.last_clipboard_sequence = sequence;
        } else if sequence != self.last_clipboard_sequence
            && let Ok(paths) = native::clipboard_files()
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

/// Hands received files to the clipboard or the drop target; Documents transfers are already in place.
fn deliver(
    paths: &[PathBuf],
    destination: &FileDestination,
    last_clipboard_sequence: &mut u64,
) -> anyhow::Result<()> {
    match destination {
        FileDestination::Clipboard | FileDestination::ClipboardPaste { .. } => {
            native::set_clipboard_files(paths)?;
            *last_clipboard_sequence = native::clipboard_sequence();
            if let FileDestination::ClipboardPaste { display_id } = destination {
                native::paste_files(*display_id)?;
            }
        }
        FileDestination::Drop { display_id, x, y } => {
            let accepted = native::drop_files(paths, *display_id, *x, *y).unwrap_or_else(|error| {
                tracing::warn!(%error, "native drop unavailable; saving to Documents");
                false
            });
            if !accepted {
                commit_documents(paths.to_vec(), native::documents()?)?;
                remove_batch(paths);
            }
        }
        FileDestination::Documents => {}
    }
    Ok(())
}
