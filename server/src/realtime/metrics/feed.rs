//! The readings website event sockets send: only those of the devices each
//! socket watches.
//!
//! Each socket says which devices' live readings its tab shows
//! ([`Subscription::watch`]); the fleet-wide count of sockets watching each
//! device is the [`Watched`] registry. While any socket is subscribed, one
//! task wakes every [`FEED_INTERVAL`] and takes the latest reading of each
//! watched device that reported since its last scan: one scan, under the
//! lock Agents' reports take, of the watched devices or of the fleet,
//! whichever is fewer. It broadcasts them as the tick. A socket sends the
//! tick's readings of the devices it watches, looked up from whichever of
//! the two is smaller, so what a socket costs a tick grows with what it
//! watches, not with the fleet. Each reading's JSON is made once, by the
//! first socket that sends it, and every other socket sends the same bytes
//! within its own message. The first socket to subscribe starts the task,
//! and it stops at the first tick after the last one goes.
//!
//! A socket that starts watching a device sends the device's latest reading
//! at once, without waiting for the tick; one that asks for a new snapshot
//! or falls behind sends every watched device's latest again.
//!
//! A socket also hears when the screen thumbnail of a device it watches is
//! replaced ([`Metrics::thumbnail_replaced`]), as
//! `{"type":"thumbnails","device_ids":[...]}`, so its tab loads the new
//! image at once. An upload is announced only while some socket watches the
//! device, which is when its Agent refreshes the image every few minutes;
//! every socket hears it and drops it unless it watches the device, a hash
//! lookup. One that falls behind is sent every device it watches.
//!
//! Readings are numbered in the order they are recorded, and a socket keeps
//! the number of the last reading it sent for each device it watches, so it
//! never sends a device's reading twice, but when it starts over, or one
//! older than it sent. Nor does
//! it miss one: a device is counted as watched before its latest reading is
//! taken, so any newer reading is in a tick scanned after that, and the
//! socket subscribed to the ticks before it watched anything.
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::extract::ws::Utf8Bytes;
use meshrmm_protocol_types::METRICS_WATCHED_INTERVAL_SECONDS;
use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::{Duration, Instant, MissedTickBehavior},
};

use super::{
    Metrics, Reading,
    watched::{WatchSet, Watched},
};

/// How often new readings are sent: as often as the Agents of watched
/// devices report, which are the only ones sent.
pub const FEED_INTERVAL: Duration = Duration::from_secs(METRICS_WATCHED_INTERVAL_SECONDS);
/// The most devices one socket watches: more than a wall of the smallest
/// tiles shows on a 4K screen at half zoom (about 30 columns by 15 rows,
/// with the rows drawn either side). A watch message naming more watches
/// the first this many and ignores the rest.
pub const MAX_WATCHED: usize = 500;
/// Ticks a socket may fall behind by before it sends every watched device's
/// latest reading again.
const BACKLOG: usize = 4;
/// Replaced thumbnails a socket may fall behind by before it sends every
/// device it watches as replaced.
pub(super) const THUMBNAIL_BACKLOG: usize = 256;

/// One reading in a tick, and its JSON once a socket asked for it.
#[derive(Debug)]
struct Item {
    reading: Reading,
    json: OnceLock<Box<str>>,
}

/// The latest reading of each watched device that reported in a tick, by
/// device.
#[derive(Debug)]
struct Tick {
    readings: HashMap<String, Item>,
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
}

#[derive(Debug, Default)]
struct Counters {
    scans: AtomicU64,
    ticks: AtomicU64,
    serialized: AtomicU64,
    sent: AtomicU64,
}

/// The work the feed did, for tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedWork {
    /// Scans for new readings of watched devices: one a tick while any
    /// socket is subscribed.
    pub scans: u64,
    /// Ticks broadcast: those that found a new reading.
    pub ticks: u64,
    /// Readings serialised: each tick's once however many sockets send it,
    /// and each sent when a socket starts watching its device or starts
    /// over.
    pub serialized: u64,
    /// Readings sent, counting each socket's copy.
    pub sent: u64,
}

