//! The video channel: reassembles frames from packets and asks for a
//! keyframe when frames are lost. [`ViewerControlQueue`] decides when the
//! request goes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use meshrmm_protocol::{
    EncodedFrame, FrameReassembler, ReassemblyConfig, SessionMessage, VideoPacket, VideoStreamId,
};
use webrtc::data_channel::RTCDataChannel;

use super::control::ViewerControlQueue;
use super::{ActivePresenter, ReceiverLifecycle};
use crate::debug::DebugInfo;
use crate::platform::monotonic_timestamp_us;

/// How often the channel expires incomplete frames, and sends keyframe
/// requests that had to wait or went unanswered. Frames waiting for a lost
/// one are released at the reassembler's deadline instead.
const RECOVERY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// What a packet or the recovery poll gave the viewer.
struct Received {
    stream_id: Option<VideoStreamId>,
    /// Frames to present, in order.
    frames: Vec<EncodedFrame>,
    request_keyframe: bool,
}

struct VideoReceiveState {
    reassembler: FrameReassembler,
    stream_id: Option<VideoStreamId>,
    last_accepted_frame_id: Option<u64>,
    waiting_for_keyframe: bool,
    statistics_log: crate::debug::StatisticsLog,
}

impl VideoReceiveState {
    fn new() -> Self {
        Self {
            reassembler: FrameReassembler::new(ReassemblyConfig::default()),
            stream_id: None,
            last_accepted_frame_id: None,
            waiting_for_keyframe: true,
            statistics_log: Default::default(),
        }
    }

    fn mark_loss(&mut self) {
        self.waiting_for_keyframe = true;
    }

    /// Reassembles a packet.
    fn receive(&mut self, packet: VideoPacket, now_us: u64) -> Received {
        self.observe_stream(packet.stream_id);
        let incomplete_before = self.reassembler.stats().incomplete_frames_dropped;
        let frames = self.reassembler.push(packet, now_us).frames;
        self.accept(frames, incomplete_before)
    }

    /// Gives up the frames whose wait is over: the last packet of a burst
    /// can be lost on a static desktop, and the frames after a lost one wait
    /// only so long for its retransmission.
    fn poll_recovery(&mut self, now_us: u64) -> Received {
        let incomplete_before = self.reassembler.stats().incomplete_frames_dropped;
        let frames = self.reassembler.expire(now_us);
        if self.reassembler.stats().incomplete_frames_dropped > incomplete_before {
            tracing::warn!("incomplete video frame expired while waiting for more packets");
        }
        self.accept(frames, incomplete_before)
    }

    /// The frames to present of those released, and whether the stream needs
    /// a keyframe: a frame was given up, or a frame cannot be decoded.
    fn accept(&mut self, released: Vec<EncodedFrame>, incomplete_before: u64) -> Received {
        let mut request_keyframe = false;
        // The frames released after a given-up frame come after it.
        if self.reassembler.stats().incomplete_frames_dropped > incomplete_before {
            self.mark_loss();
            request_keyframe = true;
        }
        let mut frames = Vec::with_capacity(released.len());
        for frame in released {
            let (frame, request) = self.accept_completed(frame);
            request_keyframe |= request;
            frames.extend(frame);
        }
        Received {
            stream_id: self.stream_id,
            frames,
            request_keyframe,
        }
    }

    fn observe_stream(&mut self, stream_id: VideoStreamId) {
        if self.stream_id != Some(stream_id) {
            self.stream_id = Some(stream_id);
            self.last_accepted_frame_id = None;
            self.waiting_for_keyframe = true;
        }
    }

    /// Returns the frame to present, if it can be decoded, and whether the
    /// stream needs a keyframe.
    fn accept_completed(&mut self, frame: EncodedFrame) -> (Option<EncodedFrame>, bool) {
        self.observe_stream(frame.stream_id);

        let gap = self
            .last_accepted_frame_id
            .is_some_and(|last| frame.frame_id != last.wrapping_add(1));
        if gap && !frame.keyframe {
            self.waiting_for_keyframe = true;
            tracing::warn!(
                last_frame_id = ?self.last_accepted_frame_id,
                frame_id = frame.frame_id,
                stream_id = frame.stream_id.0,
                "video frame gap detected; suppressing deltas until a keyframe arrives"
            );
        }

        if frame.keyframe {
            self.waiting_for_keyframe = false;
            self.last_accepted_frame_id = Some(frame.frame_id);
            return (Some(frame), false);
        }
        if self.waiting_for_keyframe {
            return (None, true);
        }

        self.last_accepted_frame_id = Some(frame.frame_id);
        (Some(frame), false)
    }
}

