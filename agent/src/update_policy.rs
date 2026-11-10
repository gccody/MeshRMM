//! When the Agent checks for updates and how often it retries a release,
//! shared by the Windows service and the Linux and macOS coordinators.
//!
//! Every Agent of a server gets the same release, so a fleet's checks must not
//! line up, and a device that is never up for long must still check. After each
//! successful check the Agent records when the next one is due, so a restart
//! keeps its place in the schedule instead of starting the interval again.
use std::path::Path;
use std::time::{Duration, Instant};

use meshrmm_self_update::CURRENT_VERSION;
use serde::{Deserialize, Serialize};

pub(crate) const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Each interval is lengthened or shortened by up to this fraction at random, so devices drift
/// apart rather than staying in step with each other or with the clock.
const INTERVAL_JITTER: f64 = 0.1;
/// The check after a build's first one falls this far into the interval or more, up to the
/// longest wait. Devices installed or updated together would otherwise stay within minutes of
/// each other for days, since a tenth of the interval spreads them only slowly.
const FIRST_INTERVAL_FROM: f64 = 0.1;
/// The longest wait the schedule sets, so a recorded due time further away than this means the
/// clock was changed.
const LONGEST_SCHEDULED_WAIT: Duration =
    Duration::from_secs(UPDATE_CHECK_INTERVAL.as_secs() * 11 / 10);
/// A build's first start checks at a random point in this window, since its installer may be
/// older than the server's release. No start checks sooner, in the first seconds of a boot.
const START_CHECK_AFTER: Duration = Duration::from_secs(30);
const START_CHECK_WINDOW: Duration = Duration::from_secs(60);
/// A start that finds its check overdue, because the device was off or asleep when it fell due,
/// checks at a random point in this window. When a site's power returns after a long outage
/// every device there is overdue, and they must not all check at once.
const OVERDUE_CHECK_AFTER: Duration = Duration::from_secs(60);
const OVERDUE_CHECK_WINDOW: Duration = Duration::from_secs(29 * 60);
/// A failed check is retried after this, doubling up to the maximum, until one succeeds.
const FAILED_CHECK_RETRY: Duration = Duration::from_secs(2 * 60);
const MAX_FAILED_CHECK_RETRY: Duration = Duration::from_secs(60 * 60);
/// An update check that falls due while the Agent is busy, with a remote session or a job that
/// must not be cut off, is retried this often until nothing keeps it busy, so the update does
/// not interrupt them.
pub(crate) const DEFERRED_UPDATE_RETRY: Duration = Duration::from_secs(5 * 60);
/// A session that never ends, or a job that never returns, does not keep the Agent from
/// updating for longer than this. Update installs and application runs stop at 4 hours, scans
/// sooner, and scripts at an hour, so only one stuck past its own limit, or work started back to
/// back for a day, reaches it.
const MAX_UPDATE_DEFERRAL: Duration = Duration::from_secs(24 * 60 * 60);
/// A failed update restarts the previous Agent, which checks for updates again, so a release
/// that cannot be installed is only retried this many times per window.
pub(crate) const MAX_ATTEMPTS_PER_VERSION: u32 = 3;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// In the private update directory: the [`NextCheck`] written after the last successful check.
pub(crate) const NEXT_CHECK_FILE: &str = "next-check.json";

/// When the next update check is due, written after each successful one.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NextCheck {
    /// The build that wrote it: a record of another build means this one has not checked yet.
    pub version: String,
    pub due_unix: u64,
}

pub(crate) fn read_next_check(path: &Path) -> Option<NextCheck> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Schedules automatic update checks, and postpones them, and the install of a release one
/// finds, while the Agent is busy.
pub(crate) struct UpdateSchedule {
    next_check: Instant,
    /// When the update was first postponed, for the maximum deferral; until a check finds
    /// nothing to install, or fails.
    deferred_since: Option<Instant>,
    failures: u32,
    /// Whether this build has completed a check.
    build_checked: bool,
}

