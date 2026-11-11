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

#[derive(Clone, Default)]
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
    controls: mpsc::UnboundedReceiver<SessionMessage>,
    lifecycle: ReceiverLifecycle,
) -> anyhow::Result<ViewerServiceSetup> {
    let (stop, _) = tokio::sync::watch::channel(false);
    let mut owner = ViewerServices {
        stop: stop.clone(),
        stopping: lifecycle.shutting_down.clone(),
        tasks: Vec::new(),
    };
    let writer = spawn_control_writer(
        control.clone(),
        controls,
        lifecycle.presentation_failure.clone(),
    );
    owner.tasks.push(writer.abort_handle());
    let mut inbox = HashMap::new();
    let mut routes = HashMap::new();
    for label in SERVICE_CHANNELS {
        let route = Arc::new(ServiceRoute::default());
        routes.insert(label, route.clone());
        // The writer waits while its channel has no room. The clipboard worker keeps the rest
        // of its latest copy itself, where a newer copy replaces it, so only one chunk of an
        // outdated copy can wait here.
        let (outgoing, pending) =
            mpsc::channel::<SessionMessage>(if label == CLIPBOARD_CHANNEL { 1 } else { 64 });
        viewer
            .service_senders
            .lock()
            .unwrap()
            .insert(label, outgoing.clone());
        let writer = spawn_service_writer(label, route, control.clone(), pending);
        owner.tasks.push(writer.abort_handle());
        let (incoming, messages) = mpsc::channel(1024);
        inbox.insert(label, incoming);
        let worker = ServiceWorker {
            label,
            viewer: viewer.clone(),
            control: control.clone(),
            stopping: lifecycle.shutting_down.clone(),
            stop: stop.subscribe(),
            service_sender: outgoing,
            messages,
        };
        let runtime = tokio::runtime::Handle::current();
        std::thread::Builder::new()
            .name(format!("viewer-{label}"))
            .spawn(move || runtime.block_on(worker.run()))?;
    }
    Ok((
        ServiceInbox {
            senders: inbox,
            routes,
        },
        owner,
    ))
}

/// A control or input write webrtc-sctp has not taken after this fails the presentation.
/// Unlike the services, which wait as long as their channel is open, input this late is no
/// use to the person.
const CONTROL_WRITE_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Writes queued control messages once the control channel is open.
fn spawn_control_writer(
    control: tokio::sync::watch::Sender<Option<ServiceChannel>>,
    mut controls: mpsc::UnboundedReceiver<SessionMessage>,
    errors: mpsc::UnboundedSender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(message) = controls.recv().await {
            let Ok(channel) = wait_control_channel(&control).await else {
                break;
            };
            let sent = tokio::time::timeout(
                CONTROL_WRITE_LIMIT,
                meshrmm_session_transport::send(&channel, message),
            )
            .await
            .context("timed out")
            .and_then(|sent| sent);
            if let Err(error) = sent {
                let _ = errors.send(format!("input/control send failed: {error:#}"));
                break;
            }
        }
    })
}

/// Writes a service's messages on its own channel, or on the control channel
/// when the route falls back to it.
fn spawn_service_writer(
    label: &'static str,
    route: Arc<ServiceRoute>,
    fallback: tokio::sync::watch::Sender<Option<ServiceChannel>>,
    mut pending: mpsc::Receiver<SessionMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
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
    })
}

/// One service's thread: applies what the device sends and queues what the
/// viewer sends.
struct ServiceWorker {
    label: &'static str,
    viewer: ViewerControlQueue,
    control: tokio::sync::watch::Sender<Option<ServiceChannel>>,
    stopping: Arc<AtomicBool>,
    stop: tokio::sync::watch::Receiver<bool>,
    service_sender: mpsc::Sender<SessionMessage>,
    messages: mpsc::Receiver<SessionMessage>,
}

impl ServiceWorker {
    async fn run(self) {
        let Self {
            label,
            viewer,
            control,
            stopping,
            mut stop,
            service_sender,
            mut messages,
        } = self;
        let mut clipboard_enabled =
            label == CLIPBOARD_CHANNEL && crate::preferences::clipboard_sync();
        let mut clipboard = if clipboard_enabled {
            ClipboardSync::new(true).ok()
        } else {
            None
        };
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
                                {
                                    if let Some(problem) = content.left_out_problem() {
                                        show_clipboard_problem(&viewer, format!("Clipboard from the device: {problem}"));
                                    }
                                    if let Err(error) = clipboard.apply(content) { tracing::warn!(%error, "viewer clipboard apply failed"); }
                                }
                            }
                            Ok(None) => {},
                            Err(error) => tracing::warn!(%error, "invalid viewer clipboard payload"),
                        },
                    }
                }
                _ = chat_ready.notified(), if label == CHAT_CHANNEL => {
                    while let Some(text) = viewer.chat.poll() {
                        viewer.resume_state.note_activity();
                        viewer.send(SessionMessage::Chat { text });
                    }
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
                        match clipboard.poll() {
                            Ok(Some(content)) => {
                                if let Some(problem) = content.left_out_problem() {
                                    show_clipboard_problem(&viewer, format!("Clipboard: {problem}"));
                                }
                                // Nothing of the copy fits: the device keeps what it has.
                                if content.has_formats() {
                                    match content.messages() {
                                        Ok(messages) => outgoing = messages.into(),
                                        Err(error) => tracing::warn!(%error, "viewer clipboard encoding failed"),
                                    }
                                }
                            }
                            Ok(None) => {},
                            Err(error) => tracing::warn!(%error, "viewer clipboard poll failed"),
                        }
                    }
                }
            }
        }
        if label == CHAT_CHANNEL {
            viewer.chat.set_available(false);
        }
    }
}

/// Tells the technician that a copy arrived without what was too large for the
/// session, in the alert that shows the device's maintenance errors.
fn show_clipboard_problem(viewer: &ViewerControlQueue, problem: String) {
    if let Ok(mut state) = viewer.maintenance.lock() {
        state.error = Some(problem);
    }
    crate::platform::refresh_open_controls();
}

#[cfg(test)]
mod stall_tests {
    use super::*;
    use meshrmm_protocol::FileMessage;
    use meshrmm_session_transport::test_link::TestLink;

    fn chunk(id: u64, index: u8) -> SessionMessage {
        SessionMessage::FileTransfer(FileMessage::Chunk {
            id,
            data: vec![index; 16 * 1024],
        })
    }

    /// The writer waits out a channel without room for longer than the five seconds that used
    /// to end it, then sends what it held and what comes after.
    #[tokio::test(start_paused = true)]
    async fn a_service_writer_carries_on_after_its_channel_has_no_room_for_ten_seconds() {
        let (mut link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
        let route = Arc::new(ServiceRoute::default());
        route.attach(channels[0].clone());
        route.peer_ready();
        let (control, _) = tokio::sync::watch::channel(Some(channels[0].clone()));
        let (outgoing, pending) = mpsc::channel(64);
        let writer = spawn_service_writer(FILE_CHANNEL, route, control, pending);
        link.cut();
        for index in 0..32 {
            outgoing.try_send(chunk(1, index)).unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        assert!(!writer.is_finished());
        link.restore();
        for index in 0..32 {
            assert_eq!(
                link.next().await,
                (FILE_CHANNEL.to_owned(), chunk(1, index))
            );
        }
        outgoing.try_send(chunk(2, 0)).unwrap();
        assert_eq!(link.next().await, (FILE_CHANNEL.to_owned(), chunk(2, 0)));
        writer.abort();
        link.close().await;
    }
}
