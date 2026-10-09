use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use meshrmm_protocol::{
    ChromaMode, Codec, Display, DisplayId, HeadlessResolution, QualityPreset, SessionMessage,
    VideoFormat, VideoProfile, VideoStreamId,
};
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;

use super::control_channel::send_control_message;
use super::{CaptureStartup, ControlCommand, capture_cursor_for_session, lock_streamer};
use crate::remote::platform::{ScreenStreamer, StartedScreen};
use crate::remote::sender_failure::{initial_start_error, profile_start_error};
use crate::remote::video::LatestFrameSlot;

const DESKTOP_LIFECYCLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const DESKTOP_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

pub(super) fn profile_candidates(
    profiles: &[VideoProfile],
    requested_chroma: ChromaMode,
    rejected: &[VideoProfile],
) -> Vec<VideoProfile> {
    let mut candidates = Vec::new();
    for chroma in [requested_chroma, ChromaMode::Yuv420] {
        for codec in [Codec::H265, Codec::H264] {
            let profile = VideoProfile { codec, chroma };
            if profiles.contains(&profile)
                && !rejected.contains(&profile)
                && !candidates.contains(&profile)
            {
                candidates.push(profile);
            }
        }
    }
    candidates
}

fn start_first_profile(
    streamer: &Arc<Mutex<Box<dyn ScreenStreamer>>>,
    display_id: DisplayId,
    stream_id: VideoStreamId,
    slot: &Arc<LatestFrameSlot>,
    candidates: &[VideoProfile],
) -> anyhow::Result<StartedScreen> {
    let mut failures = Vec::new();
    for profile in candidates {
        let result = {
            let mut streamer = lock_streamer(streamer)?;
            streamer.set_codec(profile.codec);
            streamer.set_chroma(profile.chroma);
            streamer.start(Some(display_id), stream_id, Arc::clone(slot))
        };
        match result {
            Ok(started) => return Ok(started),
            Err(error) => {
                tracing::warn!(?profile, error = ?error, "encoder profile unavailable");
                failures.push((*profile, error));
            }
        }
    }
    Err(profile_start_error(failures))
}

pub(super) async fn run_capture_control(
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    slot: Arc<LatestFrameSlot>,
    control_channel: Arc<RTCDataChannel>,
    startup: CaptureStartup,
    mut commands: mpsc::Receiver<ControlCommand>,
    started_tx: tokio::sync::oneshot::Sender<anyhow::Result<StartedScreen>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let stream_id = VideoStreamId(1);
    let CaptureStartup {
        quality_ceiling,
        encoder_status,
        initial_display,
        session_close,
    } = startup;
    // Congestion steps belong to one connection; a resumed sender starts at
    // the quality bitrate and adapts again.
    lock_streamer(&streamer)?.set_congestion_bitrate(None);
    let started = lock_streamer(&streamer)?.start(initial_display, stream_id, Arc::clone(&slot));
    let started = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = started_tx.send(Err(initial_start_error(error)));
            return Ok(());
        }
    };
    let format = started.format;
    let configured_maximum_bitrate = format.bitrate_bits_per_second;
    quality_ceiling.store(configured_maximum_bitrate, Ordering::Release);
    let active_profile = format.profile();
    let mut capture = CaptureControl {
        streamer,
        slot,
        control_channel,
        quality_ceiling,
        stream_id,
        displays: started.displays.clone(),
        active_display: started.active_display.clone(),
        format,
        configured_maximum_bitrate,
        active_profile,
        viewer_profiles: vec![active_profile],
        requested_chroma: ChromaMode::Yuv420,
        capture_cursor: true,
        recording: false,
        viewer_controls_input: false,
        rejected_profiles: Vec::new(),
        capture_running: true,
        capture_unavailable_since: None,
        capture_retry_after: std::time::Instant::now(),
    };
    let _ = started_tx.send(Ok(started));
    let mut desktop_interval = tokio::time::interval(DESKTOP_LIFECYCLE_INTERVAL);
    desktop_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        session_close.set_target(&capture.active_display.session);
        // Live CodecAPI bitrate changes are unsafe on HEVC hardware encoders.
        encoder_status.publish(
            capture.format.bitrate_bits_per_second,
            capture.format.codec != Codec::H265,
            capture.recording,
        );
        tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                capture.apply(command).await?;
            }
            _ = desktop_interval.tick() => capture.poll_desktop().await?,
        }
    }
}

