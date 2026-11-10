use std::sync::Arc;

use anyhow::Context;
use meshrmm_protocol::{Annotation, FileMessage, RemoteSessionId, SessionMessage, TogglePolicy};
use meshrmm_session_transport::{
    CHAT_CHANNEL, CLIPBOARD_CHANNEL, FILE_CHANNEL, ServiceChannel, ServiceRoute,
};
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::control_channel::send_control_message;
use super::typing::{InputCommand, InputWorker};
use super::{ControlCommand, SenderCleanup};
use crate::remote::native_task::{NativeTask, command_worker};
use crate::remote::platform::ScreenInput;

/// Queues of the workers that apply viewer messages off the control channel.
#[derive(Clone)]
pub(super) struct WorkerQueues {
    pub(super) input: mpsc::Sender<InputCommand>,
    pub(super) maintenance: mpsc::Sender<SessionMessage>,
    pub(super) annotation: mpsc::Sender<Annotation>,
    pub(super) clipboard: mpsc::Sender<SessionMessage>,
    pub(super) chat: mpsc::Sender<SessionMessage>,
    pub(super) files: mpsc::Sender<FileMessage>,
}

pub(super) struct SessionWorkers {
    pub(super) queues: WorkerQueues,
    pub(super) control_service: ServiceChannel,
}