pub(super) fn install_video_handler(
    channel: Arc<RTCDataChannel>,
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    viewer_control: ViewerControlQueue,
    debug: DebugInfo,
    lifecycle: ReceiverLifecycle,
) {
    let closed = Arc::new(AtomicBool::new(false));
    {
        let debug = debug.clone();
        let closed = Arc::clone(&closed);
        let lifecycle = lifecycle.clone();
        channel.on_close(Box::new(move || {
            let debug = debug.clone();
            let closed = Arc::clone(&closed);
            let lifecycle = lifecycle.clone();
            Box::pin(async move {
                closed.store(true, Ordering::Release);
                debug.set_data_channel("meshrmm-video-v1", "closed");
                if !lifecycle.shutting_down.load(Ordering::Acquire) {
                    tracing::error!("viewer video data channel closed during an active session");
                    let _ = lifecycle
                        .presentation_failure
                        .send("remote video channel closed during an active session".into());
                }
            })
        }));
    }
    let receive_state = Arc::new(tokio::sync::Mutex::new(VideoReceiveState::new()));
    // Wakes the recovery task when frames start waiting for a lost one.
    let frames_waiting = Arc::new(tokio::sync::Notify::new());
    let output = VideoOutput {
        presenter,
        viewer_control: viewer_control.clone(),
        debug,
    };
    {
        let receive_state = Arc::clone(&receive_state);
        let viewer_control = viewer_control.clone();
        let frames_waiting = Arc::clone(&frames_waiting);
        let output = output.clone();
        tokio::spawn(async move {
            let mut next_poll = tokio::time::Instant::now() + RECOVERY_POLL_INTERVAL;
            while !closed.load(Ordering::Acquire) {
                let deadline_us = receive_state.lock().await.reassembler.deadline_us();
                let wake = deadline_us.map_or(next_poll, |deadline_us| {
                    let wait_us = deadline_us.saturating_sub(monotonic_timestamp_us());
                    next_poll.min(
                        tokio::time::Instant::now() + std::time::Duration::from_micros(wait_us),
                    )
                });
                tokio::select! {
                    () = tokio::time::sleep_until(wake) => {}
                    () = frames_waiting.notified() => continue,
                }
                let now_us = monotonic_timestamp_us();
                let received = receive_state.lock().await.poll_recovery(now_us);
                output.deliver(&receive_state, received, now_us).await;
                if tokio::time::Instant::now() >= next_poll {
                    next_poll = tokio::time::Instant::now() + RECOVERY_POLL_INTERVAL;
                    viewer_control.poll_keyframe_request(now_us);
                }
            }
        });
    }
    channel.on_message(Box::new(move |message| {
        let receive_state = Arc::clone(&receive_state);
        let frames_waiting = Arc::clone(&frames_waiting);
        let output = output.clone();
        Box::pin(async move {
            let received_at_us = monotonic_timestamp_us();
            let packet = match VideoPacket::decode(message.data) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::warn!(error = %error, "discarding invalid video packet");
                    return;
                }
            };
            let received = {
                let mut receive_state = receive_state.lock().await;
                let deadline_before = receive_state.reassembler.deadline_us();
                let received = receive_state.receive(packet, received_at_us);
                let deadline = receive_state.reassembler.deadline_us();
                if deadline
                    .is_some_and(|deadline| deadline_before.is_none_or(|before| deadline < before))
                {
                    frames_waiting.notify_one();
                }
                received
            };
            output
                .deliver(&receive_state, received, received_at_us)
                .await;
        })
    }));
}

/// Where reassembled frames go.
#[derive(Clone)]
struct VideoOutput {
    presenter: Arc<Mutex<Option<ActivePresenter>>>,
    viewer_control: ViewerControlQueue,
    debug: DebugInfo,
}

