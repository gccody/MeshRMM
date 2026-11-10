use std::future::Future;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use meshrmm_protocol::{DEFAULT_FRAGMENT_PAYLOAD, EncodedFrame, VideoStreamId, fragment_frame};
use meshrmm_session_transport::MeteredChannel;
use tokio::sync::{Notify, mpsc};
use webrtc::data_channel::RTCDataChannel;

use super::ControlCommand;
use crate::remote::bitrate::{
    AdaptiveBitrate, EncoderStatus, RestartLadder, VideoPacer, video_backlog_bytes,
    video_pacing_bitrate,
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
        VideoSender::new(
            MeteredChannel::new(channel),
            slot,
            recovery,
            quality_ceiling,
            encoder_status,
            audio_bits,
            failure,
        )
        .run(bootstrap_keyframe)
        .await;
    })
}

/// Where the sender's video packets go, and the clock it paces them by.
trait VideoChannel {
    /// Monotonic microseconds, in the clock capture timestamps use.
    fn now_us(&self) -> u64;
    /// Bytes queued behind the network (see [`MeteredChannel::queued`]).
    fn queued(&self) -> impl Future<Output = usize> + Send;
    fn send(&self, packet: Bytes) -> impl Future<Output = anyhow::Result<()>> + Send;
}

impl VideoChannel for MeteredChannel {
    fn now_us(&self) -> u64 {
        monotonic_timestamp_us()
    }

    async fn queued(&self) -> usize {
        MeteredChannel::queued(self).await
    }

    async fn send(&self, packet: Bytes) -> anyhow::Result<()> {
        MeteredChannel::send(self, &packet).await?;
        Ok(())
    }
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

struct VideoSender<C> {
    channel: C,
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

impl<C: VideoChannel> VideoSender<C> {
    fn new(
        channel: C,
        slot: Arc<LatestFrameSlot>,
        recovery: mpsc::UnboundedSender<ControlCommand>,
        quality_ceiling: Arc<AtomicU32>,
        encoder_status: Arc<EncoderStatus>,
        audio_bits: Arc<AtomicU32>,
        failure: mpsc::UnboundedSender<anyhow::Error>,
    ) -> Self {
        let maximum = quality_ceiling.load(Ordering::Acquire).max(1);
        Self {
            channel,
            slot,
            recovery,
            quality_ceiling,
            encoder_status,
            audio_bits,
            failure,
            stats: VideoStats::new(),
            last_sent: None,
            recovering: false,
            last_keyframe_request_us: 0,
            bitrate: AdaptiveBitrate::new(maximum),
            ladder: RestartLadder::new(maximum),
            pacer: VideoPacer::default(),
        }
    }

    /// The rate video fragments go out at.
    fn pacing_bitrate(&self) -> u32 {
        video_pacing_bitrate(
            self.quality_ceiling.load(Ordering::Acquire),
            self.audio_bits.load(Ordering::Relaxed),
        )
    }

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
            let now_us = self.channel.now_us();
            let (live_bitrate, queued_bytes) =
                self.adapt_bitrate(now_us, reference_chain_lost).await;
            if self.recovering && !source.keyframe {
                self.stats.recovery_frames_dropped += 1;
                self.request_keyframe(now_us);
                continue;
            }
            if self
                .drop_congested_frame(&source, now_us, live_bitrate, queued_bytes)
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
    /// queued behind the network on the video channel.
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
        let queued_bytes = self.channel.queued().await;
        if live_bitrate {
            if let Some(bits_per_second) =
                self.bitrate
                    .observe(now_us, queued_bytes, self.slot.len(), reference_chain_lost)
            {
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
                    .congested(queued_bytes, queued_frames, reference_chain_lost),
                self.bitrate.healthy(queued_bytes, queued_frames),
                self.encoder_status.recording(),
            ) {
                let _ = self
                    .recovery
                    .send(ControlCommand::RestartBitrate(bits_per_second));
                tracing::info!(
                    previous_bits_per_second = encoder_bitrate,
                    bits_per_second,
                    queued_bytes,
                    queued_frames,
                    "requested a video encoder restart at a new bitrate step"
                );
            }
        }
        (live_bitrate, queued_bytes)
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
        mut queued_bytes: usize,
    ) -> bool {
        let congested_bytes = self.bitrate.congested_bytes();
        let drain_bytes = self.bitrate.drain_bytes();
        if queued_bytes >= congested_bytes {
            let drain_started = tokio::time::Instant::now();
            while queued_bytes > drain_bytes && drain_started.elapsed() < VIDEO_BUFFER_DRAIN_TIMEOUT
            {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                queued_bytes = self.channel.queued().await;
            }
        }
        if queued_bytes < congested_bytes {
            return false;
        }
        self.stats.buffered_frames_dropped += 1;
        self.recovering = true;
        let queued_frames_dropped = self.slot.drop_pending();
        if live_bitrate
            && let Some(bits_per_second) =
                self.bitrate
                    .observe(now_us, queued_bytes, queued_frames_dropped, true)
        {
            let _ = self.recovery.send(ControlCommand::Bitrate(bits_per_second));
            tracing::warn!(
                bits_per_second,
                queued_bytes,
                queued_frames_dropped,
                "transport stayed congested; reduced bitrate and reset the predictive chain"
            );
        }
        self.request_keyframe(now_us);
        tracing::debug!(
            frame_id = source.frame_id,
            queued_bytes,
            "dropping frame and requesting H.264 recovery after the transport drain deadline"
        );
        true
    }

