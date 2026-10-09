use super::*;

struct Dispatched {
    startup: Result<(), String>,
    status: Option<Result<(), String>>,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
}

fn dispatch(kind: HelperKind, events: &[ChildEvent]) -> Dispatched {
    let (read, write) = create_pipe().unwrap();
    let (started, startup) = mpsc::sync_channel(1);
    let status: HelperStatus = Arc::new(Mutex::new(None));
    let cursor: HelperCursor = Arc::new(Mutex::new((CursorShape::Default, false, None)));
    let clipboard = Arc::new(ClipboardEvents::default());
    let files = Arc::new(FileEvents::default());
    let chat = Arc::new(ChatEvents::default());
    let maintenance: HelperMaintenance = Arc::new(Mutex::new(None));
    let credentials = Arc::new(Mutex::new(Credentials::default()));
    let reader = {
        let (status, cursor, clipboard, files, chat, maintenance, credentials) = (
            status.clone(),
            cursor.clone(),
            clipboard.clone(),
            files.clone(),
            chat.clone(),
            maintenance.clone(),
            credentials.clone(),
        );
        thread::spawn(move || {
            dispatch_input_events(
                read.into_file(),
                started,
                status,
                cursor,
                clipboard,
                files,
                chat,
                maintenance,
                credentials,
                kind,
            )
        })
    };
    let mut writer = write.into_file();
    for event in events {
        // The reader may already have stopped after a rejected event.
        if write_event(&mut writer, event).is_err() {
            break;
        }
    }
    drop(writer);
    reader.join().unwrap();
    let status = status.lock().unwrap().take();
    Dispatched {
        startup: startup.recv().unwrap(),
        status,
        cursor,
        clipboard,
        files,
        chat,
        maintenance,
        credentials,
    }
}

#[tokio::test]
async fn helper_pipe_notifies_services_and_coalesces_clipboard_changes() {
    let clipboard = dispatch(
        HelperKind::Clipboard,
        &[
            ChildEvent::InputStarted,
            ChildEvent::Clipboard(ClipboardContent::Text("first".into())),
            ChildEvent::Clipboard(ClipboardContent::Text("latest".into())),
            ChildEvent::Stopped,
        ],
    );
    let chat = dispatch(
        HelperKind::Chat,
        &[
            ChildEvent::InputStarted,
            ChildEvent::Chat("hello".into()),
            ChildEvent::Stopped,
        ],
    );
    let files = dispatch(
        HelperKind::Files,
        &[
            ChildEvent::InputStarted,
            ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
            ChildEvent::Stopped,
        ],
    );
    for helper in [&clipboard, &chat, &files] {
        assert_eq!(helper.startup, Ok(()));
        assert_eq!(helper.status, Some(Ok(())));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        clipboard.clipboard.ready.notified().await;
        files.files.ready.notified().await;
        chat.chat.ready.notified().await;
    })
    .await
    .unwrap();
    assert_eq!(
        clipboard.clipboard.latest.lock().unwrap().take(),
        Some(ClipboardContent::Text("latest".into()))
    );
    assert_eq!(
        chat.chat.queue.lock().unwrap().pop_front().as_deref(),
        Some("hello")
    );
    assert!(matches!(
        files.files.queue.lock().unwrap().pop_front(),
        Some(meshrmm_protocol::FileMessage::Available)
    ));
    assert!(clipboard.clipboard.latest.lock().unwrap().is_none());
}

#[test]
fn helpers_are_trusted_only_for_events_their_run_loops_send() {
    use HelperKind::*;
    let events = [
        ChildEvent::InputStarted,
        ChildEvent::Error("failed".into()),
        ChildEvent::Stopped,
        ChildEvent::Cursor(CursorShape::Default, true, None),
        ChildEvent::MaintenanceState {
            agent_input_blocked: false,
            blacked_out: false,
        },
        ChildEvent::CredentialPrompt(true),
        ChildEvent::MaintenanceError("wallpaper".into()),
        ChildEvent::Credentials(CredentialResult {
            encrypted: None,
            message: "done".into(),
        }),
        ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
        ChildEvent::Clipboard(ClipboardContent::Text("copied".into())),
        ChildEvent::Chat("hello".into()),
        ChildEvent::ApprovalDecision(crate::remote::connection_approval::Decision::Accepted),
    ];
    let expected: [&[HelperKind]; 12] = [
        &[Input, Files, Clipboard, Chat, Notification],
        &[Input, Files, Clipboard, Chat, Notification],
        &[Input, Files, Clipboard, Chat, Notification],
        &[Input],
        &[Input],
        &[Input],
        &[Input, Files],
        &[Input, Chat],
        &[Files],
        &[Clipboard],
        &[Chat],
        // Only the approval helper, which has its own reader.
        &[],
    ];
    for (event, senders) in events.iter().zip(expected) {
        for kind in [Input, Files, Clipboard, Chat, Notification] {
            assert_eq!(
                helper_sends(kind, event),
                senders.contains(&kind),
                "{kind:?} helper, {} event",
                child_event_name(event)
            );
        }
    }
}

