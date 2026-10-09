use std::sync::Arc;

use meshrmm_protocol::SessionMessage;
use meshrmm_session_transport::{ServiceChannel, ServiceRoute};
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::ControlCommand;
use super::control_channel::send_control_message;

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
