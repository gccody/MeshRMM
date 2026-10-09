//! Reading the helpers' events and passing them to the session.
use super::*;

/// Whether a helper of `kind` sends `event`, matching the child run loops. The
/// Files and Clipboard helpers run with the user's token, so the parent treats
/// any other event as a protocol error instead of trusting it.
pub(super) fn helper_sends(kind: HelperKind, event: &ChildEvent) -> bool {
    match event {
        ChildEvent::InputStarted | ChildEvent::Error(_) | ChildEvent::Stopped => true,
        ChildEvent::Cursor(..)
        | ChildEvent::MaintenanceState { .. }
        | ChildEvent::CredentialPrompt(_) => kind == HelperKind::Input,
        // The file helper reports wallpaper failures.
        ChildEvent::MaintenanceError(_) => matches!(kind, HelperKind::Input | HelperKind::Files),
        ChildEvent::Credentials(_) => matches!(kind, HelperKind::Input | HelperKind::Chat),
        ChildEvent::Files(_) => kind == HelperKind::Files,
        ChildEvent::Clipboard(_) => kind == HelperKind::Clipboard,
        ChildEvent::Chat(_) => kind == HelperKind::Chat,
        // Only the approval helper sends it, and it has its own reader.
        ChildEvent::Started(_)
        | ChildEvent::Frame(_)
        | ChildEvent::NoDisplays
        | ChildEvent::ApprovalDecision(_) => false,
    }
}

pub(super) fn child_event_name(event: &ChildEvent) -> &'static str {
    match event {
        ChildEvent::Credentials(_) => "credential result",
        ChildEvent::CredentialPrompt(_) => "credential detection",
        ChildEvent::Files(_) => "file transfer",
        ChildEvent::Started(_) => "video start",
        ChildEvent::InputStarted => "start",
        ChildEvent::MaintenanceState { .. } => "maintenance state",
        ChildEvent::MaintenanceError(_) => "maintenance error",
        ChildEvent::Frame(_) => "video frame",
        ChildEvent::Cursor(..) => "cursor",
        ChildEvent::Clipboard(_) => "clipboard",
        ChildEvent::Chat(_) => "chat",
        ChildEvent::ApprovalDecision(_) => "approval decision",
        ChildEvent::NoDisplays => "no displays",
        ChildEvent::Error(_) => "error",
        ChildEvent::Stopped => "stop",
    }
}

pub(super) fn dispatch_child_events(
    output: impl Read,
    sink: Arc<Mutex<Option<EncodedFrameSink>>>,
    started_tx: mpsc::Sender<Result<StartedDesktop, StartFailure>>,
    status: HelperStatus,
    cursor: HelperCursor,
    maintenance: HelperMaintenance,
    last_frame: Arc<Mutex<Instant>>,
) {
    let mut output = BufReader::new(output);
    loop {
        match read_event(&mut output) {
            Ok(ChildEvent::Started(started)) => {
                *last_frame.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                if started_tx.send(Ok(started)).is_err() {
                    break;
                }
            }
            Ok(ChildEvent::InputStarted) => {
                let message = "capture helper reported input-only startup".to_string();
                let _ = started_tx.send(Err(StartFailure::Failed(message.clone())));
                set_status(&status, Err(message));
                break;
            }
            Ok(ChildEvent::Frame(frame)) => {
                *last_frame.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                if let Some(sink) = sink
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    (sink)(frame);
                }
            }
            Ok(ChildEvent::Cursor(shape, viewer_controls_input, pointer_display)) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) =
                    (shape, viewer_controls_input, pointer_display);
            }
            Ok(ChildEvent::MaintenanceError(reason)) => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceError { reason });
            }
            Ok(
                ChildEvent::Credentials(_)
                | ChildEvent::CredentialPrompt(_)
                | ChildEvent::MaintenanceState { .. }
                | ChildEvent::Files(_)
                | ChildEvent::Clipboard(_)
                | ChildEvent::Chat(_)
                | ChildEvent::ApprovalDecision(_),
            ) => {
                set_status(
                    &status,
                    Err("capture helper reported an input-only clipboard event".into()),
                );
                break;
            }
            Ok(ChildEvent::NoDisplays) => {
                let _ = started_tx.send(Err(StartFailure::NoDisplays));
                set_status(&status, Err(NoDisplays.to_string()));
                break;
            }
            Ok(ChildEvent::Error(message)) => {
                let _ = started_tx.send(Err(StartFailure::Failed(message.clone())));
                set_status(&status, Err(message));
                break;
            }
            Ok(ChildEvent::Stopped) => {
                let _ = started_tx.send(Err(StartFailure::Failed("desktop helper stopped".into())));
                set_status(&status, Ok(()));
                break;
            }
            Err(error) => {
                let message = format!("desktop-helper IPC failed: {error}");
                let _ = started_tx.send(Err(StartFailure::Failed(message.clone())));
                set_status(&status, Err(message));
                break;
            }
        }
    }
}