#[test]
fn user_token_helpers_cannot_forge_other_helpers_events() {
    let forged = [
        (
            HelperKind::Clipboard,
            ChildEvent::Chat("forged chat".into()),
        ),
        (
            HelperKind::Clipboard,
            ChildEvent::MaintenanceError("forged".into()),
        ),
        (
            HelperKind::Files,
            ChildEvent::Cursor(CursorShape::Text, true, None),
        ),
        (
            HelperKind::Files,
            ChildEvent::MaintenanceState {
                agent_input_blocked: true,
                blacked_out: true,
            },
        ),
        (
            HelperKind::Files,
            ChildEvent::Clipboard(ClipboardContent::Text("forged".into())),
        ),
        (HelperKind::Files, ChildEvent::CredentialPrompt(true)),
        (
            HelperKind::Clipboard,
            ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
        ),
        (
            HelperKind::Chat,
            ChildEvent::Clipboard(ClipboardContent::Text("forged".into())),
        ),
        (HelperKind::Input, ChildEvent::Chat("forged chat".into())),
    ];
    for (kind, event) in forged {
        let name = child_event_name(&event);
        let result = dispatch(
            kind,
            &[
                ChildEvent::InputStarted,
                event,
                ChildEvent::Chat("after".into()),
                ChildEvent::Stopped,
            ],
        );
        assert_eq!(result.startup, Ok(()));
        let Some(Err(message)) = result.status else {
            panic!("{kind:?} helper's {name} event was accepted");
        };
        assert!(message.contains("unexpected"), "{message}");
        assert_eq!(
            *result.cursor.lock().unwrap(),
            (CursorShape::Default, false, None)
        );
        assert!(result.clipboard.latest.lock().unwrap().is_none());
        assert!(result.files.queue.lock().unwrap().is_empty());
        assert!(result.chat.queue.lock().unwrap().is_empty());
        assert!(result.maintenance.lock().unwrap().is_none());
        assert!(!result.credentials.lock().unwrap().state.can_autofill);
    }
}

#[test]
fn stderr_lines_are_bounded_without_losing_the_next_line() {
    let mut input = Vec::new();
    input.extend(std::iter::repeat_n(b'x', 10_000));
    input.extend_from_slice(b"\r\nnext\npartial");
    // A small buffer exercises lines that span several reads.
    let mut reader = BufReader::with_capacity(7, &input[..]);
    let mut line = Vec::new();
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, 16).unwrap(),
        Some(true)
    );
    assert_eq!(line, vec![b'x'; 16]);
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, 16).unwrap(),
        Some(false)
    );
    assert_eq!(line, b"next");
    assert_eq!(
        read_bounded_line(&mut reader, &mut line, 16).unwrap(),
        Some(false)
    );
    assert_eq!(line, b"partial");
    assert_eq!(read_bounded_line(&mut reader, &mut line, 16).unwrap(), None);
}

#[test]
fn stderr_budget_suppresses_floods_and_reports_them_per_window() {
    let start = Instant::now();
    let window = Duration::from_secs(60);
    let mut budget = LineBudget::new(2, window, start);
    assert_eq!(budget.admit(start), (true, 0));
    assert_eq!(budget.admit(start), (true, 0));
    assert_eq!(budget.admit(start), (false, 0));
    assert_eq!(budget.admit(start + Duration::from_secs(59)), (false, 0));
    assert_eq!(budget.admit(start + window), (true, 2));
    assert_eq!(budget.admit(start + window), (true, 0));
    assert_eq!(budget.admit(start + window), (false, 0));
    assert_eq!(budget.take_suppressed(), 1);
    assert_eq!(budget.take_suppressed(), 0);
}

#[test]
fn forged_event_before_start_fails_startup() {
    let result = dispatch(
        HelperKind::Clipboard,
        &[ChildEvent::Files(meshrmm_protocol::FileMessage::Available)],
    );
    assert!(result.startup.is_err());
    assert!(matches!(result.status, Some(Err(_))));
}
