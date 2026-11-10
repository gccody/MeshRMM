//! Bookkeeping for the swap chain's frame latency waitable, kept apart from
//! the D3D calls so it runs in tests without a swap chain.
//!
//! With a maximum frame latency of one, DXGI signals the waitable once for
//! each frame the display has taken, and at most one signal is ever due:
//! the one for the frame on screen. The renderer presents a picture once it
//! holds a signal, so `Present` does not wait for the display. Nothing here
//! assumes every present is answered: DXGI does not signal for a window
//! that is not composed, such as a minimised one, and may or may not answer
//! those presents later. So:
//!
//! - Every available signal is taken at each check, and those beyond the
//!   one that can be due are dropped. Late answers, however many, cannot
//!   leave the waitable signalled ahead of the display.
//! - A present made because no signal came in time (the worker's fallback)
//!   clears the debt instead of adding to it. That the signal did not come
//!   is the evidence the accounting is off or the display is not taking
//!   frames; forgetting a real debt costs at most a present that waits one
//!   refresh inside `Present`, while keeping a false one would make every
//!   later signal look owed and leave the worker presenting only at its
//!   fallback rate.

/// The most signals taken in one check. A count beyond this is only
/// possible after a very long time unanswered; the next check takes the
/// rest.
const MAX_SIGNALS_PER_CHECK: u32 = 1024;

#[derive(Debug, Default)]
pub(super) struct FrameLatency {
    /// A signal was taken and no frame presented since.
    ready: bool,
    /// Frames presented without a signal for a redraw, whose own signals
    /// are still to come and are not a sign the display took the next one.
    debt: u32,
}

/// Why a frame is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::platform::windows) enum PresentKind {
    /// A new picture, or a redraw, at a time of the renderer's choosing.
    Normal,
    /// A picture that waited too long for a signal.
    Overdue,
}

impl FrameLatency {
    /// Notes a signal taken from the waitable.
    pub(super) fn signalled(&mut self) {
        if self.debt > 0 {
            self.debt -= 1;
        } else {
            // A second signal while one is held is a late answer to a
            // present made without one; dropping it keeps the count true.
            self.ready = true;
        }
    }

    /// Takes every signal `take` has available, and returns whether one is
    /// held.
    pub(super) fn take_available(&mut self, mut take: impl FnMut() -> bool) -> bool {
        for _ in 0..MAX_SIGNALS_PER_CHECK {
            if !take() {
                break;
            }
            self.signalled();
        }
        self.ready
    }

