//! The worker driven through its desktop seam: no native dialogs, windows,
//! clipboard or user folders, only temporary folders and fake pickers.
use std::{
    cell::Cell,
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::SystemTime,
};

use super::*;
use crate::{
    pick::Picker,
    tests::Sandbox,
    worker::{Desktop, DropFiles, Worker, serve},
};

/// Long enough for anything here to happen on a loaded machine; reaching it
/// fails the test.
const DEADLINE: Duration = Duration::from_secs(20);

fn drop_destination() -> FileDestination {
    FileDestination::Drop {
        display_id: meshrmm_protocol::DisplayId(1),
        x: 0,
        y: 0,
    }
}

fn desktop(documents: &Path, cache: &Path, drop_files: Option<DropFiles>, pick: Picker) -> Desktop {
    let (documents, cache) = (documents.to_owned(), cache.to_owned());
    Desktop {
        documents: Box::new(move || Ok(documents.clone())),
        cache: Box::new(move || Ok(cache.clone())),
        drop_files,
        pick,
        spawn: Box::new(|work| {
            std::thread::spawn(work);
        }),
        create_file: None,
        clipboard_files: Box::new(|| Ok(Vec::new())),
        set_clipboard_files: Box::new(|_| Ok(())),
        clipboard_sequence: Box::new(|| 0),
        interactive: false,
    }
}

fn no_picker() -> Picker {
    Arc::new(|_: &crate::PickCancel| panic!("no picker in this test"))
}

/// A drop target that takes nothing, as an app refusing the files does.
fn refusing_drop() -> Option<DropFiles> {
    Some(Box::new(|_, _, _, _| Ok(false)))
}

/// An agent worker whose messages to the peer arrive on the returned channel.
fn worker(desktop: Desktop) -> (Worker, mpsc::Receiver<FileMessage>, Arc<Mutex<String>>) {
    worker_as(Role::Agent, desktop)
}

fn worker_as(
    role: Role,
    desktop: Desktop,
) -> (Worker, mpsc::Receiver<FileMessage>, Arc<Mutex<String>>) {
    let (sender, outgoing) = mpsc::sync_channel(256);
    let out = OutgoingFiles {
        sender,
        ready: Arc::default(),
    };
    let status = Arc::new(Mutex::new(String::new()));
    let mut worker = Worker::new(role, out, status.clone(), || true, None, desktop);
    worker.available = true;
    (worker, outgoing, status)
}

/// What a peer sends for `paths`.
fn transfer(id: u64, paths: Vec<PathBuf>, destination: FileDestination) -> Vec<FileMessage> {
    let mut messages = Vec::new();
    send_paths(id, paths, destination, |message| {
        messages.push(message);
        Ok(())
    })
    .unwrap();
    messages
}

fn next(outgoing: &mpsc::Receiver<FileMessage>) -> FileMessage {
    outgoing.recv_timeout(DEADLINE).expect("the worker answers")
}

/// Feeds the worker a transfer and checks it acknowledged every message.
fn receive(
    worker: &mut Worker,
    outgoing: &mpsc::Receiver<FileMessage>,
    messages: Vec<FileMessage>,
) {
    for message in messages {
        let id = message_id(&message);
        worker.step(Some(Command::Peer(message))).unwrap();
        assert_eq!(outgoing.try_recv().unwrap(), FileMessage::Ack { id });
    }
}

/// The one file under `folder`'s `.partial-*` staging folders named `name`.
fn staged(folder: &Path, name: &str) -> PathBuf {
    let mut found = fs::read_dir(folder.join(TRANSFER_FOLDER))
        .unwrap()
        .flatten()
        .map(|entry| entry.path().join(name))
        .filter(|path| path.exists());
    let path = found.next().expect("the file is staged");
    assert!(found.next().is_none());
    path
}

/// The time the test gives a staged file, which a copy would not keep.
fn mark(path: &Path) -> SystemTime {
    let marker = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(marker)
        .unwrap();
    marker
}

