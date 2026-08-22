use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meshrmm_protocol::EncodedFrame;
use tokio::sync::{Mutex, Notify};

#[derive(Default)]
pub struct LatestFrameSlot {
    frame: Mutex<Option<Arc<EncodedFrame>>>,
    keyframe: Mutex<Option<Arc<EncodedFrame>>>,
    changed: Notify,
    keyframe_changed: Notify,
    dropped: AtomicU64,
}

impl LatestFrameSlot {
    pub async fn publish(&self, frame: EncodedFrame) {
        let frame = Arc::new(frame);
        if frame.keyframe {
            *self.keyframe.lock().await = Some(Arc::clone(&frame));
            self.keyframe_changed.notify_waiters();
        }
        let mut current = self.frame.lock().await;
        if current.replace(frame).is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(current);
        self.changed.notify_one();
    }

    pub async fn next(&self) -> Arc<EncodedFrame> {
        loop {
            let notified = self.changed.notified();
            if let Some(frame) = self.frame.lock().await.take() {
                return frame;
            }
            notified.await;
        }
    }

    pub async fn has_newer(&self) -> bool {
        self.frame.lock().await.is_some()
    }

    pub async fn clear(&self) {
        *self.frame.lock().await = None;
        *self.keyframe.lock().await = None;
    }

    /// Returns the newest compressed keyframe without consuming the normal
    /// latest-frame slot. This retains one decoder bootstrap frame, not a
    /// playback queue.
    pub async fn keyframe(&self) -> Arc<EncodedFrame> {
        loop {
            let notified = self.keyframe_changed.notified();
            if let Some(frame) = self.keyframe.lock().await.clone() {
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
        slot.publish(frame(10)).await;
        slot.publish(frame(11)).await;
        assert_eq!(slot.next().await.frame_id, 11);
        assert_eq!(slot.dropped(), 1);
    }

    #[tokio::test]
    async fn keyframe_is_retained_when_latest_frame_is_replaced() {
        let slot = LatestFrameSlot::default();
        slot.publish(keyframe(10)).await;
        slot.publish(frame(11)).await;
        assert_eq!(slot.keyframe().await.frame_id, 10);
        assert_eq!(slot.next().await.frame_id, 11);
    }
}
