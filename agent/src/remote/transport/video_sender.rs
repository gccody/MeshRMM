use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use meshrmm_protocol::{DEFAULT_FRAGMENT_PAYLOAD, VideoStreamId, fragment_frame};
use tokio::sync::{Notify, mpsc};
use webrtc::data_channel::RTCDataChannel;

use super::ControlCommand;
use crate::remote::bitrate::{
    AdaptiveBitrate, EncoderStatus, RestartLadder, VideoPacer, video_pacing_bitrate,
};
use crate::remote::platform::monotonic_timestamp_us;
use crate::remote::sender_failure::transport_failure;
use crate::remote::video::LatestFrameSlot;

const VIDEO_BUFFER_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(80);
const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_video_sender(
    channel: Arc<RTCDataChannel>,
    open: Arc<Notify>,
    decoder_ready: Arc<Notify>,
    slot: Arc<LatestFrameSlot>,
    recovery: mpsc::UnboundedSender<ControlCommand>,
    quality_ceiling: Arc<AtomicU32>,
    encoder_status: Arc<EncoderStatus>,
    audio_bits: Arc<AtomicU32>,
    failure: mpsc::UnboundedSender<anyhow::Error>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        open.notified().await;
        // Control and video use independent SCTP streams. Wait for the viewer
        // to confirm its decoder/presenter is initialized before sending the
        // bootstrap keyframe. Discard predictive frames captured during
        // connection setup, but retain the newest IDR so a completely static
        // login screen can paint immediately. We still request a fresh IDR to
        // establish a current predictive chain for subsequent frames.
        decoder_ready.notified().await;
        slot.clear_pending();
        let _ = recovery.send(ControlCommand::Keyframe);
        let mut bootstrap_keyframe =
            match tokio::time::timeout(std::time::Duration::from_secs(10), slot.keyframe()).await {
                Ok(frame) => {
                    slot.discard_through(frame.frame_id);
                    Some(frame)
                }
                Err(_) => {
                    let _ = failure.send(anyhow::anyhow!(
                        "capture/encoder produced no bootstrap keyframe for 10 seconds"
                    ));
                    return;
                }
            };
        let mut frames_sent = 0_u64;
        let mut buffered_frames_dropped = 0_u64;
        let mut obsolete_frames_dropped = 0_u64;
        let mut recovery_frames_dropped = 0_u64;
        let mut bytes_sent = 0_u64;
        let mut stats_started_us = monotonic_timestamp_us();
        let mut last_sent = None::<(VideoStreamId, u64)>;
        let mut recovering = false;
        let mut last_keyframe_request_us = 0_u64;
        let mut bitrate = AdaptiveBitrate::new(quality_ceiling.load(Ordering::Acquire).max(1));
        let mut ladder = RestartLadder::new(quality_ceiling.load(Ordering::Acquire).max(1));
        let mut pacer = VideoPacer::default();
        loop {
            let source = if let Some(frame) = bootstrap_keyframe.take() {
                frame
            } else {
                // Desktop Duplication may produce no frame while the display is
                // completely static. The main sender loop independently polls
                // the capture worker for real failures, so idleness is not an
                // error and this task can wait until the next changed frame.
                slot.next().await
            };

            let mut reference_chain_lost = false;
            if let Some((last_stream_id, last_frame_id)) = last_sent
                && (source.stream_id != last_stream_id
                    || source.frame_id != last_frame_id.wrapping_add(1))
                && !source.keyframe
            {
                recovering = true;
                reference_chain_lost = true;
                tracing::warn!(
                    last_frame_id,
                    frame_id = source.frame_id,
                    stream_id = source.stream_id.0,
                    "encoded reference frame was skipped; waiting for a recovery keyframe"
                );
            }

            let now_us = monotonic_timestamp_us();
            let requested_maximum = quality_ceiling.load(Ordering::Acquire).max(1);
            let live_bitrate = encoder_status.live_bitrate();
            let encoder_bitrate = encoder_status.bits_per_second();
            if let Some(bits_per_second) = bitrate.set_maximum(requested_maximum)
                && live_bitrate
            {
                let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
            }
            ladder.set_maximum(requested_maximum);
            let mut buffered_bytes = channel.buffered_amount().await;
            if live_bitrate {
                if let Some(bits_per_second) =
                    bitrate.observe(now_us, buffered_bytes, slot.len(), reference_chain_lost)
                {
                    let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
                    tracing::info!(
                        bits_per_second,
                        "adapted video bitrate to current transport capacity"
                    );
                }
            } else {
                // Judge congestion against the rate the encoder actually
                // runs at, and change it only by restarting the encoder.
                bitrate.rebase(encoder_bitrate);
                let queued_frames = slot.len();
                if let Some(bits_per_second) = ladder.observe(
                    now_us,
                    encoder_bitrate,
                    bitrate.congested(buffered_bytes, queued_frames, reference_chain_lost),
                    bitrate.healthy(buffered_bytes, queued_frames),
                    encoder_status.recording(),
                ) {
                    let _ = recovery.send(ControlCommand::RestartBitrate(bits_per_second));
                    tracing::info!(
                        previous_bits_per_second = encoder_bitrate,
                        bits_per_second,
                        buffered_bytes,
                        queued_frames,
                        "requested a video encoder restart at a new bitrate step"
                    );
                }
            }
            if recovering && !source.keyframe {
                recovery_frames_dropped += 1;
                if last_keyframe_request_us == 0
                    || now_us.saturating_sub(last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
                {
                    let _ = recovery.send(ControlCommand::Keyframe);
                    last_keyframe_request_us = now_us.max(1);
                }
                continue;
            }

            let congested_bytes = bitrate.congested_bytes();
            let drain_bytes = bitrate.drain_bytes();
            if buffered_bytes >= congested_bytes {
                let drain_started = tokio::time::Instant::now();
                while buffered_bytes > drain_bytes
                    && drain_started.elapsed() < VIDEO_BUFFER_DRAIN_TIMEOUT
                {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    buffered_bytes = channel.buffered_amount().await;
                }
            }
            if buffered_bytes >= congested_bytes {
                buffered_frames_dropped += 1;
                recovering = true;
                let queued_frames_dropped = slot.drop_pending();
                if live_bitrate
                    && let Some(bits_per_second) =
                        bitrate.observe(now_us, buffered_bytes, queued_frames_dropped, true)
                {
                    let _ = recovery.send(ControlCommand::Bitrate(bits_per_second));
                    tracing::warn!(
                        bits_per_second,
                        buffered_bytes,
                        queued_frames_dropped,
                        "transport stayed congested; reduced bitrate and reset the predictive chain"
                    );
                }
                if last_keyframe_request_us == 0
                    || now_us.saturating_sub(last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
                {
                    let _ = recovery.send(ControlCommand::Keyframe);
                    last_keyframe_request_us = now_us.max(1);
                }
                tracing::debug!(
                    frame_id = source.frame_id,
                    buffered_bytes,
                    "dropping frame and requesting H.264 recovery after the transport drain deadline"
                );
                continue;
            }
            let mut frame = (*source).clone();
            frame.send_timestamp_us = monotonic_timestamp_us();
            let packets = match fragment_frame(&frame, DEFAULT_FRAGMENT_PAYLOAD) {
                Ok(packets) => packets,
                Err(error) => {
                    tracing::warn!(error = %error, frame_id = frame.frame_id, "failed to fragment encoded frame");
                    continue;
                }
            };
            let mut complete = true;
            for packet in packets {
                let bytes = match packet.encode() {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to encode video packet");
                        complete = false;
                        break;
                    }
                };
                let delay_us = pacer.reserve(
                    monotonic_timestamp_us(),
                    bytes.len(),
                    video_pacing_bitrate(
                        quality_ceiling.load(Ordering::Acquire),
                        audio_bits.load(Ordering::Relaxed),
                    ),
                );
                if delay_us != 0 {
                    tokio::time::sleep(std::time::Duration::from_micros(delay_us)).await;
                }
                bytes_sent = bytes_sent.saturating_add(bytes.len() as u64);
                if let Err(error) = channel.send(&Bytes::from(bytes)).await {
                    tracing::warn!(error = %error, "video data channel send failed");
                    let _ = failure.send(transport_failure(format!(
                        "video data channel send failed: {error}"
                    )));
                    return;
                }
            }
            if complete {
                frames_sent += 1;
                last_sent = Some((source.stream_id, source.frame_id));
                if source.keyframe {
                    recovering = false;
                }
            } else {
                // Never continue a predictive chain after only part of an
                // access unit was submitted to SCTP.
                recovering = true;
                obsolete_frames_dropped += 1;
            }
            let now_us = monotonic_timestamp_us();
            let elapsed_us = now_us.saturating_sub(stats_started_us);
            if elapsed_us >= 2_000_000 {
                let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
                tracing::info!(
                    stream_fps = frames_sent as f64 / elapsed_seconds,
                    transport_bitrate_bits_per_second = bytes_sent as f64 * 8.0 / elapsed_seconds,
                    frames_sent,
                    buffered_frames_dropped,
                    obsolete_frames_dropped,
                    recovery_frames_dropped,
                    encoded_frames_dropped = slot.dropped(),
                    encoder_bitrate_bits_per_second = encoder_status.bits_per_second(),
                    "video transport statistics"
                );
                frames_sent = 0;
                buffered_frames_dropped = 0;
                obsolete_frames_dropped = 0;
                recovery_frames_dropped = 0;
                bytes_sent = 0;
                stats_started_us = now_us;
            }
        }
    })
}