/// A drop no app takes, on Windows (received into the cache, then refused)
/// and on macOS and Linux (no native drop, received into Documents), ends in
/// Documents as the very file its chunks were written to, next to a file of
/// the same name it leaves alone, with no second copy and no cache batch
/// left behind, before the finish is acknowledged.
#[test]
fn a_dropped_file_is_written_once_and_moved_into_documents() {
    for drop_files in [refusing_drop(), None] {
        let native_drop = drop_files.is_some();
        let source = Sandbox::new();
        let documents = Sandbox::new();
        let cache = Sandbox::new();
        let file = source.0.join("report.bin");
        let data: Vec<u8> = (0..200_000).map(|i| (i % 249) as u8).collect();
        fs::write(&file, &data).unwrap();
        let saved = documents.0.join(TRANSFER_FOLDER);
        fs::create_dir(&saved).unwrap();
        fs::write(saved.join("report.bin"), "original").unwrap();

        let (mut worker, outgoing, _) =
            worker(desktop(&documents.0, &cache.0, drop_files, no_picker()));
        let mut messages = transfer(5, vec![file], drop_destination());
        let finish = messages.pop().unwrap();
        receive(&mut worker, &outgoing, messages);
        let marker = mark(&staged(
            if native_drop { &cache.0 } else { &documents.0 },
            "report.bin",
        ));
        receive(&mut worker, &outgoing, vec![finish]);

        assert_eq!(
            fs::read_to_string(saved.join("report.bin")).unwrap(),
            "original"
        );
        let received: Vec<_> = fs::read_dir(&saved)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.file_name().unwrap() != "report.bin")
            .collect();
        assert_eq!(received.len(), 1, "{received:?}");
        assert_eq!(fs::read(&received[0]).unwrap(), data);
        assert_eq!(
            fs::metadata(&received[0]).unwrap().modified().unwrap(),
            marker,
            "moved, not written again"
        );
        if native_drop {
            assert_eq!(
                fs::read_dir(cache.0.join(TRANSFER_FOLDER)).unwrap().count(),
                0,
                "no cache batch is left"
            );
        } else {
            assert!(
                !cache.0.join(TRANSFER_FOLDER).exists(),
                "the cache is unused"
            );
        }
    }
}

