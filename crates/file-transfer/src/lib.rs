//! Native, session-scoped file copying. File data never passes through signaling.
use anyhow::{Context, bail, ensure};
use meshrmm_protocol::{FILE_CHUNK_BYTES, FileDestination, FileMessage};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
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

pub fn clipboard_has_files() -> bool {
    native::clipboard_files().is_ok_and(|paths| !paths.is_empty())
}

pub enum Command {
    Peer(FileMessage),
    Send(Vec<PathBuf>, FileDestination),
    Pick,
}
#[derive(Clone)]
pub struct TransferSession {
    tx: mpsc::SyncSender<Command>,
    rx: Arc<Mutex<mpsc::Receiver<FileMessage>>>,
    status: Arc<Mutex<String>>,
}
impl Default for TransferSession {
    fn default() -> Self {
        Self::new()
    }
}
impl TransferSession {
    pub fn new() -> Self {
        let (tx, commands) = mpsc::sync_channel(32);
        let (out, rx) = mpsc::sync_channel(8);
        let status = Arc::new(Mutex::new("Waiting for file-transfer support…".into()));
        let worker_status = status.clone();
        std::thread::spawn(move || worker(commands, out, worker_status));
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            status,
        }
    }
    /// Run native OLE operations on a dedicated helper's main thread while its
    /// pipe transport runs separately and remains responsive to cancellation.
    pub fn run_on_current_thread<T: Send + 'static>(
        client: impl FnOnce(Self) -> T + Send + 'static,
    ) -> T {
        let (tx, commands) = mpsc::sync_channel(32);
        let (out, rx) = mpsc::sync_channel(8);
        let status = Arc::new(Mutex::new("Waiting for file-transfer support…".into()));
        let session = Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            status: status.clone(),
        };
        let transport = std::thread::spawn(move || client(session));
        worker(commands, out, status);
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