struct CaptureControl {
    streamer: Arc<Mutex<Box<dyn ScreenStreamer>>>,
    slot: Arc<LatestFrameSlot>,
    control_channel: Arc<RTCDataChannel>,
    quality_ceiling: Arc<AtomicU32>,
    stream_id: VideoStreamId,
    displays: Vec<Display>,
    active_display: Display,
    format: VideoFormat,
    configured_maximum_bitrate: u32,
    active_profile: VideoProfile,
    viewer_profiles: Vec<VideoProfile>,
    requested_chroma: ChromaMode,
    capture_cursor: bool,
    recording: bool,
    viewer_controls_input: bool,
    rejected_profiles: Vec<VideoProfile>,
    capture_running: bool,
    capture_unavailable_since: Option<std::time::Instant>,
    capture_retry_after: std::time::Instant,
}

impl CaptureControl {
    async fn apply(&mut self, command: ControlCommand) -> anyhow::Result<()> {
        match command {
            ControlCommand::Keyframe => {
                if let Err(error) = lock_streamer(&self.streamer)?.request_keyframe() {
                    tracing::warn!(error = %error, "could not request a keyframe while the desktop is changing");
                }
            }
            ControlCommand::Bitrate(value) => self.set_adaptive_bitrate(value)?,
            ControlCommand::RestartBitrate(value) => self.restart_at_bitrate(value).await?,
            ControlCommand::Quality(quality) => self.apply_quality(quality, None).await?,
            ControlCommand::ViewerCapabilities {
                profiles,
                quality,
                chroma,
                headless_resolution,
            } => {
                self.viewer_profiles = profiles;
                self.requested_chroma = chroma;
                self.rejected_profiles.clear();
                self.apply_quality(quality, Some(headless_resolution))
                    .await?;
            }
            ControlCommand::HeadlessResolution(resolution) => {
                self.apply_headless_resolution(resolution).await?;
            }
            ControlCommand::DisplayBorder(enabled) => {
                let result = lock_streamer(&self.streamer)?.set_display_border(enabled);
                if let Err(error) = result {
                    send_control_message(
                        &self.control_channel,
                        SessionMessage::MaintenanceError {
                            reason: format!("Display border: {error:#}"),
                        },
                    )
                    .await?;
                }
            }
            ControlCommand::CursorCapture(enabled) => {
                self.capture_cursor = enabled;
                tracing::info!(enabled, "viewer cursor capture selection applied");
                self.update_cursor_capture().await?;
            }
            ControlCommand::Recording(enabled) => {
                self.recording = enabled;
                self.update_cursor_capture().await?;
            }
            ControlCommand::InputOwnership(enabled) => {
                self.viewer_controls_input = enabled;
                self.update_cursor_capture().await?;
            }
            ControlCommand::Chroma(chroma) => self.apply_chroma(chroma).await?,
            ControlCommand::VideoProfileRejected { profile, reason } => {
                self.reject_profile(profile, reason).await?;
            }
            ControlCommand::SelectDisplay(display_id) => self.select_display(display_id).await?,
            _ => {}
        }
        Ok(())
    }

    fn candidates(&self) -> Vec<VideoProfile> {
        profile_candidates(
            &self.viewer_profiles,
            self.requested_chroma,
            &self.rejected_profiles,
        )
    }

    fn start(
        &self,
        display_id: DisplayId,
        candidates: &[VideoProfile],
    ) -> anyhow::Result<StartedScreen> {
        start_first_profile(
            &self.streamer,
            display_id,
            self.stream_id,
            &self.slot,
            candidates,
        )
    }