pub(super) async fn spawn_session_workers(
    input: &Arc<dyn ScreenInput>,
    control_channel: &Arc<RTCDataChannel>,
    control_tx: &mpsc::UnboundedSender<ControlCommand>,
    session_id: &RemoteSessionId,
    idle_policy: TogglePolicy,
    cleanup: &mut SenderCleanup,
) -> anyhow::Result<(SessionWorkers, [(&'static str, Arc<ServiceRoute>); 3])> {
    let (input_tx, input_task) = spawn_input_worker(
        Arc::clone(input),
        Arc::clone(control_channel),
        control_tx.clone(),
    )?;
    cleanup.workers.push(input_task);
    let (maintenance_tx, maintenance_task) = spawn_maintenance_worker(
        Arc::clone(input),
        control_tx.clone(),
        session_id.clone(),
        idle_policy,
    )?;
    cleanup.workers.push(maintenance_task);
    let (annotation_tx, annotation_task) =
        spawn_annotation_worker(Arc::clone(input), control_tx.clone())?;
    cleanup.workers.push(annotation_task);
    let control_service = ServiceChannel::new(control_channel.clone()).await;
    let clipboard_route = Arc::new(ServiceRoute::default());
    let file_route = Arc::new(ServiceRoute::default());
    let chat_route = Arc::new(ServiceRoute::default());
    let (clipboard_tx, clipboard_task) = spawn_clipboard_worker(
        Arc::clone(input),
        control_service.clone(),
        Some(clipboard_route.clone()),
    )?;
    cleanup.workers.push(clipboard_task);
    let (chat_tx, chat_task) = spawn_chat_worker(
        Arc::clone(input),
        control_service.clone(),
        Some(chat_route.clone()),
    )?;
    cleanup.workers.push(chat_task);
    let (files_tx, files_task) = spawn_file_worker(
        Arc::clone(input),
        control_service.clone(),
        Some(file_route.clone()),
    )?;
    cleanup.workers.push(files_task);
    let workers = SessionWorkers {
        queues: WorkerQueues {
            input: input_tx,
            maintenance: maintenance_tx,
            annotation: annotation_tx,
            clipboard: clipboard_tx,
            chat: chat_tx,
            files: files_tx,
        },
        control_service,
    };
    let routes = [
        (CLIPBOARD_CHANNEL, clipboard_route),
        (FILE_CHANNEL, file_route),
        (CHAT_CHANNEL, chat_route),
    ];
    Ok((workers, routes))
}

fn spawn_maintenance_worker(
    maintenance_input: Arc<dyn ScreenInput>,
    maintenance_errors: mpsc::UnboundedSender<ControlCommand>,
    restart_session: RemoteSessionId,
    idle_policy: TogglePolicy,
) -> anyhow::Result<(mpsc::Sender<SessionMessage>, NativeTask)> {
    let cleanup_input = Arc::clone(&maintenance_input);
    Ok(command_worker(
        "meshrmm-maintenance",
        32,
        std::time::Duration::from_secs(3600),
        move |message| {
            let result = match message {
                Some(
                    message @ (SessionMessage::PromptForCredentials
                    | SessionMessage::AutofillCredentials
                    | SessionMessage::ForgetCredentials),
                ) => maintenance_input.credential_command(message),
                Some(SessionMessage::SendSecureAttention) => {
                    if !maintenance_input.is_console_session() {
                        Err(anyhow::anyhow!(
                            "Ctrl+Alt+Del is only available for the console session"
                        ))
                    } else {
                        crate::remote::secure_attention::send()
                    }
                }
                Some(SessionMessage::SetPreventIdleLock { enabled }) => {
                    maintenance_input.set_prevent_idle_lock(idle_policy.effective(Some(enabled)))
                }
                Some(SessionMessage::SetWallpaperHidden { hidden }) => {
                    maintenance_input.set_wallpaper_hidden(hidden)
                }
                Some(SessionMessage::SetBlackout { enabled }) => {
                    maintenance_input.set_blackout(enabled)
                }
                Some(SessionMessage::SetAgentInputBlocked { blocked }) => {
                    maintenance_input.set_agent_input_blocked(blocked)
                }
                Some(SessionMessage::Restart { safe_mode }) => {
                    crate::remote::connection_approval::remember_across_restart(&restart_session)
                        .and_then(|()| crate::power::restart(safe_mode))
                        .context("Restart")
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                let _ =
                    maintenance_errors.send(ControlCommand::MaintenanceError(format!("{error:#}")));
            }
        },
        move || {
            let _ = cleanup_input.set_prevent_idle_lock(false);
            let _ = cleanup_input.set_wallpaper_hidden(false);
            let _ = cleanup_input.set_blackout(false);
            let _ = cleanup_input.set_agent_input_blocked(false);
        },
    )?)
}

// Its own queue: a slow overlay never holds up input or maintenance.
fn spawn_annotation_worker(
    annotation_input: Arc<dyn ScreenInput>,
    annotation_errors: mpsc::UnboundedSender<ControlCommand>,
) -> anyhow::Result<(mpsc::Sender<Annotation>, NativeTask)> {
    let cleanup_annotations = Arc::clone(&annotation_input);
    Ok(command_worker(
        "meshrmm-annotation",
        1024,
        std::time::Duration::from_secs(3600),
        move |annotation| {
            if let Some(annotation) = annotation
                && let Err(error) = annotation_input.annotate(annotation)
            {
                let _ = annotation_errors.send(ControlCommand::MaintenanceError(format!(
                    "Annotate: {error:#}"
                )));
            }
        },
        move || {
            let _ = cleanup_annotations.annotate(Annotation::Clear);
        },
    )?)
}

pub(super) fn spawn_file_worker(
    input: Arc<dyn crate::remote::platform::ScreenInput>,
    channel: ServiceChannel,
    route: Option<Arc<ServiceRoute>>,
) -> anyhow::Result<(
    mpsc::Sender<meshrmm_protocol::FileMessage>,
    crate::remote::native_task::NativeTask,
)> {
    let (sender, mut commands) = mpsc::channel(meshrmm_file_transfer::COMMAND_QUEUE);
    let task = crate::remote::native_task::NativeTask::spawn(
        "meshrmm-files",
        move |mut stop| async move {
            let channel = if let Some(route) = route {
                tokio::select! { channel = route.resolve(channel) => match channel { Ok(channel) => channel, Err(error) => { tracing::warn!(%error, "service route unavailable"); return; } }, _ = stop.changed() => return }
            } else {
                channel
            };
            let ready = input.files_ready();
            let mut pending = true;
            let mut sending = Sending::default();
            // A transfer the channel holds up is not ended here: the file helper fails it when
            // the peer has not acknowledged it for its own limit, and reports that through
            // this worker like any other message.
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    () = channel.closed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        if let Err(error) = input.apply_files(message) { tracing::warn!(%error, "file helper unavailable"); }
                    }
                    _ = ready.notified() => pending = true,
                    sent = sending.finished(), if sending.busy() => {
                        if let Err(error) = sent { tracing::warn!(%error, "file-transfer send failed"); break; }
                    }
                    capacity = channel.writable(), if !sending.busy() && pending && channel.ready_state() == RTCDataChannelState::Open => {
                        if let Err(error) = capacity { tracing::warn!(%error, "file channel unavailable"); break; }
                        if let Some(message) = input.poll_files() {
                            sending.start(&channel, SessionMessage::FileTransfer(message));
                        } else { pending = false; }
                    }
                }
            }
        },
    )?;
    Ok((sender, task))
}