thread_local! {
    static OPENED: Cell<usize> = const { Cell::new(0) };
    static WRITTEN: Cell<u64> = const { Cell::new(0) };
}
/// A received file that counts the bytes that reach it.
struct Counted(File);
impl Write for Counted {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let written = self.0.write(data)?;
        WRITTEN.set(WRITTEN.get() + written as u64);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
fn counted(path: &Path) -> io::Result<Box<dyn Write + Send>> {
    OPENED.set(OPENED.get() + 1);
    let file = File::options().write(true).create_new(true).open(path)?;
    Ok(Box::new(Counted(file)))
}

/// A folder of many small files is received with each file opened once and
/// each byte written once. The receiver holds only a writer for each file,
/// so it cannot flush one to the disk before going on to the next.
#[test]
fn many_small_files_are_each_written_once() {
    let source = Sandbox::new();
    let documents = Sandbox::new();
    let folder = source.0.join("logs");
    fs::create_dir(&folder).unwrap();
    let files = 400;
    for i in 0..files {
        fs::write(folder.join(format!("{i}.log")), vec![b'x'; 1000 + i]).unwrap();
    }
    let total: u64 = (0..files).map(|i| 1000 + i as u64).sum();
    let mut desktop = desktop(&documents.0, &documents.0, None, no_picker());
    desktop.create_file = Some(counted);
    let (mut worker, outgoing, _) = worker(desktop);
    receive(
        &mut worker,
        &outgoing,
        transfer(6, vec![folder], FileDestination::Documents),
    );
    assert_eq!(OPENED.get(), files);
    assert_eq!(WRITTEN.get(), total);
    let received = documents.0.join(TRANSFER_FOLDER).join("logs");
    assert_eq!(fs::read_dir(&received).unwrap().count(), files);
    assert_eq!(fs::read(received.join("7.log")).unwrap(), vec![b'x'; 1007]);
}

/// Where Documents is on another volume than the cache, a refused drop is
/// acknowledged once verified in the cache, and its copy to Documents runs
/// afterwards. Linux tests it between /dev/shm and the temporary folder when
/// they are different file systems.
#[cfg(target_os = "linux")]
#[test]
fn a_drop_copied_across_volumes_is_acknowledged_before_the_copy() {
    use std::os::unix::fs::MetadataExt;
    let source = Sandbox::new();
    let documents = Sandbox::new();
    let Ok(shm) = fs::metadata("/dev/shm") else {
        eprintln!("no /dev/shm; skipped");
        return;
    };
    if shm.dev() == fs::metadata(&documents.0).unwrap().dev() {
        eprintln!("/dev/shm is on the temporary folder's volume; skipped");
        return;
    }
    let cache = Sandbox(PathBuf::from(format!(
        "/dev/shm/meshrmm-transfer-test-{}",
        id()
    )));
    fs::create_dir(&cache.0).unwrap();
    let file = source.0.join("video.bin");
    fs::write(&file, vec![9; 100_000]).unwrap();
    type Copy = Box<dyn FnOnce() + Send>;
    let copies: Arc<Mutex<Vec<Copy>>> = Arc::default();
    let mut desktop = desktop(&documents.0, &cache.0, refusing_drop(), no_picker());
    let held = copies.clone();
    desktop.spawn = Box::new(move |work| held.lock().unwrap().push(work));
    let (mut worker, outgoing, _) = worker(desktop);

    receive(
        &mut worker,
        &outgoing,
        transfer(8, vec![file], drop_destination()),
    );
    let saved = documents.0.join(TRANSFER_FOLDER);
    assert!(
        !saved.exists() || fs::read_dir(&saved).unwrap().count() == 0,
        "acknowledged before anything was copied"
    );
    let copy = copies.lock().unwrap().pop().expect("a copy follows");
    copy();
    assert_eq!(fs::read(saved.join("video.bin")).unwrap(), vec![9; 100_000]);
    assert_eq!(
        fs::read_dir(cache.0.join(TRANSFER_FOLDER)).unwrap().count(),
        0,
        "the cache batch is removed after the copy"
    );
    assert!(outgoing.try_recv().is_err(), "no error follows");
}

/// A fake picker that stays open until the test chooses for it or the pick
/// is cancelled.
struct FakePicker {
    picker: Picker,
    opened: mpsc::Receiver<()>,
    /// Chooses for the open picker.
    choose: Arc<Mutex<Option<mpsc::Sender<Vec<PathBuf>>>>>,
    calls: Arc<AtomicUsize>,
}
fn fake_picker() -> FakePicker {
    let (opened_tx, opened) = mpsc::channel();
    let choose: Arc<Mutex<Option<mpsc::Sender<Vec<PathBuf>>>>> = Arc::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let (slot, count) = (choose.clone(), calls.clone());
    let picker: Picker = Arc::new(move |cancel: &crate::PickCancel| {
        count.fetch_add(1, Ordering::SeqCst);
        let (chosen_tx, chosen) = mpsc::channel();
        *slot.lock().unwrap() = Some(chosen_tx.clone());
        if !cancel.closes_with(move || drop(chosen_tx.send(Vec::new()))) {
            return Ok(Vec::new());
        }
        opened_tx.send(()).unwrap();
        let paths = chosen.recv().unwrap_or_default();
        cancel.closed();
        Ok(paths)
    });
    FakePicker {
        picker,
        opened,
        choose,
        calls,
    }
}
impl FakePicker {
    fn wait_until_open(&self) {
        self.opened
            .recv_timeout(DEADLINE)
            .expect("the picker opens");
    }
    fn choose(&self, paths: Vec<PathBuf>) {
        let chooser = self.choose.lock().unwrap().take().unwrap();
        chooser.send(paths).unwrap();
    }
}

/// Runs a worker on its own thread, as a session does; `done` reports that
/// it returned.
fn serving(
    documents: PathBuf,
    picker: Picker,
) -> (
    mpsc::SyncSender<Command>,
    mpsc::Receiver<FileMessage>,
    mpsc::Receiver<()>,
) {
    let (commands, received) = mpsc::sync_channel(COMMAND_QUEUE);
    let (sender, outgoing) = mpsc::sync_channel(256);
    let (finished, done) = mpsc::channel();
    std::thread::spawn(move || {
        let out = OutgoingFiles {
            sender,
            ready: Arc::default(),
        };
        let desktop = desktop(&documents, &documents, None, picker);
        let mut worker = Worker::new(Role::Agent, out, Arc::default(), || true, None, desktop);
        worker.available = true;
        serve(received, worker);
        let _ = finished.send(());
    });
    (commands, outgoing, done)
}

/// While the picker is open, the worker still receives the peer's chunks,
/// takes the acknowledgements for its own transfer and acts on the peer
/// cancelling; what the picker then chooses is sent.
#[test]
fn the_worker_keeps_answering_while_a_picker_is_open() {
    let source = Sandbox::new();
    let documents = Sandbox::new();
    let file = source.0.join("notes.txt");
    fs::write(&file, "notes").unwrap();
    let picker = fake_picker();
    let (commands, outgoing, done) = serving(documents.0.clone(), picker.picker.clone());
    commands.send(Command::Pick).unwrap();
    picker.wait_until_open();

    // The peer's transfer: each message is acknowledged.
    let mut messages = transfer(900, vec![file.clone()], FileDestination::Documents);
    messages.truncate(4);
    assert!(matches!(messages[3], FileMessage::Chunk { .. }));
    for message in messages {
        commands.send(Command::Peer(message)).unwrap();
        assert_eq!(next(&outgoing), FileMessage::Ack { id: 900 });
    }
    // The peer cancels it: its partial file is removed.
    let reason = "cancelled".to_owned();
    commands
        .send(Command::Peer(FileMessage::Error { id: 900, reason }))
        .unwrap();
    commands
        .send(Command::Peer(FileMessage::Finish { id: 900 }))
        .unwrap();
    assert!(matches!(
        next(&outgoing),
        FileMessage::Error { id: 900, .. }
    ));
    assert_eq!(
        fs::read_dir(documents.0.join(TRANSFER_FOLDER))
            .unwrap()
            .count(),
        0
    );

    // This side's own transfer goes on as the peer acknowledges it.
    commands
        .send(Command::Send(
            vec![file.clone()],
            FileDestination::Clipboard,
        ))
        .unwrap();
    loop {
        let message = next(&outgoing);
        let finished = matches!(message, FileMessage::Finish { .. });
        let id = message_id(&message);
        commands
            .send(Command::Peer(FileMessage::Ack { id }))
            .unwrap();
        if finished {
            break;
        }
    }

    // What the picker chooses goes to the peer's Documents.
    picker.choose(vec![file]);
    assert_eq!(
        match next(&outgoing) {
            FileMessage::Begin { destination, .. } => destination,
            message => panic!("{message:?}"),
        },
        FileDestination::Documents
    );
    drop(commands);
    done.recv_timeout(DEADLINE).expect("the worker ends");
}

/// A second pick while one is open opens nothing: a local one says so, and
/// one the peer asked for is answered so the peer's request closes.
#[test]
fn a_second_pick_while_one_is_open_is_refused() {
    let documents = Sandbox::new();
    let picker = fake_picker();
    let (mut worker, outgoing, status) = worker(desktop(
        &documents.0,
        &documents.0,
        None,
        picker.picker.clone(),
    ));
    worker.step(Some(Command::Pick)).unwrap();
    picker.wait_until_open();
    worker.step(Some(Command::Pick)).unwrap();
    assert_eq!(*status.lock().unwrap(), "A file picker is already open");
    worker.step(Some(Command::Peer(FileMessage::Pick))).unwrap();
    match outgoing.try_recv().unwrap() {
        FileMessage::Error { id, reason } => {
            assert_eq!(id, PEER_PICK_ID);
            assert!(reason.contains("already open"), "{reason}");
        }
        message => panic!("{message:?}"),
    }
    picker.choose(Vec::new());
    drop(worker);
    assert_eq!(picker.calls.load(Ordering::SeqCst), 1);
}

/// Closing the session while a picker is open closes the picker and ends
/// both the worker and the picker's thread.
#[test]
fn closing_while_a_picker_is_open_ends_cleanly() {
    let documents = Sandbox::new();
    let picker = fake_picker();
    let (commands, _outgoing, done) = serving(documents.0.clone(), picker.picker.clone());
    commands.send(Command::Pick).unwrap();
    picker.wait_until_open();
    drop(commands);
    done.recv_timeout(DEADLINE).expect("the worker ends");
    // The picker's thread held a reference to the picker until it ended.
    let FakePicker { picker, .. } = picker;
    assert_eq!(Arc::strong_count(&picker), 1, "the picker's thread ended");
}

/// A clipboard the test can read and pin: files put there replace what it
/// holds, unless the test pinned other files there (the user copied a
/// received file again).
#[derive(Clone, Default)]
struct FakeClipboard {
    files: Arc<Mutex<Vec<PathBuf>>>,
    pinned: Arc<Mutex<Option<Vec<PathBuf>>>>,
    set: Arc<AtomicUsize>,
}
impl FakeClipboard {
    fn on(&self, desktop: &mut Desktop) {
        let (files, pinned, set) = (self.files.clone(), self.pinned.clone(), self.set.clone());
        desktop.set_clipboard_files = Box::new(move |paths| {
            set.fetch_add(1, Ordering::SeqCst);
            *files.lock().unwrap() = paths.to_vec();
            Ok(())
        });
        let files = self.files.clone();
        desktop.clipboard_files = Box::new(move || {
            Ok(pinned
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| files.lock().unwrap().clone()))
        });
        // Removals run before the worker goes on, so the test sees them.
        desktop.spawn = Box::new(|work| work());
    }
    fn files(&self) -> Vec<PathBuf> {
        self.files.lock().unwrap().clone()
    }
}

