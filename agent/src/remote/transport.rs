use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use meshrmm_protocol::{
    CONTROL_CHANNEL_LABEL, CONTROL_CHANNEL_PROTOCOL, ChromaMode, DisplayId, HeadlessResolution,
    IceServer, QualityPreset, RemoteSessionId, SessionState, SignalMessage, VideoProfile,
    VideoStreamId,
};
use meshrmm_session_transport::identity::PeerIdentity;
use meshrmm_signaling_client::SessionSignaling;
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use url::Url;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::{RTCPeerConnection, peer_connection_state::RTCPeerConnectionState};

use super::bitrate::EncoderStatus;
use super::native_task::NativeTask;
use super::platform::{ScreenInput, ScreenStreamer, StartedScreen};
use super::sender_failure::failure_signal;
use super::sender_progress::SenderProgress;
use super::session_close::SessionClose;
use super::video::LatestFrameSlot;

mod audio;
mod capture_control;
mod control_channel;
mod peer;
mod sender_loop;
mod service_channels;
#[cfg(test)]
mod service_isolation_tests;
mod service_workers;
#[cfg(test)]
mod tests;
mod video_sender;

use audio::{AudioPipeline, start_audio};
use capture_control::run_capture_control;
use control_channel::{ControlRouting, spawn_control_start, wire_control_channel};
use peer::create_peer;
use sender_loop::{Negotiation, SenderEvents, SenderLoop};
use service_channels::open_service_channels;
use service_workers::{SessionWorkers, spawn_session_workers};
use video_sender::spawn_video_sender;

// Cancellation and startup errors must release resources just like normal teardown.
struct SenderCleanup {
    peer: Arc<RTCPeerConnection>,
    capture: Option<NativeTask>,
    workers: Vec<NativeTask>,
    tasks: Vec<tokio::task::AbortHandle>,
    closed: bool,
}

impl Drop for SenderCleanup {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if self.closed {
            return;
        }
        let peer = Arc::clone(&self.peer);
        tokio::spawn(async move {
            let _ = peer.close().await;
        });
    }
}

impl SenderCleanup {
    async fn finish(
        &mut self,
        mut session_state: SessionState,
        mut result: anyhow::Result<()>,
        session_id: &RemoteSessionId,
        slot: &LatestFrameSlot,
    ) -> anyhow::Result<()> {
        for worker in &mut self.workers {
            worker.shutdown().await;
        }
        if matches!(
            session_state,
            SessionState::Requested
                | SessionState::Signaling
                | SessionState::Connecting
                | SessionState::Streaming
        ) {
            session_state = session_state.transition(SessionState::Closing)?;
        }
        if let Some(capture) = self.capture.as_mut() {
            capture.shutdown().await;
        }
        if let Err(error) = self.peer.close().await {
            tracing::warn!(error = %error, "WebRTC peer did not close cleanly");
            if result.is_ok() {
                result = Err(error).context("failed to close WebRTC peer");
            }
        }
        self.closed = true;
        session_state = session_state.transition(SessionState::Idle)?;
        tracing::info!(
            session_id = %session_id,
            ?session_state,
            encoded_frames_dropped = slot.dropped(),
            "remote sender session stopped"
        );
        result
    }
}

// Recordings persist encoded frames, so their pointer must be composed by capture.
fn capture_cursor_for_session(
    show_cursor: bool,
    viewer_controls_input: bool,
    recording: bool,
) -> bool {
    recording || (show_cursor && !viewer_controls_input)
}