impl UpdateSchedule {
    /// The schedule of an Agent that starts at `now` (`now_unix` on the wall clock) and finds
    /// `record`. `random` is fresh bits from [`random_bits`].
    pub(crate) fn new(
        now: Instant,
        now_unix: u64,
        record: Option<&NextCheck>,
        random: u64,
    ) -> Self {
        let delay = start_delay(record, now_unix, random);
        tracing::info!(
            seconds = delay.as_secs(),
            "scheduled the first automatic Agent update check"
        );
        Self {
            next_check: now + delay,
            deferred_since: None,
            failures: 0,
            build_checked: record.is_some_and(|record| record.version == CURRENT_VERSION),
        }
    }

    /// Whether to check for an update now; the caller reports how it went to [`Self::checked`],
    /// or asks [`Self::install_now`] when to install the release it staged. Installing stops the
    /// Agent, so a check waits while the Agent is busy, but not beyond the maximum deferral.
    pub(crate) fn due(&mut self, now: Instant, busy: bool) -> bool {
        if now < self.next_check {
            return false;
        }
        if busy && self.postpone(now) {
            self.next_check = now + DEFERRED_UPDATE_RETRY;
            return false;
        }
        if !busy && self.deferred_since.is_some() {
            tracing::info!("nothing keeps the Agent busy now; running the postponed update check");
        }
        true
    }

    /// Whether to install, and so stop the Agent for, the release a check staged: once the
    /// Agent is not busy, or the update has waited the maximum deferral, counted from when its
    /// check was first postponed. Asked on every tick, so the update starts in the first gap
    /// between jobs rather than waiting for a retry.
    pub(crate) fn install_now(&mut self, now: Instant, busy: bool) -> bool {
        !busy || !self.postpone(now)
    }

    /// Whether to postpone the update at `now` for the Agent being busy: until it has waited
    /// the maximum deferral.
    fn postpone(&mut self, now: Instant) -> bool {
        let since = *self.deferred_since.get_or_insert(now);
        let deferred = now.duration_since(since);
        if deferred < MAX_UPDATE_DEFERRAL {
            if deferred.is_zero() {
                tracing::info!(
                    "postponing the automatic Agent update until remote sessions and running jobs end"
                );
            }
            return true;
        }
        tracing::warn!(
            deferred_hours = deferred.as_secs() / 3600,
            "updating the Agent although a remote session or job is still running"
        );
        false
    }

    /// Schedules the next check after one that finished at `now` (`now_unix` on the wall clock).
    /// After a success, returns the record for the caller to write to the [`NEXT_CHECK_FILE`];
    /// a failure's retry is not recorded, so a restart during it finds the check overdue.
    /// `random` is fresh bits from [`random_bits`].
    pub(crate) fn checked(
        &mut self,
        now: Instant,
        now_unix: u64,
        succeeded: bool,
        random: u64,
    ) -> Option<NextCheck> {
        self.deferred_since = None;
        if !succeeded {
            self.failures = self.failures.saturating_add(1);
            self.next_check = now + retry_delay(self.failures, random);
            return None;
        }
        let delay = interval_delay(!self.build_checked, random);
        self.failures = 0;
        self.build_checked = true;
        self.next_check = now + delay;
        Some(NextCheck {
            version: CURRENT_VERSION.to_owned(),
            due_unix: now_unix.saturating_add(delay.as_secs()),
        })
    }
}

/// How long after a start the first check waits, given the record of the last successful check.
fn start_delay(record: Option<&NextCheck>, now_unix: u64, random: u64) -> Duration {
    let start_window = START_CHECK_AFTER + START_CHECK_WINDOW.mul_f64(fraction(random));
    let Some(record) = record.filter(|record| record.version == CURRENT_VERSION) else {
        return start_window;
    };
    match record
        .due_unix
        .checked_sub(now_unix)
        .map(Duration::from_secs)
    {
        Some(wait) if !wait.is_zero() && wait <= LONGEST_SCHEDULED_WAIT => wait.max(start_window),
        // Overdue, or so far ahead that the clock must have been changed. Random on each start,
        // not fixed per device, so no device keeps missing its check however short its uptime.
        _ => OVERDUE_CHECK_AFTER + OVERDUE_CHECK_WINDOW.mul_f64(fraction(random)),
    }
}

/// The wait after a successful check: about an interval, or, after a build's first check, any
/// point from a tenth of the interval to the longest wait.
fn interval_delay(first_of_build: bool, random: u64) -> Duration {
    let fraction = fraction(random);
    let share = if first_of_build {
        FIRST_INTERVAL_FROM + fraction
    } else {
        1.0 - INTERVAL_JITTER + 2.0 * INTERVAL_JITTER * fraction
    };
    UPDATE_CHECK_INTERVAL.mul_f64(share)
}