    /// Notes a frame presented.
    pub(super) fn presented(&mut self, kind: PresentKind) {
        if std::mem::take(&mut self.ready) {
            return;
        }
        match kind {
            PresentKind::Normal => self.debt = self.debt.saturating_add(1),
            PresentKind::Overdue => self.debt = 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::windows::frame_schedule::MAX_PRESENT_WAIT_US;

    const REFRESH_US: u64 = 16_667;
    /// The worker checks the swap chain several times a refresh, as its
    /// waits are a few milliseconds long.
    const CHECKS_PER_REFRESH: u64 = 4;

    /// A swap chain with a frame latency of one: its waitable is a
    /// semaphore that starts with one count, and the display takes one
    /// queued frame each refresh and releases a count for it, while the
    /// window is composed.
    struct SwapChain {
        available: u32,
        queued: u32,
        composed: bool,
        /// Frames presented while the window was not composed.
        unanswered: u32,
    }

    impl SwapChain {
        fn new() -> Self {
            Self {
                available: 1,
                queued: 0,
                composed: true,
                unanswered: 0,
            }
        }

        fn refresh(&mut self) {
            if self.composed && self.queued > 0 {
                self.queued -= 1;
                self.available += 1;
            }
        }

        fn take(&mut self) -> bool {
            let taken = self.available > 0;
            self.available -= u32::from(taken);
            taken
        }

        fn present(&mut self) {
            if self.composed {
                self.queued += 1;
            } else {
                self.unanswered += 1;
            }
        }
    }

    #[derive(Debug, Default)]
    struct Shown {
        on_signal: u32,
        overdue: u32,
        /// Pictures presented on a signal while the display still had a
        /// frame queued: `Present` would have waited a refresh for them.
        blocked: u32,
    }

    /// The worker with a picture always waiting.
    struct Worker {
        latency: FrameLatency,
        now_us: u64,
        waiting_since_us: u64,
    }

    impl Worker {
        fn new() -> Self {
            Self {
                latency: FrameLatency::default(),
                now_us: 0,
                waiting_since_us: 0,
            }
        }

        /// Runs for `refreshes` display refreshes.
        fn run(&mut self, swap_chain: &mut SwapChain, refreshes: u32) -> Shown {
            let mut shown = Shown::default();
            for check in 0..u64::from(refreshes) * CHECKS_PER_REFRESH {
                self.now_us += REFRESH_US / CHECKS_PER_REFRESH;
                if check % CHECKS_PER_REFRESH == 0 {
                    swap_chain.refresh();
                }
                if self.latency.take_available(|| swap_chain.take()) {
                    if swap_chain.queued > 0 {
                        shown.blocked += 1;
                    }
                    self.latency.presented(PresentKind::Normal);
                    shown.on_signal += 1;
                } else if self.now_us - self.waiting_since_us >= MAX_PRESENT_WAIT_US {
                    self.latency.presented(PresentKind::Overdue);
                    shown.overdue += 1;
                } else {
                    continue;
                }
                swap_chain.present();
                self.waiting_since_us = self.now_us;
            }
            shown
        }

        /// A redraw after a resize: presented at once, with or without a
        /// signal.
        fn redraw(&mut self, swap_chain: &mut SwapChain) {
            self.latency.take_available(|| swap_chain.take());
            self.latency.presented(PresentKind::Normal);
            swap_chain.present();
        }
    }

    #[test]
    fn one_signal_a_present_shows_a_picture_each_refresh() {
        let mut swap_chain = SwapChain::new();
        let shown = Worker::new().run(&mut swap_chain, 120);
        assert_eq!(shown.on_signal, 120, "{shown:?}");
        assert_eq!((shown.overdue, shown.blocked), (0, 0), "{shown:?}");
    }

    #[test]
    fn a_redraw_without_a_signal_is_paid_back_by_its_own_signal() {
        let mut swap_chain = SwapChain::new();
        let mut worker = Worker::new();
        worker.run(&mut swap_chain, 10);
        // A resize redraws while the last picture is still queued.
        worker.redraw(&mut swap_chain);
        let shown = worker.run(&mut swap_chain, 120);
        assert!(shown.on_signal >= 118, "{shown:?}");
        assert_eq!((shown.overdue, shown.blocked), (0, 0), "{shown:?}");
    }

    #[test]
    fn a_redraw_whose_signal_never_comes_costs_one_late_picture() {
        let mut swap_chain = SwapChain::new();
        let mut worker = Worker::new();
        worker.run(&mut swap_chain, 10);
        worker.redraw(&mut swap_chain);
        // The display never answers it.
        swap_chain.queued -= 1;
        let shown = worker.run(&mut swap_chain, 120);
        assert!(shown.overdue <= 1, "{shown:?}");
        assert!(shown.on_signal >= 110, "{shown:?}");
    }

    /// Minimises the window for `seconds`, presenting at the fallback rate,
    /// and returns the swap chain and worker as it comes back.
    fn minimised(seconds: u32) -> (SwapChain, Worker) {
        let mut swap_chain = SwapChain::new();
        let mut worker = Worker::new();
        worker.run(&mut swap_chain, 60);
        swap_chain.composed = false;
        let shown = worker.run(&mut swap_chain, seconds * 60);
        // About ten a second, all presented without a signal.
        assert!(shown.overdue >= seconds * 9, "{shown:?}");
        assert!(shown.on_signal <= 1, "{shown:?}");
        swap_chain.composed = true;
        (swap_chain, worker)
    }

    #[test]
    fn a_window_back_from_minimised_shows_a_picture_each_refresh_again() {
        let (mut swap_chain, mut worker) = minimised(60);
        // Those presents are never answered.
        swap_chain.unanswered = 0;
        swap_chain.queued = 0;
        let shown = worker.run(&mut swap_chain, 120);
        assert!(shown.on_signal >= 110, "{shown:?}");
        assert!(shown.overdue <= 1, "{shown:?}");
        assert_eq!(shown.blocked, 0, "{shown:?}");
    }

    #[test]
    fn late_answers_to_a_minimised_window_do_not_run_ahead_of_the_display() {
        let (mut swap_chain, mut worker) = minimised(60);
        // Every present made while minimised is answered at once.
        swap_chain.available += std::mem::take(&mut swap_chain.unanswered);
        let shown = worker.run(&mut swap_chain, 120);
        assert!(shown.on_signal >= 110, "{shown:?}");
        assert!(shown.overdue <= 1, "{shown:?}");
        assert_eq!(shown.blocked, 0, "{shown:?}");
    }
}
