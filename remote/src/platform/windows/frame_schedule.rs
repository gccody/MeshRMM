//! What the decode/present worker does with the frames
//! [`Presenter::publish`] queues: it decodes every one, in order, since
//! each delta frame needs the one before it, and presents only the newest
//! picture, once the swap chain can take a frame without waiting for the
//! display. However fast frames arrive, the display then shows the newest
//! one at its next refresh instead of working through a queue one refresh
//! at a time. The Media Foundation and DXGI calls are behind
//! [`VideoPipeline`], so this runs in tests with fakes.

use super::*;

/// How long a decoded picture waits for the swap chain before it is
/// presented anyway. The swap chain signals once a refresh; this only
/// covers one that stops signalling.
pub(super) const MAX_PRESENT_WAIT_US: u64 = 100_000;

/// What the decoder made of a frame it was offered.
pub(super) struct Decoded<P> {
    /// False when the decoder no longer took input; the frame goes back to
    /// the front of the queue.
    pub(super) accepted: bool,
    /// Pictures the decoder finished, oldest first.
    pub(super) pictures: Vec<P>,
}

/// The decoder and swap chain the worker drives.
pub(super) trait VideoPipeline {
    type Picture;

    /// Pictures an asynchronous decoder finished since the last call.
    unsafe fn poll(&mut self) -> anyhow::Result<Vec<Self::Picture>>;

    /// Whether the decoder takes another frame now.
    fn wants_input(&self) -> bool;

    unsafe fn decode(&mut self, frame: &QueuedFrame) -> anyhow::Result<Decoded<Self::Picture>>;

    /// Whether a picture can be presented now without waiting for the
    /// display to take the previous one.
    unsafe fn ready_to_present(&mut self) -> bool;

    /// Presents `picture`; `overdue` when it is presented because the
    /// swap chain did not become ready in time.
    unsafe fn present(&mut self, picture: Self::Picture, overdue: bool) -> anyhow::Result<()>;
}

/// The newest decoded picture that has not been presented yet.
pub(super) struct FrameSchedule<P> {
    /// The picture, and since when a picture has waited for the swap chain.
    waiting: Option<(P, u64)>,
}

impl<P> FrameSchedule<P> {
    pub(super) fn new() -> Self {
        Self { waiting: None }
    }

    /// Whether a decoded picture waits for the swap chain.
    pub(super) fn picture_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    /// Forgets the waiting picture, which belongs to a stream that was
    /// just replaced.
    pub(super) fn clear(&mut self) {
        self.waiting = None;
    }

    /// One pass of the worker: asks for a keyframe when the queue
    /// overflowed, decodes every queued frame the decoder takes, and
    /// presents the newest picture when the swap chain can take it.
    pub(super) unsafe fn run<V: VideoPipeline<Picture = P>>(
        &mut self,
        shared: &Shared,
        video: &mut V,
        now_us: u64,
    ) -> anyhow::Result<()> {
        request_recovery_keyframe(shared);
        let pictures = unsafe { video.poll()? };
        self.keep(pictures, now_us);
        while video.wants_input() {
            let Some(queued) = shared
                .queued
                .lock()
                .ok()
                .and_then(|mut queued| queued.pop_front())
            else {
                break;
            };
            let frame_id = queued.frame.frame_id;
            let decoded = unsafe { video.decode(&queued) }
                .with_context(|| format!("video frame {frame_id} could not be decoded"))?;
            self.keep(decoded.pictures, now_us);
            if !decoded.accepted {
                tracing::warn!(
                    frame_id,
                    "video decoder readiness changed before frame submission"
                );
                if let Ok(mut pending) = shared.queued.lock() {
                    pending.push_front(queued);
                }
                break;
            }
        }
        let overdue = self
            .waiting
            .as_ref()
            .is_some_and(|(_, since)| now_us.saturating_sub(*since) >= MAX_PRESENT_WAIT_US);
        if self.waiting.is_some()
            && (overdue || unsafe { video.ready_to_present() })
            && let Some((picture, _)) = self.waiting.take()
        {
            unsafe { video.present(picture, overdue)? };
        }
        Ok(())
    }

    /// Keeps the newest of `pictures`; an older waiting one is never shown.
    fn keep(&mut self, pictures: Vec<P>, now_us: u64) {
        if let Some(newest) = pictures.into_iter().last() {
            let since = self.waiting.take().map_or(now_us, |(_, since)| since);
            self.waiting = Some((newest, since));
        }
    }
}

