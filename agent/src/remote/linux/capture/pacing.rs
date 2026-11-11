//! When a capture converts or grabs a frame, decided from the times it is
//! given, so the decisions can be tested without a display server.
use std::time::{Duration, Instant};

/// Whether a frame may be converted now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admit {
    /// Convert it now.
    Now,
    /// Hold it, newest first, and convert it at this time unless a newer
    /// frame is converted first.
    At(Instant),
}

/// Keeps a source that delivers frames faster than the stream's rate, such
/// as a compositor that ignores the maximum frame rate it was asked for, to
/// that rate before frames are converted. Frames due within an eighth of an
/// interval are converted at once, so the jitter of a source that keeps to
/// the rate costs no delay, while the average stays at or below the rate.
pub(super) struct FramePacer {
    interval: Duration,
    tolerance: Duration,
    /// When the next frame is due, once one has been converted.
    next: Option<Instant>,
}

impl FramePacer {
    pub(super) fn new(frames_per_second: u32) -> Self {
        let interval = Duration::from_secs(1) / frames_per_second.max(1);
        Self {
            interval,
            tolerance: interval / 8,
            next: None,
        }
    }

    /// Whether a frame that arrived at `now` is converted now. The first
    /// frame, and the first after a pause, always is.
    pub(super) fn admit(&mut self, now: Instant) -> Admit {
        match self.next {
            Some(next) if now + self.tolerance < next => Admit::At(next - self.tolerance),
            next => {
                self.next = Some(next.map_or(now, |next| next.max(now)) + self.interval);
                Admit::Now
            }
        }
    }
}

/// When polled capture grabs the screen. Each tick at the frame rate asks
/// [`GrabSchedule::due`]; a hint that something changed (damage reported by
/// the X server, or the pointer drawn into the frame moving) grabs at once.
/// Otherwise the screen is grabbed at the frame rate until `idle_after`
/// grabs in a row found it unchanged, then once every `idle` until a grab
/// finds a change.
pub(super) struct GrabSchedule {
    interval: Duration,
    idle_after: u32,
    idle: Duration,
    unchanged: u32,
    last: Option<Instant>,
}

impl GrabSchedule {
    pub(super) fn new(interval: Duration, idle_after: u32, idle: Duration) -> Self {
        Self {
            interval,
            idle_after,
            idle: idle.max(interval),
            unchanged: 0,
            last: None,
        }
    }

    /// The time from one grab to the next without a hint.
    pub(super) fn gap(&self) -> Duration {
        if self.unchanged < self.idle_after {
            self.interval
        } else {
            self.idle
        }
    }

    /// Whether the tick at `now` grabs. Ticks run late or early by up to
    /// half an interval and still count as on time.
    pub(super) fn due(&self, now: Instant, hint: bool) -> bool {
        hint || self
            .last
            .is_none_or(|last| now + self.interval / 2 >= last + self.gap())
    }