fn count(counter: &AtomicU64, by: usize) {
    counter.fetch_add(by as u64, Ordering::Relaxed);
}

/// A metrics message of readings already serialised:
/// `{"type":"metrics","readings":[...]}`.
fn message<'a>(readings: impl IntoIterator<Item = &'a str>) -> Utf8Bytes {
    let mut json = String::from(r#"{"type":"metrics","readings":["#);
    for (n, reading) in readings.into_iter().enumerate() {
        if n > 0 {
            json.push(',');
        }
        json.push_str(reading);
    }
    json.push_str("]}");
    json.into()
}

/// `{"type":"thumbnails","device_ids":[...]}`.
fn thumbnails_message<'a>(device_ids: impl IntoIterator<Item = &'a str>) -> Option<Utf8Bytes> {
    let device_ids: Vec<&str> = device_ids.into_iter().collect();
    if device_ids.is_empty() {
        return None;
    }
    let json = serde_json::json!({ "type": "thumbnails", "device_ids": device_ids });
    Some(json.to_string().into())
}

fn serialize(reading: &Reading) -> String {
    serde_json::to_string(reading).expect("readings serialize")
}

impl Metrics {
    fn feed(&self) -> std::sync::MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.feed
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Readings for an event socket, of the devices it watches: none until
    /// it watches some.
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
            thumbnails: self.thumbnails.subscribe(),
            watching: WatchSet::new(self.watched().clone()),
        }
    }

    /// Tells the event sockets that watch the device that its screen
    /// thumbnail was replaced. Costs one lookup when none does.
    pub fn thumbnail_replaced(&self, device_id: &str) {
        if self.watched.is_watched(device_id) {
            // No socket subscribed is no one to tell.
            let _ = self.thumbnails.send(device_id.into());
        }
    }

    /// The fleet's watched devices.
    pub fn watched(&self) -> &Watched {
        &self.watched
    }

    /// The work the feed did so far.
    pub fn feed_work(&self) -> FeedWork {
        let work = &self.feed.work;
        FeedWork {
            scans: work.scans.load(Ordering::Relaxed),
            ticks: work.ticks.load(Ordering::Relaxed),
            serialized: work.serialized.load(Ordering::Relaxed),
            sent: work.sent.load(Ordering::Relaxed),
        }
    }

    /// Broadcasts the watched devices' new readings every [`FEED_INTERVAL`]
    /// from `first` until no socket listens.
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
                    return;
                }
                feed.from
            };
            let (readings, to) = self.watched_since(from);
            count(&self.feed.work.scans, 1);
            // The next tick starts after this one, whether or not it found
            // anything: readings of devices nobody watched are not sent
            // later, as a socket starting to watch sends the latest itself.
            self.feed().from = to;
            if readings.is_empty() {
                continue;
            }
            count(&self.feed.work.ticks, 1);
            let readings = readings
                .into_iter()
                .map(|reading| {
                    let item = Item {
                        reading,
                        json: OnceLock::new(),
                    };
                    (item.reading.device_id.clone(), item)
                })
                .collect();
            let _ = ticks.send(Arc::new(Tick { readings }));
        }
    }
}

/// One event socket's readings.
pub struct Subscription {
    metrics: Metrics,
    ticks: broadcast::Receiver<Arc<Tick>>,
    thumbnails: broadcast::Receiver<Arc<str>>,
    watching: WatchSet,
}

impl Subscription {
    /// Watches exactly `device_ids`, at most [`MAX_WATCHED`] of them, and
    /// returns the latest reading of each it did not watch before, or none
    /// when none of them has one. The caller passes only devices the socket
    /// may see.
    pub fn watch(&mut self, device_ids: impl IntoIterator<Item = String>) -> Option<Utf8Bytes> {
        let device_ids: HashSet<String> = device_ids.into_iter().take(MAX_WATCHED).collect();
        let added = self.watching.replace(device_ids);
        self.send_latest(added.iter().map(|id| &**id))
    }

