//! When the Agent checks for updates and how often it retries a release,
//! shared by the Windows service and the Linux and macOS coordinators.
//!
//! Every Agent of a server gets the same release, so a fleet's checks must not
//! line up: after a restart a device first checks at its own point in the
//! interval, derived from its device ID, and each later interval is jittered.
use std::path::Path;
use std::time::{Duration, Instant};

use meshrmm_self_update::CURRENT_VERSION;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Each interval is lengthened or shortened by up to this fraction at random, so devices drift
/// apart rather than staying in step with each other or with the clock.
const INTERVAL_JITTER: f64 = 0.1;
/// A build's first start checks at a random point in this window, since its installer may be
/// older than the server's release.
const FIRST_START_CHECK_AFTER: Duration = Duration::from_secs(30);
const FIRST_START_CHECK_WINDOW: Duration = Duration::from_secs(60);
/// A failed check is retried after this, doubling up to the maximum, until one succeeds.
const FAILED_CHECK_RETRY: Duration = Duration::from_secs(2 * 60);
const MAX_FAILED_CHECK_RETRY: Duration = Duration::from_secs(60 * 60);
/// An update check that falls due during a remote session is retried this often until the
/// session ends, so the update does not interrupt it.
pub(crate) const DEFERRED_UPDATE_RETRY: Duration = Duration::from_secs(5 * 60);
/// A session that never ends does not keep the Agent from updating for longer than this.
const MAX_UPDATE_DEFERRAL: Duration = Duration::from_secs(24 * 60 * 60);
/// A failed update restarts the previous Agent, which checks for updates again, so a release
/// that cannot be installed is only retried this many times per window.
pub(crate) const MAX_ATTEMPTS_PER_VERSION: u32 = 3;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
/// In the private update directory: the build whose update check last succeeded.
pub(crate) const CHECKED_VERSION_FILE: &str = "checked-version";

/// Schedules automatic update checks and postpones them while a remote session is live.
pub(crate) struct UpdateSchedule {
    next_check: Instant,
    deferred_since: Option<Instant>,
    failures: u32,
}

impl UpdateSchedule {
    /// `first_start` is whether this build has yet to complete a check, as after an install or
    /// an update. `random` is fresh bits from [`random_bits`].
    pub(crate) fn new(now: Instant, device_id: &str, first_start: bool, random: u64) -> Self {
        Self {
            next_check: now + first_check_delay(device_id, first_start, random),
            deferred_since: None,
            failures: 0,
        }
    }

    /// Whether to check for an update now; the caller reports how it went to [`Self::checked`].
    /// Staging an update stops the service and ends the session, so a check waits for the
    /// session to end, but not beyond the maximum deferral.
    pub(crate) fn due(&mut self, now: Instant, session_active: bool) -> bool {
        if now < self.next_check {
            return false;
        }
        if session_active {
            let since = *self.deferred_since.get_or_insert(now);
            let deferred = now.duration_since(since);
            if deferred < MAX_UPDATE_DEFERRAL {
                if deferred.is_zero() {
                    tracing::info!(
                        "postponing the automatic Agent update check until the remote session ends"
                    );
                }
                self.next_check = now + DEFERRED_UPDATE_RETRY;
                return false;
            }
            tracing::warn!(
                deferred_hours = deferred.as_secs() / 3600,
                "checking for an Agent update although a remote session is still active"
            );
        } else if self.deferred_since.is_some() {
            tracing::info!("the remote session ended; running the postponed Agent update check");
        }
        self.deferred_since = None;
        true
    }

    /// Schedules the next check after one that finished at `now`. `random` is fresh bits from
    /// [`random_bits`].
    pub(crate) fn checked(&mut self, now: Instant, succeeded: bool, random: u64) {
        self.failures = if succeeded {
            0
        } else {
            self.failures.saturating_add(1)
        };
        self.next_check = now + delay_after_check(self.failures, random);
    }
}

/// How long after a start the first check waits. A build's first start checks within about two
/// minutes. An ordinary restart waits for the device's phase, up to a whole interval, so a fleet
/// that restarts together, as when a site's power returns, still checks apart.
fn first_check_delay(device_id: &str, first_start: bool, random: u64) -> Duration {
    if first_start {
        FIRST_START_CHECK_AFTER + FIRST_START_CHECK_WINDOW.mul_f64(fraction(random))
    } else {
        device_phase(device_id)
    }
}

/// The device's point in the check interval: a different one for each device, and the same one
/// for a device on every start.
fn device_phase(device_id: &str) -> Duration {
    let digest = Sha256::digest(device_id.as_bytes());
    let mut bits = [0; 8];
    bits.copy_from_slice(&digest[..8]);
    UPDATE_CHECK_INTERVAL.mul_f64(fraction(u64::from_be_bytes(bits)))
}

/// The wait after a check, given how many checks in a row have now failed: about an interval
/// after a success, and after a failure a retry delay that doubles up to an hour. The retry is
/// shortened by up to half, so Agents that lost the server together come back apart.
fn delay_after_check(failures: u32, random: u64) -> Duration {
    let fraction = fraction(random);
    if failures == 0 {
        return UPDATE_CHECK_INTERVAL
            .mul_f64(1.0 - INTERVAL_JITTER + 2.0 * INTERVAL_JITTER * fraction);
    }
    let retry = FAILED_CHECK_RETRY
        .saturating_mul(2_u32.saturating_pow(failures - 1))
        .min(MAX_FAILED_CHECK_RETRY);
    retry.mul_f64(0.5 + fraction / 2.0)
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

/// Whether this build has completed an update check before, which tells an ordinary restart from
/// the first start after an install or update. `path` is the [`CHECKED_VERSION_FILE`].
pub(crate) fn checked_before(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|version| version.trim() == CURRENT_VERSION)
}

