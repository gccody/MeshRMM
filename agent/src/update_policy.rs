//! When the Agent checks for updates and how often it retries a release,
//! shared by the Windows service and the macOS coordinator.
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub(crate) const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// An update check that falls due during a remote session is retried this often until the
/// session ends, so the update does not interrupt it.
pub(crate) const DEFERRED_UPDATE_RETRY: Duration = Duration::from_secs(5 * 60);
/// A session that never ends does not keep the Agent from updating for longer than this.
const MAX_UPDATE_DEFERRAL: Duration = Duration::from_secs(24 * 60 * 60);
/// A failed update restarts the previous Agent, which checks for updates again at once, so a
/// release that cannot be installed is only retried this many times per window.
pub(crate) const MAX_ATTEMPTS_PER_VERSION: u32 = 3;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Schedules automatic update checks and postpones them while a remote session is live.
pub(crate) struct UpdateSchedule {
    next_check: Instant,
    deferred_since: Option<Instant>,
}

impl UpdateSchedule {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            next_check: now,
            deferred_since: None,
        }
    }

    /// Whether to check for an update now. Staging an update stops the service and ends the
    /// session, so a check waits for the session to end, but not beyond the maximum deferral.
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
        self.next_check = now + UPDATE_CHECK_INTERVAL;
        true
    }
}

/// Update attempts for the release most recently offered, kept in the private update directory.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UpdateAttempts {
    pub version: String,
    pub attempts: u32,
    pub first_attempt_unix: u64,
}

impl UpdateAttempts {
    /// The record to store before trying `version`, or `None` once it has used its attempts in
    /// the current window.
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

    #[test]
    fn checks_for_updates_on_schedule_without_a_session() {
        let start = Instant::now();
        let mut updates = UpdateSchedule::new(start);
        assert!(updates.due(start, false));
        assert!(!updates.due(start + UPDATE_CHECK_INTERVAL / 2, false));
        assert!(updates.due(start + UPDATE_CHECK_INTERVAL, false));
    }

    #[test]
    fn postpones_update_checks_during_a_session_up_to_the_limit() {
        let start = Instant::now();
        let mut updates = UpdateSchedule::new(start);
        assert!(!updates.due(start, true));
        assert!(!updates.due(start + DEFERRED_UPDATE_RETRY / 2, false));
        assert!(!updates.due(start + DEFERRED_UPDATE_RETRY, true));
        // The check runs soon after the session ends, not a full interval later.
        assert!(updates.due(start + DEFERRED_UPDATE_RETRY * 2, false));

        let mut updates = UpdateSchedule::new(start);
        let mut now = start;
        while !updates.due(now, true) {
            now += DEFERRED_UPDATE_RETRY;
            assert!(now <= start + MAX_UPDATE_DEFERRAL);
        }
        assert_eq!(now, start + MAX_UPDATE_DEFERRAL);
        // A later session gets the full deferral again.
        assert!(!updates.due(now + UPDATE_CHECK_INTERVAL, true));
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
