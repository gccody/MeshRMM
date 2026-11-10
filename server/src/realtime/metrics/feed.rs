//! The readings every website event socket sends, scanned and serialised
//! once for all of them.
//!
//! While any socket listens, one task wakes every [`FEED_INTERVAL`], takes
//! the latest reading of each device that reported since its last scan (one
//! scan, under the lock Agents' reports take, however many sockets listen),
//! serialises them once outside that lock, and broadcasts the tick. The
//! first socket to subscribe starts the task, and it stops at the first tick
//! after the last one goes.
//!
//! A socket that opens, asks for a new snapshot, or falls behind first sends
//! the full set: every online device's latest reading. Sockets that ask
//! before the next tick share one full set. Then it forwards the ticks.
//!
//! Readings are numbered in the order they are recorded. A tick holds the
//! readings numbered after its `from`, up to and including its `to`, and
//! each tick starts where the one before it ended. A full set holds the
//! readings up to its `sequence`. The hand-over keeps this invariant: the
//! first tick a socket receives after a full set starts at or before the
//! full set's sequence, so no reading falls between them; and of that tick,
//! the socket sends only the readings numbered after it, so none is sent
//! twice. It holds because a socket subscribes before it takes its full set,
//! and a full set is shared only while it is no older than the start of the
//! next tick.
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};

use axum::extract::ws::Utf8Bytes;
use meshrmm_protocol_types::METRICS_INTERVAL_SECONDS;
use serde::Serialize;
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::{Duration, Instant, MissedTickBehavior},
};

use super::{Metrics, Reading};

/// How often new readings are sent: as often as Agents report.
pub const FEED_INTERVAL: Duration = Duration::from_secs(METRICS_INTERVAL_SECONDS);
/// Ticks a socket may fall behind by before it gets the full set again.
const BACKLOG: usize = 4;

/// What the website's event socket sends about devices' resource usage.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MetricsEvent<'a> {
    Metrics { readings: Vec<&'a Reading> },
}

fn serialize<'a>(readings: impl IntoIterator<Item = &'a Reading>) -> Utf8Bytes {
    serde_json::to_string(&MetricsEvent::Metrics {
        readings: readings.into_iter().collect(),
    })
    .expect("metrics events serialize")
    .into()
}

#[derive(Debug, Default)]
pub(super) struct Feed {
    state: Mutex<State>,
    work: Counters,
}

#[derive(Debug, Default)]
struct State {
    /// While the task runs: the channel new sockets subscribe to.
    ticks: Option<broadcast::Sender<Arc<Tick>>>,
    /// Where the next tick starts.
    from: u64,
    /// The latest full set, while it is no older than `from`.
    full: Option<Arc<Full>>,
}

/// The latest reading of each device that reported after `from`, up to and
/// including `to`, and their JSON.
#[derive(Debug)]
struct Tick {
    from: u64,
    to: u64,
    readings: Vec<Reading>,
    json: Utf8Bytes,
}

/// Every online device's latest reading up to `sequence`, as JSON; none
/// when no device has one.
#[derive(Debug)]
struct Full {
    sequence: u64,
    json: Option<Utf8Bytes>,
}

#[derive(Debug, Default)]
struct Counters {
    scans: AtomicU64,
    ticks: AtomicU64,
    full_sets: AtomicU64,
    hand_overs: AtomicU64,
}

/// The work the feed did, for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedWork {
    /// Scans for new readings: one a tick while any socket listens.
    pub scans: u64,
    /// Ticks broadcast, each serialised once.
    pub ticks: u64,
    /// Full sets built, each a scan of every device and serialised once.
    pub full_sets: u64,
    /// Ticks serialised again for one socket, without the readings its full
    /// set already had: at most once after each full set.
    pub hand_overs: u64,
}