    /// Keeps watching only those of its devices in `device_ids`, as those
    /// still in the fleet.
    pub fn retain(&mut self, device_ids: impl IntoIterator<Item = String>) {
        let kept = device_ids
            .into_iter()
            .filter(|id| self.watching.sent_mut(id).is_some())
            .collect();
        self.watching.replace(kept);
    }

    /// Stops watching the device, as when it was deleted.
    pub fn unwatch(&mut self, device_id: &str) {
        self.watching.remove(device_id);
    }

    /// The devices it watches.
    pub fn watching(&self) -> impl Iterator<Item = &str> {
        self.watching.ids()
    }

    /// Every watched device's latest reading, to send after a snapshot, or
    /// none when none has one; the ticks go on from it. The ticks already
    /// waiting are covered by it, so they are dropped rather than left to
    /// make the socket fall behind.
    pub fn restart(&mut self) -> Option<Utf8Bytes> {
        // Subscribed again before the readings are taken, so a newer one is
        // in a tick it receives.
        self.ticks = self.ticks.resubscribe();
        // Sent again, even those already sent: a tab that missed changes may
        // have dropped a reading of a device it thought offline.
        for (_, sent) in self.watching.iter_mut() {
            *sent = 0;
        }
        let ids: Vec<String> = self.watching.ids().map(str::to_owned).collect();
        self.send_latest(ids.iter().map(String::as_str))
    }