/// Records at `path` that this build has completed a check, if that is not recorded yet, with
/// the platform's atomic `replace_file`.
pub(crate) fn record_check(
    path: &Path,
    replace_file: impl FnOnce(&Path, &[u8]) -> anyhow::Result<()>,
) {
    if checked_before(path) {
        return;
    }
    if let Err(error) = replace_file(path, CURRENT_VERSION.as_bytes()) {
        // The next restart then checks early again, which is harmless.
        tracing::warn!(error = ?error, "could not record the Agent update check");
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    fn due_at(next_check: Instant) -> UpdateSchedule {
        UpdateSchedule {
            next_check,
            deferred_since: None,
            failures: 0,
        }
    }

    #[test]
    fn spreads_restarted_devices_over_the_interval() {
        let phases = (0..600)
            .map(|device| device_phase(&uuid::Uuid::from_u128(device).to_string()))
            .collect::<Vec<_>>();
        assert!(phases.iter().all(|phase| *phase < UPDATE_CHECK_INTERVAL));
        // Every hour of the interval gets some of them.
        for hour in 0..6 {
            let range = Duration::from_secs(hour * 3600)..Duration::from_secs((hour + 1) * 3600);
            let count = phases.iter().filter(|phase| range.contains(*phase)).count();
            assert!((50..150).contains(&count), "hour {hour}: {count}");
        }
        let device = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
        // A restart lands on the same phase whatever the random bits.
        assert_eq!(
            first_check_delay(device, false, 0),
            first_check_delay(device, false, u64::MAX)
        );
        assert_eq!(first_check_delay(device, false, 0), device_phase(device));
        assert_ne!(device_phase(device), device_phase("another device"));
    }

    #[test]
    fn checks_soon_after_a_builds_first_start() {
        let device = "6f9619ff-8b86-d011-b42d-00cf4fc964ff";
        assert_eq!(first_check_delay(device, true, 0), Duration::from_secs(30));
        let latest = first_check_delay(device, true, u64::MAX);
        assert!(latest > Duration::from_secs(89) && latest <= Duration::from_secs(90));

        let start = Instant::now();
        let mut updates = UpdateSchedule::new(start, device, true, 0);
        assert!(!updates.due(start, false));
        assert!(updates.due(start + Duration::from_secs(30), false));
    }

    #[test]
    fn jitters_each_interval_by_a_tenth() {
        let shortest = delay_after_check(0, 0);
        let longest = delay_after_check(0, u64::MAX);
        assert_eq!(shortest, UPDATE_CHECK_INTERVAL.mul_f64(0.9));
        assert!(longest <= UPDATE_CHECK_INTERVAL.mul_f64(1.1));
        assert!(longest > UPDATE_CHECK_INTERVAL.mul_f64(1.0999));
        assert_eq!(
            delay_after_check(0, u64::MAX / 2 + 1),
            UPDATE_CHECK_INTERVAL
        );
    }

    #[test]
    fn backs_off_after_failed_checks_up_to_an_hour() {
        let minutes = |minutes: u64| Duration::from_secs(minutes * 60);
        let longest = |failures| delay_after_check(failures, u64::MAX);
        assert!(longest(1) <= minutes(2) && longest(1) > minutes(2).mul_f64(0.99));
        assert!(longest(2) <= minutes(4) && longest(2) > longest(1));
        assert!(longest(6) <= minutes(60) && longest(6) > minutes(59));
        assert!(longest(u32::MAX) <= minutes(60));
        assert_eq!(delay_after_check(1, 0), minutes(1));
        assert_eq!(delay_after_check(u32::MAX, 0), minutes(30));
    }

    #[test]
    fn returns_to_the_interval_once_a_check_succeeds() {
        let start = Instant::now();
        let mut updates = due_at(start);
        let mut now = start;
        let mut previous = Duration::ZERO;
        for _ in 0..10 {
            assert!(updates.due(now, false));
            updates.checked(now, false, u64::MAX);
            let wait = updates.next_check - now;
            assert!(wait >= previous && wait <= MAX_FAILED_CHECK_RETRY);
            previous = wait;
            now = updates.next_check;
        }
        assert!(previous > MAX_FAILED_CHECK_RETRY.mul_f64(0.99));
        assert!(updates.due(now, false));
        updates.checked(now, true, 0);
        assert!(!updates.due(now + UPDATE_CHECK_INTERVAL.mul_f64(0.89), false));
        assert!(updates.due(now + UPDATE_CHECK_INTERVAL.mul_f64(0.9), false));
        // The next failure starts the backoff again from its first step.
        updates.checked(now, false, u64::MAX);
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
        updates.checked(now, true, 0);
        // A later session gets the full deferral again.
        assert!(!updates.due(now + UPDATE_CHECK_INTERVAL, true));
    }

    #[test]
    fn records_that_this_build_has_checked() {
        let directory = std::env::temp_dir().join(format!(
            "meshrmm-update-check-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CHECKED_VERSION_FILE);
        assert!(!checked_before(&path));
        std::fs::write(&path, "an older build").unwrap();
        assert!(!checked_before(&path));
        record_check(&path, |path, contents| Ok(std::fs::write(path, contents)?));
        assert!(checked_before(&path));
        record_check(&path, |_: &Path, _: &[u8]| {
            panic!("an existing record is not written again")
        });
        std::fs::remove_dir_all(&directory).unwrap();
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
}
