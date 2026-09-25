//! The video channel: reassembles frames from packets and asks for a
//! keyframe when frames are lost.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use meshrmm_protocol::{
    EncodedFrame, FrameReassembler, ReassemblyConfig, ReassemblyOutcome, SessionMessage,
    VideoPacket, VideoStreamId,
};
use webrtc::data_channel::RTCDataChannel;

use super::control::ViewerControlQueue;
use super::{ActivePresenter, ReceiverLifecycle};
use crate::debug::DebugInfo;
use crate::platform::monotonic_timestamp_us;

const KEYFRAME_RETRY_INTERVAL_US: u64 = 250_000;

struct VideoReceiveState {
    reassembler: FrameReassembler,
    stream_id: Option<VideoStreamId>,
    last_accepted_frame_id: Option<u64>,
    waiting_for_keyframe: bool,
    last_keyframe_request_us: u64,
    statistics_log: crate::debug::StatisticsLog,
}

impl VideoReceiveState {
    fn new() -> Self {
        Self {
            reassembler: FrameReassembler::new(ReassemblyConfig::default()),
            stream_id: None,
            last_accepted_frame_id: None,
            waiting_for_keyframe: true,
            last_keyframe_request_us: 0,
            statistics_log: Default::default(),
        }
    }

    fn mark_loss(&mut self) {
        self.waiting_for_keyframe = true;
    }

    fn poll_recovery(&mut self, now_us: u64) -> Option<VideoStreamId> {
        // The last packet of a burst can be lost on a static desktop. Expire
        // its incomplete frame even when no subsequent packets arrive.
        if self.reassembler.expire_stale(now_us) {
            self.mark_loss();
            tracing::warn!("incomplete video frame expired while waiting for more packets");
        }
        if self.waiting_for_keyframe && self.keyframe_request_due(now_us) {
            self.stream_id
        } else {
            None
        }
    }

    fn observe_stream(&mut self, stream_id: VideoStreamId) {
        if self.stream_id != Some(stream_id) {
            self.stream_id = Some(stream_id);
            self.last_accepted_frame_id = None;
            self.waiting_for_keyframe = true;
            self.last_keyframe_request_us = 0;
        }
    }

    fn keyframe_request_due(&mut self, now_us: u64) -> bool {
        if self.last_keyframe_request_us == 0
            || now_us.saturating_sub(self.last_keyframe_request_us) >= KEYFRAME_RETRY_INTERVAL_US
        {
            self.last_keyframe_request_us = now_us.max(1);
            true
        } else {
            false
        }
    }