    /// Breaks when the video channel can no longer send.
    async fn send_frame(&mut self, source: &EncodedFrame) -> ControlFlow<()> {
        // Frames the encoder makes while this one is paced out wait in the
        // slot; as many as the sender soon sends afterwards keep the
        // predictive chain going instead of forcing another keyframe.
        self.slot
            .set_byte_budget(video_backlog_bytes(self.pacing_bitrate()));
        let mut frame = source.clone();
        frame.send_timestamp_us = self.channel.now_us();
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
            let delay_us =
                self.pacer
                    .reserve(self.channel.now_us(), bytes.len(), self.pacing_bitrate());
            if delay_us != 0 {
                tokio::time::sleep(std::time::Duration::from_micros(delay_us)).await;
            }
            self.stats.bytes_sent = self.stats.bytes_sent.saturating_add(bytes.len() as u64);
            if let Err(error) = self.channel.send(Bytes::from(bytes)).await {
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

#[cfg(test)]
mod tests {
    //! The sender between a fake encoder, which makes frames at the stream
    //! rate and a keyframe for each request as the Windows software encoder
    //! does, and a fake viewer, which reassembles and accepts frames as the
    //! viewer does, over a simulated link, on Tokio's paused clock.

    use std::sync::Mutex;
    use std::time::Duration;

    use meshrmm_protocol::{
        FrameReassembler, ReassemblyConfig, ReassemblyOutcome, VideoPacket, VideoStreamId,
    };
    use meshrmm_session_transport::SendQueue;
    use meshrmm_session_transport::simulated_link::SimulatedLink;
    use tokio::time::Instant;

    use super::*;

    const ROUND_TRIP: Duration = Duration::from_millis(40);

    struct Link {
        link: SimulatedLink,
        queue: SendQueue,
        bytes_per_second: u64,
    }

    struct FakeChannel {
        start: Instant,
        link: Arc<Mutex<Link>>,
        /// Each packet, with when it reaches the viewer.
        viewer: mpsc::UnboundedSender<(Instant, Bytes)>,
    }

    impl VideoChannel for FakeChannel {
        fn now_us(&self) -> u64 {
            clock_us(self.start)
        }

        async fn queued(&self) -> usize {
            let now = Instant::now().into_std();
            let mut link = self.link.lock().unwrap();
            let buffered = link.link.buffered(now);
            link.queue.queued(now, buffered)
        }

        async fn send(&self, packet: Bytes) -> anyhow::Result<()> {
            let now = Instant::now();
            let mut link = self.link.lock().unwrap();
            let transmission =
                Duration::from_nanos(packet.len() as u64 * 1_000_000_000 / link.bytes_per_second);
            let arrival =
                now + link.link.queue_delay(now.into_std()) + transmission + ROUND_TRIP / 2;
            link.link.send(now.into_std(), packet.len());
            link.queue.sent(now.into_std(), packet.len());
            let _ = self.viewer.send((arrival, packet));
            Ok(())
        }
    }

    fn clock_us(start: Instant) -> u64 {
        1_000_000 + start.elapsed().as_micros() as u64
    }

    /// What the Windows software encoder (Media Foundation's H.264 MFT) made
    /// of a static desktop of code at 1920x1080 and 3840x2160: its first
    /// keyframe, then the re-encodes that refine it, of which the first is
    /// larger. It makes one frame for each one it is given.
    #[derive(Clone, Copy, Debug)]
    struct Scenario {
        ceiling: u32,
        audio_bits_per_second: u32,
        frames_per_second: u64,
        keyframe_bytes: usize,
        first_delta_bytes: usize,
        delta_bytes: usize,
        link_bits_per_second: u64,
        seconds: u64,
    }

    const ULTRA_DATA_SAVER_1080P: Scenario = Scenario {
        ceiling: 1_000_000,
        audio_bits_per_second: 110_000,
        frames_per_second: 24,
        keyframe_bytes: 171_204,
        first_delta_bytes: 2_954,
        delta_bytes: 167,
        link_bits_per_second: 20_000_000,
        seconds: 6,
    };
    const ULTRA_DATA_SAVER_4K: Scenario = Scenario {
        audio_bits_per_second: 0,
        keyframe_bytes: 408_369,
        first_delta_bytes: 4_918,
        delta_bytes: 258,
        seconds: 8,
        ..ULTRA_DATA_SAVER_1080P
    };
    const DATA_SAVER_1080P: Scenario = Scenario {
        ceiling: 3_000_000,
        audio_bits_per_second: 0,
        frames_per_second: 30,
        keyframe_bytes: 238_311,
        first_delta_bytes: 4_279,
        delta_bytes: 156,
        link_bits_per_second: 20_000_000,
        seconds: 6,
    };
    const DATA_SAVER_4K: Scenario = Scenario {
        keyframe_bytes: 584_395,
        first_delta_bytes: 8_381,
        delta_bytes: 255,
        ..DATA_SAVER_1080P
    };

    #[derive(Default)]
    struct EncoderLog {
        frames: u64,
        keyframes: u64,
        bitrates: Vec<u32>,
    }

    #[derive(Debug, Clone, Copy)]
    struct Decoded {
        frame_id: u64,
        keyframe: bool,
        capture_us: u64,
        arrival_us: u64,
    }

    struct Outcome {
        encoder: EncoderLog,
        decoded: Vec<Decoded>,
        queued_frames: usize,
    }

    /// Makes a frame every frame interval once asked for a keyframe, and a
    /// keyframe for every request.
    async fn encode(
        scenario: Scenario,
        start: Instant,
        slot: Arc<LatestFrameSlot>,
        mut commands: mpsc::UnboundedReceiver<ControlCommand>,
        log: Arc<Mutex<EncoderLog>>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_micros(
            1_000_000 / scenario.frames_per_second,
        ));
        let mut keyframe_pending = false;
        let mut since_keyframe = 0;
        let mut frame_id = 0;
        loop {
            interval.tick().await;
            while let Ok(command) = commands.try_recv() {
                match command {
                    ControlCommand::Keyframe => keyframe_pending = true,
                    ControlCommand::Bitrate(bits_per_second) => {
                        log.lock().unwrap().bitrates.push(bits_per_second);
                    }
                    _ => {}
                }
            }
            if frame_id == 0 && !keyframe_pending {
                continue;
            }
            frame_id += 1;
            let keyframe = std::mem::take(&mut keyframe_pending);
            since_keyframe = if keyframe { 0 } else { since_keyframe + 1 };
            let bytes = match since_keyframe {
                0 => scenario.keyframe_bytes,
                1 => scenario.first_delta_bytes,
                _ => scenario.delta_bytes,
            };
            {
                let mut log = log.lock().unwrap();
                log.frames += 1;
                log.keyframes += u64::from(keyframe);
            }
            slot.publish(EncodedFrame {
                stream_id: VideoStreamId(1),
                frame_id,
                capture_timestamp_us: clock_us(start),
                encode_complete_timestamp_us: clock_us(start),
                send_timestamp_us: 0,
                keyframe,
                data: vec![0; bytes],
            });
        }
    }

    /// Reassembles and accepts frames as the viewer's video channel does,
    /// and asks for keyframes within the same limits.
    async fn view(
        start: Instant,
        mut packets: mpsc::UnboundedReceiver<(Instant, Bytes)>,
        requests: mpsc::UnboundedSender<ControlCommand>,
        decoded: Arc<Mutex<Vec<Decoded>>>,
    ) {
        let mut reassembler = FrameReassembler::new(ReassemblyConfig::default());
        let mut last_accepted: Option<u64> = None;
        let mut waiting_for_keyframe = true;
        let mut last_request: Option<Instant> = None;
        let mut outstanding = false;
        while let Some((arrival, packet)) = packets.recv().await {
            tokio::time::sleep_until(arrival).await;
            let now_us = clock_us(start);
            let incomplete = reassembler.stats().incomplete_frames_dropped;
            let outcome = reassembler.push(VideoPacket::decode(&packet).unwrap(), now_us);
            let mut want_keyframe = reassembler.stats().incomplete_frames_dropped > incomplete;
            waiting_for_keyframe |= want_keyframe;
            if let ReassemblyOutcome::Completed(frame) = outcome {
                if last_accepted.is_some_and(|last| frame.frame_id != last + 1) {
                    waiting_for_keyframe = true;
                }
                if frame.keyframe || !waiting_for_keyframe {
                    waiting_for_keyframe = false;
                    outstanding &= !frame.keyframe;
                    last_accepted = Some(frame.frame_id);
                    decoded.lock().unwrap().push(Decoded {
                        frame_id: frame.frame_id,
                        keyframe: frame.keyframe,
                        capture_us: frame.capture_timestamp_us,
                        arrival_us: now_us,
                    });
                } else {
                    want_keyframe = true;
                }
            }
            // As the viewer's `KeyframeRequests`: 250 ms apart, and 1 s
            // while one goes unanswered.
            let wait = Duration::from_millis(if outstanding { 1_000 } else { 250 });
            if want_keyframe && last_request.is_none_or(|last| last.elapsed() >= wait) {
                last_request = Some(Instant::now());
                outstanding = true;
                let _ = requests.send(ControlCommand::Keyframe);
            }
        }
    }

    async fn simulate(scenario: Scenario) -> Outcome {
        let start = Instant::now();
        let slot = Arc::new(LatestFrameSlot::default());
        let (commands, encoder_commands) = mpsc::unbounded_channel();
        let (viewer_packets, packets) = mpsc::unbounded_channel();
        let (failure, _failures) = mpsc::unbounded_channel();
        let encoder_log = Arc::new(Mutex::new(EncoderLog::default()));
        let decoded = Arc::new(Mutex::new(Vec::new()));
        let encoder_status = Arc::new(EncoderStatus::default());
        encoder_status.publish(scenario.ceiling, true, false);
        let link_bytes_per_second = scenario.link_bits_per_second / 8;
        let channel = FakeChannel {
            start,
            link: Arc::new(Mutex::new(Link {
                link: SimulatedLink::new(start.into_std(), link_bytes_per_second, ROUND_TRIP),
                queue: SendQueue::default(),
                bytes_per_second: link_bytes_per_second,
            })),
            viewer: viewer_packets,
        };
        let sender = VideoSender::new(
            channel,
            Arc::clone(&slot),
            commands.clone(),
            Arc::new(AtomicU32::new(scenario.ceiling)),
            encoder_status,
            Arc::new(AtomicU32::new(scenario.audio_bits_per_second)),
            failure.clone(),
        );
        let tasks = [
            tokio::spawn(encode(
                scenario,
                start,
                Arc::clone(&slot),
                encoder_commands,
                Arc::clone(&encoder_log),
            )),
            tokio::spawn(view(start, packets, commands.clone(), Arc::clone(&decoded))),
            tokio::spawn({
                let slot = Arc::clone(&slot);
                async move {
                    let bootstrap = bootstrap_keyframe(&slot, &commands, &failure)
                        .await
                        .unwrap();
                    sender.run(bootstrap).await;
                }
            }),
        ];
        tokio::time::sleep(Duration::from_secs(scenario.seconds)).await;
        for task in tasks {
            task.abort();
        }
        let encoder = std::mem::take(&mut *encoder_log.lock().unwrap());
        let decoded = std::mem::take(&mut *decoded.lock().unwrap());
        Outcome {
            encoder,
            decoded,
            queued_frames: slot.len(),
        }
    }

    /// How long pacing `bytes` takes in the scenario.
    fn pacing_us(scenario: Scenario, bytes: usize) -> u64 {
        bytes as u64 * 8_000_000
            / u64::from(video_pacing_bitrate(
                scenario.ceiling,
                scenario.audio_bits_per_second,
            ))
    }

    #[tokio::test(start_paused = true)]
    async fn a_keyframe_longer_to_send_than_the_frame_queue_lasts_continues_the_stream() {
        for scenario in [
            ULTRA_DATA_SAVER_1080P,
            ULTRA_DATA_SAVER_4K,
            DATA_SAVER_1080P,
            DATA_SAVER_4K,
        ] {
            // Pacing the keyframe outlasts the queue of 15 frames, and the
            // 250 ms the viewer waits for a missing fragment.
            let pacing = pacing_us(scenario, scenario.keyframe_bytes);
            assert!(pacing > 15 * 1_000_000 / scenario.frames_per_second);
            assert!(pacing > 2 * 250_000);

            let outcome = simulate(scenario).await;
            let context = format!("{scenario:?}");
            assert_eq!(outcome.encoder.keyframes, 1, "{context}");
            let first = outcome.decoded.first().expect(&context);
            assert!(first.keyframe && first.frame_id == 1, "{context}");
            // Every frame after it is decodable, up to the last second.
            for (index, frame) in outcome.decoded.iter().enumerate() {
                assert_eq!(frame.frame_id, index as u64 + 1, "{context}");
            }
            let last = outcome.decoded.last().unwrap().frame_id;
            assert!(
                last + scenario.frames_per_second >= outcome.encoder.frames,
                "{context}: {last} of {} frames decoded",
                outcome.encoder.frames
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn latency_returns_to_normal_once_a_long_keyframe_is_through() {
        for scenario in [ULTRA_DATA_SAVER_4K, DATA_SAVER_1080P] {
            let outcome = simulate(scenario).await;
            let context = format!("{scenario:?}");
            let keyframe = outcome.decoded.first().expect(&context);
            assert!(keyframe.keyframe, "{context}");
            // The frames made while the keyframe was paced out follow it
            // within the backlog's 250 ms; from then on frames arrive live.
            let live_from_us = keyframe.arrival_us + 300_000;
            let live: Vec<_> = outcome
                .decoded
                .iter()
                .filter(|frame| frame.arrival_us >= live_from_us)
                .collect();
            assert!(live.len() as u64 >= scenario.frames_per_second, "{context}");
            for frame in live {
                let latency_us = frame.arrival_us - frame.capture_us;
                assert!(latency_us < 100_000, "{context}: {frame:?}");
            }
            assert!(outcome.queued_frames <= 1, "{context}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn congestion_while_a_keyframe_is_paced_out_still_cuts_the_bitrate() {
        // A link at half the Data Saver ceiling.
        let scenario = Scenario {
            link_bits_per_second: 1_500_000,
            ..DATA_SAVER_1080P
        };
        let outcome = simulate(scenario).await;
        let keyframe = outcome.decoded.first().expect("the keyframe arrives");
        assert!(keyframe.keyframe && keyframe.frame_id == 1);
        // The keyframe took the link about 1.3 s.
        assert!(keyframe.arrival_us - keyframe.capture_us > 1_200_000);
        let cut = outcome.encoder.bitrates.first().copied();
        assert!(
            cut.is_some_and(|bits_per_second| bits_per_second < scenario.ceiling),
            "{:?}",
            outcome.encoder.bitrates
        );
    }
}
