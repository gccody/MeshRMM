//! Ends a remote session after the technician has been idle in the viewer
//! for the chosen time. The company sets the default and whether technicians
//! may choose another time; a choice lasts only for the current session.

use std::time::{Duration, Instant};

use meshrmm_protocol::{IDLE_DISCONNECT_MINUTES, IdleDisconnectPolicy};

/// How often the session checks whether it has been idle too long.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(1);

pub struct IdleDisconnect {
    policy: IdleDisconnectPolicy,
    /// This session's choice: `None` follows the policy, `Some(None)` never
    /// disconnects.
    choice: Option<Option<u32>>,
    last_activity: Instant,
}

impl Default for IdleDisconnect {
    fn default() -> Self {
        Self {
            policy: IdleDisconnectPolicy::default(),
            choice: None,
            last_activity: Instant::now(),
        }
    }
}

impl IdleDisconnect {
    pub fn set_policy(&mut self, policy: IdleDisconnectPolicy) {
        self.policy = policy;
    }

    pub fn allow_override(&self) -> bool {
        self.policy.allow_override
    }

    /// The idle time in force, in minutes; `None` never disconnects.
    pub fn minutes(&self) -> Option<u32> {
        self.policy.effective(self.choice)
    }

    /// Chooses the idle time for the rest of this session, when the company
    /// allows it. Choosing counts as activity.
    pub fn choose(&mut self, minutes: Option<u32>, now: Instant) {
        if self.policy.allow_override {
            self.choice = Some(minutes);
            self.last_activity = now;
        }
    }

    pub fn note_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// The idle time that has run out by `now`, if one has.
    pub fn expired(&self, now: Instant) -> Option<u32> {
        let minutes = self.minutes()?;
        let limit = Duration::from_secs(u64::from(minutes) * 60);
        (now.saturating_duration_since(self.last_activity) >= limit).then_some(minutes)
    }
}

/// The idle times a technician can choose from, in menu order.
pub fn choices() -> impl Iterator<Item = Option<u32>> {
    std::iter::once(None).chain(IDLE_DISCONNECT_MINUTES.map(Some))
}

pub fn label(minutes: Option<u32>) -> String {
    match minutes {
        None => "Never".to_owned(),
        Some(60) => "1 hour".to_owned(),
        Some(minutes) if minutes % 60 == 0 => format!("{} hours", minutes / 60),
        Some(minutes) => format!("{minutes} minutes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(minutes: Option<u32>, allow_override: bool, now: Instant) -> IdleDisconnect {
        let mut idle = IdleDisconnect {
            last_activity: now,
            ..IdleDisconnect::default()
        };
        idle.set_policy(IdleDisconnectPolicy {
            minutes,
            allow_override,
        });
        idle
    }

    #[test]
    fn expires_after_the_idle_time_and_activity_restarts_it() {
        let start = Instant::now();
        let mut idle = session(Some(15), true, start);
        let fifteen = Duration::from_secs(15 * 60);
        assert_eq!(idle.expired(start + fifteen - Duration::from_secs(1)), None);
        assert_eq!(idle.expired(start + fifteen), Some(15));
        idle.note_activity(start + fifteen);
        assert_eq!(
            idle.expired(start + fifteen + Duration::from_secs(60)),
            None
        );
        assert_eq!(idle.expired(start + fifteen * 2), Some(15));
    }

    #[test]
    fn never_does_not_expire() {
        let start = Instant::now();
        let idle = session(None, true, start);
        assert_eq!(
            idle.expired(start + Duration::from_secs(365 * 24 * 60 * 60)),
            None
        );
    }

    #[test]
    fn a_session_choice_applies_only_when_the_company_allows_it() {
        let start = Instant::now();
        let later = start + Duration::from_secs(10 * 60);
        let mut allowed = session(Some(5), true, start);
        allowed.choose(None, later);
        assert_eq!(allowed.minutes(), None);
        assert_eq!(
            allowed.expired(later + Duration::from_secs(24 * 60 * 60)),
            None
        );
        allowed.choose(Some(30), later);
        assert_eq!(allowed.minutes(), Some(30));
        // Choosing counts as activity, so the new time runs from the choice.
        assert_eq!(allowed.expired(later + Duration::from_secs(29 * 60)), None);

        let mut managed = session(Some(5), false, start);
        managed.choose(None, later);
        assert_eq!(managed.minutes(), Some(5));
        assert_eq!(managed.expired(later), Some(5));
    }

    #[test]
    fn a_new_session_starts_from_the_company_default() {
        let start = Instant::now();
        let mut first = session(Some(15), true, start);
        first.choose(None, start);
        let second = session(Some(15), true, start);
        assert_eq!(second.minutes(), Some(15));
    }

    #[test]
    fn choices_start_with_never_and_labels_read_naturally() {
        let labels: Vec<String> = choices().map(label).collect();
        assert_eq!(
            labels,
            [
                "Never",
                "5 minutes",
                "10 minutes",
                "15 minutes",
                "30 minutes",
                "1 hour",
                "2 hours",
                "4 hours",
                "8 hours"
            ]
        );
    }
}
