use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use meshrmm_protocol::{DEFAULT_FRAGMENT_PAYLOAD, EncodedFrame, VideoStreamId, fragment_frame};
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
        let Some(bootstrap_keyframe) = bootstrap_keyframe(&slot, &recovery, &failure).await else {
            return;
        };
        let stats = VideoStats::new();
        let bitrate = AdaptiveBitrate::new(quality_ceiling.load(Ordering::Acquire).max(1));
        let ladder = RestartLadder::new(quality_ceiling.load(Ordering::Acquire).max(1));
        VideoSender {
            channel,
            slot,
            recovery,
            quality_ceiling,
            encoder_status,
            audio_bits,
            failure,
            stats,
            last_sent: None,
            recovering: false,
            last_keyframe_request_us: 0,
            bitrate,
            ladder,
            pacer: VideoPacer::default(),
        }
        .run(bootstrap_keyframe)
        .await;
    })
}

async fn bootstrap_keyframe(
    slot: &LatestFrameSlot,
    recovery: &mpsc::UnboundedSender<ControlCommand>,
    failure: &mpsc::UnboundedSender<anyhow::Error>,
) -> Option<Arc<EncodedFrame>> {
    slot.clear_pending();
    let _ = recovery.send(ControlCommand::Keyframe);
    match tokio::time::timeout(std::time::Duration::from_secs(10), slot.keyframe()).await {
        Ok(frame) => {
            slot.discard_through(frame.frame_id);
            Some(frame)
        }
        Err(_) => {
            let _ = failure.send(anyhow::anyhow!(
                "capture/encoder produced no bootstrap keyframe for 10 seconds"
            ));
            None
        }
    }
}

struct VideoStats {
    frames_sent: u64,
    buffered_frames_dropped: u64,
    obsolete_frames_dropped: u64,
    recovery_frames_dropped: u64,
    bytes_sent: u64,
    started_us: u64,
}

impl VideoStats {
    fn new() -> Self {
        Self {
            frames_sent: 0,
            buffered_frames_dropped: 0,
            obsolete_frames_dropped: 0,
            recovery_frames_dropped: 0,
            bytes_sent: 0,
            started_us: monotonic_timestamp_us(),
        }
    }

    fn log_if_due(&mut self, slot: &LatestFrameSlot, encoder_status: &EncoderStatus) {
        let now_us = monotonic_timestamp_us();
        let elapsed_us = now_us.saturating_sub(self.started_us);
        if elapsed_us < 2_000_000 {
            return;
        }
        let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
        tracing::info!(
            stream_fps = self.frames_sent as f64 / elapsed_seconds,
            transport_bitrate_bits_per_second = self.bytes_sent as f64 * 8.0 / elapsed_seconds,
            frames_sent = self.frames_sent,
            buffered_frames_dropped = self.buffered_frames_dropped,
            obsolete_frames_dropped = self.obsolete_frames_dropped,
            recovery_frames_dropped = self.recovery_frames_dropped,
            encoded_frames_dropped = slot.dropped(),
            encoder_bitrate_bits_per_second = encoder_status.bits_per_second(),
            "video transport statistics"
        );
        self.frames_sent = 0;
        self.buffered_frames_dropped = 0;
        self.obsolete_frames_dropped = 0;
        self.recovery_frames_dropped = 0;
        self.bytes_sent = 0;
        self.started_us = now_us;
    }
}

struct VideoSender {
    channel: Arc<RTCDataChannel>,
    slot: Arc<LatestFrameSlot>,
    recovery: mpsc::UnboundedSender<ControlCommand>,
    quality_ceiling: Arc<AtomicU32>,
    encoder_status: Arc<EncoderStatus>,
    audio_bits: Arc<AtomicU32>,
    failure: mpsc::UnboundedSender<anyhow::Error>,
    stats: VideoStats,
    last_sent: Option<(VideoStreamId, u64)>,
    recovering: bool,
    last_keyframe_request_us: u64,
    bitrate: AdaptiveBitrate,
    ladder: RestartLadder,
    pacer: VideoPacer,
}

impl VideoSender {
    async fn run(mut self, bootstrap_keyframe: Arc<EncodedFrame>) {
        let mut bootstrap_keyframe = Some(bootstrap_keyframe);
        loop {
            let source = if let Some(frame) = bootstrap_keyframe.take() {
                frame
            } else {
                // Desktop Duplication may produce no frame while the display is
                // completely static. The main sender loop independently polls
                // the capture worker for real failures, so idleness is not an
                // error and this task can wait until the next changed frame.
                self.slot.next().await
            };

            let reference_chain_lost = self.track_reference_chain(&source);
            let now_us = monotonic_timestamp_us();
            let (live_bitrate, buffered_bytes) =
                self.adapt_bitrate(now_us, reference_chain_lost).await;
            if self.recovering && !source.keyframe {
                self.stats.recovery_frames_dropped += 1;
                self.request_keyframe(now_us);
                continue;
            }
            if self
                .drop_congested_frame(&source, now_us, live_bitrate, buffered_bytes)
                .await
            {
                continue;
            }
            if self.send_frame(&source).await.is_break() {
                return;
            }
        }
    }

    fn track_reference_chain(&mut self, source: &EncodedFrame) -> bool {
        if let Some((last_stream_id, last_frame_id)) = self.last_sent
            && (source.stream_id != last_stream_id
                || source.frame_id != last_frame_id.wrapping_add(1))
            && !source.keyframe
        {
            self.recovering = true;
            tracing::warn!(
                last_frame_id,
                frame_id = source.frame_id,
                stream_id = source.stream_id.0,
                "encoded reference frame was skipped; waiting for a recovery keyframe"
            );
            return true;
        }
        false
    }