enum ControlCommand {
    MaintenanceError(String),
    Keyframe,
    Bitrate(u32),
    /// Restart an encoder that cannot change bitrate live (HEVC).
    RestartBitrate(u32),
    ViewerCapabilities {
        profiles: Vec<VideoProfile>,
        quality: QualityPreset,
        chroma: ChromaMode,
        headless_resolution: HeadlessResolution,
    },
    Quality(QualityPreset),
    HeadlessResolution(HeadlessResolution),
    Chroma(ChromaMode),
    CursorCapture(bool),
    Recording(bool),
    DisplayBorder(bool),
    InputOwnership(bool),
    VideoProfileRejected {
        profile: VideoProfile,
        reason: String,
    },
    SelectDisplay(DisplayId),
    ChannelClosed,
    Stop,
}

struct CaptureStartup {
    quality_ceiling: Arc<AtomicU32>,
    encoder_status: Arc<EncoderStatus>,
    initial_display: Option<DisplayId>,
    session_close: Arc<SessionClose>,
}

const DISCONNECTED_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(10);

// Session-scoped state is owned by the caller so it survives sender reconnects.
#[allow(clippy::too_many_arguments)]
pub async fn run_sender(
    signal_url: Url,
    signaling_token: &str,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    idle_policy: meshrmm_protocol::TogglePolicy,
    start_in_background: bool,
    session_close: Arc<SessionClose>,
    progress: &SenderProgress,
) -> anyhow::Result<()> {
    let mut signal = SessionSignaling::connect(signal_url, signaling_token.to_owned()).await?;
    let mut failure_reported = false;
    let result = run_connected_sender(
        &mut signal,
        ice_servers,
        streamer,
        session_id,
        &mut failure_reported,
        idle_policy,
        start_in_background,
        session_close,
        progress,
    )
    .await;
    if let Err(error) = &result
        && !failure_reported
    {
        report_sender_failure(&mut signal, error).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_connected_sender(
    signal: &mut SessionSignaling,
    ice_servers: Vec<IceServer>,
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    session_id: RemoteSessionId,
    failure_reported: &mut bool,
    idle_policy: meshrmm_protocol::TogglePolicy,
    start_in_background: bool,
    session_close: Arc<SessionClose>,
    progress: &SenderProgress,
) -> anyhow::Result<()> {
    // Input has its own synchronized controller so capture startup, encoder
    // recovery, and video teardown never hold the path used by control events.
    let input = lock_streamer(&streamer)?.input_controller();
    let (outgoing_tx, outgoing) = mpsc::unbounded_channel::<SignalMessage>();
    let (control_tx, control) = mpsc::unbounded_channel::<ControlCommand>();
    let (state_tx, state) = mpsc::unbounded_channel::<RTCPeerConnectionState>();
    let (video_failure_tx, video_failure) = mpsc::unbounded_channel::<anyhow::Error>();
    let mut events = SenderEvents {
        outgoing,
        control,
        state,
        video_failure,
    };
    let identity = PeerIdentity::load(&crate::installer::identity_directory()?)?;
    let peer = create_peer(
        &ice_servers,
        outgoing_tx.clone(),
        state_tx,
        identity.certificate.clone(),
    )
    .await?;
    let mut cleanup = SenderCleanup {
        peer: Arc::clone(&peer),
        capture: None,
        workers: Vec::new(),
        tasks: Vec::new(),
        closed: false,
    };
    let (channels, _workers) = open_session_channels(
        &peer,
        &input,
        &control_tx,
        &session_id,
        idle_policy,
        &session_close,
        &mut cleanup,
    )
    .await?;
    let capture = start_capture(
        &streamer,
        &channels.control,
        start_in_background,
        session_close,
        video_failure_tx.clone(),
        &mut cleanup,
    )
    .await?;
    let video_sender = spawn_video_sender(
        Arc::clone(&channels.video),
        Arc::clone(&channels.video_open),
        channels.decoder_ready,
        Arc::clone(&capture.slot),
        control_tx.clone(),
        Arc::clone(&capture.quality_ceiling),
        capture.encoder_status,
        Arc::clone(&channels.audio.bits),
        video_failure_tx,
    );
    cleanup.tasks.push(video_sender.abort_handle());
    let control_start = spawn_control_start(
        Arc::clone(&channels.control),
        Arc::clone(&channels.control_open),
        session_id.clone(),
        capture.started.displays.clone(),
        capture.started.active_display.id,
        VideoStreamId(1),
        capture.started.format,
        Arc::clone(&channels.audio.mode),
    );
    cleanup.tasks.push(control_start.abort_handle());

    let mut session_state = SessionState::Requested.transition(SessionState::Signaling)?;
    outgoing_tx.send(SignalMessage::Ready)?;
    session_state = session_state.transition(SessionState::Connecting)?;
    let mut sender_loop = SenderLoop {
        signal: &mut *signal,
        peer: &peer,
        identity: &identity,
        outgoing_tx: &outgoing_tx,
        control_channel: &channels.control,
        capture_tx: &capture.commands,
        session_id: &session_id,
        progress,
        session_state,
        negotiation: Negotiation::default(),
        disconnected_since: None,
    };
    let result = sender_loop.run(&mut events).await;
    let session_state = sender_loop.session_state;

    if let Err(error) = &result {
        report_sender_failure(signal, error).await;
        *failure_reported = true;
    }
    video_sender.abort();
    let _ = video_sender.await;
    control_start.abort();
    let _ = control_start.await;
    cleanup
        .finish(session_state, result, &session_id, &capture.slot)
        .await
}

struct SessionChannels {
    audio: AudioPipeline,
    video: Arc<RTCDataChannel>,
    video_open: Arc<Notify>,
    control: Arc<RTCDataChannel>,
    control_open: Arc<Notify>,
    decoder_ready: Arc<Notify>,
}

async fn open_session_channels(
    peer: &RTCPeerConnection,
    input: &Arc<dyn ScreenInput>,
    control_tx: &mpsc::UnboundedSender<ControlCommand>,
    session_id: &RemoteSessionId,
    idle_policy: meshrmm_protocol::TogglePolicy,
    session_close: &Arc<SessionClose>,
    cleanup: &mut SenderCleanup,
) -> anyhow::Result<(SessionChannels, SessionWorkers)> {
    let audio = start_audio(peer, input, cleanup).await?;
    let video_open = Arc::new(Notify::new());
    let control_open = Arc::new(Notify::new());
    let decoder_ready = Arc::new(Notify::new());
    let video = peer
        .create_data_channel(
            "meshrmm-video-v1",
            Some(RTCDataChannelInit {
                // Video must not sit behind a lost SCTP message. Frame IDs and
                // keyframe recovery already protect the predictive chain, while
                // the reliable control stream remains independently ordered.
                ordered: Some(false),
                max_retransmits: Some(1),
                protocol: Some("meshrmm.video.v1".into()),
                ..Default::default()
            }),
        )
        .await
        .context("failed to create unreliable video data channel")?;
    {
        let notify = Arc::clone(&video_open);
        video.on_open(Box::new(move || {
            let notify = Arc::clone(&notify);
            Box::pin(async move { notify.notify_one() })
        }));
    }
    let control = peer
        .create_data_channel(
            CONTROL_CHANNEL_LABEL,
            Some(RTCDataChannelInit {
                ordered: Some(true),
                protocol: Some(CONTROL_CHANNEL_PROTOCOL.into()),
                ..Default::default()
            }),
        )
        .await
        .context("failed to create reliable control data channel")?;
    let (workers, service_routes) = spawn_session_workers(
        input,
        &control,
        control_tx,
        session_id,
        idle_policy,
        cleanup,
    )
    .await?;
    let routing = ControlRouting {
        queues: workers.queues.clone(),
        commands: control_tx.clone(),
        decoder_ready: Arc::clone(&decoder_ready),
        session_close: Arc::clone(session_close),
        audio_mode: Arc::clone(&audio.mode),
    };
    wire_control_channel(
        &control,
        &workers.control_service,
        &control_open,
        idle_policy,
        routing,
    );
    open_service_channels(peer, service_routes, &workers.queues, control_tx).await?;
    let channels = SessionChannels {
        audio,
        video,
        video_open,
        control,
        control_open,
        decoder_ready,
    };
    Ok((channels, workers))
}

struct StartedCapture {
    slot: Arc<LatestFrameSlot>,
    quality_ceiling: Arc<AtomicU32>,
    encoder_status: Arc<EncoderStatus>,
    commands: mpsc::Sender<ControlCommand>,
    started: StartedScreen,
}

async fn start_capture(
    streamer: &Arc<Mutex<Box<dyn ScreenStreamer>>>,
    control_channel: &Arc<RTCDataChannel>,
    start_in_background: bool,
    session_close: Arc<SessionClose>,
    capture_failure: mpsc::UnboundedSender<anyhow::Error>,
    cleanup: &mut SenderCleanup,
) -> anyhow::Result<StartedCapture> {
    let slot = Arc::new(LatestFrameSlot::default());
    let quality_ceiling = Arc::new(AtomicU32::new(1));
    let encoder_status = Arc::new(EncoderStatus::default());
    let (capture_tx, capture_rx) = mpsc::channel(64);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let capture_streamer = Arc::clone(streamer);
    let capture_slot = Arc::clone(&slot);
    let capture_channel = Arc::clone(control_channel);
    let startup = CaptureStartup {
        quality_ceiling: Arc::clone(&quality_ceiling),
        encoder_status: Arc::clone(&encoder_status),
        initial_display: start_in_background
            .then_some(DisplayId(meshrmm_protocol::BACKGROUND_DISPLAY_ID.0)),
        session_close,
    };
    let capture_task = NativeTask::spawn("meshrmm-capture-control", move |stop| async move {
        let result = run_capture_control(
            capture_streamer.clone(),
            capture_slot,
            capture_channel,
            startup,
            capture_rx,
            started_tx,
            stop,
        )
        .await;
        if let Err(error) = result {
            let _ = capture_failure.send(error.context("capture worker"));
        }
        if let Err(error) = lock_streamer(&capture_streamer).and_then(|mut s| s.shutdown()) {
            tracing::warn!(%error, "capture worker cleanup failed");
        }
    })?;
    cleanup.capture = Some(capture_task);
    let started = match started_rx
        .await
        .context("capture worker stopped before startup")
        .and_then(|result| result)
    {
        Ok(started) => started,
        Err(error) => {
            // Finish the old worker before a reconnect can reuse the streamer.
            // Heartbeats continue in the independent signaling pump.
            for worker in &mut cleanup.workers {
                worker.shutdown().await;
            }
            if let Some(capture) = cleanup.capture.as_mut() {
                capture.shutdown().await;
            }
            return Err(error);
        }
    };
    Ok(StartedCapture {
        slot,
        quality_ceiling,
        encoder_status,
        commands: capture_tx,
        started,
    })
}

async fn report_sender_failure(connection: &mut SessionSignaling, error: &anyhow::Error) {
    let Some(signal) = failure_signal(error) else {
        tracing::debug!(error = %error, "not reporting a transport failure the viewer detects itself");
        return;
    };
    match serde_json::to_string(&signal) {
        Ok(message) => {
            if let Err(send_error) = connection.send(Message::Text(message.into())).await {
                tracing::warn!(error = %send_error, "failed to report sender failure to viewer");
            }
        }
        Err(send_error) => {
            tracing::warn!(error = %send_error, "failed to encode sender failure for viewer");
        }
    }
}

fn lock_streamer(
    streamer: &Arc<Mutex<Box<dyn ScreenStreamer>>>,
) -> anyhow::Result<std::sync::MutexGuard<'_, Box<dyn ScreenStreamer>>> {
    streamer
        .lock()
        .map_err(|_| anyhow::anyhow!("screen streamer lock is poisoned"))
}
