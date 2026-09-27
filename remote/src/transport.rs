//! The viewer's connection to a device. [`receiver`] runs the signaling
//! loop and sets up the WebRTC peer, [`control`] carries input and session
//! messages on the control channel, [`services`] runs the file, chat and
//! clipboard channels, and [`video`] reassembles the video stream.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use meshrmm_protocol::{ChromaMode, QualityPreset, SessionMessage, VideoProfile, VideoStreamId};
use tokio::sync::mpsc;

use crate::platform::Presenter;
use crate::reconnect::{AttemptProgress, ReconnectPhase, ReconnectStatus};

mod control;
mod failure;
mod receiver;
mod services;
mod video;

pub use failure::{FailureKind, SessionFailure, failure_kind};
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
    progress: Arc<Mutex<AttemptProgress>>,
    reconnect_status: Arc<Mutex<Option<ReconnectStatus>>>,
}

impl ReceiverLifecycle {
    /// Records a frame confirmed by the native presentation path. The attempt's first frame
    /// ends the reconnect, so the next failure starts a new one.
    fn observe_presentation(&self, at: Option<std::time::Instant>) {
        let Some(at) = at else { return };
        let first = self
            .progress
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mark_frame_presented(at);
        if first {
            self.reconnect_status
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
        }
    }
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
    /// Whether the remote display has appeared, in this attempt and ever.
    progress: Arc<Mutex<AttemptProgress>>,
    /// Why and since when the session is reconnecting; `None` while the
    /// remote display is up.
    reconnect_status: Arc<Mutex<Option<ReconnectStatus>>>,
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
            progress: Default::default(),
            reconnect_status: Default::default(),
        }
    }
}

impl ViewerResumeState {
    /// A session's state, with remote audio muted as the viewer last left it.
    pub fn with_audio_muted(muted: bool) -> Self {
        Self {
            audio: meshrmm_audio::PlaybackState::new(muted),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn audio_muted(&self) -> bool {
        self.audio.muted()
    }

    /// Keeps the window of a connection that failed with `error` up, showing
    /// why it is reconnecting, until the next connection opens its own.
    fn keep_while_reconnecting(&self, active: ActivePresenter, error: &anyhow::Error) {
        let status = self.record_failure(error, std::time::Instant::now());
        active.presenter.set_reconnect_status(Some(status));
        let previous = self
            .reconnecting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .replace(active);
        if let Some(mut previous) = previous {
            previous.presenter.stop();
        }
    }

    fn reconnect_status(&self) -> std::sync::MutexGuard<'_, Option<ReconnectStatus>> {
        self.reconnect_status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Records that an attempt failed with `error` and returns the new
    /// status. It is not shown until [`Self::update_reconnect_status`] or
    /// [`Self::keep_while_reconnecting`] passes it on.
    pub fn record_failure(
        &self,
        error: &anyhow::Error,
        now: std::time::Instant,
    ) -> ReconnectStatus {
        let reason = crate::reconnect::classify(error);
        let mut current = self.reconnect_status();
        let status = ReconnectStatus::after_failure(*current, reason, now);
        *current = Some(status);
        status
    }

    /// Changes the reconnect status, if there is one, and shows it in the
    /// kept window.
    pub fn update_reconnect_status(&self, update: impl FnOnce(&mut ReconnectStatus)) {
        let status = {
            let mut current = self.reconnect_status();
            let Some(status) = current.as_mut() else {
                return;
            };
            update(status);
            *status
        };
        if let Some(kept) = self
            .reconnecting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            kept.presenter.set_reconnect_status(Some(status));
        }
    }

    /// Sets what the kept window shows next: waiting out the backoff, or
    /// attempting to reconnect.
    pub fn set_reconnect_phase(&self, phase: ReconnectPhase) {
        self.update_reconnect_status(|status| status.phase = phase);
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

    fn progress(&self) -> std::sync::MutexGuard<'_, AttemptProgress> {
        self.progress
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Starts tracking a new connection attempt.
    pub fn begin_attempt(&self) {
        self.progress().begin_attempt();
    }

    /// Whether any attempt has shown the remote display.
    pub fn ever_presented(&self) -> bool {
        self.progress().ever_presented()
    }

    /// How long the current attempt has shown the remote display, if it has.
    pub fn attempt_streamed_for(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.progress().attempt_streamed_for(now)
    }

    pub fn select_background_display(&self) {
        *self
            .display_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(meshrmm_protocol::BACKGROUND_DISPLAY_ID);
    }
}

#[cfg(test)]
mod presentation_progress_tests {
    use super::*;
    use crate::reconnect::{Disposition, ReconnectReason};
    use std::time::{Duration, Instant};

    #[test]
    fn failed_decoding_before_the_first_displayed_frame_keeps_the_startup_cap() {
        let (presentation_failure, _) = mpsc::unbounded_channel();
        let progress = Arc::new(Mutex::new(AttemptProgress::default()));
        let now = Instant::now();
        let reconnect_status = Arc::new(Mutex::new(Some(ReconnectStatus::after_failure(
            None,
            ReconnectReason::VideoRestarting,
            now,
        ))));
        let lifecycle = ReceiverLifecycle {
            presentation_failure,
            shutting_down: Default::default(),
            progress: Arc::clone(&progress),
            reconnect_status: Arc::clone(&reconnect_status),
        };
        let error =
            SessionFailure::new(FailureKind::PresentationFailed, "decoder rejected input").into();
        for attempt in 1..=3 {
            progress.lock().unwrap().begin_attempt();
            // Encoded frames may have arrived or been accepted into a queue,
            // but neither operation acknowledges native presentation.
            lifecycle.observe_presentation(None);
            let progress = progress.lock().unwrap();
            assert!(!progress.ever_presented());
            assert_eq!(progress.attempt_streamed_for(now), None);
            assert_eq!(
                crate::reconnect::disposition(
                    &error,
                    progress.ever_presented(),
                    attempt,
                    Duration::from_secs(5)
                ),
                if attempt == 3 {
                    Disposition::GiveUp
                } else {
                    Disposition::Retry
                }
            );
            assert!(reconnect_status.lock().unwrap().is_some());
        }
        lifecycle.observe_presentation(Some(now));
        // Later health polls preserve the actual first presentation time.
        lifecycle.observe_presentation(Some(now + Duration::from_secs(2)));
        assert_eq!(
            progress
                .lock()
                .unwrap()
                .attempt_streamed_for(now + Duration::from_secs(5)),
            Some(Duration::from_secs(5))
        );
        assert!(reconnect_status.lock().unwrap().is_none());
        assert_eq!(
            crate::reconnect::disposition(
                &error,
                progress.lock().unwrap().ever_presented(),
                3,
                Duration::from_secs(70)
            ),
            Disposition::Retry
        );
    }
}
