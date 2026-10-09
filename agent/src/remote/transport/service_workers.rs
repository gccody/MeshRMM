use std::sync::Arc;

use anyhow::Context;
use meshrmm_protocol::{
    Annotation, FileMessage, RemoteInput, RemoteSessionId, SessionMessage, TogglePolicy,
};
use meshrmm_session_transport::{
    CHAT_CHANNEL, CLIPBOARD_CHANNEL, FILE_CHANNEL, ServiceChannel, ServiceRoute,
};
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::control_channel::send_control_message;
use super::{ControlCommand, SenderCleanup};
use crate::remote::native_task::{NativeTask, command_worker};
use crate::remote::platform::ScreenInput;

/// Queues of the workers that apply viewer messages off the control channel.
#[derive(Clone)]
pub(super) struct WorkerQueues {
    pub(super) input: mpsc::Sender<RemoteInput>,
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
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        if let Err(error) = input.apply_files(message) { tracing::warn!(%error, "file helper unavailable"); }
                    }
                    _ = ready.notified() => pending = true,
                    capacity = channel.writable(), if pending && channel.ready_state() == RTCDataChannelState::Open => {
                        if let Err(error) = capacity { tracing::warn!(%error, "file channel unavailable"); break; }
                        if let Some(message) = input.poll_files() {
                            if let Err(error) = send_control_message(&channel, SessionMessage::FileTransfer(message)).await {
                                tracing::warn!(%error, "file-transfer send failed");
                                break;
                            }
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
            let result: anyhow::Result<()> = async {
            loop {
                if *stop.borrow() { break; }
                tokio::select! {
                    _ = stop.changed() => break,
                    message = commands.recv() => {
                        let Some(message) = message else { break; };
                        match message {
                            SessionMessage::ChatAvailable => {
                                input.start_chat()?;
                                send_control_message(&channel, SessionMessage::ChatAvailable).await?;
                            }
                            SessionMessage::Chat { text } => input.apply_chat(text)?,
                            _ => {},
                        }
                    }
                    _ = ready.notified() => {
                        while let Some(text) = input.poll_chat()? {
                            send_control_message(&channel, SessionMessage::Chat { text }).await?;
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
            let mut outgoing = std::collections::VecDeque::new();
            let mut poll = tokio::time::interval(std::time::Duration::from_millis(250));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if *stop.borrow() {
                    break;
                }
                tokio::select! {
                    _ = stop.changed() => break,
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
                    capacity = channel.writable(), if !outgoing.is_empty() => {
                        if let Err(error) = capacity { tracing::warn!(%error, "clipboard channel unavailable"); break; }
                        if let Some(message) = outgoing.pop_front()
                            && let Err(error) = send_control_message(&channel, message).await {
                                tracing::warn!(%error, "clipboard send failed");
                                break;
                            }
                    }
                }
            }
        },
    )?;
    Ok((sender, task))
}

pub(super) fn spawn_input_worker(
    input: Arc<dyn crate::remote::platform::ScreenInput>,
    channel: Arc<RTCDataChannel>,
    errors: mpsc::UnboundedSender<ControlCommand>,
) -> anyhow::Result<(
    mpsc::Sender<meshrmm_protocol::RemoteInput>,
    crate::remote::native_task::NativeTask,
)> {
    let cleanup_input = Arc::clone(&input);
    let status_channel = Arc::clone(&channel);
    let input_errors = errors.clone();
    let (updates, mut pending) = mpsc::channel(8);
    // This task owns no native resources; dropping the worker closes its queue.
    tokio::spawn(async move {
        while let Some(message) = pending.recv().await {
            if channel.ready_state() != RTCDataChannelState::Open {
                continue;
            }
            if let Err(error) = send_control_message(&channel, message).await {
                let _ = errors.send(ControlCommand::MaintenanceError(format!(
                    "input status: {error:#}"
                )));
                break;
            }
        }
    });
    let mut cursor = None;
    let mut ownership = None;
    let mut pointer_display = None;
    let mut state = None;
    let mut credentials = None;
    Ok(crate::remote::native_task::command_worker(
        "meshrmm-input",
        1024,
        std::time::Duration::from_millis(16),
        move |event| {
            if let Some(event) = event {
                if let Err(error) = input.apply(event) {
                    tracing::warn!(%error, "remote input failed; releasing session input");
                    let _ = input_errors.send(ControlCommand::Stop);
                }
            } else if status_channel.ready_state() == RTCDataChannelState::Open {
                let viewer_controls_input = input.viewer_controls_input();
                if ownership != Some(viewer_controls_input)
                    && input_errors
                        .send(ControlCommand::InputOwnership(viewer_controls_input))
                        .is_ok()
                {
                    ownership = Some(viewer_controls_input);
                }
                if let Some(next) = input.credential_state()
                    && credentials.as_ref() != Some(&next)
                    && updates
                        .try_send(SessionMessage::CredentialState(next.clone()))
                        .is_ok()
                {
                    credentials = Some(next);
                }
                let next_pointer = input.agent_pointer_display();
                if pointer_display != Some(next_pointer)
                    && updates
                        .try_send(SessionMessage::AgentPointerDisplay {
                            display_id: next_pointer,
                        })
                        .is_ok()
                {
                    pointer_display = Some(next_pointer);
                }
                let shape = input.cursor_shape();
                if cursor != Some(shape)
                    && updates
                        .try_send(SessionMessage::CursorShape { shape })
                        .is_ok()
                {
                    cursor = Some(shape);
                }
                if let Some(next) = input.maintenance_state()
                    && (state.as_ref() != Some(&next)
                        || matches!(next, SessionMessage::MaintenanceError { .. }))
                    && updates.try_send(next.clone()).is_ok()
                {
                    state = Some(next);
                }
            }
        },
        move || {
            let _ = cleanup_input.release_all();
        },
    )?)
}