// Each event destination is shared independently with the parent session.
#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch_input_events(
    output: File,
    started_tx: mpsc::SyncSender<Result<(), String>>,
    status: HelperStatus,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
    kind: HelperKind,
) {
    let mut output = BufReader::new(output);
    let mut started_tx = Some(started_tx);
    let fail = |started_tx: &mut Option<mpsc::SyncSender<Result<(), String>>>, message: String| {
        if let Some(sender) = started_tx.take() {
            let _ = sender.send(Err(message.clone()));
        }
        set_status(&status, Err(message));
    };
    loop {
        let event = match read_helper_event(&mut output, kind) {
            Ok(event) => event,
            Err(message) => {
                fail(&mut started_tx, message);
                break;
            }
        };
        match event {
            ChildEvent::InputStarted => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Ok(()));
                } else {
                    set_status(
                        &status,
                        Err("desktop input helper sent duplicate start event".into()),
                    );
                    break;
                }
            }
            ChildEvent::Credentials(result) => {
                if !store_credential_result(&credentials, kind, result) {
                    set_status(&status, Err("unexpected credential result".into()));
                    break;
                }
            }
            ChildEvent::CredentialPrompt(ready) => {
                credentials.lock().unwrap().state.can_autofill = ready;
            }
            ChildEvent::MaintenanceError(reason) => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceError { reason });
            }
            ChildEvent::MaintenanceState {
                agent_input_blocked,
                blacked_out,
            } => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceState {
                        agent_input_blocked,
                        blacked_out,
                    });
            }
            ChildEvent::Cursor(shape, viewer_controls_input, pointer_display) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) =
                    (shape, viewer_controls_input, pointer_display);
            }
            ChildEvent::Files(message) => queue_file_message(&files, message),
            ChildEvent::Chat(text) => queue_chat_text(&chat, text),
            ChildEvent::Clipboard(text) => {
                *clipboard
                    .latest
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(text);
                clipboard.ready.notify_one();
            }
            ChildEvent::Error(message) => {
                fail(&mut started_tx, message);
                break;
            }
            ChildEvent::Stopped => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err("desktop input helper stopped before startup".into()));
                }
                set_status(&status, Ok(()));
                break;
            }
            // helper_sends rejects video and approval events before this match.
            ChildEvent::Started(_)
            | ChildEvent::Frame(_)
            | ChildEvent::NoDisplays
            | ChildEvent::ApprovalDecision(_) => {
                fail(
                    &mut started_tx,
                    "desktop input helper reported a video event".into(),
                );
                break;
            }
        }
    }
}

/// Reads the helper's next event, or why the helper must stop.
fn read_helper_event(output: &mut impl Read, kind: HelperKind) -> Result<ChildEvent, String> {
    match read_event(output) {
        Ok(event) if helper_sends(kind, &event) => Ok(event),
        Ok(event) => {
            tracing::warn!(
                helper_kind = ?kind,
                event = child_event_name(&event),
                "desktop helper sent an event it never sends; stopping it"
            );
            Err(format!(
                "desktop {kind:?} helper sent an unexpected {} event",
                child_event_name(&event)
            ))
        }
        Err(error) => Err(format!("desktop input-helper IPC failed: {error}")),
    }
}

/// Records a credential result and saves the credentials it carries.
/// Returns false, recording nothing, when credentials arrive from anything
/// but a prompt the chat helper is showing.
fn store_credential_result(
    credentials: &HelperCredentials,
    kind: HelperKind,
    result: CredentialResult,
) -> bool {
    let mut current = credentials.lock().unwrap();
    if result.encrypted.is_some() && (kind != HelperKind::Chat || !current.state.prompt_active) {
        return false;
    }
    current.state.message = result.message;
    if let Some(encrypted) = result.encrypted {
        match crate::remote::credentials::save(&current.store, &encrypted) {
            Ok(()) => current.state.saved = true,
            Err(error) => {
                current.state.message =
                    format!("Validated, but could not save credentials: {error:#}")
            }
        }
    }
    if kind == HelperKind::Chat {
        current.state.prompt_active = false;
    }
    true
}

fn queue_file_message(files: &HelperFiles, message: meshrmm_protocol::FileMessage) {
    // A window of the helper's transfer plus its acknowledgements
    // of the viewer's fits; the windows keep it from growing further.
    let mut queue = files.queue.lock().unwrap();
    if queue.len() < meshrmm_file_transfer::COMMAND_QUEUE {
        queue.push_back(message);
        files.ready.notify_one();
    } else {
        tracing::warn!("dropped a file-transfer message from the desktop helper");
    }
}

fn queue_chat_text(chat: &HelperChat, text: String) {
    let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
    if queue.len() < 32 {
        queue.push_back(text);
        chat.ready.notify_one();
    }
}

pub(super) fn set_status(status: &HelperStatus, value: Result<(), String>) {
    let mut status = status.lock().unwrap_or_else(|error| error.into_inner());
    if status.is_none() {
        *status = Some(value);
    }
}