pub(super) fn spawn_chat_worker(
    input: Arc<dyn crate::remote::platform::ScreenInput>,
    channel: ServiceChannel,
    route: Option<Arc<ServiceRoute>>,
) -> anyhow::Result<(
    mpsc::Sender<SessionMessage>,
    crate::remote::native_task::NativeTask,
)> {
    let (sender, mut commands) = mpsc::channel(64);
    let task = crate::remote::native_task::NativeTask::spawn(
        "meshrmm-chat",
        move |mut stop| async move {
            let channel = if let Some(route) = route {
                tokio::select! { channel = route.resolve(channel) => match channel { Ok(channel) => channel, Err(error) => { tracing::warn!(%error, "service route unavailable"); return; } }, _ = stop.changed() => return }
            } else {
                channel
            };
            let ready = input.chat_ready();
            // Whether the viewer is owed a ChatAvailable, and whether the chat window may have
            // text to send. Text stays in the chat helper until the channel has room for it.
            let mut announce = false;
            let mut pending = false;
            let mut sending = Sending::default();
            let result: anyhow::Result<()> = async {
            loop {
                if *stop.borrow() { break; }
                tokio::select! {
                    _ = stop.changed() => break,
                    () = channel.closed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        match message {
                            SessionMessage::ChatAvailable => {
                                input.start_chat()?;
                                announce = true;
                            }
                            SessionMessage::Chat { text } => input.apply_chat(text)?,
                            _ => {},
                        }
                    }
                    _ = ready.notified() => pending = true,
                    sent = sending.finished(), if sending.busy() => sent?,
                    capacity = channel.writable(), if !sending.busy() && (announce || pending) => {
                        capacity?;
                        if announce {
                            announce = false;
                            sending.start(&channel, SessionMessage::ChatAvailable);
                        } else if let Some(text) = input.poll_chat()? {
                            sending.start(&channel, SessionMessage::Chat { text });
                        } else {
                            pending = false;
                        }
                    }
                }
            }
            Ok(())
        }.await;
            input.stop_chat();
            if let Err(error) = result {
                tracing::warn!(%error, "chat worker stopped");
            }
        },
    )?;
    Ok((sender, task))
}

pub(super) fn spawn_clipboard_worker(
    input: Arc<dyn crate::remote::platform::ScreenInput>,
    channel: ServiceChannel,
    route: Option<Arc<ServiceRoute>>,
) -> anyhow::Result<(
    mpsc::Sender<SessionMessage>,
    crate::remote::native_task::NativeTask,
)> {
    let (sender, mut commands) = mpsc::channel(1024);
    let task = crate::remote::native_task::NativeTask::spawn(
        "meshrmm-clipboard",
        move |mut stop| async move {
            let channel = if let Some(route) = route {
                tokio::select! { channel = route.resolve(channel) => match channel { Ok(channel) => channel, Err(error) => { tracing::warn!(%error, "service route unavailable"); return; } }, _ = stop.changed() => return }
            } else {
                channel
            };
            let ready = input.clipboard_ready();
            let mut receiver = meshrmm_protocol::ClipboardReceiver::default();
            // The rest of the latest copy only: a newer copy, or one from the viewer, replaces
            // it, so a channel without room holds at most one copy and one chunk in a send.
            let mut outgoing = std::collections::VecDeque::new();
            let mut sending = Sending::default();
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    () = channel.closed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        match receiver.receive(message) {
                            Ok(Some(content)) => {
                                outgoing.clear();
                                if let Err(error) = input.apply_clipboard(content) { tracing::warn!(%error, "clipboard apply failed"); }
                            }
                            Ok(None) => {},
                            Err(error) => tracing::warn!(%error, "invalid clipboard payload"),
                        }
                    }
                    _ = async {
                        if let Some(ready) = &ready { ready.notified().await; }
                        else { poll.tick().await; }
                    }, if channel.ready_state() == RTCDataChannelState::Open => {
                        match input.poll_clipboard().and_then(|content| Ok(content.map(|c| c.messages()).transpose()?)) {
                            Ok(Some(messages)) => outgoing = messages.into(),
                            Ok(None) => {},
                            Err(error) => tracing::warn!(%error, "clipboard poll failed"),
                        }
                    }
                    sent = sending.finished(), if sending.busy() => {
                        if let Err(error) = sent { tracing::warn!(%error, "clipboard send failed"); break; }
                    }
                    capacity = channel.writable(), if !sending.busy() && !outgoing.is_empty() => {
                        if let Err(error) = capacity { tracing::warn!(%error, "clipboard channel unavailable"); break; }
                        if let Some(message) = outgoing.pop_front() {
                            sending.start(&channel, message);
                        }
                    }
                }
            }
        },
    )?;
    Ok((sender, task))
}

/// The one message a service worker is handing to its channel. The worker keeps serving its
/// stop signal and its peer's messages beside it, and the send waits for as long as the
/// channel is open: a stall the connection survives delays the service, never ends it.
#[derive(Default)]
struct Sending(Option<std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>>>>>);