/// The wait after `failures` checks in a row have failed: a delay that doubles up to an hour,
/// shortened by up to half so Agents that lost the server together come back apart.
fn retry_delay(failures: u32, random: u64) -> Duration {
    FAILED_CHECK_RETRY
        .saturating_mul(2_u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_FAILED_CHECK_RETRY)
        .mul_f64(0.5 + fraction(random) / 2.0)
}

/// `random` as a fraction in `[0, 1)`: its top 53 bits, which an `f64` holds exactly.
fn fraction(random: u64) -> f64 {
    (random >> 11) as f64 / (1_u64 << 53) as f64
}

/// Random bits for the schedule's jitter.
pub(crate) fn random_bits() -> u64 {
    getrandom::u64().unwrap_or_else(|error| {
        tracing::warn!(%error, "no random source for the Agent update schedule");
        // The clock still differs between devices; the multiplier spreads it over all the bits.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos() as u64);
        nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    })
}

pub(crate) fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Update attempts for the release most recently offered, kept in the private update directory.
/// An attempt counts once its build has downloaded in full: a download the server or network cut
/// short is retried on the failed-check backoff, while a build that is rejected, or installs and
/// rolls back, is not fetched again more than a few times a day.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UpdateAttempts {
    pub version: String,
    pub attempts: u32,
    pub first_attempt_unix: u64,
}

impl UpdateAttempts {
    /// The record to store once `version` has downloaded, or `None` once it has used its
    /// attempts in the current window.
    pub(crate) fn next(previous: Option<&Self>, version: &str, now: u64) -> Option<Self> {
        match previous {
            Some(previous)
                if previous.version == version
                    && now
                        .checked_sub(previous.first_attempt_unix)
                        .is_some_and(|elapsed| elapsed < ATTEMPT_WINDOW.as_secs()) =>
            {
                (previous.attempts < MAX_ATTEMPTS_PER_VERSION).then(|| Self {
                    version: version.to_owned(),
                    attempts: previous.attempts + 1,
                    first_attempt_unix: previous.first_attempt_unix,
                })
            }
            _ => Some(Self {
                version: version.to_owned(),
                attempts: 1,
                first_attempt_unix: now,
            }),
        }
    }
}

pub(crate) fn read_attempts(path: &Path) -> Option<UpdateAttempts> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// How long an installed update has to prove it works: its coordinator must connect to the
/// server, be accepted, and handle the server's first message within this time, or the previous
/// build is put back. The control connection retries at least every two minutes, so this rides
/// out a server restart or a short network outage. A server or network that stays down longer
/// rolls the update back to the build that was working, which is harmless while nothing can
/// reach the Agent anyway, and the release is tried again on the attempt schedule above: an
/// outage costs a device at most [`MAX_ATTEMPTS_PER_VERSION`] tries a day, and delays a good
/// release by at most a day once the server is back, without ever refusing it. An Agent that
/// cannot reach its server installs the update all the same, since the release may be what it
/// needs to connect to a newer server, and is rolled back if the new build cannot either.
pub(crate) const CONFIRM_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How often the installer looks for the new coordinator's [`CONNECTED_FILE`].
const CONFIRM_POLL: Duration = Duration::from_secs(2);
/// In the private update directory: the [`PendingUpdate`] an installer is confirming.
pub(crate) const PENDING_FILE: &str = "update-pending.json";
/// In the private update directory: the [`Connected`] record of the last coordinator the server
/// accepted. Only the Agent's own privileged processes can write or remove files there, so a
/// local user can neither forge a confirmation nor stop one from being written.
pub(crate) const CONNECTED_FILE: &str = "connected.json";

/// An installed update waiting for its coordinator to connect, written by the installer before
/// the new build starts and removed when it commits or rolls back.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingUpdate {
    pub version: String,
    pub until_unix: u64,
}

impl PendingUpdate {
    /// Whether the installer may still be waiting at `now_unix`, so the new build must not start
    /// an update of its own, which would replace the executable the installer can put back. One
    /// left by an installer that never finished, as when the computer restarted meanwhile,
    /// expires at its deadline, and one due further ahead than the timeout means the clock was
    /// changed.
    pub(crate) fn holds(&self, now_unix: u64) -> bool {
        self.until_unix
            .checked_sub(now_unix)
            .is_some_and(|left| left > 0 && left <= CONFIRM_TIMEOUT.as_secs())
    }
}

