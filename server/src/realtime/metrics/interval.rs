//! How often each connected Agent reports its resource usage and refreshes
//! its screen thumbnail: every [`METRICS_WATCHED_INTERVAL_SECONDS`] and
//! [`THUMBNAIL_WATCHED_INTERVAL_SECONDS`] while a website tab shows its
//! device, every [`METRICS_IDLE_INTERVAL_SECONDS`] and
//! [`THUMBNAIL_IDLE_INTERVAL_SECONDS`] otherwise.
//!
//! Each Agent connection's task follows its device in the [`super::Watched`]
//! registry with a [`ReportingInterval`]: it tells the Agent both intervals
//! when it connects, always, so the Agent's intervals are the server's, and
//! again whenever the device's watched state changes. Becoming watched is
//! told at once, so an opened page has a fresh reading and a fresh image
//! within seconds. No longer being watched is told only after
//! [`UNWATCHED_LINGER`], and not at all if the device is watched again
//! first, so a wall of tiles being scrolled, a tab reloading or an event
//! socket reconnecting does not flap Agents between intervals. The two
//! intervals are two commands, [`AgentCommand::SetMetricsInterval`] and
//! [`AgentCommand::SetThumbnailInterval`], always sent together.
//!
//! A follower is one listener in the registry: following, being told of a
//! change and stopping each take the registry's lock once, and one waits
//! without cost while nothing changes. A tab watching or leaving 500 devices
//! wakes those devices' connections, not the fleet's.
use std::time::Duration;

use meshrmm_protocol_types::{
    AgentCommand, METRICS_IDLE_INTERVAL_SECONDS, METRICS_WATCHED_INTERVAL_SECONDS,
    THUMBNAIL_IDLE_INTERVAL_SECONDS, THUMBNAIL_WATCHED_INTERVAL_SECONDS,
};
use tokio::time::Instant;

use super::WatchedSignal;

/// How long a device that stopped being watched keeps the watched
/// intervals: longer than a page reload or an event socket's
/// reconnect takes, and short enough that a device left alone soon costs
/// the server a reading a minute.
pub const UNWATCHED_LINGER: Duration = Duration::from_secs(25);

const WATCHED_SECONDS: u32 = METRICS_WATCHED_INTERVAL_SECONDS as u32;
const IDLE_SECONDS: u32 = METRICS_IDLE_INTERVAL_SECONDS as u32;
const THUMBNAIL_WATCHED_SECONDS: u32 = THUMBNAIL_WATCHED_INTERVAL_SECONDS as u32;
const THUMBNAIL_IDLE_SECONDS: u32 = THUMBNAIL_IDLE_INTERVAL_SECONDS as u32;

/// What a connection tells its Agent about how often to report and to
/// refresh its thumbnail.
#[derive(Debug)]
pub struct ReportingInterval {
    signal: WatchedSignal,
    linger: Duration,
    /// Whether the Agent was last told the watched intervals.
    told: bool,
    /// When to tell it the idle intervals, while the device's change to
    /// unwatched lingers.
    slow_at: Option<Instant>,
}

impl ReportingInterval {
    /// Follows the device from now on, lingering `linger` after it stops
    /// being watched.
    pub(super) fn new(signal: WatchedSignal, linger: Duration) -> Self {
        let told = signal.is_watched();
        Self {
            signal,
            linger,
            told,
            slow_at: None,
        }
    }

    /// What to tell the Agent when it connects: the intervals for the
    /// device as it is watched now.
    pub fn greeting(&self) -> [AgentCommand; 2] {
        intervals(self.told)
    }

    /// What to tell the Agent next, once the device's watched state changed
    /// from what it was last told. Cancel safe.
    pub async fn next(&mut self) -> [AgentCommand; 2] {
        loop {
            tokio::select! {
                watched = self.signal.changed() => {
                    if watched {
                        // Watched again while lingering: nothing to tell.
                        self.slow_at = None;
                        if !self.told {
                            return self.tell(true);
                        }
                    } else if self.told && self.slow_at.is_none() {
                        self.slow_at = Some(Instant::now() + self.linger);
                    }
                }
                () = tokio::time::sleep_until(self.slow_at.unwrap_or_else(Instant::now)),
                    if self.slow_at.is_some() =>
                {
                    self.slow_at = None;
                    return self.tell(false);
                }
            }
        }
    }