impl Sending {
    fn start(&mut self, channel: &ServiceChannel, message: SessionMessage) {
        let channel = channel.clone();
        self.0 = Some(Box::pin(async move {
            meshrmm_session_transport::send(&channel, message).await
        }));
    }

    fn busy(&self) -> bool {
        self.0.is_some()
    }

    /// The send's result once it finishes. Cancel-safe: a send this leaves unfinished
    /// carries on at the next call.
    async fn finished(&mut self) -> anyhow::Result<()> {
        let Some(sending) = self.0.as_mut() else {
            return std::future::pending().await;
        };
        let result = sending.await;
        self.0 = None;
        result
    }
}

/// Starts the worker that applies the viewer's input and types its text a piece at a time,
/// taking the input queued meanwhile between pieces. It also reports the input's state to the
/// viewer.
pub(super) fn spawn_input_worker(
    input: Arc<dyn crate::remote::platform::ScreenInput>,
    channel: Arc<RTCDataChannel>,
    errors: mpsc::UnboundedSender<ControlCommand>,
) -> anyhow::Result<(mpsc::Sender<InputCommand>, NativeTask)> {
    let status_channel = Arc::clone(&channel);
    let (updates, mut pending) = mpsc::channel(8);
    // This task owns no native resources; dropping the worker closes its queue.
    let status_errors = errors.clone();
    tokio::spawn(async move {
        while let Some(message) = pending.recv().await {
            if channel.ready_state() != RTCDataChannelState::Open {
                continue;
            }
            if let Err(error) = send_control_message(&channel, message).await {
                let _ = status_errors.send(ControlCommand::MaintenanceError(format!(
                    "input status: {error:#}"
                )));
                break;
            }
        }
    });
    let (sender, mut commands) = mpsc::channel(1024);
    let task = NativeTask::spawn("meshrmm-input", move |mut stop| async move {
        let mut status = InputStatus::default();
        let mut worker = InputWorker::new(Arc::clone(&input), errors.clone());
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(16));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *stop.borrow() {
                break;
            }
            // In this order: input that arrived while a piece was typed goes first.
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    worker.command(command);
                }
                _ = tick.tick() => {
                    if status_channel.ready_state() == RTCDataChannelState::Open {
                        status.report(input.as_ref(), worker.typing(), &updates, &errors);
                    }
                }
                () = std::future::ready(()), if worker.typing() => worker.type_piece(),
            }
        }
        // Pieces end with their keys released; this releases the viewer's own.
        let _ = input.release_all();
    })?;
    Ok((sender, task))
}

/// The input state the viewer was last told.
#[derive(Default)]
struct InputStatus {
    cursor: Option<meshrmm_protocol::CursorShape>,
    ownership: Option<bool>,
    pointer_display: Option<Option<meshrmm_protocol::DisplayId>>,
    state: Option<SessionMessage>,
    credentials: Option<meshrmm_protocol::CredentialState>,
    typing: Option<bool>,
}

impl InputStatus {
    /// Tells the viewer what changed. What does not fit in `updates` is sent next time.
    fn report(
        &mut self,
        input: &dyn ScreenInput,
        typing: bool,
        updates: &mpsc::Sender<SessionMessage>,
        errors: &mpsc::UnboundedSender<ControlCommand>,
    ) {
        let viewer_controls_input = input.viewer_controls_input();
        if self.ownership != Some(viewer_controls_input)
            && errors
                .send(ControlCommand::InputOwnership(viewer_controls_input))
                .is_ok()
        {
            self.ownership = Some(viewer_controls_input);
        }
        if let Some(next) = input.credential_state()
            && self.credentials.as_ref() != Some(&next)
            && updates
                .try_send(SessionMessage::CredentialState(next.clone()))
                .is_ok()
        {
            self.credentials = Some(next);
        }
        let next_pointer = input.agent_pointer_display();
        if self.pointer_display != Some(next_pointer)
            && updates
                .try_send(SessionMessage::AgentPointerDisplay {
                    display_id: next_pointer,
                })
                .is_ok()
        {
            self.pointer_display = Some(next_pointer);
        }
        let shape = input.cursor_shape();
        if self.cursor != Some(shape)
            && updates
                .try_send(SessionMessage::CursorShape { shape })
                .is_ok()
        {
            self.cursor = Some(shape);
        }
        if let Some(next) = input.maintenance_state()
            && (self.state.as_ref() != Some(&next)
                || matches!(next, SessionMessage::MaintenanceError { .. }))
            && updates.try_send(next.clone()).is_ok()
        {
            self.state = Some(next);
        }
        if self.typing.unwrap_or(false) != typing
            && updates
                .try_send(SessionMessage::Typing { active: typing })
                .is_ok()
        {
            self.typing = Some(typing);
        }
    }
}