/// Whether an installer is confirming the update that started this build; see
/// [`PendingUpdate::holds`].
pub(crate) fn confirming(updates: &Path, now_unix: u64) -> bool {
    serde_json::from_slice::<PendingUpdate>(
        &std::fs::read(updates.join(PENDING_FILE)).unwrap_or_default(),
    )
    .is_ok_and(|pending| pending.holds(now_unix))
}

/// Written by a coordinator the first time the server accepts it and it handles the server's
/// first message.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Connected {
    pub version: String,
}

/// Records that this build connected, for an installer confirming it.
pub(crate) fn record_connected(updates: &Path) -> anyhow::Result<()> {
    let record = Connected {
        version: CURRENT_VERSION.to_owned(),
    };
    crate::installer::replace_file(&updates.join(CONNECTED_FILE), &serde_json::to_vec(&record)?)
}

/// What an installer waiting for the new build has seen.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Confirmed {
    /// The new build's coordinator connected: the update is committed.
    Connected,
    Waiting,
    /// It did not connect within [`CONFIRM_TIMEOUT`]: the update is rolled back.
    TimedOut,
}

/// An installer's wait for the build it installs to connect. Dropping it removes the
/// [`PENDING_FILE`], once the update is committed or rolled back.
pub(crate) struct Confirmation {
    updates: std::path::PathBuf,
    version: String,
    deadline: Instant,
}

impl Confirmation {
    /// Starts confirming `version` at `now` (`now_unix` on the wall clock), before it is
    /// installed: only a connection recorded from here on counts.
    pub(crate) fn begin(
        updates: &Path,
        version: &str,
        now: Instant,
        now_unix: u64,
    ) -> anyhow::Result<Self> {
        match std::fs::remove_file(updates.join(CONNECTED_FILE)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(anyhow::Error::new(error)
                    .context("could not remove the last coordinator's connection record"));
            }
            _ => {}
        }
        let pending = PendingUpdate {
            version: version.to_owned(),
            until_unix: now_unix.saturating_add(CONFIRM_TIMEOUT.as_secs()),
        };
        crate::installer::replace_file(
            &updates.join(PENDING_FILE),
            &serde_json::to_vec(&pending)?,
        )?;
        Ok(Self {
            updates: updates.to_owned(),
            version: version.to_owned(),
            deadline: now + CONFIRM_TIMEOUT,
        })
    }

    /// Whether the new build has connected by `now`.
    pub(crate) fn poll(&self, now: Instant) -> Confirmed {
        let connected = std::fs::read(self.updates.join(CONNECTED_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Connected>(&bytes).ok())
            .is_some_and(|record| record.version == self.version);
        if connected {
            Confirmed::Connected
        } else if now >= self.deadline {
            Confirmed::TimedOut
        } else {
            Confirmed::Waiting
        }
    }

    /// Waits until the new build connects, returning true, or the time runs out. `now` reads the
    /// clock and `pause` waits.
    pub(crate) fn wait(
        &self,
        mut now: impl FnMut() -> Instant,
        mut pause: impl FnMut(Duration),
    ) -> bool {
        loop {
            match self.poll(now()) {
                Confirmed::Connected => return true,
                Confirmed::TimedOut => return false,
                Confirmed::Waiting => pause(CONFIRM_POLL),
            }
        }
    }
}

