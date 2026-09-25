//! The control channel: queues the viewer's input and session messages for
//! the device, and applies the display configuration and session state the
//! device sends back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, ChromaMode, Codec, CursorShape, SessionMessage, VideoProfile,
};
use meshrmm_session_transport::ServiceChannel;
use tokio::sync::{Notify, mpsc};

use super::services::ServiceInbox;
use super::{ActivePresenter, ViewerResumeState};
use crate::debug::DebugInfo;
use crate::platform::{ControlSink, ControlSinkParts, Presenter};

const POINTER_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

#[cfg(target_os = "macos")]
fn can_reset_presenter_in_place(
    current: meshrmm_protocol::VideoFormat,
    format: meshrmm_protocol::VideoFormat,
) -> bool {
    // The sample-buffer layer reads dimensions from the replacement keyframe.
    // Display identity and resolution do not require a new native window.
    current.codec == format.codec && current.pixel_format == format.pixel_format
}

/// Mouse-move events can arrive substantially faster than the network can
/// usefully deliver them. Keep only the newest unsent position so transient
/// congestion cannot put keyboard and button events behind an unbounded trail
/// of stale pointer positions on the reliable control stream.
#[derive(Clone)]
pub(super) struct ViewerControlQueue {
    pub(super) recording: crate::recording::Recorder,
    pub(super) maintenance: Arc<Mutex<crate::platform::MaintenanceState>>,
    pub(super) credentials: Arc<Mutex<meshrmm_protocol::CredentialState>>,
    pub(super) files: meshrmm_file_transfer::TransferSession,
    pub(super) chat: meshrmm_chat::ChatSession,
    outgoing: mpsc::UnboundedSender<SessionMessage>,
    pub(super) service_senders: Arc<Mutex<HashMap<&'static str, mpsc::Sender<SessionMessage>>>>,
    input: Arc<Mutex<ViewerInputState>>,
    pointer_changed: Arc<Notify>,
    pub(super) resume_state: ViewerResumeState,
}

struct ViewerInputState {
    enabled: bool,
    pending_pointer: Option<SessionMessage>,
}

impl ViewerControlQueue {
    pub(super) fn new(
        outgoing: mpsc::UnboundedSender<SessionMessage>,
        resume_state: ViewerResumeState,
    ) -> Self {
        *resume_state
            .recording_outgoing
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(outgoing.clone());
        Self {
            recording: resume_state.recording.clone(),
            maintenance: Arc::new(Mutex::new(crate::platform::MaintenanceState::default())),
            credentials: Arc::new(Mutex::new(Default::default())),
            files: meshrmm_file_transfer::TransferSession::viewer(
                crate::preferences::clipboard_sync,
            ),
            chat: meshrmm_chat::ChatSession::default(),
            outgoing,
            service_senders: Arc::new(Mutex::new(HashMap::new())),
            input: Arc::new(Mutex::new(ViewerInputState {
                enabled: false,
                pending_pointer: None,
            })),
            pointer_changed: Arc::new(Notify::new()),
            resume_state,
        }
    }

    pub(super) fn send(&self, message: SessionMessage) {
        if let SessionMessage::SelectDisplay { display_id } = &message
            && let Ok(mut selected) = self.resume_state.display_id.lock()
        {
            *selected = Some(*display_id);
        }
        let is_input = matches!(&message, SessionMessage::Input(_));
        let Ok(mut input) = self.input.lock() else {
            return;
        };
        if is_input
            && (!input.enabled || self.resume_state.technician_blocked.load(Ordering::SeqCst))
        {
            return;
        }
        if matches!(
            &message,
            SessionMessage::Input(meshrmm_protocol::RemoteInput::PointerMove { .. })
        ) {
            input.pending_pointer = Some(message);
            drop(input);
            self.pointer_changed.notify_one();
            return;
        }

        let pending_pointer = if matches!(
            &message,
            SessionMessage::Input(
                meshrmm_protocol::RemoteInput::PointerButtonAt { .. }
                    | meshrmm_protocol::RemoteInput::WheelAt { .. }
            )
        ) {
            // The positioned action supersedes any older unsent motion.
            input.pending_pointer.take();
            None
        } else {
            // Preserve pointer-before-action ordering for legacy/non-positioned
            // messages while still coalescing ordinary motion.
            input.pending_pointer.take()
        };
        drop(input);
        if let Some(pending_pointer) = pending_pointer {
            let _ = self.outgoing.send(pending_pointer);
        }
        if let Some(label) = meshrmm_session_transport::service_label(&message)
            && let Some(sender) = self
                .service_senders
                .lock()
                .ok()
                .and_then(|map| map.get(label).cloned())
        {
            if sender.try_send(message).is_err() {
                tracing::warn!(label, "viewer service queue full or closed");
            }
        } else {
            let _ = self.outgoing.send(message);
        }
    }

