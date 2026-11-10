//! When the Agent scans for updates on its own. The server only says what the
//! schedule is ([`UpdateScanSchedule`]): the instance's interval and, when the
//! Agent connects, the last scan it received from the device. The Agent keeps
//! to it, so the server does no periodic work for scheduled scans and a
//! fleet's scans are spread out by construction:
//!
//! - The next scan is an interval after the last one, put off by up to
//!   [`JITTER`] of the interval at random, so devices that scanned together
//!   drift apart. Every scan counts as the last one, whoever asked for it.
//! - A device whose last scan is older than the interval when it connects
//!   (or that never scanned), as a whole fleet is after the server was down,
//!   scans at a random moment between [`FIRST_DELAY`] and [`OVERDUE_SPREAD`]
//!   (or the interval, when shorter) after connecting, never at once.
//! - A device whose last scan said it had to restart scans between
//!   [`FIRST_DELAY`] and [`RESTART_SPREAD`] after connecting, but only if it
//!   has started since that scan, so the website sees the restart done; a
//!   reconnect without a restart follows the schedule. This holds with the
//!   interval off too, as one scan after each such restart.
//! - Without an interval there are no other scheduled scans.
//!
//! Scans run only while connected: the schedule is forgotten when a
//! connection starts and the server's arrives first thing on it, so a scan
//! that came due while offline runs by the new schedule.
use std::time::Duration;

use meshrmm_protocol::{UpdateScan, UpdateScanSchedule};

/// The most a scheduled scan is put off past its interval, as a share of it.
pub(crate) const JITTER: f64 = 0.1;
/// The soonest a scheduled scan runs after the schedule arrives, so the
/// connection and the computer's own start settle first.
pub(crate) const FIRST_DELAY: Duration = Duration::from_secs(60);
/// The latest an overdue device scans after connecting: a fleet of 100,000
/// that reconnects together scans at about 28 a second.
pub(crate) const OVERDUE_SPREAD: Duration = Duration::from_secs(60 * 60);
/// The latest a device that restarted to finish installing updates scans
/// after connecting.
pub(crate) const RESTART_SPREAD: Duration = Duration::from_secs(5 * 60);

/// A scan, by the device's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scanned {
    pub(crate) at_unix_ms: i64,
    pub(crate) reboot_pending: bool,
}

