use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use meshrmm_protocol::EncodedFrame;
use tokio::sync::Notify;

#[derive(Default)]
pub struct LatestFrameSlot {
    frame: Mutex<Option<Arc<EncodedFrame>>>,
    keyframe: Mutex<Option<Arc<EncodedFrame>>>,
    changed: Notify,
    keyframe_changed: Notify,
    dropped: AtomicU64,
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
        let Ok(mut current) = self.frame.lock() else {
            return;
        };
        if current.replace(frame).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(current);
        self.changed.notify_one();
    }

    pub async fn next(&self) -> Arc<EncodedFrame> {
        loop {
            let notified = self.changed.notified();
            if let Some(frame) = self.frame.lock().ok().and_then(|mut frame| frame.take()) {
                return frame;
            }
            notified.await;
        }
    }

    pub fn clear(&self) {
        if let Ok(mut frame) = self.frame.lock() {
            *frame = None;
        }
        if let Ok(mut keyframe) = self.keyframe.lock() {
            *keyframe = None;
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
    async fn publishing_replaces_obsolete_frame() {
        let slot = LatestFrameSlot::default();
        slot.publish(frame(10));
        slot.publish(frame(11));
        assert_eq!(slot.next().await.frame_id, 11);
        assert_eq!(slot.dropped(), 1);
    }

    #[tokio::test]
    async fn keyframe_is_retained_when_latest_frame_is_replaced() {
        let slot = LatestFrameSlot::default();
        slot.publish(keyframe(10));
        slot.publish(frame(11));
        assert_eq!(slot.keyframe().await.frame_id, 10);
        assert_eq!(slot.next().await.frame_id, 11);
    }
}