    fn tell(&mut self, watched: bool) -> [AgentCommand; 2] {
        self.told = watched;
        intervals(watched)
    }
}

/// The two intervals for a device that is `watched` or not.
fn intervals(watched: bool) -> [AgentCommand; 2] {
    let (metrics, thumbnails) = if watched {
        (WATCHED_SECONDS, THUMBNAIL_WATCHED_SECONDS)
    } else {
        (IDLE_SECONDS, THUMBNAIL_IDLE_SECONDS)
    };
    [
        AgentCommand::SetMetricsInterval { seconds: metrics },
        AgentCommand::SetThumbnailInterval {
            seconds: thumbnails,
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::realtime::metrics::watched::{WatchSet, Watched};

    const LINGER: Duration = Duration::from_secs(25);

    /// What the Agent is told for a device `watched` or not.
    fn interval(watched: bool) -> [AgentCommand; 2] {
        let (metrics, thumbnails) = if watched { (5, 300) } else { (60, 3600) };
        [
            AgentCommand::SetMetricsInterval { seconds: metrics },
            AgentCommand::SetThumbnailInterval {
                seconds: thumbnails,
            },
        ]
    }

    fn watching(registry: &Watched, device_id: &str) -> WatchSet {
        let mut socket = WatchSet::new(registry.clone());
        socket.replace(HashSet::from([device_id.to_owned()]));
        socket
    }

    /// What the follower tells within `wait`, if anything.
    async fn told_within(
        follower: &mut ReportingInterval,
        wait: Duration,
    ) -> Option<[AgentCommand; 2]> {
        tokio::time::timeout(wait, follower.next()).await.ok()
    }

    #[tokio::test(start_paused = true)]
    async fn an_agent_is_greeted_with_the_interval_for_its_device_now() {
        let registry = Watched::default();
        let idle = ReportingInterval::new(registry.listen("a"), LINGER);
        assert_eq!(idle.greeting(), interval(false));
        let _socket = watching(&registry, "a");
        let watched = ReportingInterval::new(registry.listen("a"), LINGER);
        assert_eq!(watched.greeting(), interval(true));
    }

    #[tokio::test(start_paused = true)]
    async fn watching_is_told_at_once_and_unwatching_after_the_linger() {
        let registry = Watched::default();
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        let socket = watching(&registry, "a");
        let start = Instant::now();
        assert_eq!(follower.next().await, interval(true));
        assert_eq!(start.elapsed(), Duration::ZERO, "at once");

        drop(socket);
        let left = Instant::now();
        assert_eq!(
            told_within(&mut follower, LINGER - Duration::from_millis(1)).await,
            None
        );
        assert_eq!(follower.next().await, interval(false));
        assert_eq!(left.elapsed(), LINGER);
        // Nothing more while nothing changes.
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_device_watched_again_while_lingering_is_told_nothing() {
        let registry = Watched::default();
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        let socket = watching(&registry, "a");
        assert_eq!(follower.next().await, interval(true));
        drop(socket);
        assert_eq!(told_within(&mut follower, LINGER / 2).await, None);
        // A tab reloading watches it again.
        let socket = watching(&registry, "a");
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
        // And leaving again lingers afresh.
        drop(socket);
        let left = Instant::now();
        assert_eq!(follower.next().await, interval(false));
        assert_eq!(left.elapsed(), LINGER);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_tab_coming_or_going_changes_nothing() {
        let registry = Watched::default();
        let first = watching(&registry, "a");
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        assert_eq!(follower.greeting(), interval(true));
        let second = watching(&registry, "a");
        drop(first);
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
        drop(second);
        assert_eq!(follower.next().await, interval(false));
    }

    #[tokio::test(start_paused = true)]
    async fn a_watch_that_ends_before_it_is_seen_is_told_nothing() {
        let registry = Watched::default();
        let mut follower = ReportingInterval::new(registry.listen("a"), LINGER);
        drop(watching(&registry, "a"));
        assert_eq!(told_within(&mut follower, 10 * LINGER).await, None);
    }
}
