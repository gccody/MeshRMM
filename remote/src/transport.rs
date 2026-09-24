//! The viewer's connection to a device. [`receiver`] runs the signaling
//! loop and sets up the WebRTC peer, [`control`] carries input and session
//! messages on the control channel, [`services`] runs the file, chat and
//! clipboard channels, and [`video`] reassembles the video stream.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use meshrmm_protocol::{ChromaMode, QualityPreset, SessionMessage, VideoProfile, VideoStreamId};
use tokio::sync::mpsc;

use crate::platform::Presenter;

mod control;
mod receiver;
mod services;
mod video;

pub use receiver::run_receiver;

/// How long the end of a session waits for a recording to be saved.
const RECORDING_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct ActivePresenter {
    stream_id: VideoStreamId,
    format: meshrmm_protocol::VideoFormat,
    profile: VideoProfile,
    presenter: Presenter,
}

#[derive(Clone)]
struct ReceiverLifecycle {
    presentation_failure: mpsc::UnboundedSender<String>,
    shutting_down: Arc<AtomicBool>,
}

/// Viewer choices that should survive rebuilding the signaling and WebRTC
/// transports after a network change or remote reboot.
#[derive(Clone)]
pub struct ViewerResumeState {
    idle: Arc<Mutex<crate::platform::IdlePreference>>,
    display_border: Arc<Mutex<Option<bool>>>,
    technician_blocked: Arc<AtomicBool>,
    remote_cursor_hidden: Arc<AtomicBool>,
    wallpaper_hidden: Arc<AtomicBool>,
    session_close_action: Arc<Mutex<meshrmm_protocol::SessionCloseAction>>,
    quality: Arc<Mutex<QualityPreset>>,
    chroma: Arc<Mutex<ChromaMode>>,
    display_id: Arc<Mutex<Option<meshrmm_protocol::DisplayId>>>,
    audio: meshrmm_audio::PlaybackState,
    /// Keeps recording across reconnects; each new stream starts a new part.
    recording: crate::recording::Recorder,
    /// The current connection's control queue, for recording state changes.
    recording_outgoing: Arc<Mutex<Option<mpsc::UnboundedSender<SessionMessage>>>>,
    /// The window of a lost connection, shown as reconnecting until the next
    /// connection opens its own.
    reconnecting: Arc<Mutex<Option<ActivePresenter>>>,
}

impl Default for ViewerResumeState {
    fn default() -> Self {
        let recording_outgoing =
            Arc::new(Mutex::new(None::<mpsc::UnboundedSender<SessionMessage>>));
        let activity = Arc::clone(&recording_outgoing);
        Self {
            idle: Default::default(),
            display_border: Default::default(),
            technician_blocked: Default::default(),
            remote_cursor_hidden: Default::default(),
            wallpaper_hidden: Arc::new(AtomicBool::new(true)),
            session_close_action: Default::default(),
            quality: Default::default(),
            chroma: Default::default(),
            display_id: Default::default(),
            audio: Default::default(),
            recording: crate::recording::Recorder::with_activity_callback(move |enabled| {
                if let Some(outgoing) = activity
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    let _ = outgoing.send(SessionMessage::SetRecording { enabled });
                }
            }),
            recording_outgoing,
            reconnecting: Default::default(),
        }
    }
}

impl ViewerResumeState {
    fn keep_while_reconnecting(&self, active: ActivePresenter) {
        active.presenter.set_reconnecting(true);
        let previous = self
            .reconnecting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .replace(active);
        if let Some(mut previous) = previous {
            previous.presenter.stop();
        }
    }

    /// Closes the window kept from a lost connection.
    pub fn close_reconnecting_window(&self) {
        let kept = self
            .reconnecting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(mut kept) = kept {
            kept.presenter.stop();
        }
    }

    /// Ends a recording that is still running once the session is over, and
    /// waits for it to be saved. Returns the notice to show the user.
    pub fn finish_recording(&self) -> Option<String> {
        self.recording.finish(RECORDING_FINISH_TIMEOUT)
    }

    pub fn select_background_display(&self) {
        *self
            .display_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(meshrmm_protocol::BACKGROUND_DISPLAY_ID);
    }
}