    fn flush_pointer(&self) {
        let pending = self.input.lock().ok().and_then(|mut input| {
            input
                .enabled
                .then(|| input.pending_pointer.take())
                .flatten()
        });
        if let Some(message) = pending {
            let _ = self.outgoing.send(message);
        }
    }

    fn set_input_enabled(&self, enabled: bool) {
        if let Ok(mut input) = self.input.lock() {
            input.enabled = enabled;
            if !enabled {
                input.pending_pointer = None;
            }
        }
    }
}

pub(super) async fn flush_pointer_motion(queue: ViewerControlQueue) {
    loop {
        queue.pointer_changed.notified().await;
        tokio::time::sleep(POINTER_FLUSH_INTERVAL).await;
        queue.flush_pointer();
    }
}

pub(super) fn install_control_handler(
    channel: ServiceChannel,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    viewer_control: ViewerControlQueue,
    remote_text: ServiceInbox,
    presentation_failure: mpsc::UnboundedSender<String>,
    debug: DebugInfo,
    shutting_down: Arc<AtomicBool>,
) {
    {
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        let shutting_down = Arc::clone(&shutting_down);
        let closing = channel.notifier();
        channel.on_close(Box::new(move || {
            closing.notify_waiters();
            let presentation_failure = presentation_failure.clone();
            let debug = debug.clone();
            let shutting_down = Arc::clone(&shutting_down);
            Box::pin(async move {
                debug.set_data_channel(CONTROL_CHANNEL_LABEL, "closed");
                if shutting_down.load(Ordering::Acquire) {
                    tracing::info!("viewer control data channel closed during viewer shutdown");
                    return;
                }
                tracing::error!("viewer control data channel closed while video was active");
                let _ = presentation_failure.send(
                    "remote input/control channel closed while video was still active".into(),
                );
            })
        }));
    }
    let cursor_shape = Arc::new(Mutex::new(CursorShape::Default));
    let pointer_display = Arc::new(Mutex::new(None));
    let capabilities_sent = Arc::new(AtomicBool::new(false));
    let supported_profiles = Arc::new(OnceLock::<Arc<Vec<VideoProfile>>>::new());
    let configurations_seen = Arc::new(AtomicU64::new(0));
    let resume_state = viewer_control.resume_state.clone();
    let quality_preset = Arc::clone(&resume_state.quality);
    let chroma_mode = Arc::clone(&resume_state.chroma);
    let selected_display_id = Arc::clone(&resume_state.display_id);
    channel.on_message(Box::new(move |message| {
        let presenter = Arc::clone(&presenter);
        let cursor_shape = Arc::clone(&cursor_shape);
        let pointer_display = Arc::clone(&pointer_display);
        let capabilities_sent = Arc::clone(&capabilities_sent);
        let supported_profiles = Arc::clone(&supported_profiles);
        let configurations_seen = Arc::clone(&configurations_seen);
        let quality_preset = Arc::clone(&quality_preset);
        let chroma_mode = Arc::clone(&chroma_mode);
        let viewer_control = viewer_control.clone();
        let remote_text = remote_text.clone();
        let presentation_failure = presentation_failure.clone();
        let debug = debug.clone();
        let selected_display_id = Arc::clone(&selected_display_id);
        Box::pin(async move {
            match SessionMessage::decode(&message.data) {
                Ok(SessionMessage::DisplayConfiguration {
                    displays,
                    active_display_id,
                    stream_id,
                    format,
                }) => {
                    let configuration_sequence =
                        configurations_seen.fetch_add(1, Ordering::AcqRel) + 1;
                    let previous = presenter.lock().ok().and_then(|guard| {
                        guard
                            .as_ref()
                            .map(|active| (active.stream_id, active.profile))
                    });
                    tracing::info!(
                        configuration_sequence,
                        previous_stream_id = previous.map(|value| value.0.0),
                        previous_profile = ?previous.map(|value| value.1),
                        stream_id = stream_id.0,
                        display_id = active_display_id.0,
                        width = format.width,
                        height = format.height,
                        fps = format.frames_per_second,
                        bitrate_bits_per_second = format.bitrate_bits_per_second,
                        codec = ?format.codec,
                        "viewer received display configuration"
                    );
                    let Some(active_display) = displays
                        .iter()
                        .find(|display| display.id == active_display_id)
                        .cloned()
                    else {
                        tracing::error!(display_id = active_display_id.0, "Agent selected an unknown display");
                        return;
                    };
                    let message_queue = viewer_control.clone();
                    let input_gate = viewer_control.clone();
                    // Probing Windows hardware MFTs can take noticeable time.
                    // Codec support is a viewer capability, so do it once per
                    // connection rather than again for the negotiated echo.
                    let profiles = supported_profiles
                        .get_or_init(|| {
                            Arc::new(crate::platform::supported_video_profiles(format))
                        })
                        .clone();
                    let resumed_display = selected_display_id
                        .lock()
                        .ok()
                        .and_then(|selected| *selected)
                        .filter(|selected| {
                            configuration_sequence == 1
                                && *selected != active_display_id
                                && displays.iter().any(|display| display.id == *selected)
                        });
                    // Replay a remembered choice only on transport reconnect. Later
                    // configurations acknowledge selection or recovery; replaying a
                    // failed choice there would cause an endless switch/restore loop.
                    if resumed_display.is_none()
                        && let Ok(mut selected) = selected_display_id.lock() {
                        *selected = Some(active_display_id);
                    }
                    let sink = ControlSink::new(ControlSinkParts {
                        idle: Arc::clone(&viewer_control.resume_state.idle),
                        display_border: Arc::clone(&viewer_control.resume_state.display_border),
                        files: viewer_control.files.clone(),
                        chat: viewer_control.chat.clone(),
                        audio: viewer_control.resume_state.audio.clone(),
                        recording: viewer_control.recording.clone(),
                        send: Arc::new(move |message| message_queue.send(message)),
                        set_input_enabled: Arc::new(move |enabled| input_gate.set_input_enabled(enabled)),
                        maintenance: Arc::clone(&viewer_control.maintenance),
                        credentials: Arc::clone(&viewer_control.credentials),
                        technician_blocked: Arc::clone(&viewer_control.resume_state.technician_blocked),
                        wallpaper_hidden: Arc::clone(&viewer_control.resume_state.wallpaper_hidden),
                        remote_cursor_hidden: Arc::clone(&viewer_control.resume_state.remote_cursor_hidden),
                        session_close_action: Arc::clone(&viewer_control.resume_state.session_close_action),
                        quality: Arc::clone(&quality_preset),
                        chroma: Arc::clone(&chroma_mode),
                        #[cfg(windows)]
                        profiles: Arc::clone(&profiles),
                    });
                    debug.configure_stream(
                        active_display.name.clone(),
                        format.width,
                        format.height,
                        format.frames_per_second,
                        format.codec,
                    );
                    #[cfg(target_os = "macos")]
                    let reset_in_place = if capabilities_sent.load(Ordering::Acquire)
                        && let Ok(mut guard) = presenter.lock()
                        && let Some(active) = guard.as_mut()
                        && can_reset_presenter_in_place(active.format, format)
                    {
                        match active.presenter.reset_stream(format, active_display.clone(), displays.clone()) {
                            Ok(()) => {
                                let previous_stream_id = active.stream_id;
                                active.stream_id = stream_id;
                                active.format = format;
                                active.profile = format.profile();
                                tracing::info!(
                                    configuration_sequence,
                                    previous_stream_id = previous_stream_id.0,
                                    stream_id = stream_id.0,
                                    bitrate_bits_per_second = format.bitrate_bits_per_second,
                                    codec = ?format.codec,
                                    "reset the macOS decoder in place for a replacement stream"
                                );
                                true
                            }
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    configuration_sequence,
                                    previous_stream_id = active.stream_id.0,
                                    stream_id = stream_id.0,
                                    "could not reset the macOS decoder in place; replacing the presenter"
                                );
                                false
                            }
                        }
                    } else {
                        false
                    };
                    #[cfg(not(target_os = "macos"))]
                    let reset_in_place = false;

                    if reset_in_place {
                        viewer_control.send(SessionMessage::RequestKeyframe { stream_id });
                        if let Some(display_id) = resumed_display {
                            viewer_control.send(SessionMessage::SelectDisplay { display_id });
                        }
                        tracing::info!(configuration_sequence, stream_id = stream_id.0, display_id = active_display_id.0, display_name = %active_display.name, width = format.width, height = format.height, fps = format.frames_per_second, bitrate_bits_per_second = format.bitrate_bits_per_second, codec = ?format.codec, "remote control stream reconfigured without replacing its window");
                        return;
                    }
                    if !capabilities_sent.swap(true, Ordering::AcqRel) {
                        // The first configuration describes the Agent's mandatory
                        // bootstrap profile. Negotiate the best common profile
                        // before creating a visible presenter; the Agent echoes a
                        // settled configuration even when that profile is retained.
                        viewer_control.send(SessionMessage::SetPreventIdleLock { enabled: sink.prevent_idle_lock() });
                        viewer_control.send(SessionMessage::SetSessionCloseAction { action: sink.session_close_action() });
                        viewer_control.send(SessionMessage::SetClearClipboardOnClose { enabled: sink.clear_clipboard_on_close() });
                        viewer_control.send(SessionMessage::SetDisplayBorder { enabled: sink.display_border() });
                        viewer_control.send(SessionMessage::SetWallpaperHidden { hidden: sink.wallpaper_hidden() });
                        viewer_control.send(SessionMessage::SetRecording {
                            enabled: viewer_control.recording.active(),
                        });
                        viewer_control.send(SessionMessage::SetCursorCapture {
                            enabled: sink.show_remote_cursor(),
                        });
                        viewer_control.send(SessionMessage::ViewerCapabilities {
                            profiles: profiles.as_ref().clone(),
                            quality: sink.quality_preset(),
                            chroma: sink.chroma_mode(),
                        });
                        tracing::info!(
                            configuration_sequence,
                            stream_id = stream_id.0,
                            "viewer capabilities sent; waiting for the settled video profile"
                        );
                        return;
                    }
                    match Presenter::start(
                        format,
                        active_display.clone(),
                        displays,
                        sink.clone(),
                        debug.clone(),
                    ) {
                        Ok(new_presenter) => {
                            if let Ok(display) = pointer_display.lock() { new_presenter.set_agent_pointer_display(*display); }
                            if let Ok(shape) = cursor_shape.lock() {
                                new_presenter.set_cursor_shape(*shape);
                            }
                            let mut old = presenter
                                .lock()
                                .ok()
                                .and_then(|mut guard| guard.replace(ActivePresenter {
                                    stream_id,
                                    format,
                                    profile: format.profile(),
                                    presenter: new_presenter,
                                }));
                            if let Some(old) = old.as_mut() {
                                tracing::warn!(
                                    configuration_sequence,
                                    previous_stream_id = old.stream_id.0,
                                    previous_profile = ?old.profile,
                                    stream_id = stream_id.0,
                                    codec = ?format.codec,
                                    "replacing the active presenter after display configuration"
                                );
                                old.presenter.stop();
                            }
                            // The new window is up; retire the one kept while reconnecting.
                            viewer_control.resume_state.close_reconnecting_window();
                            let request = SessionMessage::RequestKeyframe { stream_id };
                            viewer_control.send(request);
                            if let Some(display_id) = resumed_display {
                                viewer_control.send(SessionMessage::SelectDisplay { display_id });
                                tracing::info!(
                                    display_id = display_id.0,
                                    "restored viewer display selection after reconnect"
                                );
                            }
                            tracing::info!(configuration_sequence, stream_id = stream_id.0, display_id = active_display_id.0, display_name = %active_display.name, width = format.width, height = format.height, fps = format.frames_per_second, bitrate_bits_per_second = format.bitrate_bits_per_second, codec = ?format.codec, "remote control stream configured");
                        }
                        Err(error) => {
                            let message = format!(
                                "hardware decoder/presenter initialization failed: {error:#}"
                            );
                            tracing::error!(error = %error, "hardware decoder/presenter initialization failed");
                            if format.profile()
                                != (VideoProfile {
                                    codec: Codec::H264,
                                    chroma: ChromaMode::Yuv420,
                                })
                            {
                                viewer_control.send(SessionMessage::VideoProfileRejected {
                                    profile: format.profile(),
                                    reason: message,
                                });
                            } else {
                                let _ = presentation_failure.send(message);
                            }
                        }
                    }
                }
                Ok(SessionMessage::CredentialState(state)) => {
                    if let Ok(mut current) = viewer_control.credentials.lock() { *current = state; }
                    if let Ok(guard) = presenter.lock() && let Some(active) = guard.as_ref() { active.presenter.refresh_controls(); }
                }
                Ok(SessionMessage::MaintenanceError { reason }) => {
                    if let Ok(mut state) = viewer_control.maintenance.lock() { state.error = Some(reason); }
                    if let Ok(guard) = presenter.lock() && let Some(active) = guard.as_ref() { active.presenter.refresh_controls(); }
                }
                Ok(SessionMessage::MaintenanceState { agent_input_blocked, blacked_out }) => {
                    if let Ok(mut state) = viewer_control.maintenance.lock() {
                        *state = crate::platform::MaintenanceState { available: true, agent_input_blocked, blacked_out, error: None };
                    }
                }
                Ok(SessionMessage::Stop { reason }) => tracing::info!(reason, "Agent stopped stream"),
                Ok(SessionMessage::AgentPointerDisplay { display_id }) => {
                    if let Ok(mut current) = pointer_display.lock() { *current = display_id; }
                    if let Ok(guard) = presenter.lock() && let Some(active) = guard.as_ref() { active.presenter.set_agent_pointer_display(display_id); }
                }
                Ok(SessionMessage::CursorShape { shape }) => {
                    if let Ok(mut current) = cursor_shape.lock() {
                        *current = shape;
                    }
                    if let Ok(guard) = presenter.lock()
                        && let Some(active) = guard.as_ref()
                    {
                        active.presenter.set_cursor_shape(shape);
                    }
                }
                Ok(message @ (SessionMessage::FileTransfer(_) | SessionMessage::Clipboard { .. } | SessionMessage::ClipboardChunk { .. } | SessionMessage::Chat { .. } | SessionMessage::ChatAvailable)) => {
                    remote_text.send(message);
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(error = %error, "discarding invalid control message"),
            }
        })
    }));
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{DisplayId, PointerButton, RemoteInput};

    use super::*;

    fn pointer(x: u16, y: u16) -> SessionMessage {
        SessionMessage::Input(RemoteInput::PointerMove {
            display_id: DisplayId(1),
            x,
            y,
        })
    }

    fn active_queue() -> (ViewerControlQueue, mpsc::UnboundedReceiver<SessionMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, ViewerResumeState::default());
        queue.set_input_enabled(true);
        (queue, rx)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn technician_block_survives_focus_and_transport_rebuild() {
        let state = ViewerResumeState::default();
        state.technician_blocked.store(true, Ordering::SeqCst);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, state.clone());
        queue.set_input_enabled(true);
        queue.send(SessionMessage::Input(RemoteInput::Key {
            display_id: DisplayId(1),
            scan_code: 30,
            extended: false,
            pressed: true,
        }));
        assert!(rx.try_recv().is_err());
        state.technician_blocked.store(false, Ordering::SeqCst);
        queue.send(SessionMessage::Input(RemoteInput::Key {
            display_id: DisplayId(1),
            scan_code: 30,
            extended: false,
            pressed: true,
        }));
        assert!(rx.try_recv().is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn monitor_resolution_changes_reuse_the_presenter_but_codec_changes_do_not() {
        let current = meshrmm_protocol::VideoFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 60,
            codec: Codec::H264,
            pixel_format: meshrmm_protocol::PixelFormat::Nv12,
            bitrate_bits_per_second: 12_000_000,
        };
        let replacement = meshrmm_protocol::VideoFormat {
            width: 2560,
            height: 1440,
            frames_per_second: 30,
            bitrate_bits_per_second: 8_000_000,
            ..current
        };
        assert!(can_reset_presenter_in_place(current, replacement));
        assert!(!can_reset_presenter_in_place(
            current,
            meshrmm_protocol::VideoFormat {
                codec: Codec::H265,
                ..replacement
            }
        ));
        assert!(!can_reset_presenter_in_place(
            current,
            meshrmm_protocol::VideoFormat {
                pixel_format: meshrmm_protocol::PixelFormat::Ayuv,
                ..replacement
            }
        ));
    }

    #[test]
    fn pointer_motion_is_coalesced_to_the_latest_position() {
        let (queue, mut rx) = active_queue();

        queue.send(pointer(10, 20));
        queue.send(pointer(30, 40));
        assert!(rx.try_recv().is_err());

        queue.flush_pointer();
        assert_eq!(rx.try_recv().unwrap(), pointer(30, 40));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pointer_position_is_flushed_before_a_button_event() {
        let (queue, mut rx) = active_queue();
        let button = SessionMessage::Input(RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: PointerButton::Left,
            pressed: true,
        });

        queue.send(pointer(30, 40));
        queue.send(button.clone());

        assert_eq!(rx.try_recv().unwrap(), pointer(30, 40));
        assert_eq!(rx.try_recv().unwrap(), button);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn positioned_button_supersedes_pending_pointer_motion() {
        let (queue, mut rx) = active_queue();
        let button = SessionMessage::Input(RemoteInput::PointerButtonAt {
            display_id: DisplayId(1),
            x: 50,
            y: 60,
            button: PointerButton::Left,
            pressed: true,
        });

        queue.send(pointer(30, 40));
        queue.send(button.clone());

        assert_eq!(rx.try_recv().unwrap(), button);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn input_is_discarded_until_the_viewer_is_foreground() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, ViewerResumeState::default());
        let button = SessionMessage::Input(RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: PointerButton::Left,
            pressed: true,
        });

        queue.send(pointer(10, 20));
        queue.send(button);
        queue.flush_pointer();
        assert!(rx.try_recv().is_err());

        queue.set_input_enabled(true);
        queue.send(pointer(30, 40));
        queue.flush_pointer();
        assert_eq!(rx.try_recv().unwrap(), pointer(30, 40));
    }

    #[test]
    fn secure_attention_is_sent_while_toolbar_has_keyboard_focus() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, ViewerResumeState::default());
        // Native toolbar controls can take focus away from the remote desktop.
        queue.set_input_enabled(false);
        queue.send(SessionMessage::SendSecureAttention);
        assert_eq!(rx.try_recv().unwrap(), SessionMessage::SendSecureAttention);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn backgrounding_discards_pending_pointer_motion() {
        let (queue, mut rx) = active_queue();

        queue.send(pointer(30, 40));
        queue.set_input_enabled(false);
        queue.flush_pointer();

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn display_selection_is_retained_for_a_reconnected_transport() {
        let resume_state = ViewerResumeState::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let queue = ViewerControlQueue::new(tx, resume_state.clone());

        queue.send(SessionMessage::SelectDisplay {
            display_id: DisplayId(42),
        });

        assert_eq!(
            *resume_state.display_id.lock().unwrap(),
            Some(DisplayId(42))
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            SessionMessage::SelectDisplay {
                display_id: DisplayId(42)
            }
        );
    }

    #[test]
    fn resumed_connections_keep_viewer_choices_and_recording() {
        let state = ViewerResumeState::default();
        // Hidden is the default until the technician shows the wallpaper.
        assert!(state.wallpaper_hidden.load(Ordering::SeqCst));
        state.wallpaper_hidden.store(false, Ordering::SeqCst);
        let (first_tx, mut first_rx) = mpsc::unbounded_channel();
        let first = ViewerControlQueue::new(first_tx, state.clone());
        let (second_tx, mut second_rx) = mpsc::unbounded_channel();
        let second = ViewerControlQueue::new(second_tx, state.clone());
        assert!(!state.wallpaper_hidden.load(Ordering::SeqCst));
        // Both connections record into the same recorder...
        assert!(first.recording.is_same(&second.recording));
        // ...and its state changes go to the newest connection.
        state
            .recording_outgoing
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(SessionMessage::SetRecording { enabled: true })
            .unwrap();
        assert_eq!(
            second_rx.try_recv().unwrap(),
            SessionMessage::SetRecording { enabled: true }
        );
        assert!(first_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn movement_burst_automatically_flushes_its_final_position() {
        let (queue, mut rx) = active_queue();
        let flusher = tokio::spawn(flush_pointer_motion(queue.clone()));

        queue.send(pointer(10, 20));
        queue.send(pointer(30, 40));
        queue.send(pointer(50, 60));

        let sent = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .expect("pointer flush timed out")
            .expect("pointer queue closed");
        assert_eq!(sent, pointer(50, 60));
        assert!(rx.try_recv().is_err());

        flusher.abort();
    }
}
