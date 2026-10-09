use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use meshrmm_protocol::{ChromaMode, Codec, DisplayId, SessionMessage, VideoProfile, VideoStreamId};
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
    let mut stream_id = VideoStreamId(1);
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
    let mut displays = started.displays.clone();
    let mut active_display = started.active_display.clone();
    let mut format = started.format;
    let configured_maximum_bitrate = format.bitrate_bits_per_second;
    quality_ceiling.store(configured_maximum_bitrate, Ordering::Release);
    let mut active_profile = format.profile();
    let mut viewer_profiles = vec![active_profile];
    let mut requested_chroma = ChromaMode::Yuv420;
    let mut capture_cursor = true;
    let mut recording = false;
    let mut viewer_controls_input = false;
    let mut rejected_profiles = Vec::new();
    let mut capture_running = true;
    let mut capture_unavailable_since = None::<std::time::Instant>;
    let mut capture_retry_after = std::time::Instant::now();
    let _ = started_tx.send(Ok(started));
    let mut desktop_interval = tokio::time::interval(DESKTOP_LIFECYCLE_INTERVAL);
    desktop_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        session_close.set_target(&active_display.session);
        // Live CodecAPI bitrate changes are unsafe on HEVC hardware encoders.
        encoder_status.publish(
            format.bitrate_bits_per_second,
            format.codec != Codec::H265,
            recording,
        );
        tokio::select! {
            biased;
            _ = stop.changed() => return Ok(()),
            command = commands.recv() => {
                let Some(command) = command else { return Ok(()); };
                match command {
                    ControlCommand::Keyframe => {
                        if let Err(error) = lock_streamer(&streamer)?.request_keyframe() {
                            tracing::warn!(error = %error, "could not request a keyframe while the desktop is changing");
                        }
                    }
                    ControlCommand::Bitrate(value) => {
                        // Several hardware HEVC MFTs accept the CodecAPI call and
                        // then terminate asynchronously on the next frame. That
                        // turns every AIMD adjustment into a capture restart and
                        // bootstrap keyframe. Keep HEVC at the selected quality
                        // preset; congestion handling can still drop frames and
                        // request recovery without destabilizing the encoder.
                        // A queued adjustment from before a preset change must
                        // never raise the encoder above the new quality ceiling.
                        let value = value.min(quality_ceiling.load(Ordering::Acquire));
                        if let Err(error) = lock_streamer(&streamer)?.set_adaptive_bitrate(value) {
                            tracing::warn!(error = %error, "could not set bitrate while the desktop is changing");
                        }
                    }
                    ControlCommand::RestartBitrate(value) => {
                        // Restarting with static settings is the safe way to
                        // change an HEVC encoder's bitrate. Later restarts in
                        // this connection keep the congestion bitrate.
                        let ceiling = quality_ceiling.load(Ordering::Acquire);
                        let value = value.min(ceiling).max(1);
                        let previous = format.bitrate_bits_per_second;
                        if !capture_running || format.codec != Codec::H265 || value == previous {
                            continue;
                        }
                        lock_streamer(&streamer)?.set_congestion_bitrate((value < ceiling).then_some(value));
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let candidates = profile_candidates(&viewer_profiles, requested_chroma, &rejected_profiles);
                        match start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates) {
                            Ok(started) => {
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                                format = started.format;
                                capture_unavailable_since = None;
                                send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                }).await?;
                                tracing::info!(previous_bits_per_second = previous, bits_per_second = format.bitrate_bits_per_second, ?active_profile, stream_id = stream_id.0, "restarted the video encoder at a congestion bitrate step");
                            }
                            Err(error) => {
                                // A congestion step must never end the session;
                                // the desktop lifecycle retries the start.
                                capture_running = false;
                                capture_unavailable_since = Some(std::time::Instant::now());
                                capture_retry_after = std::time::Instant::now();
                                tracing::warn!(error = ?error, bits_per_second = value, "video encoder did not restart at a congestion bitrate step; retrying");
                            }
                        }
                    }
                    ControlCommand::Quality(quality)
                    | ControlCommand::ViewerCapabilities { quality, .. } => {
                        let value = quality.bitrate(configured_maximum_bitrate);
                        let mut headless_resolution = None;
                        if let ControlCommand::ViewerCapabilities { profiles, chroma, headless_resolution: resolution, .. } = command {
                            viewer_profiles = profiles;
                            requested_chroma = chroma;
                            rejected_profiles.clear();
                            headless_resolution = Some(resolution);
                        }
                        quality_ceiling.store(value, Ordering::Release);
                        // Recreate the encoder with its static bitrate settings.
                        // Live CodecAPI updates may be ignored, rejected, or even
                        // terminate HEVC encoders after the call reports success.
                        lock_streamer(&streamer)?.set_bitrate(value);
                        let mut capture_changed = lock_streamer(&streamer)?.set_quality(quality);
                        if let Some(resolution) = headless_resolution {
                            capture_changed |= lock_streamer(&streamer)?.set_headless_resolution(resolution);
                        }
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        if capture_running
                            && !capture_changed
                            && candidates.first() == Some(&active_profile)
                            && format.bitrate_bits_per_second == value
                        {
                            // Echo the settled configuration even when no
                            // restart is needed. The viewer deliberately does
                            // not paint the mandatory bootstrap profile until
                            // capability negotiation has completed.
                            send_control_message(
                                &control_channel,
                                SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                },
                            )
                            .await?;
                            tracing::info!(?active_profile, ?quality, ?requested_chroma, "video quality/profile selection retained active configuration");
                            continue;
                        }

                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let started = start_first_profile(
                            &streamer,
                            active_display.id,
                            stream_id,
                            &slot,
                            &candidates,
                        )?;
                        displays = started.displays;
                        active_display = started.active_display;
                        active_profile = started.format.profile();
                        format = started.format;
                        capture_running = true;
                        capture_unavailable_since = None;

                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                        tracing::info!(?active_profile, ?quality, bits_per_second = value, "video quality/profile selection applied");
                    }
                    ControlCommand::HeadlessResolution(resolution) => {
                        let restart = lock_streamer(&streamer)?.set_headless_resolution(resolution);
                        tracing::info!(width = resolution.width, height = resolution.height, restart, "viewer headless resolution applied");
                        if !restart || !capture_running {
                            continue;
                        }
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let candidates = profile_candidates(&viewer_profiles, requested_chroma, &rejected_profiles);
                        match start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates) {
                            Ok(started) => {
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                                format = started.format;
                                capture_unavailable_since = None;
                                send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                }).await?;
                            }
                            Err(error) => {
                                // The desktop lifecycle retries the start.
                                capture_running = false;
                                capture_unavailable_since = Some(std::time::Instant::now());
                                capture_retry_after = std::time::Instant::now();
                                tracing::warn!(error = ?error, "capture did not restart on the resized virtual display; retrying");
                            }
                        }
                    }
                    ControlCommand::DisplayBorder(enabled) => {
                        let result = lock_streamer(&streamer)?.set_display_border(enabled);
                        if let Err(error) = result {
                            send_control_message(&control_channel, SessionMessage::MaintenanceError { reason: format!("Display border: {error:#}") }).await?;
                        }
                    }
                    ControlCommand::CursorCapture(enabled) | ControlCommand::InputOwnership(enabled) | ControlCommand::Recording(enabled) => {
                        if matches!(command, ControlCommand::CursorCapture(_)) {
                            capture_cursor = enabled;
                            tracing::info!(enabled, "viewer cursor capture selection applied");
                        } else if matches!(command, ControlCommand::Recording(_)) {
                            recording = enabled;
                        } else {
                            viewer_controls_input = enabled;
                        }
                        let update = lock_streamer(&streamer)?.set_cursor_capture(capture_cursor_for_session(capture_cursor, viewer_controls_input, recording));
                        match update {
                            Ok(false) => {}
                            Err(error) => tracing::warn!(%error, "could not update cursor capture while the desktop is changing"),
                            Ok(true) => {
                                // The console-mode WGC backend needs its existing
                                // reconfiguration path; service capture updates in place.
                                lock_streamer(&streamer)?.stop()?;
                                slot.clear();
                                stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                                let candidates = profile_candidates(&viewer_profiles, requested_chroma, &rejected_profiles);
                                let started = start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates)?;
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                                format = started.format;
                                capture_running = true;
                                capture_unavailable_since = None;
                                send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format,
                                }).await?;
                            }
                        }
                    }
                    ControlCommand::Chroma(chroma) => {
                        requested_chroma = chroma;
                        rejected_profiles.clear();
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        if candidates.first() == Some(&active_profile) {
                            tracing::info!(?active_profile, ?requested_chroma, "chroma selection retained active profile");
                            continue;
                        }
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let started = start_first_profile(
                            &streamer,
                            active_display.id,
                            stream_id,
                            &slot,
                            &candidates,
                        )?;
                        displays = started.displays;
                        active_display = started.active_display;
                        active_profile = started.format.profile();
                        format = started.format;
                        capture_running = true;
                        capture_unavailable_since = None;

                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                        tracing::info!(?active_profile, ?requested_chroma, "viewer chroma selection applied");
                    }
                    ControlCommand::VideoProfileRejected { profile, reason } => {
                        if profile != active_profile {
                            tracing::warn!(?profile, reason, "viewer rejected an inactive video profile");
                            continue;
                        }
                        rejected_profiles.push(profile);
                        tracing::warn!(?profile, reason, "viewer rejected video profile; trying fallback");
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        lock_streamer(&streamer)?.stop()?;
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let started = start_first_profile(
                            &streamer,
                            active_display.id,
                            stream_id,
                            &slot,
                            &candidates,
                        )?;
                        displays = started.displays;
                        active_display = started.active_display;
                        active_profile = started.format.profile();
                        format = started.format;
                        capture_running = true;
                        capture_unavailable_since = None;

                        send_control_message(
                            &control_channel,
                            SessionMessage::DisplayConfiguration {
                                displays: displays.clone(),
                                active_display_id: active_display.id,
                                stream_id,
                                format: started.format,
                            },
                        ).await?;
                    }
                    ControlCommand::SelectDisplay(display_id) => {
                        if display_id == active_display.id && capture_running {
                            continue;
                        }
                        let Some(selected) = displays.iter().find(|display| display.id == display_id).cloned() else {
                            tracing::warn!(display_id = display_id.0, "viewer requested an unavailable display");
                            continue;
                        };
                        let switch_started = std::time::Instant::now();
                        capture_running = false;
                        capture_unavailable_since = Some(std::time::Instant::now());
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                        let candidates = profile_candidates(
                            &viewer_profiles,
                            requested_chroma,
                            &rejected_profiles,
                        );
                        let restart = lock_streamer(&streamer)?.switch_display(
                            selected.id, stream_id, Arc::clone(&slot),
                        );
                        let restart = match restart {
                            Ok(started) => Ok(started),
                            Err(error) => {
                                tracing::warn!(?error, "fast display switch failed; retrying supported profiles");
                                lock_streamer(&streamer)?.stop()?;
                                start_first_profile(&streamer, selected.id, stream_id, &slot, &candidates)
                            }
                        };
                        match restart {
                            Ok(started) => {
                                displays = started.displays;
                                active_display = started.active_display;
                                active_profile = started.format.profile();
                        format = started.format;
                                capture_running = true;
                                capture_unavailable_since = None;

                                send_control_message(
                                    &control_channel,
                                    SessionMessage::DisplayConfiguration {
                                        displays: displays.clone(),
                                        active_display_id: active_display.id,
                                        stream_id,
                                        format: started.format,
                                    },
                                ).await?;
                                tracing::info!(switch_ms = switch_started.elapsed().as_millis(), display_id = active_display.id.0, display_name = %active_display.name, stream_id = stream_id.0, "remote display switched");
                            }
                            Err(error) => {
                                if selected.session != active_display.session
                                {
                                    send_control_message(&control_channel, SessionMessage::MaintenanceError {
                                        reason: format!("{} session could not start: {error:#}", selected.session.label()),
                                    }).await?;
                                    // A failed session switch must not strand the viewer
                                    // on a blank desktop or silently inject console input.
                                    let restored = start_first_profile(&streamer, active_display.id, stream_id, &slot, &candidates)?;
                                    displays = restored.displays;
                                    active_display = restored.active_display;
                                    active_profile = restored.format.profile();
                                    format = restored.format;
                                    capture_running = true;
                                    capture_unavailable_since = None;
                                    send_control_message(&control_channel, SessionMessage::DisplayConfiguration {
                                        displays: displays.clone(), active_display_id: active_display.id,
                                        stream_id, format,
                                    }).await?;
                                } else {
                                    active_display = selected;
                                    capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
                                    tracing::warn!(error = ?error, "display switch is waiting for an interactive desktop");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ = desktop_interval.tick() => {
                if capture_running {
                    let capture_ended = lock_streamer(&streamer)?.poll_ended();
                    if let Some(capture_result) = capture_ended {
                        if let Err(error) = capture_result {
                            tracing::warn!(error = ?error, stream_id = stream_id.0, ?active_profile, configured_bitrate_bits_per_second = quality_ceiling.load(Ordering::Acquire), "visible desktop changed; restarting capture");
                        } else {
                            tracing::warn!(stream_id = stream_id.0, ?active_profile, configured_bitrate_bits_per_second = quality_ceiling.load(Ordering::Acquire), "desktop capture helper stopped; replacing it");
                        }
                        capture_running = false;
                        capture_unavailable_since = Some(std::time::Instant::now());
                        capture_retry_after = std::time::Instant::now();
                        slot.clear();
                        stream_id = VideoStreamId(stream_id.0.wrapping_add(1).max(1));
                    }
                }
                if !capture_running && std::time::Instant::now() >= capture_retry_after {
                    let candidates = profile_candidates(
                        &viewer_profiles,
                        requested_chroma,
                        &rejected_profiles,
                    );
                    let restart = start_first_profile(
                        &streamer,
                        active_display.id,
                        stream_id,
                        &slot,
                        &candidates,
                    );
                    match restart {
                        Ok(started) => {
                            displays = started.displays;
                            active_display = started.active_display;
                            active_profile = started.format.profile();
                        format = started.format;
                            capture_running = true;

                            let recovery_ms = capture_unavailable_since
                                .take()
                                .map(|started| started.elapsed().as_millis())
                                .unwrap_or_default();
                            send_control_message(
                                &control_channel,
                                SessionMessage::DisplayConfiguration {
                                    displays: displays.clone(),
                                    active_display_id: active_display.id,
                                    stream_id,
                                    format: started.format,
                                },
                            ).await?;
                            tracing::info!(stream_id = stream_id.0, display_id = active_display.id.0, recovery_ms, "remote session moved to the visible desktop");
                        }
                        Err(error) => {
                            capture_retry_after = std::time::Instant::now() + DESKTOP_RETRY_INTERVAL;
                            tracing::warn!(error = ?error, "waiting for a login or application desktop");
                        }
                    }
                }
            },

        }
    }
}