fn worker(
    commands: mpsc::Receiver<Command>,
    out: mpsc::SyncSender<FileMessage>,
    status: Arc<Mutex<String>>,
) {
    let _native = match native::initialize() {
        Ok(v) => v,
        Err(e) => {
            *status.lock().unwrap() = format!("File transfers unavailable: {e:#}");
            return;
        }
    };
    let _ = out.send(FileMessage::Available);
    let mut incoming: Option<Incoming> = None;
    let mut sender: Option<(u64, mpsc::SyncSender<bool>)> = None;
    let mut sender_thread: Option<std::thread::JoinHandle<()>> = None;
    let mut pending = std::collections::VecDeque::new();
    let mut available = false;
    let mut last_clipboard_sequence = native::clipboard_sequence();
    let mut poll = Instant::now();
    loop {
        let command = match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => None,
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
            Some(Command::Send(paths, destination)) => {
                if available {
                    send = Some((paths, destination));
                }
            }
            Some(Command::Peer(FileMessage::Available)) => {
                available = true;
                *status.lock().unwrap() = "Ready".into();
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
                *status.lock().unwrap() = format!("Transfer failed: {reason}");
                if let Some((active, tx)) = &sender
                    && *active == id
                {
                    let _ = tx.try_send(false);
                }
                if incoming.as_ref().is_some_and(|i| i.id == id) {
                    incoming = None;
                }
            }
            Some(Command::Peer(message)) => {
                let packet_id = message_id(&message);
                let result = (|| -> anyhow::Result<()> {
                    if let FileMessage::Begin { id, destination } = message {
                        ensure!(incoming.is_none(), "another transfer is in progress");
                        tracing::info!(id, ?destination, "receiving file transfer");
                        let storage = if destination == FileDestination::Documents {
                            native::documents()?
                        } else {
                            native::cache()?
                        };
                        incoming = Some(Incoming::new(id, destination, storage)?);
                    } else {
                        let state = incoming.as_mut().context("transfer has not started")?;
                        ensure!(state.id == packet_id, "transfer ID mismatch");
                        if let FileMessage::Finish { .. } = message {
                            let paths = state.finish()?;
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
                                        commit_documents(paths, native::documents()?)?;
                                    }
                                }
                                FileDestination::Documents => {}
                            }
                            tracing::info!(id = packet_id, "file transfer received and verified");
                            *status.lock().unwrap() = "Transfer complete".into();
                            incoming = None;
                        } else {
                            state.accept(message)?;
                        }
                    }
                    Ok(())
                })();
                let response = match result {
                    Ok(()) => FileMessage::Ack { id: packet_id },
                    Err(e) => {
                        incoming = None;
                        let reason = format!("{e:#}");
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
            if sequence != last_clipboard_sequence
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
            let (ack, acknowledgements) = mpsc::sync_channel(1);
            sender = Some((transfer_id, ack));
            let out = out.clone();
            let status = status.clone();
            sender_thread = Some(std::thread::spawn(move || {
                let _native = native::initialize();
                *status.lock().unwrap() = "Transferring files…".into();
                let result = send_paths(transfer_id, paths, destination, |message| {
                    out.send(message).context("session closed")?;
                    ensure!(
                        acknowledgements
                            .recv_timeout(Duration::from_secs(120))
                            .context("transfer acknowledgement timed out")?,
                        "peer rejected transfer"
                    );
                    Ok(())
                });
                *status.lock().unwrap() = match &result {
                    Ok(()) => "Transfer complete".into(),
                    Err(e) => format!("Transfer failed: {e:#}"),
                };
                if let Err(e) = result {
                    let _ = out.send(FileMessage::Error {
                        id: transfer_id,
                        reason: format!("{e:#}"),
                    });
                }
            }));
        }
    }
}
fn message_id(m: &FileMessage) -> u64 {
    match m {
        FileMessage::Begin { id, .. }
        | FileMessage::Entry { id, .. }
        | FileMessage::Chunk { id, .. }
        | FileMessage::EndEntry { id, .. }
        | FileMessage::Finish { id }
        | FileMessage::Ack { id }
        | FileMessage::Error { id, .. } => *id,
        _ => 0,
    }
}
fn valid_path(path: &str) -> anyhow::Result<PathBuf> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "invalid path length"
    );
    let mut result = PathBuf::new();
    for component in path.split('/') {
        ensure!(
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.contains(['\\', ':', '\0'])
                && !component.ends_with(['.', ' ']),
            "unsafe file name"
        );
        let stem = component.split('.').next().unwrap().to_ascii_uppercase();
        ensure!(
            ![
                "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
                "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
                "LPT9"
            ]
            .contains(&stem.as_str()),
            "reserved file name"
        );
        result.push(component);
    }
    Ok(result)
}
struct Incoming {
    id: u64,
    destination: FileDestination,
    stage: PathBuf,
    base: PathBuf,
    roots: Vec<String>,
    file: Option<(File, u64, Sha256)>,
    entries: usize,
}
impl Incoming {
    fn new(id: u64, destination: FileDestination, documents: PathBuf) -> anyhow::Result<Self> {
        let base = documents.join("MeshRMM Transferred Files");
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
        })
    }
    fn accept(&mut self, message: FileMessage) -> anyhow::Result<()> {
        match message {
            FileMessage::Entry { path, size, .. } => {
                ensure!(self.file.is_none(), "previous file is unfinished");
                self.entries += 1;
                ensure!(self.entries <= 100_000, "too many entries");
                let relative = valid_path(&path)?;
                let root = path.split('/').next().unwrap().to_owned();
                if !self.roots.contains(&root) {
                    self.roots.push(root);
                }
                let target = self.stage.join(relative);
                if let Some(parent) = target.parent() {
                    ensure!(parent.is_dir(), "parent directory missing");
                }
                if let Some(size) = size {
                    self.file = Some((
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(target)?,
                        size,
                        Sha256::new(),
                    ));
                } else {
                    fs::create_dir(target)?;
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
            self.file.is_none() && !self.roots.is_empty(),
            "incomplete transfer"
        );
        if self.destination != FileDestination::Documents {
            let batch = self.base.join(format!("transfer-{}", crate::id()));
            fs::rename(&self.stage, &batch)?;
            return Ok(self.roots.iter().map(|root| batch.join(root)).collect());
        }
        let mut paths = Vec::new();
        for root in &self.roots {
            let mut target = self.base.join(root);
            if target.try_exists()? {
                target = self.base.join(format!("{}-{}", crate::id(), root));
            }
            ensure!(!target.try_exists()?, "destination already exists");
            fs::rename(self.stage.join(root), &target)?;
            paths.push(target);
        }
        Ok(paths)
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
    send(FileMessage::Begin { id, destination })?;
    for (path, relative, size) in entries {
        send(FileMessage::Entry {
            id,
            path: relative,
            size,
        })?;
        if size.is_some() {
            let mut file = File::open(path)?;
            let mut hash = Sha256::new();
            let mut buffer = vec![0; FILE_CHUNK_BYTES];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
                send(FileMessage::Chunk {
                    id,
                    data: buffer[..count].to_vec(),
                })?;
            }
            send(FileMessage::EndEntry {
                id,
                sha256: hash.finalize().to_vec(),
            })?;
        }
    }
    send(FileMessage::Finish { id })
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