impl VideoOutput {
    async fn deliver(
        &self,
        receive_state: &tokio::sync::Mutex<VideoReceiveState>,
        received: Received,
        now_us: u64,
    ) {
        if received.request_keyframe
            && let Some(stream_id) = received.stream_id
        {
            self.viewer_control
                .send(SessionMessage::RequestKeyframe { stream_id });
        }
        if received.frames.is_empty() {
            return;
        }
        let (stats, log_statistics) = {
            let mut state = receive_state.lock().await;
            (state.reassembler.stats(), state.statistics_log.due())
        };
        for frame in received.frames {
            if frame.keyframe {
                self.viewer_control.keyframe_arrived(frame.stream_id);
            }
            let encode_us = frame
                .encode_complete_timestamp_us
                .saturating_sub(frame.capture_timestamp_us);
            self.debug.record_received_frame(
                encode_us,
                stats.completed_frames,
                stats.incomplete_frames_dropped,
                stats.stale_packets_dropped,
                stats.duplicate_packets,
                stats.invalid_packets,
            );
            if let Ok(guard) = self.presenter.lock()
                && let Some(active) = guard.as_ref()
                && active.stream_id == frame.stream_id
            {
                self.viewer_control.recording.receive(&frame, active.format);
                active.presenter.publish(frame, now_us);
            }
            tracing::trace!(
                encode_us,
                received_at_us = now_us,
                "encoded frame reassembled"
            );
        }
        if log_statistics {
            tracing::info!(
                frames_received = stats.completed_frames,
                incomplete_frames_dropped = stats.incomplete_frames_dropped,
                stale_packets_dropped = stats.stale_packets_dropped,
                duplicate_packets = stats.duplicate_packets,
                invalid_packets = stats.invalid_packets,
                "video reassembly statistics"
            );
        }
    }
}

#[cfg(test)]
#[path = "video_loss_simulation.rs"]
mod loss_simulation;

#[cfg(test)]
mod tests {

    use super::*;

    fn encoded_frame(frame_id: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(7),
            frame_id,
            capture_timestamp_us: 1,
            encode_complete_timestamp_us: 2,
            send_timestamp_us: 3,
            keyframe,
            data: vec![1].into(),
        }
    }

    #[test]
    fn video_recovery_suppresses_deltas_across_a_frame_gap() {
        let mut state = VideoReceiveState::new();

        let (frame, request) = state.accept_completed(encoded_frame(10, false));
        assert!(frame.is_none());
        assert!(request);

        let (frame, request) = state.accept_completed(encoded_frame(11, true));
        assert_eq!(frame.unwrap().frame_id, 11);
        assert!(!request);

        let (frame, request) = state.accept_completed(encoded_frame(12, false));
        assert_eq!(frame.unwrap().frame_id, 12);
        assert!(!request);

        let (frame, request) = state.accept_completed(encoded_frame(14, false));
        assert!(frame.is_none());
        assert!(request);

        // Still waiting: the control queue's limiter keeps this from
        // becoming a second request.
        let (frame, request) = state.accept_completed(encoded_frame(15, false));
        assert!(frame.is_none());
        assert!(request);

        let (frame, request) = state.accept_completed(encoded_frame(16, true));
        assert_eq!(frame.unwrap().frame_id, 16);
        assert!(!request);
    }

    #[test]
    fn video_recovery_expires_a_trailing_fragment_without_new_packets() {
        let mut state = VideoReceiveState::new();
        state.accept_completed(encoded_frame(1, true));
        let mut frame = encoded_frame(2, false);
        frame.data = vec![1; 6].into();
        let packet = meshrmm_protocol::fragment_frame(&frame, 3)
            .unwrap()
            .remove(0);
        assert_eq!(
            state.reassembler.push(packet, 2_000).outcome,
            meshrmm_protocol::ReassemblyOutcome::Accepted
        );
        assert!(!state.poll_recovery(2_001).request_keyframe);

        let expired_at = 2_000 + ReassemblyConfig::default().stale_after.as_micros() as u64;
        let received = state.poll_recovery(expired_at);
        assert!(received.request_keyframe);
        assert_eq!(received.stream_id, Some(VideoStreamId(7)));
        assert_eq!(state.reassembler.stats().incomplete_frames_dropped, 1);
        assert!(state.waiting_for_keyframe);
        // The control queue retries the request; the frame expired once.
        assert!(!state.poll_recovery(expired_at + 1).request_keyframe);
        state.accept_completed(encoded_frame(3, true));
        assert!(!state.waiting_for_keyframe);
    }

    #[test]
    fn video_recovery_does_not_treat_a_static_desktop_as_packet_loss() {
        let mut state = VideoReceiveState::new();
        state.accept_completed(encoded_frame(1, true));
        assert!(!state.poll_recovery(60_000_000).request_keyframe);
        assert!(!state.waiting_for_keyframe);
    }
}