    /// Returns whether the encoder takes live bitrate changes, and the bytes
    /// buffered on the video channel.
    async fn adapt_bitrate(&mut self, now_us: u64, reference_chain_lost: bool) -> (bool, usize) {
        let requested_maximum = self.quality_ceiling.load(Ordering::Acquire).max(1);
        let live_bitrate = self.encoder_status.live_bitrate();
        let encoder_bitrate = self.encoder_status.bits_per_second();
        if let Some(bits_per_second) = self.bitrate.set_maximum(requested_maximum)
            && live_bitrate
        {
            let _ = self.recovery.send(ControlCommand::Bitrate(bits_per_second));
        }
        self.ladder.set_maximum(requested_maximum);
        let buffered_bytes = self.channel.buffered_amount().await;
        if live_bitrate {
            if let Some(bits_per_second) = self.bitrate.observe(
                now_us,
                buffered_bytes,
                self.slot.len(),
                reference_chain_lost,
            ) {
                let _ = self.recovery.send(ControlCommand::Bitrate(bits_per_second));
                tracing::info!(
                    bits_per_second,
                    "adapted video bitrate to current transport capacity"
                );
            }
        } else {
            // Judge congestion against the rate the encoder actually
            // runs at, and change it only by restarting the encoder.
            self.bitrate.rebase(encoder_bitrate);
            let queued_frames = self.slot.len();
            if let Some(bits_per_second) = self.ladder.observe(
                now_us,
                encoder_bitrate,
                self.bitrate
                    .congested(buffered_bytes, queued_frames, reference_chain_lost),
                self.bitrate.healthy(buffered_bytes, queued_frames),
                self.encoder_status.recording(),
            ) {
                let _ = self
                    .recovery
                    .send(ControlCommand::RestartBitrate(bits_per_second));
                tracing::info!(
                    previous_bits_per_second = encoder_bitrate,
                    bits_per_second,
                    buffered_bytes,
                    queued_frames,
                    "requested a video encoder restart at a new bitrate step"
                );
            }
        }
        (live_bitrate, buffered_bytes)
    }

    fn request_keyframe(&mut self, now_us: u64) {
        if self.last_keyframe_request_us == 0
            || now_us.saturating_sub(self.last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
        {
            let _ = self.recovery.send(ControlCommand::Keyframe);
            self.last_keyframe_request_us = now_us.max(1);
        }
    }

    /// Waits briefly for a congested channel to drain, and drops the frame
    /// when it does not.
    async fn drop_congested_frame(
        &mut self,
        source: &EncodedFrame,
        now_us: u64,
        live_bitrate: bool,
        mut buffered_bytes: usize,
    ) -> bool {
        let congested_bytes = self.bitrate.congested_bytes();
        let drain_bytes = self.bitrate.drain_bytes();
        if buffered_bytes >= congested_bytes {
            let drain_started = tokio::time::Instant::now();
            while buffered_bytes > drain_bytes
                && drain_started.elapsed() < VIDEO_BUFFER_DRAIN_TIMEOUT
            {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                buffered_bytes = self.channel.buffered_amount().await;
            }
        }
        if buffered_bytes < congested_bytes {
            return false;
        }
        self.stats.buffered_frames_dropped += 1;
        self.recovering = true;
        let queued_frames_dropped = self.slot.drop_pending();
        if live_bitrate
            && let Some(bits_per_second) =
                self.bitrate
                    .observe(now_us, buffered_bytes, queued_frames_dropped, true)
        {
            let _ = self.recovery.send(ControlCommand::Bitrate(bits_per_second));
            tracing::warn!(
                bits_per_second,
                buffered_bytes,
                queued_frames_dropped,
                "transport stayed congested; reduced bitrate and reset the predictive chain"
            );
        }
        self.request_keyframe(now_us);
        tracing::debug!(
            frame_id = source.frame_id,
            buffered_bytes,
            "dropping frame and requesting H.264 recovery after the transport drain deadline"
        );
        true
    }

    /// Breaks when the video channel can no longer send.
    async fn send_frame(&mut self, source: &EncodedFrame) -> ControlFlow<()> {
        let mut frame = source.clone();
        frame.send_timestamp_us = monotonic_timestamp_us();
        let packets = match fragment_frame(&frame, DEFAULT_FRAGMENT_PAYLOAD) {
            Ok(packets) => packets,
            Err(error) => {
                tracing::warn!(error = %error, frame_id = frame.frame_id, "failed to fragment encoded frame");
                return ControlFlow::Continue(());
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
            let delay_us = self.pacer.reserve(
                monotonic_timestamp_us(),
                bytes.len(),
                video_pacing_bitrate(
                    self.quality_ceiling.load(Ordering::Acquire),
                    self.audio_bits.load(Ordering::Relaxed),
                ),
            );
            if delay_us != 0 {
                tokio::time::sleep(std::time::Duration::from_micros(delay_us)).await;
            }
            self.stats.bytes_sent = self.stats.bytes_sent.saturating_add(bytes.len() as u64);
            if let Err(error) = self.channel.send(&Bytes::from(bytes)).await {
                tracing::warn!(error = %error, "video data channel send failed");
                let _ = self.failure.send(transport_failure(format!(
                    "video data channel send failed: {error}"
                )));
                return ControlFlow::Break(());
            }
        }
        if complete {
            self.stats.frames_sent += 1;
            self.last_sent = Some((source.stream_id, source.frame_id));
            if source.keyframe {
                self.recovering = false;
            }
        } else {
            // Never continue a predictive chain after only part of an
            // access unit was submitted to SCTP.
            self.recovering = true;
            self.stats.obsolete_frames_dropped += 1;
        }
        self.stats.log_if_due(&self.slot, &self.encoder_status);
        ControlFlow::Continue(())
    }
}
