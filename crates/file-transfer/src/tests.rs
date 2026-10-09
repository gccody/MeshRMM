use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io,
    path::Path,
};

use super::receive::{move_unique, rename_no_replace};
use super::send::send_file;
use super::sweep::{sweep_batches, sweep_partials};
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
    viewer.request_peer_pick(start);
    viewer.close_peer_pick();
    assert!(
        viewer
            .admit(&FileDestination::Documents, true, start)
            .is_err(),
        "a request the peer answered without files admits nothing"
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
        let mut receiver = Incoming::new(id, FileDestination::Clipboard, target.0.clone()).unwrap();
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
