//! The file transfer, chat and clipboard channels, each served by its own
//! thread so a slow service cannot hold up input or video.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use meshrmm_protocol::SessionMessage;
use meshrmm_session_transport::{
    CHAT_CHANNEL, CLIPBOARD_CHANNEL, FILE_CHANNEL, SERVICE_CHANNELS, ServiceChannel, ServiceRoute,
};
use tokio::sync::mpsc;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::ReceiverLifecycle;
use super::control::ViewerControlQueue;
use crate::clipboard::ClipboardSync;

const CLIPBOARD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone)]
pub(super) struct ServiceInbox {
    senders: HashMap<&'static str, mpsc::Sender<SessionMessage>>,
    pub(super) routes: HashMap<&'static str, Arc<ServiceRoute>>,
}
impl ServiceInbox {
    pub(super) fn send(&self, message: SessionMessage) {
        if let Some(label) = meshrmm_session_transport::service_label(&message)
            && let Some(sender) = self.senders.get(label)
            && sender.try_send(message).is_err()
        {
            tracing::warn!(label, "viewer incoming service queue full or closed");
        }
    }
}
pub(super) struct ViewerServices {
    stop: tokio::sync::watch::Sender<bool>,
    stopping: Arc<AtomicBool>,
    tasks: Vec<tokio::task::AbortHandle>,
}
impl Drop for ViewerServices {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.stop.send_replace(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn wait_control_channel(
    control: &tokio::sync::watch::Sender<Option<ServiceChannel>>,
) -> anyhow::Result<ServiceChannel> {
    let mut receiver = control.subscribe();
    loop {
        let current = receiver.borrow_and_update().clone();
        if let Some(channel) = current {
            channel.wait_open().await?;
            return Ok(channel);
        }
        receiver
            .changed()
            .await
            .context("control channel subscription closed")?;
    }
}

pub(super) type ViewerServiceSetup = (ServiceInbox, ViewerServices);
pub(super) fn start_viewer_services(
    viewer: ViewerControlQueue,
    control: tokio::sync::watch::Sender<Option<ServiceChannel>>,
    mut controls: mpsc::UnboundedReceiver<SessionMessage>,
    lifecycle: ReceiverLifecycle,
) -> anyhow::Result<ViewerServiceSetup> {
    let (stop, _) = tokio::sync::watch::channel(false);
    let mut owner = ViewerServices {
        stop: stop.clone(),
        stopping: lifecycle.shutting_down.clone(),
        tasks: Vec::new(),
    };
    let control_writer = control.clone();
    let errors = lifecycle.presentation_failure.clone();
    let writer = tokio::spawn(async move {
        while let Some(message) = controls.recv().await {
            let Ok(channel) = wait_control_channel(&control_writer).await else {
                break;
            };
            if let Err(error) = meshrmm_session_transport::send(&channel, message).await {
                let _ = errors.send(format!("input/control send failed: {error:#}"));
                break;
            }
        }
    });
    owner.tasks.push(writer.abort_handle());
    let mut inbox = HashMap::new();
    let mut routes = HashMap::new();
    for label in SERVICE_CHANNELS {
        let route = Arc::new(ServiceRoute::default());
        routes.insert(label, route.clone());
        let (outgoing, mut pending) =
            mpsc::channel::<SessionMessage>(if label == CLIPBOARD_CHANNEL { 1024 } else { 64 });
        viewer
            .service_senders
            .lock()
            .unwrap()
            .insert(label, outgoing.clone());
        let fallback = control.clone();
        let writer = tokio::spawn(async move {
            let Ok(fallback) = wait_control_channel(&fallback).await else {
                return;
            };
            let Ok(channel) = route.resolve(fallback).await else {
                return;
            };
            while let Some(message) = pending.recv().await {
                if let Err(error) = channel.writable().await {
                    tracing::warn!(label, %error, "viewer service channel unavailable");
                    break;
                }
                if let Err(error) = meshrmm_session_transport::send(&channel, message).await {
                    tracing::warn!(label, %error, "viewer service send failed");
                    break;
                }
            }
        });
        owner.tasks.push(writer.abort_handle());
        let (incoming, mut messages) = mpsc::channel(1024);
        inbox.insert(label, incoming);
        let viewer = viewer.clone();
        let control = control.clone();
        let stopping = lifecycle.shutting_down.clone();
        let runtime = tokio::runtime::Handle::current();
        let mut stop = stop.subscribe();
        let service_sender = outgoing;
        std::thread::Builder::new().name(format!("viewer-{label}")).spawn(move || {
            runtime.block_on(async move {
                let mut clipboard_enabled = label == CLIPBOARD_CHANNEL && crate::preferences::clipboard_sync();
                let mut clipboard = if clipboard_enabled { ClipboardSync::new(true).ok() } else { None };
                let mut receiver = meshrmm_protocol::ClipboardReceiver::default();
                let mut outgoing = std::collections::VecDeque::new();
                let chat_ready = viewer.chat.outgoing_ready();
                let files_ready = viewer.files.outgoing_ready();
                let mut files_pending = true;
                if label == CHAT_CHANNEL {
                    tokio::select! {
                        result = wait_control_channel(&control) => if result.is_err() { return; },
                        _ = stop.wait_for(|stopped| *stopped) => return,
                    }
                    viewer.send(SessionMessage::ChatAvailable);
                }
                let mut poll = tokio::time::interval(CLIPBOARD_POLL_INTERVAL);
                poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                while !stopping.load(Ordering::Acquire) {
                    tokio::select! {
                        _ = stop.wait_for(|stopped| *stopped) => break,
                        message = messages.recv() => {
                            let Some(message) = message else { break; };
                            match message {
                                SessionMessage::FileTransfer(message) => viewer.files.receive(message),
                                SessionMessage::ChatAvailable => viewer.chat.set_available(true),
                                SessionMessage::Chat { text } => { viewer.chat.set_available(true); viewer.chat.receive(text); },
                                message => match receiver.receive(message) {
                                    Ok(Some(content)) => {
                                        outgoing.clear();
                                        if crate::preferences::clipboard_sync()
                                            && let Some(clipboard) = clipboard.as_mut()
                                            && let Err(error) = clipboard.apply(content) { tracing::warn!(%error, "viewer clipboard apply failed"); }
                                    }
                                    Ok(None) => {},
                                    Err(error) => tracing::warn!(%error, "invalid viewer clipboard payload"),
                                },
                            }
                        }
                        _ = chat_ready.notified(), if label == CHAT_CHANNEL => {
                            while let Some(text) = viewer.chat.poll() { viewer.send(SessionMessage::Chat { text }); }
                        }
                        _ = files_ready.notified(), if label == FILE_CHANNEL => files_pending = true,
                        permit = service_sender.reserve(), if label == FILE_CHANNEL && files_pending => {
                            let Ok(permit) = permit else { break; };
                            if let Some(message) = viewer.files.poll() {
                                permit.send(SessionMessage::FileTransfer(message));
                            } else { files_pending = false; }
                        }
                        permit = service_sender.reserve(), if label == CLIPBOARD_CHANNEL && !outgoing.is_empty() => {
                            let Ok(permit) = permit else { break; };
                            if let Some(message) = outgoing.pop_front() { permit.send(message); }
                        }
                        _ = poll.tick(), if label == CLIPBOARD_CHANNEL => {
                            let enabled = crate::preferences::clipboard_sync();
                            if enabled != clipboard_enabled {
                                clipboard_enabled = enabled;
                                outgoing.clear();
                                // Re-enabling syncs later copies only; content copied while off stays local.
                                clipboard = if enabled { ClipboardSync::new(false).ok() } else { None };
                            }
                            let open = control.borrow().as_ref().is_some_and(|c| c.ready_state() == RTCDataChannelState::Open);
                            if !open { continue; }
                            if let Some(clipboard) = clipboard.as_mut() {
                                match clipboard.poll().and_then(|c| Ok(c.map(|c| c.messages()).transpose()?)) {
                                    Ok(Some(messages)) => outgoing = messages.into(),
                                    Ok(None) => {},
                                    Err(error) => tracing::warn!(%error, "viewer clipboard poll failed"),
                                }
                            }
                        }
                    }
                }
                if label == CHAT_CHANNEL { viewer.chat.set_available(false); }
            });
        })?;
    }
    Ok((
        ServiceInbox {
            senders: inbox,
            routes,
        },
        owner,
    ))
}