/// The folders in the cache's transfer folder whose names start with `prefix`.
fn cache_folders(cache: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(children) = fs::read_dir(cache.join(TRANSFER_FOLDER)) else {
        return Vec::new();
    };
    children
        .flatten()
        .filter(|child| child.file_name().to_string_lossy().starts_with(prefix))
        .map(|child| child.path())
        .collect()
}

/// The bytes of every file under `folder`.
fn stored_bytes(folder: &Path) -> u64 {
    fs::read_dir(folder).map_or(0, |children| {
        children
            .flatten()
            .map(|child| match child.file_type() {
                Ok(kind) if kind.is_dir() => stored_bytes(&child.path()),
                _ => child.metadata().map_or(0, |metadata| metadata.len()),
            })
            .sum()
    })
}

/// A file of `size` bytes named like a copied photo, different for each `copy`.
fn copied_file(source: &Path, copy: u8, size: usize) -> PathBuf {
    let folder = source.join(format!("copy-{copy}"));
    fs::create_dir(&folder).unwrap();
    let file = folder.join("photo.jpg");
    fs::write(&file, vec![copy; size]).unwrap();
    file
}

fn limits(transfers: usize, bytes: u64) -> ClipboardLimits {
    ClipboardLimits {
        transfers,
        bytes,
        ..CLIPBOARD_LIMITS
    }
}