    /// Records a grab at `now` and whether it differed from the last.
    pub(super) fn grabbed(&mut self, now: Instant, changed: bool) {
        self.last = Some(now);
        self.unchanged = if changed {
            0
        } else {
            self.unchanged.saturating_add(1)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames `period` apart for `duration`, from `start`.
    fn arrivals(start: Instant, period: Duration, duration: Duration) -> Vec<Instant> {
        (0..)
            .map(|index| start + period * index)
            .take_while(|&at| at < start + duration)
            .collect()
    }

    /// Runs frames arriving at `times` through a pacer the way a capture
    /// does: a frame converted now releases any held one; a frame held
    /// replaces an older held frame and is converted when its time comes.
    /// Returns when each converted frame was converted and which it was.
    fn pace(frames_per_second: u32, times: &[Instant]) -> Vec<(Instant, usize)> {
        let mut pacer = FramePacer::new(frames_per_second);
        let mut held: Option<(usize, Instant)> = None;
        let mut converted = Vec::new();
        for (index, &now) in times.iter().enumerate() {
            // The held frame's timer fires before this arrival.
            if let Some((frame, at)) = held
                && at <= now
            {
                assert_eq!(pacer.admit(at), Admit::Now);
                converted.push((at, frame));
            }
            // A frame converted now releases any held one; a frame held
            // replaces it.
            match pacer.admit(now) {
                Admit::Now => {
                    held = None;
                    converted.push((now, index));
                }
                Admit::At(at) => held = Some((index, at)),
            }
        }
        if let Some((frame, at)) = held {
            assert_eq!(pacer.admit(at), Admit::Now);
            converted.push((at, frame));
        }
        converted
    }

    /// The most frames converted in the second after any conversion.
    fn most_in_any_second(converted: &[(Instant, usize)]) -> usize {
        converted
            .iter()
            .map(|&(start, _)| {
                converted
                    .iter()
                    .filter(|&&(at, _)| at > start && at <= start + Duration::from_secs(1))
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn a_fast_compositor_is_converted_at_the_stream_rate_ending_on_its_last_frame() {
        let start = Instant::now();
        for hertz in [60, 120, 144, 240] {
            let times = arrivals(
                start,
                Duration::from_secs(1) / hertz,
                Duration::from_millis(3010),
            );
            let converted = pace(30, &times);
            let most = most_in_any_second(&converted);
            assert!((28..=30).contains(&most), "{hertz} Hz: {most} a second");
            // The burst's last frame is shown, within an interval of it.
            let &(at, last) = converted.last().unwrap();
            assert_eq!(last, times.len() - 1, "{hertz} Hz");
            assert!(at - times[last] <= Duration::from_millis(34), "{hertz} Hz");
            // The first frame is converted as it arrives.
            assert_eq!(converted[0], (start, 0));
        }
    }

    #[test]
    fn a_compositor_that_keeps_to_the_rate_waits_for_nothing() {
        let start = Instant::now();
        // Mutter's interval in whole microseconds, a little under ours, with
        // a frame now and then a millisecond early or late.
        let times: Vec<Instant> = (0..90_u32)
            .map(|index| {
                let jitter = match index % 7 {
                    3 => -1000,
                    5 => 1000,
                    _ => 0,
                };
                start + Duration::from_micros((i64::from(index) * 33_333 + jitter) as u64)
            })
            .collect();
        let converted = pace(30, &times);
        assert_eq!(converted.len(), times.len());
        for (&(at, frame), &arrived) in converted.iter().zip(&times) {
            assert_eq!(at, arrived, "frame {frame} waited");
        }
    }

    #[test]
    fn the_first_frame_after_a_pause_is_converted_at_once() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30);
        assert_eq!(pacer.admit(start), Admit::Now);
        let early = start + Duration::from_millis(5);
        assert!(matches!(pacer.admit(early), Admit::At(at) if at > early));
        assert_eq!(pacer.admit(start + Duration::from_millis(400)), Admit::Now);
    }

    #[test]
    fn a_static_screen_is_grabbed_less_often_until_it_changes() {
        let interval = Duration::from_secs(1) / 30;
        let idle = Duration::from_millis(250);
        let mut schedule = GrabSchedule::new(interval, 15, idle);
        let start = Instant::now();
        let ticks = |seconds: u32| (0..30 * seconds).map(move |tick| start + interval * tick);
        let mut grabs = Vec::new();
        for now in ticks(3) {
            if schedule.due(now, false) {
                // The screen changes only on the first grab.
                schedule.grabbed(now, grabs.is_empty());
                grabs.push(now);
            }
        }
        // 16 grabs at the frame rate (the change and 15 unchanged), then one
        // every 250 ms.
        assert_eq!(&grabs[..16], &ticks(1).take(16).collect::<Vec<_>>()[..]);
        assert_eq!(schedule.gap(), idle);
        for pair in grabs[15..].windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap >= idle - interval / 2 && gap <= idle + interval,
                "{gap:?}"
            );
        }
        assert!(grabs.len() <= 16 + 11, "{} grabs", grabs.len());
        // A hint grabs at the next tick, and a change found restores the
        // frame rate at once.
        let now = start + interval * 90;
        assert!(schedule.due(now, true));
        assert!(!schedule.due(now, false));
        schedule.grabbed(now, true);
        assert_eq!(schedule.gap(), interval);
        assert!(schedule.due(now + interval, false));
    }

    #[test]
    fn with_damage_an_unchanged_screen_is_grabbed_only_as_a_safety_net() {
        let interval = Duration::from_secs(1) / 30;
        let mut schedule = GrabSchedule::new(interval, 0, Duration::from_secs(1));
        let start = Instant::now();
        assert!(schedule.due(start, false));
        schedule.grabbed(start, true);
        let grabs = (1..90)
            .map(|tick| start + interval * tick)
            .filter(|&now| {
                let due = schedule.due(now, false);
                if due {
                    schedule.grabbed(now, false);
                }
                due
            })
            .count();
        assert_eq!(grabs, 2);
        // Damage grabs at once.
        assert!(schedule.due(start + interval * 91, true));
    }
}