    fn accept_completed(
        &mut self,
        frame: EncodedFrame,
        now_us: u64,
    ) -> (Option<EncodedFrame>, bool) {
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
            self.last_keyframe_request_us = 0;
            self.last_accepted_frame_id = Some(frame.frame_id);
            return (Some(frame), false);
        }
        if self.waiting_for_keyframe {
            return (None, self.keyframe_request_due(now_us));
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
    {
        let receive_state = Arc::clone(&receive_state);
        let viewer_control = viewer_control.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_micros(KEYFRAME_RETRY_INTERVAL_US));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            while !closed.load(Ordering::Acquire) {
                interval.tick().await;
                let now_us = monotonic_timestamp_us();
                let stream_id = {
                    let mut state = receive_state.lock().await;
                    state.poll_recovery(now_us)
                };
                if let Some(stream_id) = stream_id {
                    viewer_control.send(SessionMessage::RequestKeyframe { stream_id });
                    tracing::debug!(
                        stream_id = stream_id.0,
                        "retrying a video recovery keyframe while no decodable frames arrive"
                    );
                }
            }
        });
    }
    channel.on_message(Box::new(move |message| {
        let receive_state = Arc::clone(&receive_state);
        let presenter = Arc::clone(&presenter);
        let viewer_control = viewer_control.clone();
        let debug = debug.clone();
        Box::pin(async move {
            let received_at_us = monotonic_timestamp_us();
            let packet = match VideoPacket::decode(&message.data) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::warn!(error = %error, "discarding invalid video packet");
                    return;
                }
            };
            let packet_stream_id = packet.stream_id;
            let mut receive_state = receive_state.lock().await;
            receive_state.observe_stream(packet_stream_id);
            let incomplete_before = receive_state.reassembler.stats().incomplete_frames_dropped;
            let outcome = receive_state.reassembler.push(packet, received_at_us);
            let stats = receive_state.reassembler.stats();
            let mut request_keyframe = false;
            if stats.incomplete_frames_dropped > incomplete_before {
                receive_state.mark_loss();
                request_keyframe = receive_state.keyframe_request_due(received_at_us);
            }
            let completed = if let ReassemblyOutcome::Completed(frame) = outcome {
                let (frame, request) = receive_state.accept_completed(frame, received_at_us);
                request_keyframe |= request;
                frame
            } else {
                None
            };
            let log_statistics = completed.is_some() && receive_state.statistics_log.due();
            drop(receive_state);
            if request_keyframe {
                viewer_control.send(SessionMessage::RequestKeyframe {
                    stream_id: packet_stream_id,
                });
                tracing::warn!(
                    stream_id = packet_stream_id.0,
                    "requested a video recovery keyframe"
                );
            }
            if let Some(frame) = completed {
                let encode_us = frame
                    .encode_complete_timestamp_us
                    .saturating_sub(frame.capture_timestamp_us);
                debug.record_received_frame(
                    encode_us,
                    stats.completed_frames,
                    stats.incomplete_frames_dropped,
                    stats.stale_packets_dropped,
                    stats.duplicate_packets,
                    stats.invalid_packets,
                );
                if let Ok(guard) = presenter.lock()
                    && let Some(active) = guard.as_ref()
                    && active.stream_id == frame.stream_id
                {
                    viewer_control.recording.receive(&frame, active.format);
                    active.presenter.publish(frame, received_at_us);
                }
                tracing::trace!(encode_us, received_at_us, "encoded frame reassembled");
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
        })
    }));
}

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
            data: vec![1],
        }
    }

    #[test]
    fn video_recovery_suppresses_deltas_across_a_frame_gap() {
        let mut state = VideoReceiveState::new();

        let (frame, request) = state.accept_completed(encoded_frame(10, false), 1_000);
        assert!(frame.is_none());
        assert!(request);

        let (frame, request) = state.accept_completed(encoded_frame(11, true), 2_000);
        assert_eq!(frame.unwrap().frame_id, 11);
        assert!(!request);

        let (frame, request) = state.accept_completed(encoded_frame(12, false), 3_000);
        assert_eq!(frame.unwrap().frame_id, 12);
        assert!(!request);

        let (frame, request) = state.accept_completed(encoded_frame(14, false), 4_000);
        assert!(frame.is_none());
        assert!(request);

        let (frame, request) = state.accept_completed(encoded_frame(15, false), 5_000);
        assert!(frame.is_none());
        assert!(!request, "recovery requests are rate limited");

        let (frame, request) = state.accept_completed(encoded_frame(16, true), 6_000);
        assert_eq!(frame.unwrap().frame_id, 16);
        assert!(!request);
    }

    #[test]
    fn video_recovery_retries_a_missing_keyframe() {
        let mut state = VideoReceiveState::new();
        let (_, first_request) = state.accept_completed(encoded_frame(1, false), 1_000);
        let (_, retry) =
            state.accept_completed(encoded_frame(2, false), 1_000 + KEYFRAME_RETRY_INTERVAL_US);
        assert!(first_request);
        assert!(retry);
    }

    #[test]
    fn video_recovery_can_retry_when_only_packet_fragments_arrive() {
        let mut state = VideoReceiveState::new();
        let stream_id = VideoStreamId(7);
        state.observe_stream(stream_id);
        state.mark_loss();

        assert_eq!(state.stream_id, Some(stream_id));
        assert!(state.waiting_for_keyframe);
        assert!(state.keyframe_request_due(1_000));
        assert!(!state.keyframe_request_due(1_001));
        assert!(state.keyframe_request_due(1_000 + KEYFRAME_RETRY_INTERVAL_US));
    }

    #[test]
    fn video_recovery_expires_a_trailing_fragment_without_new_packets() {
        let mut state = VideoReceiveState::new();
        state.accept_completed(encoded_frame(1, true), 1_000);
        let mut frame = encoded_frame(2, false);
        frame.data = vec![1; 6];
        let packet = meshrmm_protocol::fragment_frame(&frame, 3)
            .unwrap()
            .remove(0);
        assert!(matches!(
            state.reassembler.push(packet, 2_000),
            ReassemblyOutcome::Accepted
        ));
        assert_eq!(state.poll_recovery(2_001), None);

        let expired_at = 2_000 + ReassemblyConfig::default().stale_after.as_micros() as u64;
        assert_eq!(state.poll_recovery(expired_at), Some(VideoStreamId(7)));
        assert_eq!(state.reassembler.stats().incomplete_frames_dropped, 1);
        assert_eq!(state.poll_recovery(expired_at + 1), None);
        assert_eq!(
            state.poll_recovery(expired_at + KEYFRAME_RETRY_INTERVAL_US),
            Some(VideoStreamId(7))
        );
        state.accept_completed(
            encoded_frame(3, true),
            expired_at + KEYFRAME_RETRY_INTERVAL_US + 1,
        );
        assert_eq!(
            state.poll_recovery(expired_at + 2 * KEYFRAME_RETRY_INTERVAL_US),
            None
        );
    }

    #[test]
    fn video_recovery_does_not_treat_a_static_desktop_as_packet_loss() {
        let mut state = VideoReceiveState::new();
        state.accept_completed(encoded_frame(1, true), 1_000);
        assert_eq!(state.poll_recovery(60_000_000), None);
        assert!(!state.waiting_for_keyframe);
    }
}