impl Drop for Confirmation {
    fn drop(&mut self) {
        match std::fs::remove_file(self.updates.join(PENDING_FILE)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                // It expires at its deadline.
                tracing::warn!(%error, "could not remove the pending Agent update record");
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;
    const HOUR: u64 = 60 * 60;
    /// Some wall-clock time.
    const NOW: u64 = 1_800_000_000;

    fn record(due_unix: u64) -> NextCheck {
        NextCheck {
            version: CURRENT_VERSION.to_owned(),
            due_unix,
        }
    }

    fn due_at(next_check: Instant) -> UpdateSchedule {
        UpdateSchedule {
            next_check,
            deferred_since: None,
            failures: 0,
            build_checked: true,
        }
    }

    fn in_start_window(delay: Duration) -> bool {
        (Duration::from_secs(30)..=Duration::from_secs(90)).contains(&delay)
    }

    fn in_overdue_window(delay: Duration) -> bool {
        (Duration::from_secs(60)..=Duration::from_secs(30 * 60)).contains(&delay)
    }

    #[test]
    fn checks_soon_after_a_builds_first_start() {
        assert_eq!(start_delay(None, NOW, 0), Duration::from_secs(30));
        let latest = start_delay(None, NOW, u64::MAX);
        assert!(in_start_window(latest) && latest > Duration::from_secs(89));
        // A record another build wrote, as before an update, does not count.
        let older = NextCheck {
            version: "an older build".to_owned(),
            due_unix: NOW + HOUR,
        };
        for random in [0, u64::MAX / 3, u64::MAX] {
            assert!(in_start_window(start_delay(Some(&older), NOW, random)));
        }
    }

    #[test]
    fn keeps_a_recorded_due_time_across_a_restart() {
        let start = Instant::now();
        let mut updates = UpdateSchedule::new(start, NOW, Some(&record(NOW + HOUR)), 0);
        let saved = updates.checked(start, NOW, true, u64::MAX / 2 + 1).unwrap();
        assert_eq!(saved, record(NOW + 6 * HOUR));

        // Restarted three hours later, it still checks at the recorded time.
        let restart = start + Duration::from_secs(3 * HOUR);
        let mut updates = UpdateSchedule::new(restart, NOW + 3 * HOUR, Some(&saved), 0);
        assert!(!updates.due(restart + Duration::from_secs(3 * HOUR - 1), false));
        assert!(updates.due(restart + Duration::from_secs(3 * HOUR), false));
        // A due time close ahead still leaves the first seconds of a boot alone.
        assert_eq!(
            start_delay(Some(&record(NOW + 5)), NOW, 0),
            Duration::from_secs(30)
        );
        let longest = NOW + LONGEST_SCHEDULED_WAIT.as_secs();
        assert_eq!(
            start_delay(Some(&record(longest)), NOW, 0),
            LONGEST_SCHEDULED_WAIT
        );
    }

    #[test]
    fn checks_within_half_an_hour_when_overdue_or_the_clock_moved() {
        let longest = NOW + LONGEST_SCHEDULED_WAIT.as_secs();
        for due in [NOW - 30 * DAY, NOW - 1, NOW, longest + 1, NOW + 365 * DAY] {
            assert_eq!(
                start_delay(Some(&record(due)), NOW, 0),
                Duration::from_secs(60),
                "{due}"
            );
            for random in [u64::MAX / 7, u64::MAX / 2, u64::MAX] {
                assert!(in_overdue_window(start_delay(
                    Some(&record(due)),
                    NOW,
                    random
                )));
            }
        }
        // Not tied to the device: the next start picks another point.
        assert_ne!(
            start_delay(Some(&record(NOW - 1)), NOW, 0),
            start_delay(Some(&record(NOW - 1)), NOW, u64::MAX)
        );
    }

    #[test]
    fn spreads_the_check_after_a_builds_first_one_over_the_interval() {
        let start = Instant::now();
        let first = |random| {
            let mut updates = UpdateSchedule::new(start, NOW, None, 0);
            let saved = updates.checked(start, NOW, true, random).unwrap();
            Duration::from_secs(saved.due_unix - NOW)
        };
        let seconds = |share| Duration::from_secs(UPDATE_CHECK_INTERVAL.mul_f64(share).as_secs());
        assert_eq!(first(0), seconds(0.1));
        assert!(first(u64::MAX) <= LONGEST_SCHEDULED_WAIT);
        assert!(first(u64::MAX) >= seconds(1.0999));
        assert_eq!(first(u64::MAX / 2 + 1), seconds(0.6));

        // Later checks keep to the interval, give or take a tenth.
        let mut updates = UpdateSchedule::new(start, NOW, None, 0);
        updates.checked(start, NOW, true, 0);
        for random in [0, u64::MAX / 2 + 1, u64::MAX] {
            let saved = updates.checked(start, NOW, true, random).unwrap();
            let wait = Duration::from_secs(saved.due_unix - NOW);
            assert!(wait >= UPDATE_CHECK_INTERVAL.mul_f64(0.9) - Duration::from_secs(1));
            assert!(wait <= UPDATE_CHECK_INTERVAL.mul_f64(1.1));
        }
        assert_eq!(interval_delay(false, 0), UPDATE_CHECK_INTERVAL.mul_f64(0.9));
        assert_eq!(
            interval_delay(false, u64::MAX / 2 + 1),
            UPDATE_CHECK_INTERVAL
        );
        assert!(interval_delay(false, u64::MAX) <= UPDATE_CHECK_INTERVAL.mul_f64(1.1));
    }

    #[test]
    fn checks_on_a_device_that_is_only_ever_up_for_three_hours() {
        // Up three hours a day from a boot each morning, with the record carried over.
        let mut saved = Some(record(NOW + 5 * HOUR));
        let mut checks = 0;
        for day in 0..4 {
            let boot_unix = NOW + day * DAY;
            let boot = Instant::now();
            let mut updates = UpdateSchedule::new(boot, boot_unix, saved.as_ref(), u64::MAX);
            for tick in (0..3 * HOUR).step_by(30) {
                let now = boot + Duration::from_secs(tick);
                if updates.due(now, false) {
                    checks += 1;
                    saved = updates.checked(now, boot_unix + tick, true, u64::MAX);
                }
            }
        }
        // The first morning's check is due after shutdown; every later boot finds one overdue.
        assert_eq!(checks, 3);
    }

    #[test]
    fn backs_off_after_failed_checks_up_to_an_hour() {
        let minutes = |minutes: u64| Duration::from_secs(minutes * 60);
        let longest = |failures| retry_delay(failures, u64::MAX);
        assert!(longest(1) <= minutes(2) && longest(1) > minutes(2).mul_f64(0.99));
        assert!(longest(2) <= minutes(4) && longest(2) > longest(1));
        assert!(longest(6) <= minutes(60) && longest(6) > minutes(59));
        assert!(longest(u32::MAX) <= minutes(60));
        assert_eq!(retry_delay(1, 0), minutes(1));
        assert_eq!(retry_delay(u32::MAX, 0), minutes(30));
    }

    #[test]
    fn returns_to_the_interval_once_a_check_succeeds() {
        let start = Instant::now();
        let mut updates = due_at(start);
        let mut now = start;
        let mut previous = Duration::ZERO;
        for _ in 0..10 {
            assert!(updates.due(now, false));
            // A failure is not recorded, so a restart during the retry finds the check overdue.
            assert_eq!(updates.checked(now, NOW, false, u64::MAX), None);
            let wait = updates.next_check - now;
            assert!(wait >= previous && wait <= MAX_FAILED_CHECK_RETRY);
            previous = wait;
            now = updates.next_check;
        }
        assert!(previous > MAX_FAILED_CHECK_RETRY.mul_f64(0.99));
        assert!(updates.due(now, false));
        assert!(updates.checked(now, NOW, true, 0).is_some());
        assert!(!updates.due(now + UPDATE_CHECK_INTERVAL.mul_f64(0.89), false));
        assert!(updates.due(now + UPDATE_CHECK_INTERVAL.mul_f64(0.9), false));
        // The next failure starts the backoff again from its first step.
        updates.checked(now, NOW, false, u64::MAX);
        assert!(updates.due(now + FAILED_CHECK_RETRY, false));
    }

    #[test]
    fn postpones_update_checks_during_a_session_up_to_the_limit() {
        let start = Instant::now();
        let mut updates = due_at(start);
        assert!(!updates.due(start, true));
        assert!(!updates.due(start + DEFERRED_UPDATE_RETRY / 2, false));
        assert!(!updates.due(start + DEFERRED_UPDATE_RETRY, true));
        // The check runs soon after the session ends, not a full interval later.
        assert!(updates.due(start + DEFERRED_UPDATE_RETRY * 2, false));

        let mut updates = due_at(start);
        let mut now = start;
        while !updates.due(now, true) {
            now += DEFERRED_UPDATE_RETRY;
            assert!(now <= start + MAX_UPDATE_DEFERRAL);
        }
        assert_eq!(now, start + MAX_UPDATE_DEFERRAL);
        updates.checked(now, NOW, true, 0);
        // A later session gets the full deferral again.
        assert!(!updates.due(now + UPDATE_CHECK_INTERVAL, true));
    }

    #[test]
    fn waits_to_install_a_release_until_the_work_that_started_meanwhile_ends() {
        let start = Instant::now();
        let mut updates = due_at(start);
        // Nothing ran when the check fell due, but a job was accepted while it downloaded.
        assert!(updates.due(start, false));
        let staged = start + Duration::from_secs(60);
        for tick in 0..10 {
            assert!(!updates.install_now(staged + Duration::from_secs(30 * tick), true));
        }
        // It installs on the first tick the job is over, not a retry later.
        assert!(updates.install_now(staged + Duration::from_secs(301), false));
    }

    #[test]
    fn work_that_keeps_coming_holds_an_update_no_longer_than_the_limit() {
        let start = Instant::now();
        let mut updates = due_at(start);
        // Busy from when the check fell due until it ran, 23 hours later.
        let mut now = start;
        while !updates.due(now, now < start + Duration::from_secs(23 * HOUR)) {
            now += DEFERRED_UPDATE_RETRY;
        }
        assert_eq!(now, start + Duration::from_secs(23 * HOUR));
        // New work started right after it then holds the install only for the rest of the day.
        while !updates.install_now(now, true) {
            now += Duration::from_secs(30);
        }
        assert_eq!(now, start + MAX_UPDATE_DEFERRAL);
    }

    #[test]
    fn a_check_that_finds_nothing_ends_the_deferral() {
        let start = Instant::now();
        let mut updates = due_at(start);
        assert!(!updates.due(start, true));
        let ran = start + Duration::from_secs(20 * HOUR);
        assert!(updates.due(ran, false));
        updates.checked(ran, NOW, true, 0);
        // The next update the Agent finds waits a full day for its work again.
        let found = ran + UPDATE_CHECK_INTERVAL.mul_f64(1.1);
        assert!(updates.due(found, false));
        assert!(!updates.install_now(found, true));
        assert!(!updates.install_now(found + MAX_UPDATE_DEFERRAL / 2, true));
        assert!(updates.install_now(found + MAX_UPDATE_DEFERRAL, true));
    }

    #[test]
    fn checks_wait_for_jobs_and_their_results_as_for_sessions() {
        let link = crate::remote::service_link::ServiceLink::new(false);
        let start = Instant::now();
        let mut updates = due_at(start);
        let install = link.activity();
        assert!(!updates.due(start, link.busy()), "a running install");
        // The install ended; its result waits for the connection.
        install.finished();
        let retry = start + DEFERRED_UPDATE_RETRY;
        assert!(!updates.due(retry, link.busy()), "an unsent result");
        drop(install);
        assert!(updates.due(retry + DEFERRED_UPDATE_RETRY, link.busy()));
    }

    #[test]
    fn limits_attempts_for_the_same_release() {
        let first = UpdateAttempts::next(None, "1.2.0", 1_000).unwrap();
        assert_eq!(first.attempts, 1);
        let second = UpdateAttempts::next(Some(&first), "1.2.0", 1_060).unwrap();
        let third = UpdateAttempts::next(Some(&second), "1.2.0", 1_120).unwrap();
        assert_eq!((third.attempts, third.first_attempt_unix), (3, 1_000));
        assert_eq!(UpdateAttempts::next(Some(&third), "1.2.0", 1_180), None);
        assert_eq!(
            UpdateAttempts::next(Some(&third), "1.2.0", 1_000 + DAY - 1),
            None
        );
    }

    #[test]
    fn retries_after_the_window_or_for_another_release() {
        let exhausted = UpdateAttempts {
            version: "1.2.0".to_owned(),
            attempts: MAX_ATTEMPTS_PER_VERSION,
            first_attempt_unix: 1_000,
        };
        let later = UpdateAttempts::next(Some(&exhausted), "1.2.0", 1_000 + DAY).unwrap();
        assert_eq!((later.attempts, later.first_attempt_unix), (1, 1_000 + DAY));
        let newer = UpdateAttempts::next(Some(&exhausted), "1.2.1", 1_100).unwrap();
        assert_eq!((newer.attempts, newer.version.as_str()), (1, "1.2.1"));
        // A clock set back before the recorded attempt must not block updates until it catches up.
        let rewound = UpdateAttempts::next(Some(&exhausted), "1.2.0", 10).unwrap();
        assert_eq!(rewound.attempts, 1);
    }

    /// A private update directory for one test, removed when dropped.
    struct Updates(std::path::PathBuf);

    impl Updates {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "meshrmm-update-confirmation-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn connected(&self, version: &str) {
            let record = Connected {
                version: version.to_owned(),
            };
            std::fs::write(
                self.0.join(CONNECTED_FILE),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
        }
    }

    impl Drop for Updates {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Waits on a clock that only `pause` moves, calling `paused` with the time waited so far
    /// before each pause; returns the result and the time it waited.
    fn wait_on_fake_clock(
        confirmation: &Confirmation,
        start: Instant,
        mut paused: impl FnMut(Duration),
    ) -> (bool, Duration) {
        let now = std::cell::Cell::new(start);
        let connected = confirmation.wait(
            || now.get(),
            |pause| {
                paused(now.get() - start);
                now.set(now.get() + pause);
            },
        );
        (connected, now.get() - start)
    }

    #[test]
    fn waits_minutes_for_the_new_build_to_connect_then_rolls_back() {
        assert!(CONFIRM_TIMEOUT >= Duration::from_secs(10 * 60));
        let updates = Updates::new();
        let start = Instant::now();
        let confirmation = Confirmation::begin(&updates.0, "9.9.9", start, NOW).unwrap();
        // A build that runs, or even connects as another version, but never connects as the
        // one installed is rolled back once the time runs out, and not before.
        let (connected, waited) = wait_on_fake_clock(&confirmation, start, |waited| {
            if waited == Duration::from_secs(60) {
                updates.connected("1.0.0");
            }
        });
        assert!(!connected);
        assert_eq!(waited, CONFIRM_TIMEOUT);
        assert_eq!(confirmation.poll(start), Confirmed::Waiting);
        assert_eq!(
            confirmation.poll(start + CONFIRM_TIMEOUT),
            Confirmed::TimedOut
        );
    }

    #[test]
    fn commits_the_update_once_its_coordinator_connects() {
        let updates = Updates::new();
        let start = Instant::now();
        let confirmation = Confirmation::begin(&updates.0, CURRENT_VERSION, start, NOW).unwrap();
        let (connected, waited) = wait_on_fake_clock(&confirmation, start, |waited| {
            if waited == Duration::from_secs(5 * 60) {
                record_connected(&updates.0).unwrap();
            }
        });
        assert!(connected);
        assert_eq!(waited, Duration::from_secs(5 * 60) + CONFIRM_POLL);
        // A connection at the last moment still counts.
        assert_eq!(
            confirmation.poll(start + CONFIRM_TIMEOUT * 2),
            Confirmed::Connected
        );
    }

    #[test]
    fn a_connection_recorded_before_the_install_does_not_count() {
        let updates = Updates::new();
        updates.connected("9.9.9");
        let start = Instant::now();
        let confirmation = Confirmation::begin(&updates.0, "9.9.9", start, NOW).unwrap();
        assert_eq!(confirmation.poll(start), Confirmed::Waiting);
        assert!(!updates.0.join(CONNECTED_FILE).exists());
    }

    #[test]
    fn the_new_build_holds_its_own_updates_while_it_is_confirmed() {
        let updates = Updates::new();
        assert!(!confirming(&updates.0, NOW), "no update installing");
        let confirmation = Confirmation::begin(&updates.0, "9.9.9", Instant::now(), NOW).unwrap();
        assert!(confirming(&updates.0, NOW));
        assert!(confirming(&updates.0, NOW + CONFIRM_TIMEOUT.as_secs() - 1));
        // One left by an installer that never finished expires.
        assert!(!confirming(&updates.0, NOW + CONFIRM_TIMEOUT.as_secs()));
        // A clock set back far enough to put the deadline beyond the timeout means it moved.
        assert!(!confirming(&updates.0, NOW - 1));
        // Committing or rolling back ends it at once.
        drop(confirmation);
        assert!(!updates.0.join(PENDING_FILE).exists());
        assert!(!confirming(&updates.0, NOW));
    }

    #[test]
    fn a_damaged_pending_record_holds_nothing_back() {
        let updates = Updates::new();
        std::fs::write(updates.0.join(PENDING_FILE), b"{").unwrap();
        assert!(!confirming(&updates.0, NOW));
    }
}
