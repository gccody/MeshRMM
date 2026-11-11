//! How often each connected Agent reports its resource usage: every
//! [`METRICS_WATCHED_INTERVAL_SECONDS`] while a website tab shows its
//! device's live readings, every [`METRICS_IDLE_INTERVAL_SECONDS`]
//! otherwise.
//!
//! Each Agent connection's task follows its device in the [`super::Watched`]
//! registry with a [`ReportingInterval`]: it tells the Agent the interval
//! when it connects, always, so the Agent's interval is the server's, and
//! again whenever the device's watched state changes. Becoming watched is
//! told at once, so an opened page has a fresh reading within seconds. No
//! longer being watched is told only after [`UNWATCHED_LINGER`], and not at
//! all if the device is watched again first, so a wall of tiles being
//! scrolled, a tab reloading or an event socket reconnecting does not flap
//! Agents between intervals.
//!
//! A follower is one listener in the registry: following, being told of a
//! change and stopping each take the registry's lock once, and one waits
//! without cost while nothing changes. A tab watching or leaving 500 devices
//! wakes those devices' connections, not the fleet's.
use std::time::Duration;

use meshrmm_protocol_types::{
    AgentCommand, METRICS_IDLE_INTERVAL_SECONDS, METRICS_WATCHED_INTERVAL_SECONDS,
};
use tokio::time::Instant;

use super::WatchedSignal;

/// How long a device that stopped being watched keeps reporting at the
/// watched interval: longer than a page reload or an event socket's
/// reconnect takes, and short enough that a device left alone soon costs
/// the server a reading a minute.
pub const UNWATCHED_LINGER: Duration = Duration::from_secs(25);

const WATCHED_SECONDS: u32 = METRICS_WATCHED_INTERVAL_SECONDS as u32;
const IDLE_SECONDS: u32 = METRICS_IDLE_INTERVAL_SECONDS as u32;

/// What a connection tells its Agent about how often to report.
#[derive(Debug)]
pub struct ReportingInterval {
    signal: WatchedSignal,
    linger: Duration,
    /// The interval the Agent was last told, in seconds.
    told: u32,
    /// When to tell it the idle interval, while the device's change to
    /// unwatched lingers.
    slow_at: Option<Instant>,
}

impl ReportingInterval {
    /// Follows the device from now on, lingering `linger` after it stops
    /// being watched.
    pub(super) fn new(signal: WatchedSignal, linger: Duration) -> Self {
        let told = if signal.is_watched() {
            WATCHED_SECONDS
        } else {
            IDLE_SECONDS
        };
        Self {
            signal,
            linger,
            told,
            slow_at: None,
        }
    }

    /// What to tell the Agent when it connects: the interval for the device
    /// as it is watched now.
    pub fn greeting(&self) -> AgentCommand {
        AgentCommand::SetMetricsInterval { seconds: self.told }
    }

    /// What to tell the Agent next, once the device's watched state changed
    /// from what it was last told. Cancel safe.
    pub async fn next(&mut self) -> AgentCommand {
        loop {
            tokio::select! {
                watched = self.signal.changed() => {
                    if watched {
                        // Watched again while lingering: nothing to tell.
                        self.slow_at = None;
                        if self.told != WATCHED_SECONDS {
                            return self.tell(WATCHED_SECONDS);
                        }
                    } else if self.told != IDLE_SECONDS && self.slow_at.is_none() {
                        self.slow_at = Some(Instant::now() + self.linger);
                    }
                }
                () = tokio::time::sleep_until(self.slow_at.unwrap_or_else(Instant::now)),
                    if self.slow_at.is_some() =>
                {
                    self.slow_at = None;
                    return self.tell(IDLE_SECONDS);
                }
            }
        }
    }

    fn tell(&mut self, seconds: u32) -> AgentCommand {
        self.told = seconds;
        AgentCommand::SetMetricsInterval { seconds }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::realtime::metrics::watched::{WatchSet, Watched};

    const LINGER: Duration = Duration::from_secs(25);

    fn interval(seconds: u32) -> AgentCommand {
        AgentCommand::SetMetricsInterval { seconds }
    }

    fn watching(registry: &Watched, device_id: &str) -> WatchSet {
        let mut socket = WatchSet::new(registry.clone());
        socket.replace(HashSet::from([device_id.to_owned()]));
        socket
    }

    /// What the follower tells within `wait`, if anything.
    async fn told_within(follower: &mut ReportingInterval, wait: Duration) -> Option<AgentCommand> {
        tokio::time::timeout(wait, follower.next()).await.ok()
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_is_greeted_with_the_interval_for_its_device_now() {
        let registry = Watched::default();
        let idle = ReportingInterval::new(registry.listen("a"), LINGER);
        assert_eq!(idle.greeting(), interval(60));
        let _socket = watching(&registry, "a");
        let watched = ReportingInterval::new(registry.listen("a"), LINGER);
        assert_eq!(watched.greeting(), interval(5));
    }

    #[tokio::test(start_paused = true)]
    async fn watching_is_told_at_once_and_unwatching_after_the_linger() {
        let registry = Watched::default();
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        let socket = watching(&registry, "a");
        let start = Instant::now();
        assert_eq!(follower.next().await, interval(5));
        assert_eq!(start.elapsed(), Duration::ZERO, "at once");

        drop(socket);
        let left = Instant::now();
        assert_eq!(
            told_within(&mut follower, LINGER - Duration::from_millis(1)).await,
            None
        );
        assert_eq!(follower.next().await, interval(60));
        assert_eq!(left.elapsed(), LINGER);
        // Nothing more while nothing changes.
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_watched_again_while_lingering_is_told_nothing() {
        let registry = Watched::default();
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        let socket = watching(&registry, "a");
        assert_eq!(follower.next().await, interval(5));
        drop(socket);
        assert_eq!(told_within(&mut follower, LINGER / 2).await, None);
        // A tab reloading watches it again.
        let socket = watching(&registry, "a");
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
        // And leaving again lingers afresh.
        drop(socket);
        let left = Instant::now();
        assert_eq!(follower.next().await, interval(60));
        assert_eq!(left.elapsed(), LINGER);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_tab_coming_or_going_changes_nothing() {
        let registry = Watched::default();
        let first = watching(&registry, "a");
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        assert_eq!(follower.greeting(), interval(5));
        let second = watching(&registry, "a");
        drop(first);
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
        drop(second);
        assert_eq!(follower.next().await, interval(60));
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_that_ends_before_it_is_seen_is_told_nothing() {
        let registry = Watched::default();
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        drop(watching(&registry, "a"));
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
    }
}
