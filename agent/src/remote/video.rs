use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use meshrmm_protocol::EncodedFrame;
use tokio::sync::Notify;

/// Moonlight uses a bounded 15-unit decode queue. Matching that amount on the
/// encoded sender side absorbs short scheduler/network bursts while keeping
/// the maximum queued media below 250 ms at 60 FPS.
pub const MAX_ENCODED_FRAME_QUEUE: usize = 15;

pub struct LatestFrameSlot {
    frames: Mutex<VecDeque<Arc<EncodedFrame>>>,
    keyframe: Mutex<Option<Arc<EncodedFrame>>>,
    changed: Notify,
    keyframe_changed: Notify,
    dropped: AtomicU64,
}

impl Default for LatestFrameSlot {
    fn default() -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(MAX_ENCODED_FRAME_QUEUE)),
            keyframe: Mutex::new(None),
            changed: Notify::new(),
            keyframe_changed: Notify::new(),
            dropped: AtomicU64::new(0),
        }
    }
}

impl LatestFrameSlot {
    /// Publishes directly from the capture callback. Keeping this operation
    /// synchronous preserves encoder output order and avoids spawning one
    /// Tokio task per video frame.
    pub fn publish(&self, frame: EncodedFrame) {
        let frame = Arc::new(frame);
        if frame.keyframe {
            if let Ok(mut keyframe) = self.keyframe.lock() {
                *keyframe = Some(Arc::clone(&frame));
            }
            self.keyframe_changed.notify_waiters();
        }
        let Ok(mut frames) = self.frames.lock() else {
            return;
        };
        if frames.len() >= MAX_ENCODED_FRAME_QUEUE {
            self.dropped
                .fetch_add(frames.len() as u64, Ordering::Relaxed);
            frames.clear();
        }
        frames.push_back(frame);
        drop(frames);
        self.changed.notify_one();
    }

    pub async fn next(&self) -> Arc<EncodedFrame> {
        loop {
            let notified = self.changed.notified();
            if let Some(frame) = self
                .frames
                .lock()
                .ok()
                .and_then(|mut frames| frames.pop_front())
            {
                return frame;
            }
            notified.await;
        }
    }

    pub fn clear(&self) {
        self.clear_pending();
        if let Ok(mut keyframe) = self.keyframe.lock() {
            *keyframe = None;
        }
    }

    /// Discards queued playback frames while retaining the newest keyframe.
    /// The retained IDR can paint a newly initialized decoder immediately,
    /// even when the Windows login desktop is completely static.
    pub fn clear_pending(&self) {
        if let Ok(mut frames) = self.frames.lock() {
            frames.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.frames.lock().map_or(0, |frames| frames.len())
    }

    pub fn drop_pending(&self) -> usize {
        self.frames.lock().map_or(0, |mut frames| {
            let dropped = frames.len();
            frames.clear();
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
            dropped
        })
    }

    pub fn discard_through(&self, frame_id: u64) {
        if let Ok(mut frames) = self.frames.lock() {
            while frames
                .front()
                .is_some_and(|frame| frame.frame_id <= frame_id)
            {
                frames.pop_front();
            }
        }
    }

    /// Returns the newest compressed keyframe without consuming the normal
    /// latest-frame slot. This retains one decoder bootstrap frame, not a
    /// playback queue.
    pub async fn keyframe(&self) -> Arc<EncodedFrame> {
        loop {
            let notified = self.keyframe_changed.notified();
            if let Some(frame) = self.keyframe.lock().ok().and_then(|frame| frame.clone()) {
                return frame;
            }
            notified.await;
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::VideoStreamId;

    use super::*;

    fn frame(frame_id: u64) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(1),
            frame_id,
            capture_timestamp_us: 1,
            encode_complete_timestamp_us: 2,
            send_timestamp_us: 0,
            keyframe: false,
            data: vec![frame_id as u8],
        }
    }

    fn keyframe(frame_id: u64) -> EncodedFrame {
        EncodedFrame {
            keyframe: true,
            ..frame(frame_id)
        }
    }

    #[tokio::test]
    async fn publishing_preserves_encoder_order() {
        let slot = LatestFrameSlot::default();
        slot.publish(frame(10));
        slot.publish(frame(11));
        assert_eq!(slot.next().await.frame_id, 10);
        assert_eq!(slot.next().await.frame_id, 11);
        assert_eq!(slot.dropped(), 0);
    }

    #[tokio::test]
    async fn keyframe_is_retained_alongside_the_ordered_queue() {
        let slot = LatestFrameSlot::default();
        slot.publish(keyframe(10));
        slot.publish(frame(11));
        assert_eq!(slot.keyframe().await.frame_id, 10);
        assert_eq!(slot.next().await.frame_id, 10);
        assert_eq!(slot.next().await.frame_id, 11);
    }

    #[tokio::test]
    async fn bootstrap_frame_is_removed_from_the_normal_queue() {
        let slot = LatestFrameSlot::default();
        slot.publish(keyframe(10));
        slot.publish(frame(11));
        slot.discard_through(10);
        assert_eq!(slot.next().await.frame_id, 11);
    }

    #[tokio::test]
    async fn clearing_pending_frames_retains_the_bootstrap_keyframe() {
        let slot = LatestFrameSlot::default();
        slot.publish(keyframe(10));
        slot.publish(frame(11));
        slot.clear_pending();
        assert_eq!(slot.len(), 0);
        assert_eq!(slot.keyframe().await.frame_id, 10);
    }

    #[tokio::test]
    async fn overflow_drops_a_whole_stale_chain_instead_of_individual_frames() {
        let slot = LatestFrameSlot::default();
        for frame_id in 1..=MAX_ENCODED_FRAME_QUEUE as u64 + 1 {
            slot.publish(frame(frame_id));
        }
        assert_eq!(slot.len(), 1);
        assert_eq!(slot.dropped(), MAX_ENCODED_FRAME_QUEUE as u64,);
        assert_eq!(
            slot.next().await.frame_id,
            MAX_ENCODED_FRAME_QUEUE as u64 + 1
        );
    }
}