    fn next_stream(&mut self) {
        self.slot.clear();
        self.stream_id = VideoStreamId(self.stream_id.0.wrapping_add(1).max(1));
    }

    fn stop_capture(&mut self) -> anyhow::Result<()> {
        lock_streamer(&self.streamer)?.stop()?;
        self.next_stream();
        Ok(())
    }

    fn mark_unavailable(&mut self) {
        self.capture_running = false;
        self.capture_unavailable_since = Some(std::time::Instant::now());
        self.capture_retry_after = std::time::Instant::now();
    }

    fn adopt(&mut self, started: StartedScreen) {
        self.displays = started.displays;
        self.active_display = started.active_display;
        self.active_profile = started.format.profile();
        self.format = started.format;
    }

    async fn resume(&mut self, started: StartedScreen) -> anyhow::Result<()> {
        self.adopt(started);
        self.capture_running = true;
        self.capture_unavailable_since = None;
        self.send_configuration().await
    }

    async fn send_configuration(&self) -> anyhow::Result<()> {
        send_control_message(
            &self.control_channel,
            SessionMessage::DisplayConfiguration {
                displays: self.displays.clone(),
                active_display_id: self.active_display.id,
                stream_id: self.stream_id,
                format: self.format,
            },
        )
        .await
    }

    fn set_adaptive_bitrate(&self, value: u32) -> anyhow::Result<()> {
        // Several hardware HEVC MFTs accept the CodecAPI call and
        // then terminate asynchronously on the next frame. That
        // turns every AIMD adjustment into a capture restart and
        // bootstrap keyframe. Keep HEVC at the selected quality
        // preset; congestion handling can still drop frames and
        // request recovery without destabilizing the encoder.
        // A queued adjustment from before a preset change must
        // never raise the encoder above the new quality ceiling.
        let value = value.min(self.quality_ceiling.load(Ordering::Acquire));
        if let Err(error) = lock_streamer(&self.streamer)?.set_adaptive_bitrate(value) {
            tracing::warn!(error = %error, "could not set bitrate while the desktop is changing");
        }
        Ok(())
    }

    async fn restart_at_bitrate(&mut self, value: u32) -> anyhow::Result<()> {
        // Restarting with static settings is the safe way to
        // change an HEVC encoder's bitrate. Later restarts in
        // this connection keep the congestion bitrate.
        let ceiling = self.quality_ceiling.load(Ordering::Acquire);
        let value = value.min(ceiling).max(1);
        let previous = self.format.bitrate_bits_per_second;
        if !self.capture_running || self.format.codec != Codec::H265 || value == previous {
            return Ok(());
        }
        lock_streamer(&self.streamer)?.set_congestion_bitrate((value < ceiling).then_some(value));
        self.stop_capture()?;
        let candidates = self.candidates();
        match self.start(self.active_display.id, &candidates) {
            Ok(started) => {
                self.resume(started).await?;
                tracing::info!(
                    previous_bits_per_second = previous,
                    bits_per_second = self.format.bitrate_bits_per_second,
                    active_profile = ?self.active_profile,
                    stream_id = self.stream_id.0,
                    "restarted the video encoder at a congestion bitrate step"
                );
            }
            Err(error) => {
                // A congestion step must never end the session;
                // the desktop lifecycle retries the start.
                self.mark_unavailable();
                tracing::warn!(error = ?error, bits_per_second = value, "video encoder did not restart at a congestion bitrate step; retrying");
            }
        }
        Ok(())
    }