/// Copy after copy from the peer leaves the newest two clipboard batches in
/// the cache, the one on the clipboard among them, and also any older batch
/// the clipboard points into again, on both the viewer and the agent.
#[test]
fn repeated_clipboard_copies_keep_only_the_newest_batches() {
    const SIZE: usize = 50_000;
    for role in [Role::Viewer, Role::Agent] {
        let source = Sandbox::new();
        let cache = Sandbox::new();
        let clipboard = FakeClipboard::default();
        let mut desktop = desktop(&cache.0, &cache.0, None, no_picker());
        clipboard.on(&mut desktop);
        let (mut worker, outgoing, _) = worker_as(role, desktop);
        let mut first = None;
        for copy in 1..=5u8 {
            let file = copied_file(&source.0, copy, SIZE);
            let messages = transfer(100 + copy as u64, vec![file], FileDestination::Clipboard);
            receive(&mut worker, &outgoing, messages);

            let on_clipboard = clipboard.files();
            assert_eq!(on_clipboard.len(), 1);
            assert_eq!(fs::read(&on_clipboard[0]).unwrap(), vec![copy; SIZE]);
            let batch = on_clipboard[0].parent().unwrap().to_owned();
            if copy == 1 {
                // The user copies the received photo again, in Explorer.
                *clipboard.pinned.lock().unwrap() = Some(on_clipboard.clone());
                first = Some(batch.clone());
            }
            let batches = cache_folders(&cache.0, "transfer-");
            assert!(batches.contains(&batch), "{role:?} copy {copy}");
            assert!(
                batches.contains(first.as_ref().unwrap()),
                "{role:?} copy {copy}: the batch the clipboard points into stays"
            );
            let expected = (copy as usize).min(2) + usize::from(copy > 2);
            assert_eq!(batches.len(), expected, "{role:?} copy {copy}: {batches:?}");
            assert_eq!(
                stored_bytes(&cache.0.join(TRANSFER_FOLDER)),
                (expected * SIZE) as u64
            );
        }
        // Once the clipboard moves on, the old batch goes with the next copy.
        *clipboard.pinned.lock().unwrap() = None;
        let file = copied_file(&source.0, 6, SIZE);
        receive(
            &mut worker,
            &outgoing,
            transfer(106, vec![file], FileDestination::Clipboard),
        );
        let batches = cache_folders(&cache.0, "transfer-");
        assert_eq!(batches.len(), 2, "{role:?}: {batches:?}");
        assert!(!batches.contains(first.as_ref().unwrap()));
        assert!(cache_folders(&cache.0, ".partial-").is_empty());
    }
}