/// The Unix milliseconds of the next scheduled scan, or `None` for none, at
/// `now`, given the interval, the last scan, when the computer started and a
/// random `fraction` in `[0, 1)`.
pub(crate) fn next_scan(
    interval: Option<Duration>,
    last: Option<Scanned>,
    booted_at: Option<i64>,
    now: i64,
    fraction: f64,
) -> Option<i64> {
    let fraction = fraction.clamp(0.0, 1.0);
    let within = |earliest: Duration, latest: Duration| {
        let latest = latest.max(earliest);
        now.saturating_add(millis(earliest + (latest - earliest).mul_f64(fraction)))
    };
    let restarted = last.is_some_and(|last| {
        last.reboot_pending && booted_at.is_some_and(|booted| booted > last.at_unix_ms)
    });
    if restarted {
        return Some(within(FIRST_DELAY, RESTART_SPREAD));
    }
    let interval = interval?;
    match last {
        Some(last) if now.saturating_sub(last.at_unix_ms) <= millis(interval) => {
            let due = last
                .at_unix_ms
                .saturating_add(millis(interval + interval.mul_f64(JITTER * fraction)));
            Some(due.max(now.saturating_add(millis(FIRST_DELAY))))
        }
        _ => Some(within(FIRST_DELAY, OVERDUE_SPREAD.min(interval))),
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// The coordinator's schedule. Created once per Agent process, so a scan
/// that finishes while offline still counts as the last one.
#[derive(Debug)]
pub(crate) struct Schedule {
    interval: Option<Duration>,
    last: Option<Scanned>,
    next: Option<i64>,
    /// Drawn once, so a schedule sent again without a change keeps its time,
    /// and each device keeps its own place in the interval.
    fraction: f64,
}

impl Schedule {
    pub(crate) fn new(fraction: f64) -> Self {
        Self {
            interval: None,
            last: None,
            next: None,
            fraction,
        }
    }

    /// A connection started: nothing is scheduled until its schedule arrives.
    pub(crate) fn connected(&mut self) {
        self.interval = None;
        self.next = None;
    }

    /// The server sent the schedule at `now`; the computer started at
    /// `booted_at`.
    pub(crate) fn set(&mut self, schedule: UpdateScanSchedule, now: i64, booted_at: Option<i64>) {
        self.interval = schedule
            .interval_hours
            .filter(|hours| *hours > 0)
            .map(|hours| Duration::from_secs(u64::from(hours) * 60 * 60));
        if let Some(last) = schedule.last_scan {
            let received = Scanned {
                at_unix_ms: now.saturating_sub(last.received_ms_ago.max(0)),
                reboot_pending: last.reboot_pending,
            };
            if self
                .last
                .is_none_or(|known| known.at_unix_ms < received.at_unix_ms)
            {
                self.last = Some(received);
            }
        }
        self.next = next_scan(self.interval, self.last, booted_at, now, self.fraction);
        tracing::info!(
            interval_hours = schedule.interval_hours,
            last_scan_unix_ms = self.last.map(|last| last.at_unix_ms),
            reboot_pending = self.last.is_some_and(|last| last.reboot_pending),
            next_scan_in_seconds = self.next.map(|next| next.saturating_sub(now) / 1000),
            "set the update scan schedule"
        );
    }

    /// A scan finished, at `now`; the next is an interval after it. A failed
    /// scan keeps what the last said about restarting, as the server does.
    pub(crate) fn scanned(&mut self, scan: &UpdateScan, now: i64) {
        let reboot_pending = if scan.error.is_some() {
            self.last.is_some_and(|last| last.reboot_pending)
        } else {
            scan.reboot_pending
        };
        if self
            .last
            .is_none_or(|last| last.at_unix_ms <= scan.scanned_at_unix_ms)
        {
            self.last = Some(Scanned {
                at_unix_ms: scan.scanned_at_unix_ms,
                reboot_pending,
            });
        }
        // It started before the scan, so a restart it said was pending
        // hasn't happened.
        self.next = next_scan(self.interval, self.last, None, now, self.fraction);
    }

    /// The scheduled scan started at `now`. Until it reports, the next is an
    /// interval away, as if it had finished now, so it isn't started again
    /// and one that never finishes doesn't stop the schedule.
    pub(crate) fn started(&mut self, now: i64) {
        let started = Scanned {
            at_unix_ms: now,
            reboot_pending: false,
        };
        self.next = next_scan(self.interval, Some(started), None, now, self.fraction);
    }

    /// How long until the next scheduled scan at `now`, if there is one.
    pub(crate) fn due_in(&self, now: i64) -> Option<Duration> {
        self.next
            .map(|next| Duration::from_millis(u64::try_from(next.saturating_sub(now)).unwrap_or(0)))
    }

    /// Completes when the next scheduled scan is due; never without one.
    /// Cancel safe.
    pub(crate) async fn due(&self) {
        match self.due_in(super::now_unix_ms()) {
            Some(wait) => tokio::time::sleep(wait).await,
            None => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::LastUpdateScan;

    use super::*;

    const HOUR: i64 = 60 * 60 * 1000;
    const MINUTE: i64 = 60 * 1000;
    const NOW: i64 = 1_800_000_000_000;
    const DAY: Option<Duration> = Some(Duration::from_secs(24 * 60 * 60));

    fn scanned(ago: i64, reboot_pending: bool) -> Option<Scanned> {
        Some(Scanned {
            at_unix_ms: NOW - ago,
            reboot_pending,
        })
    }

    #[test]
    fn a_device_that_never_scanned_scans_within_the_hour_but_not_at_once() {
        for fraction in [0.0, 0.5, 0.999_999] {
            let next = next_scan(DAY, None, Some(NOW - HOUR), NOW, fraction).unwrap();
            assert!((NOW + MINUTE..=NOW + HOUR).contains(&next), "{fraction}");
        }
        assert_eq!(next_scan(DAY, None, None, NOW, 0.0), Some(NOW + MINUTE));
    }

    #[test]
    fn a_device_on_schedule_scans_an_interval_after_its_last_scan() {
        // Two hours ago, at the start of its tenth of the interval.
        assert_eq!(
            next_scan(DAY, scanned(2 * HOUR, false), None, NOW, 0.0),
            Some(NOW + 22 * HOUR)
        );
        // Half way through it.
        assert_eq!(
            next_scan(DAY, scanned(2 * HOUR, false), None, NOW, 0.5),
            Some(NOW + 22 * HOUR + 24 * HOUR / 20)
        );
        // Due in a few seconds: still not before the connection settles.
        assert_eq!(
            next_scan(DAY, scanned(24 * HOUR - 5_000, false), None, NOW, 0.0),
            Some(NOW + MINUTE)
        );
    }

    #[test]
    fn the_jitter_is_up_to_a_tenth_of_the_interval() {
        for hours in [1, 24, 168] {
            let interval = Duration::from_secs(hours * 60 * 60);
            let interval_ms = millis(interval);
            let mut latest = 0;
            for step in 0..1_000 {
                let fraction = f64::from(step) / 1_000.0;
                let next =
                    next_scan(Some(interval), scanned(0, false), None, NOW, fraction).unwrap();
                let late = next - (NOW + interval_ms);
                assert!(late >= 0 && late < interval_ms / 10, "{hours} {fraction}");
                assert!(late >= latest, "later with a larger fraction");
                latest = late;
            }
            assert!(latest > interval_ms / 10 * 99 / 100, "{hours}");
        }
    }

    #[test]
    fn an_overdue_device_scans_within_the_spread_never_at_once() {
        for ago in [24 * HOUR + 1, 30 * HOUR, 400 * 24 * HOUR] {
            for fraction in [0.0, 0.25, 0.999_999] {
                let next = next_scan(DAY, scanned(ago, false), None, NOW, fraction).unwrap();
                assert!(
                    (NOW + MINUTE..=NOW + HOUR).contains(&next),
                    "{ago} {fraction}"
                );
            }
        }
        // An interval shorter than the spread spreads over the interval.
        let half_hour = Some(Duration::from_secs(30 * 60));
        let next = next_scan(half_hour, scanned(HOUR, false), None, NOW, 0.999_999).unwrap();
        assert!(next <= NOW + 30 * MINUTE && next > NOW + 29 * MINUTE);
    }

    #[test]
    fn ten_thousand_overdue_devices_spread_over_the_hour() {
        // A reproducible random source: splitmix64.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut fraction = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            ((z ^ (z >> 31)) >> 11) as f64 / (1_u64 << 53) as f64
        };
        let mut minutes = [0_u32; 60];
        for _ in 0..10_000 {
            let next = next_scan(DAY, None, None, NOW, fraction()).unwrap();
            assert!((NOW + MINUTE..=NOW + HOUR).contains(&next));
            minutes[usize::try_from((next - NOW) / MINUTE).unwrap().min(59)] += 1;
        }
        // None in the first minute; about 169 in each of the other 59.
        assert_eq!(minutes[0], 0);
        for (minute, count) in minutes.iter().enumerate().skip(1) {
            assert!((110..=230).contains(count), "minute {minute}: {count}");
        }
    }

    #[test]
    fn a_pending_restart_scans_soon_only_after_the_computer_restarted() {
        let last = scanned(3 * HOUR, true);
        // Started an hour ago, after the scan.
        for fraction in [0.0, 0.999_999] {
            let next = next_scan(DAY, last, Some(NOW - HOUR), NOW, fraction).unwrap();
            assert!((NOW + MINUTE..=NOW + 5 * MINUTE).contains(&next));
        }
        // Started before the scan: a reconnect, which follows the schedule.
        assert_eq!(
            next_scan(DAY, last, Some(NOW - 4 * HOUR), NOW, 0.0),
            Some(NOW + 21 * HOUR)
        );
        // Without knowing when, it follows the schedule too.
        assert_eq!(next_scan(DAY, last, None, NOW, 0.0), Some(NOW + 21 * HOUR));
        // With the interval off, a restart still gets its one scan.
        assert!(next_scan(None, last, Some(NOW - HOUR), NOW, 0.0).is_some());
        assert_eq!(next_scan(None, last, Some(NOW - 4 * HOUR), NOW, 0.0), None);
    }

    #[test]
    fn without_an_interval_nothing_is_scheduled() {
        assert_eq!(next_scan(None, None, Some(NOW - HOUR), NOW, 0.5), None);
        assert_eq!(
            next_scan(None, scanned(400 * HOUR, false), None, NOW, 0.5),
            None
        );
    }

    fn schedule(
        interval_hours: Option<u32>,
        ago: Option<i64>,
        reboot_pending: bool,
    ) -> UpdateScanSchedule {
        UpdateScanSchedule {
            interval_hours,
            last_scan: ago.map(|ago| LastUpdateScan {
                received_ms_ago: ago,
                reboot_pending,
            }),
        }
    }

    fn scan_at(at: i64, reboot_pending: bool) -> UpdateScan {
        UpdateScan {
            scanned_at_unix_ms: at,
            reboot_pending,
            ..Default::default()
        }
    }

    #[test]
    fn a_changed_interval_replaces_the_schedule_without_scanning_at_once() {
        let mut waiting = Schedule::new(0.0);
        waiting.set(schedule(Some(24), Some(2 * HOUR), false), NOW, None);
        assert_eq!(waiting.next, Some(NOW + 22 * HOUR));
        // The same schedule again keeps its time.
        waiting.set(schedule(Some(24), Some(2 * HOUR), false), NOW, None);
        assert_eq!(waiting.next, Some(NOW + 22 * HOUR));
        // A change of interval says no last scan: the Agent keeps its own.
        waiting.set(schedule(Some(48), None, false), NOW + HOUR, None);
        assert_eq!(waiting.next, Some(NOW + 46 * HOUR));
        // Shorter than the time since the last scan: overdue, so spread.
        waiting.set(schedule(Some(1), None, false), NOW + HOUR, None);
        let next = waiting.next.unwrap();
        assert_eq!(next, NOW + HOUR + MINUTE);
        // Off: nothing.
        waiting.set(schedule(None, None, false), NOW + HOUR, None);
        assert_eq!(waiting.next, None);
        assert_eq!(waiting.due_in(NOW), None);
    }

    #[test]
    fn every_scan_moves_the_schedule_and_a_started_one_is_not_started_again() {
        let mut waiting = Schedule::new(0.0);
        // Nothing until the connection's schedule arrives.
        assert_eq!(waiting.due_in(NOW), None);
        waiting.set(schedule(Some(24), None, false), NOW, None);
        assert_eq!(waiting.due_in(NOW), Some(Duration::from_secs(60)));
        // Due: started, and not due again for an interval.
        waiting.started(NOW + MINUTE);
        assert_eq!(waiting.next, Some(NOW + MINUTE + 24 * HOUR));
        // It finished; so did one a person asked for later.
        waiting.scanned(&scan_at(NOW + 2 * MINUTE, false), NOW + 2 * MINUTE);
        assert_eq!(waiting.next, Some(NOW + 2 * MINUTE + 24 * HOUR));
        waiting.scanned(&scan_at(NOW + 5 * HOUR, true), NOW + 5 * HOUR);
        assert_eq!(waiting.next, Some(NOW + 29 * HOUR));
        // A failed scan counts, keeping the restart pending.
        let failed = UpdateScan {
            error: Some("offline".into()),
            ..scan_at(NOW + 6 * HOUR, false)
        };
        waiting.scanned(&failed, NOW + 6 * HOUR);
        assert_eq!(waiting.last, scanned(-6 * HOUR, true));
        // A reconnect forgets the schedule, keeping the last scan, which is
        // newer than the server's while its report is still on the way.
        waiting.connected();
        assert_eq!(waiting.due_in(NOW + 7 * HOUR), None);
        waiting.set(
            schedule(Some(24), Some(2 * HOUR), false),
            NOW + 7 * HOUR,
            Some(NOW),
        );
        assert_eq!(waiting.last, scanned(-6 * HOUR, true));
        assert_eq!(waiting.next, Some(NOW + 30 * HOUR));
        // The server's newer scan wins over the Agent's older one.
        waiting.set(schedule(Some(24), Some(0), false), NOW + 8 * HOUR, None);
        assert_eq!(waiting.last, scanned(-8 * HOUR, false));
    }

    #[test]
    fn a_restarted_computer_scans_once_soon_after_connecting() {
        let mut waiting = Schedule::new(0.0);
        // The scan three hours ago said to restart; it restarted an hour ago.
        waiting.set(
            schedule(Some(24), Some(3 * HOUR), true),
            NOW,
            Some(NOW - HOUR),
        );
        assert_eq!(waiting.next, Some(NOW + MINUTE));
        waiting.started(NOW + MINUTE);
        waiting.scanned(&scan_at(NOW + 2 * MINUTE, false), NOW + 2 * MINUTE);
        assert_eq!(waiting.next, Some(NOW + 2 * MINUTE + 24 * HOUR));
        // A reconnect later, with the server's word of that scan, doesn't
        // scan again.
        waiting.connected();
        waiting.set(
            schedule(Some(24), Some(HOUR), false),
            NOW + 2 * MINUTE + HOUR,
            Some(NOW - HOUR),
        );
        assert_eq!(waiting.next, Some(NOW + 2 * MINUTE + 24 * HOUR));
    }
}