    async fn apply_quality(
        &mut self,
        quality: QualityPreset,
        headless_resolution: Option<HeadlessResolution>,
    ) -> anyhow::Result<()> {
        let value = quality.bitrate(self.configured_maximum_bitrate);
        self.quality_ceiling.store(value, Ordering::Release);
        // Recreate the encoder with its static bitrate settings.
        // Live CodecAPI updates may be ignored, rejected, or even
        // terminate HEVC encoders after the call reports success.
        lock_streamer(&self.streamer)?.set_bitrate(value);
        let mut capture_changed = lock_streamer(&self.streamer)?.set_quality(quality);
        if let Some(resolution) = headless_resolution {
            capture_changed |= lock_streamer(&self.streamer)?.set_headless_resolution(resolution);
        }
        let candidates = self.candidates();
        if self.capture_running
            && !capture_changed
            && candidates.first() == Some(&self.active_profile)
            && self.format.bitrate_bits_per_second == value
        {
            // Echo the settled configuration even when no
            // restart is needed. The viewer deliberately does
            // not paint the mandatory bootstrap profile until
            // capability negotiation has completed.
            self.send_configuration().await?;
            tracing::info!(
                active_profile = ?self.active_profile,
                ?quality,
                requested_chroma = ?self.requested_chroma,
                "video quality/profile selection retained active configuration"
            );
            return Ok(());
        }

        self.stop_capture()?;
        let started = self.start(self.active_display.id, &candidates)?;
        self.resume(started).await?;
        tracing::info!(
            active_profile = ?self.active_profile,
            ?quality,
            bits_per_second = value,
            "video quality/profile selection applied"
        );
        Ok(())
    }

    async fn apply_headless_resolution(
        &mut self,
        resolution: HeadlessResolution,
    ) -> anyhow::Result<()> {
        let restart = lock_streamer(&self.streamer)?.set_headless_resolution(resolution);
        tracing::info!(
            width = resolution.width,
            height = resolution.height,
            restart,
            "viewer headless resolution applied"
        );
        if !restart || !self.capture_running {
            return Ok(());
        }
        self.stop_capture()?;
        let candidates = self.candidates();
        match self.start(self.active_display.id, &candidates) {
            Ok(started) => self.resume(started).await?,
            Err(error) => {
                // The desktop lifecycle retries the start.
                self.mark_unavailable();
                tracing::warn!(error = ?error, "capture did not restart on the resized virtual display; retrying");
            }
        }
        Ok(())
    }