    /// The latest readings of the watched `device_ids` newer than any sent.
    fn send_latest<'a>(
        &mut self,
        device_ids: impl IntoIterator<Item = &'a str>,
    ) -> Option<Utf8Bytes> {
        let mut sent = Vec::new();
        for reading in self.metrics.latest_readings(device_ids) {
            if let Some(last) = self.watching.sent_mut(&reading.device_id)
                && reading.sequence > *last
            {
                *last = reading.sequence;
                sent.push(serialize(&reading));
            }
        }
        if sent.is_empty() {
            return None;
        }
        let work = &self.metrics.feed.work;
        count(&work.serialized, sent.len());
        count(&work.sent, sent.len());
        Some(message(sent.iter().map(String::as_str)))
    }

    /// The next readings, or replaced thumbnails, to send. Safe to cancel:
    /// nothing is lost by dropping it while it waits.
    pub async fn next(&mut self) -> Utf8Bytes {
        loop {
            tokio::select! {
                tick = self.ticks.recv() => {
                    let tick = match tick {
                        Ok(tick) => tick,
                        Err(RecvError::Lagged(_)) => match self.restart() {
                            Some(json) => return json,
                            None => continue,
                        },
                        // The feed keeps a sender while anyone is subscribed.
                        Err(RecvError::Closed) => std::future::pending().await,
                    };
                    if let Some(json) = self.pick(&tick) {
                        return json;
                    }
                }
                replaced = self.thumbnails.recv() => {
                    let json = match replaced {
                        Ok(device_id) => thumbnails_message(
                            self.watching.sent_mut(&device_id).is_some().then_some(&*device_id),
                        ),
                        Err(RecvError::Lagged(_)) => thumbnails_message(self.watching.ids()),
                        // The store this holds keeps the sender.
                        Err(RecvError::Closed) => std::future::pending().await,
                    };
                    if let Some(json) = json {
                        return json;
                    }
                }
            }
        }
    }

    /// The tick's readings of the watched devices that are newer than those
    /// sent, looked up from the smaller of the two.
    fn pick(&mut self, tick: &Tick) -> Option<Utf8Bytes> {
        let mut picked: Vec<&Item> = Vec::new();
        if tick.readings.len() <= self.watching.len() {
            for (device_id, item) in &tick.readings {
                if let Some(sent) = self.watching.sent_mut(device_id)
                    && item.reading.sequence > *sent
                {
                    *sent = item.reading.sequence;
                    picked.push(item);
                }
            }
        } else {
            for (device_id, sent) in self.watching.iter_mut() {
                if let Some(item) = tick.readings.get(device_id)
                    && item.reading.sequence > *sent
                {
                    *sent = item.reading.sequence;
                    picked.push(item);
                }
            }
        }
        if picked.is_empty() {
            return None;
        }
        let work = &self.metrics.feed.work;
        count(&work.sent, picked.len());
        Some(message(picked.into_iter().map(|item| {
            &**item.json.get_or_init(|| {
                count(&work.serialized, 1);
                serialize(&item.reading).into()
            })
        })))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use meshrmm_protocol_types::MetricsReport;
    use serde_json::Value;

    use super::*;
    use crate::{db::Database, time::SECOND_MS};

    fn usage(cpu: f32) -> MetricsReport {
        MetricsReport {
            cpu_percent: cpu,
            memory_used_bytes: 1,
            memory_total_bytes: 2,
            network_received_bytes_per_second: 0,
            network_sent_bytes_per_second: 0,
            uptime_seconds: 60,
            volumes: Some(Vec::new()),
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

    /// The socket's next message, if one is ready.
    async fn ready(socket: &mut Subscription) -> Option<BTreeMap<String, i64>> {
        tokio::time::timeout(Duration::ZERO, socket.next())
            .await
            .ok()
            .map(|json| readings(&json))
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn device(name: &str, at: i64) -> (String, i64) {
        (name.to_owned(), at)
    }

    #[tokio::test]
    async fn a_socket_sends_only_the_devices_it_watches() {
        let metrics = store().await;
        let mut socket = metrics.subscribe();
        for name in ["a", "b", "c"] {
            metrics.record_at(name, usage(1.0), 0);
        }
        // Watching sends the latest readings at once.
        let sent = socket.watch(ids(&["a", "b"])).unwrap();
        assert_eq!(
            readings(&sent),
            BTreeMap::from([device("a", 0), device("b", 0)])
        );
        let ten = 10 * SECOND_MS;
        for name in ["a", "b", "c"] {
            metrics.record_at(name, usage(1.0), ten);
        }
        tick().await;
        assert_eq!(
            ready(&mut socket).await,
            Some(BTreeMap::from([device("a", ten), device("b", ten)]))
        );

        // A new set: the devices it adds are sent at once, without waiting
        // for the tick; those it keeps are not sent again, and those it
        // drops no longer.
        let sent = socket.watch(ids(&["b", "c"])).unwrap();
        assert_eq!(readings(&sent), BTreeMap::from([device("c", ten)]));
        assert_eq!(socket.watch(ids(&["b", "c"])), None, "nothing new");
        let twenty = 20 * SECOND_MS;
        for name in ["a", "b", "c"] {
            metrics.record_at(name, usage(1.0), twenty);
        }
        tick().await;
        assert_eq!(
            ready(&mut socket).await,
            Some(BTreeMap::from([device("b", twenty), device("c", twenty)]))
        );

        // Watching none sends none.
        assert_eq!(socket.watch(Vec::new()), None);
        metrics.record_at("b", usage(1.0), 30 * SECOND_MS);
        tick().await;
        assert_eq!(ready(&mut socket).await, None);
        // A device without a reading, as one offline, is watched and sent
        // once it reports.
        assert_eq!(socket.watch(ids(&["d"])), None);
        metrics.record_at("d", usage(1.0), 30 * SECOND_MS);
        tick().await;
        assert_eq!(
            ready(&mut socket).await,
            Some(BTreeMap::from([device("d", 30 * SECOND_MS)]))
        );
    }

    /// The devices of the socket's next message, if one is ready and it
    /// announces replaced thumbnails.
    async fn replaced(socket: &mut Subscription) -> Option<Vec<String>> {
        let json = tokio::time::timeout(Duration::ZERO, socket.next())
            .await
            .ok()?;
        let message: Value = serde_json::from_str(json.as_str()).unwrap();
        assert_eq!(message["type"], "thumbnails");
        let mut ids: Vec<String> = serde_json::from_value(message["device_ids"].clone()).unwrap();
        ids.sort();
        Some(ids)
    }

    #[tokio::test]
    async fn replaced_thumbnails_reach_only_the_sockets_watching_them() {
        let metrics = store().await;
        let mut watching = metrics.subscribe();
        let mut other = metrics.subscribe();
        watching.watch(ids(&["a", "b"]));
        other.watch(ids(&["c"]));
        metrics.thumbnail_replaced("a");
        assert_eq!(replaced(&mut watching).await, Some(ids(&["a"])));
        assert_eq!(replaced(&mut watching).await, None);
        assert_eq!(replaced(&mut other).await, None);
        // Nobody watches it: not even sent to be dropped.
        metrics.thumbnail_replaced("z");
        assert_eq!(metrics.thumbnails.len(), 0);
        assert_eq!(replaced(&mut watching).await, None);

        // A socket that fell behind is told every device it watches.
        for _ in 0..=THUMBNAIL_BACKLOG {
            metrics.thumbnail_replaced("c");
        }
        assert_eq!(replaced(&mut watching).await, Some(ids(&["a", "b"])));
        assert_eq!(replaced(&mut watching).await, None);
    }

    #[tokio::test]
    async fn a_watch_takes_at_most_the_bound() {
        let metrics = store().await;
        let mut socket = metrics.subscribe();
        let many = (0..MAX_WATCHED + 50).map(|n| format!("d{n:04}"));
        socket.watch(many);
        assert_eq!(socket.watching().count(), MAX_WATCHED);
        assert_eq!(metrics.watched().len(), MAX_WATCHED);
        // The first ones are kept.
        assert!(socket.watching().all(|id| id < "d0500"));
    }

    #[tokio::test]
    async fn a_tick_costs_a_socket_what_it_watches_not_the_fleet() {
        let metrics = store().await;
        let fleet = (0..2_000).map(|n| format!("d{n:04}")).collect::<Vec<_>>();
        let mut sockets = (0..10).map(|_| metrics.subscribe()).collect::<Vec<_>>();
        for socket in &mut sockets {
            assert_eq!(socket.watch(ids(&["d0007", "d1234"])), None);
        }
        tick().await;
        assert_eq!(metrics.feed_work().ticks, 0, "nothing reported");

        for device in &fleet {
            metrics.record_at(device, usage(1.0), 0);
        }
        tick().await;
        let mut sent = Vec::new();
        for socket in &mut sockets {
            sent.push(ready(socket).await.unwrap());
        }
        let expected = BTreeMap::from([device("d0007", 0), device("d1234", 0)]);
        assert!(
            sent.iter().all(|readings| readings == &expected),
            "{sent:?}"
        );
        // One scan a tick, each sent reading serialised once for every
        // socket, and none of the 1,998 readings nobody watches.
        assert_eq!(
            metrics.feed_work(),
            FeedWork {
                scans: 2,
                ticks: 1,
                serialized: 2,
                sent: 20,
            }
        );

        // The feed stops at the first tick after the last socket goes.
        drop(sockets);
        assert!(metrics.watched().is_empty(), "the sockets left");
        tick().await;
        tick().await;
        assert_eq!(metrics.feed_work().scans, 2);
        assert!(metrics.feed().ticks.is_none());
    }

    #[tokio::test]
    async fn sockets_watching_while_devices_report_get_each_reading_once() {
        let metrics = store().await;
        let names = (0..8).map(|n| format!("d{n}")).collect::<Vec<_>>();
        let mut sockets: Vec<(Subscription, Vec<BTreeMap<String, i64>>)> = Vec::new();
        let mut recorded = HashSet::new();
        for round in 0..6_usize {
            // Devices report, and sockets open and watch, or change what
            // they watch, in between: each sends what it starts watching at
            // once.
            for n in 0..8 {
                if n % 3 == round % 3 {
                    let mut socket = metrics.subscribe();
                    let sent = socket.watch(names.clone()).map(|json| readings(&json));
                    sockets.push((socket, sent.into_iter().collect()));
                }
                if n % 4 == round % 4
                    && let Some((socket, sent)) = sockets.first_mut()
                {
                    let half = names.iter().skip(n % 2).step_by(2).cloned();
                    sent.extend(socket.watch(half).map(|json| readings(&json)));
                    sent.extend(socket.watch(names.clone()).map(|json| readings(&json)));
                }
                let at = (round as i64) * 10 * SECOND_MS + n as i64;
                metrics.record_at(&names[n], usage(1.0), at);
                recorded.insert(device(&names[n], at));
            }
            tick().await;
            for (socket, sent) in &mut sockets {
                sent.extend(ready(socket).await);
            }
        }
        assert_eq!(metrics.feed_work().scans, 6);
        for (_, sent) in &sockets {
            let mut newest = BTreeMap::new();
            for (id, at) in sent.iter().flatten() {
                assert!(recorded.contains(&(id.clone(), *at)));
                let last = newest.entry(id.clone()).or_insert(*at);
                assert!(*at >= *last, "{id} at {at} is older than one sent");
                *last = *at;
            }
            // Ending with every device's latest reading.
            for name in &names {
                let latest = metrics.latest(name).unwrap();
                assert_eq!(newest[&latest.device_id], latest.at);
            }
        }
        // A socket that never changed its set sent each reading once: only
        // a device dropped and watched again is sent again.
        let (_, steady) = &sockets[1];
        let all = steady.iter().map(BTreeMap::len).sum::<usize>();
        let distinct = steady.iter().flatten().collect::<HashSet<_>>().len();
        assert_eq!(all, distinct, "{steady:?}");
    }

    #[tokio::test]
    async fn a_socket_that_falls_behind_sends_its_devices_latest_again() {
        let metrics = store().await;
        let mut lagging = metrics.subscribe();
        let mut keeping_up = metrics.subscribe();
        let names = (0..=BACKLOG + 2)
            .map(|n| format!("d{n}"))
            .collect::<Vec<_>>();
        lagging.watch(names.clone());
        keeping_up.watch(names.clone());
        for (round, name) in names.iter().enumerate() {
            metrics.record_at(name, usage(1.0), round as i64);
            tick().await;
            keeping_up.next().await;
        }
        let again = readings(&lagging.next().await);
        assert_eq!(again.len(), names.len(), "{again:?}");
        // Then the ticks again, alike.
        metrics.record_at("d0", usage(1.0), 10 * SECOND_MS);
        tick().await;
        assert_eq!(lagging.next().await, keeping_up.next().await);
    }

    #[tokio::test]
    async fn starting_over_sends_every_watched_devices_latest() {
        let metrics = store().await;
        let mut socket = metrics.subscribe();
        metrics.record_at("a", usage(1.0), 0);
        metrics.record_at("b", usage(1.0), 0);
        socket.watch(ids(&["a"]));
        assert_eq!(
            socket.restart().map(|json| readings(&json)),
            Some(BTreeMap::from([device("a", 0)]))
        );
        socket.unwatch("a");
        assert_eq!(socket.restart(), None);
        assert!(metrics.watched().is_empty());
    }

    #[test]
    fn the_message_is_as_the_website_reads_it() {
        let reading = Reading {
            device_id: "a".into(),
            at: 5,
            metrics: usage(3.0).with_volumes(Vec::new()),
            sequence: 9,
        };
        let one = serialize(&reading);
        let both = message([one.as_str(), one.as_str()]);
        let json: Value = serde_json::from_str(both.as_str()).unwrap();
        let reading = serde_json::json!({
            "device_id": "a",
            "at": 5,
            "cpu_percent": 3.0,
            "memory_used_bytes": 1,
            "memory_total_bytes": 2,
            "network_received_bytes_per_second": 0,
            "network_sent_bytes_per_second": 0,
            "uptime_seconds": 60,
            "volumes": [],
        });
        assert_eq!(
            json,
            serde_json::json!({ "type": "metrics", "readings": [reading, reading] })
        );
        let empty: Value = serde_json::from_str(message([]).as_str()).unwrap();
        assert_eq!(
            empty,
            serde_json::json!({ "type": "metrics", "readings": [] })
        );
    }
}
