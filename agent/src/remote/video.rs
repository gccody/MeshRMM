use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use meshrmm_protocol::EncodedFrame;
use tokio::sync::Notify;

/// Moonlight uses a bounded 15-unit decode queue. Matching that amount on the
/// encoded sender side absorbs short scheduler/network bursts while keeping
/// the maximum queued media below 250 ms at 60 FPS.
pub const MAX_ENCODED_FRAME_QUEUE: usize = 15;

/// The most frames the queue holds past [`MAX_ENCODED_FRAME_QUEUE`] while
/// they fit in its byte budget (see [`LatestFrameSlot::set_byte_budget`]).
const MAX_SMALL_FRAME_QUEUE: usize = MAX_ENCODED_FRAME_QUEUE * 16;

#[derive(Default)]
struct Queue {
    frames: VecDeque<Arc<EncodedFrame>>,
    bytes: usize,
}

impl Queue {
    fn pop_front(&mut self) -> Option<Arc<EncodedFrame>> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.data.len();
        Some(frame)
    }

    fn clear(&mut self) -> usize {
        let dropped = self.frames.len();
        self.frames.clear();
        self.bytes = 0;
        dropped
    }
}

/// An access unit's bytes with its codec configuration, if any, in front:
/// the access unit itself when there is none, which is every frame but a
/// keyframe.
pub(crate) fn with_codec_config(codec_config: Option<Vec<u8>>, data: Vec<u8>) -> Vec<u8> {
    match codec_config {
        Some(mut joined) => {
            joined.extend_from_slice(&data);
            joined
        }
        None => data,
    }
}

pub struct LatestFrameSlot {
    queue: Mutex<Queue>,
    keyframe: Mutex<Option<Arc<EncodedFrame>>>,
    changed: Notify,
    keyframe_changed: Notify,
    dropped: AtomicU64,
    byte_budget: AtomicUsize,
}

impl Default for LatestFrameSlot {
    fn default() -> Self {
        Self {
            queue: Mutex::new(Queue {
                frames: VecDeque::with_capacity(MAX_ENCODED_FRAME_QUEUE),
                bytes: 0,
            }),
            keyframe: Mutex::new(None),
            changed: Notify::new(),
            keyframe_changed: Notify::new(),
            dropped: AtomicU64::new(0),
            byte_budget: AtomicUsize::new(0),
        }
    }
}

impl LatestFrameSlot {
    /// Publishes directly from the capture callback. Keeping this operation
    /// synchronous preserves encoder output order and avoids spawning one
    /// Tokio task per video frame.
    ///
    /// A full queue is dropped whole, breaking the predictive chain, unless
    /// all of it still fits in the byte budget: frames made while the sender
    /// paces out a large keyframe are usually tiny re-encodes of a static
    /// screen, and sending them keeps the stream going from that keyframe
    /// for less than it costs to send another.
    pub fn publish(&self, frame: EncodedFrame) {
        let frame = Arc::new(frame);
        if frame.keyframe {
            if let Ok(mut keyframe) = self.keyframe.lock() {
                *keyframe = Some(Arc::clone(&frame));
            }
            self.keyframe_changed.notify_waiters();
        }
        let Ok(mut queue) = self.queue.lock() else {
            return;
        };
        if queue.frames.len() >= MAX_ENCODED_FRAME_QUEUE
            && (queue.frames.len() >= MAX_SMALL_FRAME_QUEUE
                || queue.bytes + frame.data.len() > self.byte_budget.load(Ordering::Relaxed))
        {
            let dropped = queue.clear();
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        queue.bytes += frame.data.len();
        queue.frames.push_back(frame);
        drop(queue);
        self.changed.notify_one();
    }

    /// The most bytes of encoded frames the queue may hold past
    /// [`MAX_ENCODED_FRAME_QUEUE`] frames: what the sender sends in a short
    /// time, so holding them adds little latency.
    pub fn set_byte_budget(&self, bytes: usize) {
        self.byte_budget.store(bytes, Ordering::Relaxed);
    }

    pub async fn next(&self) -> Arc<EncodedFrame> {
        loop {
            let notified = self.changed.notified();
            if let Some(frame) = self
                .queue
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front())
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
        if let Ok(mut queue) = self.queue.lock() {
            queue.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.queue.lock().map_or(0, |queue| queue.frames.len())
    }

    pub fn drop_pending(&self) -> usize {
        self.queue.lock().map_or(0, |mut queue| {
            let dropped = queue.clear();
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
            dropped
        })
    }

    pub fn discard_through(&self, frame_id: u64) {
        if let Ok(mut queue) = self.queue.lock() {
            while queue
                .frames
                .front()
                .is_some_and(|frame| frame.frame_id <= frame_id)
            {
                queue.pop_front();
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
            data: vec![frame_id as u8].into(),
        }
    }

    fn keyframe(frame_id: u64) -> EncodedFrame {
        EncodedFrame {
            keyframe: true,
            ..frame(frame_id)
        }
    }

    #[test]
    fn an_access_unit_without_codec_configuration_is_not_copied() {
        let data = vec![0, 0, 0, 1, 0x41];
        let pointer = data.as_ptr();
        let joined = with_codec_config(None, data);
        assert_eq!(joined.as_ptr(), pointer);
        assert_eq!(
            with_codec_config(Some(vec![0, 0, 0, 1, 0x67]), vec![0, 0, 0, 1, 0x65]),
            [0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x65]
        );
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

    fn sized(frame_id: u64, bytes: usize) -> EncodedFrame {
        EncodedFrame {
            data: vec![0; bytes].into(),
            ..frame(frame_id)
        }
    }

    #[tokio::test]
    async fn frames_that_fit_the_byte_budget_are_kept_past_the_frame_limit() {
        // Three seconds of 250-byte re-encodes of a static screen at 30 FPS,
        // made while a keyframe was being sent, within a 30 kB budget.
        let slot = LatestFrameSlot::default();
        slot.set_byte_budget(30_000);
        for frame_id in 1..=90 {
            slot.publish(sized(frame_id, 250));
        }
        assert_eq!((slot.len(), slot.dropped()), (90, 0));
        for frame_id in 1..=90 {
            assert_eq!(slot.next().await.frame_id, frame_id);
        }

        // Ordinary frames still drop the whole queue at the frame limit, as
        // do small ones past the budget or past the larger frame limit.
        for (bytes, budget, limit) in [
            (12_000, 30_000, MAX_ENCODED_FRAME_QUEUE),
            (250, 3_000, MAX_ENCODED_FRAME_QUEUE),
            (1, usize::MAX, MAX_SMALL_FRAME_QUEUE),
        ] {
            let slot = LatestFrameSlot::default();
            slot.set_byte_budget(budget);
            for frame_id in 1..=limit as u64 + 1 {
                slot.publish(sized(frame_id, bytes));
            }
            assert_eq!(
                (slot.len(), slot.dropped()),
                (1, limit as u64),
                "{bytes} B frames, {budget} B budget"
            );
        }
    }
}