fn count(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

impl Metrics {
    fn feed(&self) -> std::sync::MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.feed
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Readings for an event socket: every online device's latest, then
    /// each newer one about every [`FEED_INTERVAL`].
    pub fn subscribe(&self) -> Subscription {
        let mut feed = self.feed();
        let ticks = match &feed.ticks {
            Some(ticks) => ticks.subscribe(),
            None => {
                let (sender, receiver) = broadcast::channel(BACKLOG);
                feed.from = self.sequence();
                feed.ticks = Some(sender.clone());
                let first = Instant::now() + FEED_INTERVAL;
                tokio::spawn(self.clone().broadcast(sender, first));
                receiver
            }
        };
        Subscription {
            metrics: self.clone(),
            ticks,
            sent: None,
        }
    }

    /// The work the feed did so far.
    pub fn feed_work(&self) -> FeedWork {
        let work = &self.feed.work;
        FeedWork {
            scans: work.scans.load(Ordering::Relaxed),
            ticks: work.ticks.load(Ordering::Relaxed),
            full_sets: work.full_sets.load(Ordering::Relaxed),
            hand_overs: work.hand_overs.load(Ordering::Relaxed),
        }
    }

    /// Broadcasts the new readings every [`FEED_INTERVAL`] from `first` until
    /// no socket listens.
    async fn broadcast(self, ticks: broadcast::Sender<Arc<Tick>>, first: Instant) {
        let mut interval = tokio::time::interval_at(first, FEED_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let from = {
                let mut feed = self.feed();
                // Under the lock sockets subscribe under, so none subscribes
                // to a task that has stopped.
                if ticks.receiver_count() == 0 {
                    feed.ticks = None;
                    feed.full = None;
                    return;
                }
                feed.from
            };
            let (readings, to) = self.since(from);
            count(&self.feed.work.scans);
            if readings.is_empty() {
                // The next tick starts from the same place.
                continue;
            }
            {
                let mut feed = self.feed();
                feed.from = to;
                if feed.full.as_ref().is_some_and(|full| full.sequence < to) {
                    feed.full = None;
                }
            }
            let json = serialize(&readings);
            count(&self.feed.work.ticks);
            let _ = ticks.send(Arc::new(Tick {
                from,
                to,
                readings,
                json,
            }));
        }
    }

    /// Every online device's latest reading: the one taken since the last
    /// tick, or a new one.
    fn full_set(&self) -> Arc<Full> {
        if let Some(full) = &self.feed().full {
            return Arc::clone(full);
        }
        let (readings, sequence) = self.since(0);
        let full = Arc::new(Full {
            sequence,
            json: (!readings.is_empty()).then(|| serialize(&readings)),
        });
        count(&self.feed.work.full_sets);
        let mut feed = self.feed();
        // A tick scanned since starts after it, so it can't be shared.
        if sequence >= feed.from
            && feed
                .full
                .as_ref()
                .is_none_or(|kept| kept.sequence < sequence)
        {
            feed.full = Some(Arc::clone(&full));
        }
        full
    }
}

/// One event socket's readings.
pub struct Subscription {
    metrics: Metrics,
    ticks: broadcast::Receiver<Arc<Tick>>,
    /// The readings up to this were sent; none while the full set is due.
    sent: Option<u64>,
}

impl Subscription {
    /// Every online device's latest reading, to send after a snapshot, or
    /// none when no device has one; the ticks go on from it. The ticks
    /// already waiting are covered by it, so they are dropped rather than
    /// left to make the socket fall behind.
    pub fn restart(&mut self) -> Option<Utf8Bytes> {
        // Subscribed again before the full set is taken, as the hand-over
        // needs.
        self.ticks = self.ticks.resubscribe();
        let full = self.metrics.full_set();
        self.sent = Some(full.sequence);
        full.json.clone()
    }

    /// The next readings to send: the full set first if it wasn't, and the
    /// full set again after falling behind. Safe to cancel: nothing is lost
    /// by dropping it while it waits.
    pub async fn next(&mut self) -> Utf8Bytes {
        loop {
            let Some(sent) = self.sent else {
                match self.restart() {
                    Some(json) => return json,
                    None => continue,
                }
            };
            let tick = match self.ticks.recv().await {
                Ok(tick) => tick,
                Err(RecvError::Lagged(_)) => {
                    self.sent = None;
                    continue;
                }
                // The feed keeps a sender while anyone is subscribed.
                Err(RecvError::Closed) => std::future::pending().await,
            };
            if tick.from > sent {
                // Readings after `sent` would be missing. The invariant rules
                // this out; were it broken, a full set makes the socket whole.
                self.sent = None;
                continue;
            }
            if tick.to <= sent {
                // The full set had all of it.
                continue;
            }
            self.sent = Some(tick.to);
            if tick.readings.iter().all(|reading| reading.sequence > sent) {
                return tick.json.clone();
            }
            // The full set had some of it: send only the rest.
            let mut rest = tick
                .readings
                .iter()
                .filter(|reading| reading.sequence > sent)
                .peekable();
            if rest.peek().is_some() {
                count(&self.metrics.feed.work.hand_overs);
                return serialize(rest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use meshrmm_protocol_types::SystemMetrics;
    use serde_json::Value;

    use super::*;
    use crate::{db::Database, time::SECOND_MS};

    fn usage(cpu: f32) -> SystemMetrics {
        SystemMetrics {
            cpu_percent: cpu,
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            network_received_bytes_per_second: 0,
            network_sent_bytes_per_second: 0,
            uptime_seconds: 60,
            volumes: Vec::new(),
        }
    }

    /// A store whose feed runs on paused time.
    async fn store() -> Metrics {
        let metrics = Metrics::new(
            Database::connect("sqlite::memory:", 1)
                .await
                .expect("an in-memory database opens"),
        );
        tokio::time::pause();
        metrics
    }

    /// Lets one interval pass, and the feed's task finish its tick. Timers
    /// round their deadlines up to the millisecond, hence the one more.
    async fn tick() {
        tokio::time::advance(FEED_INTERVAL + Duration::from_millis(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    /// The readings in a message, as each device's time.
    fn readings(json: &Utf8Bytes) -> BTreeMap<String, i64> {
        let message: Value = serde_json::from_str(json.as_str()).unwrap();
        assert_eq!(message["type"], "metrics");
        message["readings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reading| {
                (
                    reading["device_id"].as_str().unwrap().to_owned(),
                    reading["at"].as_i64().unwrap(),
                )
            })
            .collect()
    }

    /// Whether every socket sent the very same bytes.
    fn same_bytes(sent: &[Utf8Bytes]) -> bool {
        sent.iter()
            .all(|json| json.as_str().as_ptr() == sent[0].as_str().as_ptr())
    }

    fn device(name: &str, at: i64) -> (String, i64) {
        (name.to_owned(), at)
    }

    #[tokio::test]
    async fn one_scan_a_tick_however_many_sockets_listen_and_none_while_none_do() {
        let metrics = store().await;
        metrics.record_at("a", usage(1.0), 0);
        tick().await;
        tick().await;
        assert_eq!(metrics.feed_work(), FeedWork::default(), "nobody listens");

        // Sockets that open in the same tick share one full set.
        let mut sockets = (0..10).map(|_| metrics.subscribe()).collect::<Vec<_>>();
        let mut sent = Vec::new();
        for socket in &mut sockets {
            sent.push(socket.next().await);
        }
        assert!(same_bytes(&sent));
        assert_eq!(readings(&sent[0]), BTreeMap::from([device("a", 0)]));

        // Each tick is one scan and one serialisation, sent by every socket.
        for round in 1..=3 {
            let at = round * 10 * SECOND_MS;
            metrics.record_at("a", usage(1.0), at);
            tick().await;
            let mut sent = Vec::new();
            for socket in &mut sockets {
                sent.push(socket.next().await);
            }
            assert!(same_bytes(&sent));
            assert_eq!(readings(&sent[0]), BTreeMap::from([device("a", at)]));
        }
        // A tick without new readings scans, and sends nothing.
        tick().await;
        let busy = FeedWork {
            scans: 4,
            ticks: 3,
            full_sets: 1,
            hand_overs: 0,
        };
        assert_eq!(metrics.feed_work(), busy);

        // The feed stops at the first tick after the last socket goes.
        drop(sockets);
        tick().await;
        metrics.record_at("a", usage(1.0), 100 * SECOND_MS);
        tick().await;
        tick().await;
        assert_eq!(metrics.feed_work(), busy);
        assert!(metrics.feed().ticks.is_none());

        // And starts again for the next.
        let mut socket = metrics.subscribe();
        assert_eq!(
            readings(&socket.next().await),
            BTreeMap::from([device("a", 100 * SECOND_MS)])
        );
        tick().await;
        assert_eq!(metrics.feed_work().scans, busy.scans + 1);
    }

    #[tokio::test]
    async fn a_socket_opening_mid_tick_misses_no_reading_and_repeats_none() {
        let metrics = store().await;
        let ten = 10 * SECOND_MS;
        let mut early = metrics.subscribe();
        metrics.record_at("a", usage(1.0), 0);
        metrics.record_at("b", usage(1.0), 0);
        // This full set has a and b; the next tick has them too, and what
        // is recorded after the full set.
        let mut late = metrics.subscribe();
        let mut sent = vec![readings(&late.next().await)];
        metrics.record_at("a", usage(1.0), ten);
        metrics.record_at("c", usage(1.0), 0);
        tick().await;
        // The hand-over: only what the full set lacked.
        sent.push(readings(&late.next().await));
        assert_eq!(metrics.feed_work().hand_overs, 1);
        // A socket asking after the tick gets a new full set, and skips the
        // tick that it covers.
        let mut sent_early = vec![readings(&early.next().await)];
        metrics.record_at("b", usage(1.0), ten);
        tick().await;
        let (next_early, next_late) = (early.next().await, late.next().await);
        assert!(same_bytes(&[next_early.clone(), next_late.clone()]));
        sent.push(readings(&next_late));
        sent_early.push(readings(&next_early));

        assert_eq!(
            sent,
            [
                BTreeMap::from([device("a", 0), device("b", 0)]),
                BTreeMap::from([device("a", ten), device("c", 0)]),
                BTreeMap::from([device("b", ten)]),
            ]
        );
        assert_eq!(
            sent_early,
            [
                BTreeMap::from([device("a", ten), device("b", 0), device("c", 0)]),
                BTreeMap::from([device("b", ten)]),
            ]
        );
        assert_eq!(
            metrics.feed_work(),
            FeedWork {
                scans: 2,
                ticks: 2,
                full_sets: 2,
                hand_overs: 1,
            }
        );
    }

    #[tokio::test]
    async fn sockets_opening_while_devices_report_get_each_reading_once() {
        let metrics = store().await;
        let mut sockets = Vec::new();
        let mut sent: Vec<Vec<BTreeMap<String, i64>>> = Vec::new();
        let mut recorded = HashSet::new();
        for round in 0..6_i64 {
            // Devices report, and sockets open in between, each sending its
            // full set at once.
            for n in 0..8 {
                if n % 3 == round % 3 && !recorded.is_empty() {
                    let mut socket = metrics.subscribe();
                    sent.push(vec![readings(&socket.next().await)]);
                    sockets.push(socket);
                }
                let reading = device(&format!("d{n}"), round * 10 * SECOND_MS + n);
                metrics.record_at(&reading.0, usage(1.0), reading.1);
                recorded.insert(reading);
            }
            tick().await;
            for (socket, sent) in sockets.iter_mut().zip(&mut sent) {
                sent.push(readings(&socket.next().await));
            }
        }
        assert!(metrics.feed_work().hand_overs > 0, "some ticks straddled");
        assert_eq!(metrics.feed_work().scans, 6);
        for sent in &sent {
            let mut seen = HashSet::new();
            let mut newest = BTreeMap::new();
            for reading in sent.iter().flatten() {
                let reading = (reading.0.clone(), *reading.1);
                assert!(recorded.contains(&reading));
                let last = newest.entry(reading.0.clone()).or_insert(reading.1);
                assert!(reading.1 >= *last, "{reading:?} is older than one sent");
                *last = reading.1;
                assert!(seen.insert(reading.clone()), "{reading:?} was sent twice");
            }
            // Ending with every device's latest reading.
            for n in 0..8 {
                let latest = metrics.latest(&format!("d{n}")).unwrap();
                assert_eq!(newest[&latest.device_id], latest.at);
            }
        }
    }

    #[tokio::test]
    async fn a_socket_that_falls_behind_gets_the_full_set_again() {
        let metrics = store().await;
        let mut lagging = metrics.subscribe();
        let mut keeping_up = metrics.subscribe();
        metrics.record_at("d0", usage(1.0), 0);
        lagging.next().await;
        keeping_up.next().await;
        let rounds = BACKLOG as i64 + 2;
        for round in 1..=rounds {
            metrics.record_at(&format!("d{round}"), usage(1.0), 0);
            tick().await;
            keeping_up.next().await;
        }
        let full = readings(&lagging.next().await);
        assert_eq!(full.len(), rounds as usize + 1, "{full:?}");
        assert_eq!(metrics.feed_work().full_sets, 2);
        // Then the ticks again, shared with the socket that kept up.
        metrics.record_at("d0", usage(1.0), 10 * SECOND_MS);
        tick().await;
        let (behind, ahead) = (lagging.next().await, keeping_up.next().await);
        assert!(same_bytes(&[behind.clone(), ahead]));
        assert_eq!(
            readings(&behind),
            BTreeMap::from([device("d0", 10 * SECOND_MS)])
        );
    }

    #[test]
    fn the_message_is_as_the_website_reads_it() {
        let reading = Reading {
            device_id: "a".into(),
            at: 5,
            metrics: usage(3.0),
            sequence: 9,
        };
        let json: Value = serde_json::from_str(serialize([&reading]).as_str()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "metrics",
                "readings": [{
                    "device_id": "a",
                    "at": 5,
                    "cpu_percent": 3.0,
                    "memory_used_bytes": 1,
                    "memory_total_bytes": 2,
                    "network_received_bytes_per_second": 0,
                    "network_sent_bytes_per_second": 0,
                    "uptime_seconds": 60,
                    "volumes": [],
                }],
            })
        );
    }
}