    async fn update_cursor_capture(&mut self) -> anyhow::Result<()> {
        let update = lock_streamer(&self.streamer)?.set_cursor_capture(capture_cursor_for_session(
            self.capture_cursor,
            self.viewer_controls_input,
            self.recording,
        ));
        match update {
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %error,
                "could not update cursor capture while the desktop is changing"
            ),
            Ok(true) => {
                // The console-mode WGC backend needs its existing
                // reconfiguration path; service capture updates in place.
                self.stop_capture()?;
                let candidates = self.candidates();
                let started = self.start(self.active_display.id, &candidates)?;
                self.resume(started).await?;
            }
        }
        Ok(())
    }

    async fn apply_chroma(&mut self, chroma: ChromaMode) -> anyhow::Result<()> {
        self.requested_chroma = chroma;
        self.rejected_profiles.clear();
        let candidates = self.candidates();
        if candidates.first() == Some(&self.active_profile) {
            tracing::info!(
                active_profile = ?self.active_profile,
                requested_chroma = ?self.requested_chroma,
                "chroma selection retained active profile"
            );
            return Ok(());
        }
        self.stop_capture()?;
        let started = self.start(self.active_display.id, &candidates)?;
        self.resume(started).await?;
        tracing::info!(
            active_profile = ?self.active_profile,
            requested_chroma = ?self.requested_chroma,
            "viewer chroma selection applied"
        );
        Ok(())
    }

    async fn reject_profile(
        &mut self,
        profile: VideoProfile,
        reason: String,
    ) -> anyhow::Result<()> {
        if profile != self.active_profile {
            tracing::warn!(
                ?profile,
                reason,
                "viewer rejected an inactive video profile"
            );
            return Ok(());
        }
        self.rejected_profiles.push(profile);
        tracing::warn!(
            ?profile,
            reason,
            "viewer rejected video profile; trying fallback"
        );
        let candidates = self.candidates();
        self.stop_capture()?;
        let started = self.start(self.active_display.id, &candidates)?;
        self.resume(started).await
    }

    async fn select_display(&mut self, display_id: DisplayId) -> anyhow::Result<()> {
        if display_id == self.active_display.id && self.capture_running {
            return Ok(());
        }
        let Some(selected) = self
            .displays
            .iter()
            .find(|display| display.id == display_id)
            .cloned()
        else {
            tracing::warn!(
                display_id = display_id.0,
                "viewer requested an unavailable display"
            );
            return Ok(());
        };
        let switch_started = std::time::Instant::now();
        self.capture_running = false;
        self.capture_unavailable_since = Some(std::time::Instant::now());
        self.next_stream();
        let candidates = self.candidates();
        let restart = lock_streamer(&self.streamer)?.switch_display(
            selected.id,
            self.stream_id,
            Arc::clone(&self.slot),
        );
        let restart = match restart {
            Ok(started) => Ok(started),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "fast display switch failed; retrying supported profiles"
                );
                lock_streamer(&self.streamer)?.stop()?;
                self.start(selected.id, &candidates)
            }
        };
        match restart {
            Ok(started) => {
                self.resume(started).await?;
                tracing::info!(switch_ms = switch_started.elapsed().as_millis(), display_id = self.active_display.id.0, display_name = %self.active_display.name, stream_id = self.stream_id.0, "remote display switched");
            }
            Err(error) => {
                self.recover_failed_switch(selected, &candidates, error)
                    .await?
            }
        }
        Ok(())
    }

    async fn recover_failed_switch(
        &mut self,
        selected: Display,
        candidates: &[VideoProfile],
        error: anyhow::Error,
    ) -> anyhow::Result<()> {
        if selected.session != self.active_display.session {
            send_control_message(
                &self.control_channel,
                SessionMessage::MaintenanceError {
                    reason: format!(
                        "{} session could not start: {error:#}",
                        selected.session.label()
                    ),
                },
            )
            .await?;
            // A failed session switch must not strand the viewer
            // on a blank desktop or silently inject console input.
            let restored = self.start(self.active_display.id, candidates)?;
            self.resume(restored).await?;
        } else {
            self.active_display = selected;
            self.capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
            tracing::warn!(error = ?error, "display switch is waiting for an interactive desktop");
        }
        Ok(())
    }

    async fn poll_desktop(&mut self) -> anyhow::Result<()> {
        if self.capture_running {
            let capture_ended = lock_streamer(&self.streamer)?.poll_ended();
            if let Some(capture_result) = capture_ended {
                if let Err(error) = capture_result {
                    tracing::warn!(error = ?error, stream_id = self.stream_id.0, active_profile = ?self.active_profile, configured_bitrate_bits_per_second = self.quality_ceiling.load(Ordering::Acquire), "visible desktop changed; restarting capture");
                } else {
                    tracing::warn!(stream_id = self.stream_id.0, active_profile = ?self.active_profile, configured_bitrate_bits_per_second = self.quality_ceiling.load(Ordering::Acquire), "desktop capture helper stopped; replacing it");
                }
                self.mark_unavailable();
                self.next_stream();
            }
        }
        if !self.capture_running && std::time::Instant::now() >= self.capture_retry_after {
            self.retry_capture().await?;
        }
        Ok(())
    }

    async fn retry_capture(&mut self) -> anyhow::Result<()> {
        let candidates = self.candidates();
        match self.start(self.active_display.id, &candidates) {
            Ok(started) => {
                self.adopt(started);
                self.capture_running = true;
                let recovery_ms = self
                    .capture_unavailable_since
                    .take()
                    .map(|started| started.elapsed().as_millis())
                    .unwrap_or_default();
                self.send_configuration().await?;
                tracing::info!(
                    stream_id = self.stream_id.0,
                    display_id = self.active_display.id.0,
                    recovery_ms,
                    "remote session moved to the visible desktop"
                );
            }
            Err(error) => {
                self.capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
                tracing::warn!(error = ?error, "waiting for a login or application desktop");
            }
        }
        Ok(())
    }
}