/// Past the number of copies its budget allows, the viewer refuses the
/// peer's next copy when it begins: the peer learns why, the technician's
/// status line says the copied files were not fetched, and the messages
/// that follow write nothing and replace nothing on the clipboard.
#[test]
fn clipboard_copies_past_the_count_budget_are_refused_when_they_begin() {
    let source = Sandbox::new();
    let cache = Sandbox::new();
    let clipboard = FakeClipboard::default();
    let mut desktop = desktop(&cache.0, &cache.0, None, no_picker());
    clipboard.on(&mut desktop);
    desktop.create_file = Some(counted);
    let (mut worker, outgoing, status) = worker_as(Role::Viewer, desktop);
    worker.admission = Admission::with_limits(Role::Viewer, limits(2, u64::MAX));
    OPENED.set(0);
    for copy in 1..=2u8 {
        let file = copied_file(&source.0, copy, 1000);
        let messages = transfer(copy as u64, vec![file], FileDestination::Clipboard);
        receive(&mut worker, &outgoing, messages);
    }
    let on_clipboard = clipboard.files();
    assert_eq!(OPENED.get(), 2);

    let file = copied_file(&source.0, 3, 1000);
    for message in transfer(3, vec![file], FileDestination::Clipboard) {
        worker.step(Some(Command::Peer(message))).unwrap();
    }
    match outgoing.try_recv().unwrap() {
        FileMessage::Error { id: 3, reason } => {
            assert!(reason.contains("at most 2 times in a minute"), "{reason}");
            assert!(reason.contains("Receive files"), "{reason}");
        }
        message => panic!("{message:?}"),
    }
    assert!(outgoing.try_recv().is_err(), "nothing else is answered");
    let notice = status.lock().unwrap().clone();
    assert!(
        notice.starts_with("Files copied on the remote device were not fetched"),
        "{notice}"
    );
    assert_eq!(OPENED.get(), 2, "no file of the refused copy was opened");
    assert_eq!(clipboard.files(), on_clipboard);
    assert_eq!(clipboard.set.load(Ordering::SeqCst), 2);
    assert_eq!(cache_folders(&cache.0, "transfer-").len(), 2);
    assert!(cache_folders(&cache.0, ".partial-").is_empty());
}