/// Asks for the keyframe [`Presenter::publish`] wants after dropping
/// frames. It asks from here, not from `publish`, so a window thread that
/// stalls in a drag or a menu asks once, when it runs again, rather than
/// each time the queue fills while it cannot decode. The session's limiter
/// decides when the request goes.
fn request_recovery_keyframe(shared: &Shared) {
    let wanted = shared
        .keyframe_wanted
        .lock()
        .ok()
        .and_then(|mut wanted| wanted.take());
    if let Some(stream_id) = wanted {
        tracing::warn!(
            stream_id = stream_id.0,
            "video frames arrived faster than they were decoded; asking for a keyframe"
        );
        shared
            .control
            .send(SessionMessage::RequestKeyframe { stream_id });
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::VideoStreamId;

    use super::*;

    const FRAME_INTERVAL_US: u64 = 16_667;

    /// A decoder that finishes each frame at once, and a swap chain with a
    /// frame latency of one: after a present, the next one can go once the
    /// display has refreshed.
    struct FakeVideo {
        accepting: bool,
        /// How many more frames the decoder takes, when limited.
        accept_budget: Option<usize>,
        decoded: Vec<u64>,
        presented: Vec<u64>,
        can_present: bool,
        on_screen_pending: bool,
    }

    impl FakeVideo {
        fn new() -> Self {
            Self {
                accepting: true,
                accept_budget: None,
                decoded: Vec::new(),
                presented: Vec::new(),
                can_present: true,
                on_screen_pending: false,
            }
        }

        fn refresh(&mut self) {
            if std::mem::take(&mut self.on_screen_pending) {
                self.can_present = true;
            }
        }
    }

    impl VideoPipeline for FakeVideo {
        type Picture = u64;

        unsafe fn poll(&mut self) -> anyhow::Result<Vec<u64>> {
            Ok(Vec::new())
        }

        fn wants_input(&self) -> bool {
            self.accepting && self.accept_budget != Some(0)
        }

        unsafe fn decode(&mut self, frame: &QueuedFrame) -> anyhow::Result<Decoded<u64>> {
            if !self.wants_input() {
                return Ok(Decoded {
                    accepted: false,
                    pictures: Vec::new(),
                });
            }
            if let Some(budget) = self.accept_budget.as_mut() {
                *budget -= 1;
            }
            self.decoded.push(frame.frame.frame_id);
            Ok(Decoded {
                accepted: true,
                pictures: vec![frame.frame.frame_id],
            })
        }

        unsafe fn ready_to_present(&mut self) -> bool {
            self.can_present
        }

        unsafe fn present(&mut self, picture: u64, _overdue: bool) -> anyhow::Result<()> {
            self.can_present = false;
            self.on_screen_pending = true;
            self.presented.push(picture);
            Ok(())
        }
    }

    fn frame(frame_id: u64, keyframe: bool) -> EncodedFrame {
        EncodedFrame {
            stream_id: VideoStreamId(2),
            frame_id,
            capture_timestamp_us: 0,
            encode_complete_timestamp_us: 0,
            send_timestamp_us: 0,
            keyframe,
            data: vec![0, 0, 0, 1],
        }
    }

    fn presenter() -> (Presenter, Arc<Mutex<Vec<SessionMessage>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let shared = Shared::new(
            test_sink(Arc::clone(&sent), Default::default()),
            DebugInfo::new("test"),
        );
        shared.running.store(true, Ordering::Release);
        let presenter = Presenter {
            shared: Arc::new(shared),
            worker: None,
        };
        (presenter, sent)
    }

    fn queued(presenter: &Presenter) -> usize {
        presenter.shared.queued.lock().unwrap().len()
    }

    /// Runs `frames` frames of a stream at `fps` on a display refreshing
    /// at `refresh_hz`, with the worker running after every event.
    fn stream(fps: u64, refresh_hz: u64, frames: u64) -> (FakeVideo, Vec<SessionMessage>) {
        let (presenter, sent) = presenter();
        let mut video = FakeVideo::new();
        let mut schedule = FrameSchedule::new();
        let frame_interval_us = 1_000_000 / fps;
        let refresh_interval_us = 1_000_000 / refresh_hz;
        let mut next_frame_us = 0;
        let mut next_refresh_us = refresh_interval_us / 2;
        let mut frame_id = 1;
        let mut max_queued = 0;
        while frame_id <= frames {
            let now_us = next_frame_us.min(next_refresh_us);
            if now_us == next_frame_us {
                assert!(presenter.publish(frame(frame_id, frame_id == 1), now_us));
                frame_id += 1;
                next_frame_us += frame_interval_us;
            } else {
                video.refresh();
                next_refresh_us += refresh_interval_us;
            }
            max_queued = max_queued.max(queued(&presenter));
            unsafe { schedule.run(&presenter.shared, &mut video, now_us) }.unwrap();
            let decoded = video.decoded.last().copied();
            if let Some(presented) = video.presented.last() {
                // Only the newest picture is ever shown.
                assert!(
                    Some(*presented) == decoded || schedule.picture_waiting(),
                    "presented {presented} while {decoded:?} was decoded"
                );
            }
            assert_eq!(queued(&presenter), 0);
        }
        assert!(max_queued <= 1, "{max_queued} frames queued");
        let sent = sent.lock().unwrap().clone();
        (video, sent)
    }

    #[test]
    fn a_stream_faster_than_the_display_decodes_every_frame_and_shows_the_newest() {
        let (video, sent) = stream(60, 30, 120);
        // Every frame is decoded, in order.
        assert_eq!(video.decoded, (1..=120).collect::<Vec<_>>());
        // One picture a refresh, not one a frame.
        assert!(
            (59..=61).contains(&video.presented.len()),
            "{} presented",
            video.presented.len()
        );
        assert!(video.presented.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(video.presented.last().is_some_and(|last| *last >= 117));
        assert!(sent.is_empty(), "asked for a keyframe: {sent:?}");
    }

    #[test]
    fn a_stream_slightly_faster_than_the_display_does_not_queue() {
        let (video, sent) = stream(61, 60, 183);
        assert_eq!(video.decoded.len(), 183);
        assert!(video.presented.len() <= 181);
        assert!(sent.is_empty(), "asked for a keyframe: {sent:?}");
    }

    #[test]
    fn a_picture_waits_for_the_display_and_then_shows() {
        let (presenter, _) = presenter();
        let mut video = FakeVideo::new();
        video.can_present = false;
        let mut schedule = FrameSchedule::new();
        assert!(presenter.publish(frame(1, true), 0));
        unsafe { schedule.run(&presenter.shared, &mut video, 0) }.unwrap();
        assert!(video.presented.is_empty());
        assert!(schedule.picture_waiting());
        // A newer picture replaces it while the display is busy.
        assert!(presenter.publish(frame(2, false), 1_000));
        unsafe { schedule.run(&presenter.shared, &mut video, 1_000) }.unwrap();
        assert!(video.presented.is_empty());
        video.can_present = true;
        unsafe { schedule.run(&presenter.shared, &mut video, 2_000) }.unwrap();
        assert_eq!(video.presented, [2]);
        assert!(!schedule.picture_waiting());
    }

    #[test]
    fn a_swap_chain_that_stops_signalling_still_shows_pictures() {
        let (presenter, _) = presenter();
        let mut video = FakeVideo::new();
        video.can_present = false;
        let mut schedule = FrameSchedule::new();
        assert!(presenter.publish(frame(1, true), 0));
        unsafe { schedule.run(&presenter.shared, &mut video, 0) }.unwrap();
        unsafe { schedule.run(&presenter.shared, &mut video, MAX_PRESENT_WAIT_US - 1) }.unwrap();
        assert!(video.presented.is_empty());
        unsafe { schedule.run(&presenter.shared, &mut video, MAX_PRESENT_WAIT_US) }.unwrap();
        assert_eq!(video.presented, [1]);
    }

    #[test]
    fn a_decoder_that_stops_taking_input_keeps_the_rest_in_order() {
        let (presenter, _) = presenter();
        let mut video = FakeVideo::new();
        video.accept_budget = Some(2);
        let mut schedule = FrameSchedule::new();
        for frame_id in 1..=5 {
            assert!(presenter.publish(frame(frame_id, frame_id == 1), 0));
        }
        unsafe { schedule.run(&presenter.shared, &mut video, 0) }.unwrap();
        assert_eq!(video.decoded, [1, 2]);
        assert_eq!(queued(&presenter), 3);
        video.accept_budget = None;
        unsafe { schedule.run(&presenter.shared, &mut video, FRAME_INTERVAL_US) }.unwrap();
        assert_eq!(video.decoded, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_stalled_worker_asks_for_one_keyframe_when_it_runs_again() {
        let (presenter, sent) = presenter();
        let mut video = FakeVideo::new();
        let mut schedule = FrameSchedule::new();
        assert!(presenter.publish(frame(1, true), 0));
        // A window drag holds the worker's thread: the queue fills, more
        // than once.
        for frame_id in 2..=60 {
            presenter.publish(frame(frame_id, false), frame_id * FRAME_INTERVAL_US);
        }
        assert!(presenter.shared.recovering.load(Ordering::Acquire));
        assert!(sent.lock().unwrap().is_empty());
        unsafe { schedule.run(&presenter.shared, &mut video, 61 * FRAME_INTERVAL_US) }.unwrap();
        unsafe { schedule.run(&presenter.shared, &mut video, 62 * FRAME_INTERVAL_US) }.unwrap();
        assert_eq!(
            *sent.lock().unwrap(),
            [SessionMessage::RequestKeyframe {
                stream_id: VideoStreamId(2)
            }]
        );
        // The keyframe ends the recovery.
        assert!(presenter.publish(frame(61, true), 63 * FRAME_INTERVAL_US));
        unsafe { schedule.run(&presenter.shared, &mut video, 63 * FRAME_INTERVAL_US) }.unwrap();
        assert_eq!(video.decoded, [61]);
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_keyframe_that_arrives_first_answers_the_overflow() {
        let (presenter, sent) = presenter();
        let mut video = FakeVideo::new();
        let mut schedule = FrameSchedule::new();
        for frame_id in 1..=20 {
            presenter.publish(frame(frame_id, frame_id == 1), 0);
        }
        // Another loss already brought a keyframe before the worker ran.
        assert!(presenter.publish(frame(21, true), 0));
        unsafe { schedule.run(&presenter.shared, &mut video, 0) }.unwrap();
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(video.decoded, [21]);
    }
}