/// A copy that would take the viewer past its byte budget, or below its
/// free-space floor, is refused at its totals, before any file is opened,
/// and its staging folder is removed.
#[test]
fn clipboard_copies_past_the_byte_budget_or_free_space_are_refused_at_their_totals() {
    let source = Sandbox::new();
    let first = copied_file(&source.0, 1, 100_000);
    let second = copied_file(&source.0, 2, 100_000);
    let low_space = ClipboardLimits {
        free_floor: u64::MAX,
        ..CLIPBOARD_LIMITS
    };
    for (limits, accepted, expected) in [
        (limits(10, 150_000), 1, "at most 1 MiB of files copied here"),
        (low_space, 0, "low on disk space"),
    ] {
        let cache = Sandbox::new();
        let clipboard = FakeClipboard::default();
        let mut desktop = desktop(&cache.0, &cache.0, None, no_picker());
        clipboard.on(&mut desktop);
        desktop.create_file = Some(counted);
        let (mut worker, outgoing, status) = worker_as(Role::Viewer, desktop);
        worker.admission = Admission::with_limits(Role::Viewer, limits);
        OPENED.set(0);
        let files = [first.clone(), second.clone()];
        for (i, file) in files[..accepted].iter().enumerate() {
            let messages = transfer(
                10 + i as u64,
                vec![file.clone()],
                FileDestination::Clipboard,
            );
            receive(&mut worker, &outgoing, messages);
        }
        let refused = transfer(
            20,
            vec![files[accepted].clone()],
            FileDestination::Clipboard,
        );
        for message in refused {
            worker.step(Some(Command::Peer(message))).unwrap();
        }
        assert_eq!(outgoing.try_recv().unwrap(), FileMessage::Ack { id: 20 });
        match outgoing.try_recv().unwrap() {
            FileMessage::Error { id: 20, reason } => {
                assert!(reason.contains(expected), "{reason}");
            }
            message => panic!("{message:?}"),
        }
        assert!(outgoing.try_recv().is_err(), "nothing else is answered");
        assert!(
            status
                .lock()
                .unwrap()
                .starts_with("Files copied on the remote device were not fetched")
        );
        assert_eq!(
            OPENED.get(),
            accepted,
            "no file of the refused copy was opened"
        );
        assert_eq!(clipboard.set.load(Ordering::SeqCst), accepted);
        assert_eq!(cache_folders(&cache.0, "transfer-").len(), accepted);
        assert!(cache_folders(&cache.0, ".partial-").is_empty());
    }
}

/// The clipboard budget is the viewer's alone and only for clipboard
/// copies: files the technician asked for with Receive files, and copies the
/// agent receives from the technician, are taken whatever it says.
#[test]
fn documents_and_the_agent_are_outside_the_clipboard_budget() {
    let source = Sandbox::new();
    let file = copied_file(&source.0, 1, 100_000);
    let tiny = limits(1, 1);

    let documents = Sandbox::new();
    let (mut viewer, outgoing, _) = worker_as(
        Role::Viewer,
        desktop(&documents.0, &documents.0, None, no_picker()),
    );
    viewer.admission = Admission::with_limits(Role::Viewer, tiny);
    for id in 1..=3 {
        viewer.step(Some(Command::RequestPeerPick)).unwrap();
        assert_eq!(outgoing.try_recv().unwrap(), FileMessage::Pick);
        let messages = transfer(id, vec![file.clone()], FileDestination::Documents);
        receive(&mut viewer, &outgoing, messages);
    }
    let saved = documents.0.join(TRANSFER_FOLDER);
    assert_eq!(fs::read_dir(&saved).unwrap().count(), 3);

    let cache = Sandbox::new();
    let clipboard = FakeClipboard::default();
    let mut desktop = desktop(&cache.0, &cache.0, None, no_picker());
    clipboard.on(&mut desktop);
    let (mut agent, outgoing, _) = worker(desktop);
    agent.admission = Admission::with_limits(Role::Agent, tiny);
    for id in 1..=3 {
        let messages = transfer(id, vec![file.clone()], FileDestination::Clipboard);
        receive(&mut agent, &outgoing, messages);
    }
    assert_eq!(clipboard.set.load(Ordering::SeqCst), 3);
}
