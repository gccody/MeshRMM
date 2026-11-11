//! Devices' resource usage, as their Agents report it on the control
//! connection: every few seconds while a website tab shows the device's
//! readings, about once a minute otherwise ([`ReportingInterval`],
//! `metrics/interval.rs`).
//!
//! Each online device's latest reading and its readings from the last
//! [`LIVE_WINDOW_MS`] stay in memory for the website's live view. Each
//! minute's readings are averaged into one `device_metrics` row when the
//! next minute's first reading arrives or the Agent disconnects, and rows are
//! kept for [`HISTORY_MS`]. Readings are stamped with the server's clock, so
//! an Agent's wrong clock can't misplace them.
//!
//! A reading's loads and rates are averages over the time since the
//! previous one, which is five seconds while someone watches and a minute
//! otherwise, so averages weight each reading by the time it covers
//! ([`covered_ms`]): a minute someone watched for a while counts as much as
//! any other, not twelve times as much.
//!
//! An Agent sends its volumes only when it read them again and in the first
//! report on each connection. The device's last list is kept with its
//! latest reading and given to every reading, so readings always carry it.
//!
//! Finished minutes wait in memory and the writer task stores them every
//! [`FLUSH_INTERVAL`], all in one transaction, so a minute reaches the history
//! within that long. Writing each on its own would be one statement per
//! device per minute: 167 a second for 10,000 Agents.
//!
//! The website's event sockets get new readings from one feed
//! (`metrics/feed.rs`), and only those of the devices each socket watches:
//! every [`FEED_INTERVAL`] it scans once for the watched devices' readings
//! since its last scan, and each reading's JSON is made once however many
//! sockets send it. Which devices any socket watches is the [`Watched`]
//! registry (`metrics/watched.rs`).
mod feed;
mod interval;
mod watched;

use std::{
    collections::{BTreeMap, HashMap, VecDeque, btree_map::Entry},
    sync::{Arc, Mutex},
    time::Duration,
};

use meshrmm_protocol_types::{
    METRICS_IDLE_INTERVAL_SECONDS, METRICS_WATCHED_INTERVAL_SECONDS, MetricsReport, SystemMetrics,
    VolumeUsage,
};
use sea_query::{Expr, ExprTrait, InsertStatement, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};

pub use self::{
    feed::{FEED_INTERVAL, FeedWork, MAX_WATCHED, Subscription},
    interval::{ReportingInterval, UNWATCHED_LINGER},
    watched::{Watched, WatchedSignal},
};
use crate::{
    db::{self, Database, tables::DeviceMetrics},
    time::{DAY_MS, HOUR_MS, MINUTE_MS, SECOND_MS, now_ms},
};

/// How far back the live view reaches.
pub const LIVE_WINDOW_MS: i64 = 15 * MINUTE_MS;
/// The interval an Agent reports at while someone watches its device.
pub const WATCHED_INTERVAL_MS: i64 = METRICS_WATCHED_INTERVAL_SECONDS as i64 * SECOND_MS;
/// The interval an Agent reports at while nobody does.
pub const IDLE_INTERVAL_MS: i64 = METRICS_IDLE_INTERVAL_SECONDS as i64 * SECOND_MS;
/// Points a live window has room for before it has to grow: one per idle
/// interval, which is what most devices report at, and a few more for
/// jitter and for interval changes, whose first reading comes early. A
/// watched device's window grows to hold a reading every few seconds, and
/// shrinks back once the window no longer holds them.
const LIVE_ROOM: usize = (LIVE_WINDOW_MS / IDLE_INTERVAL_MS) as usize + 4;
/// The time a connection's first reading counts as covering. It is the
/// average since the Agent's previous sample, which may be seconds ago or
/// from before the connection: the server can't tell, so it counts for as
/// little as any reading does.
pub const FIRST_READING_COVERS_MS: i64 = WATCHED_INTERVAL_MS;
/// The most time a reading counts as covering: two idle intervals, so one
/// late reading counts in full, but one after a long silence does not
/// outweigh everything around it.
pub const LONGEST_READING_COVERS_MS: i64 = 2 * IDLE_INTERVAL_MS;

/// The time a reading taken at `at` covers, after the connection's previous
/// one at `previous`, for weighting averages: the time between them, at most
/// [`LONGEST_READING_COVERS_MS`], or [`FIRST_READING_COVERS_MS`] for the
/// first. Never less than a millisecond.
pub fn covered_ms(previous: Option<i64>, at: i64) -> i64 {
    previous
        .map_or(FIRST_READING_COVERS_MS, |previous| at - previous)
        .clamp(1, LONGEST_READING_COVERS_MS)
}
/// How long per-minute history is kept.
pub const HISTORY_MS: i64 = 7 * DAY_MS;
/// Readings closer together than this are dropped, which bounds what an
/// Agent that reports too often can make the server keep. Agents report at
/// least five seconds apart, but the first reading after an interval change
/// comes at a random point of the new one, so now and then one is dropped;
/// its volumes are kept, and the next reading covers its time.
const MIN_SPACING_MS: i64 = 2 * SECOND_MS;
/// How often the writer task stores the finished minutes.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
/// The most finished minutes kept waiting while the database can't take
/// them: 2 minutes of 100,000 Agents, or 20 of 10,000, in about 50 MB. The
/// oldest go first.
const MAX_PENDING_MINUTES: usize = 250_000;
/// Rows per insert statement. Each binds [`ROW_COLUMNS`] (11) parameters, so
/// 2,000 rows bind 22,000: under SQLite's limit of 32,766 and PostgreSQL's of
/// 65,535.
const ROWS_PER_STATEMENT: usize = 2_000;
/// When a batch is refused, minutes are tried one at a time; if this many in
/// a row are refused with none stored, it is the database, not the minutes,
/// and the rest wait for the next flush. Without it a database refusing
/// everything would take one statement per waiting minute, every flush. The
/// trade-off: if this many genuinely bad minutes sort first (by minute, so
/// one minute's devices), the good ones behind them wait too, until the bad
/// ones age out of the bound.
const REFUSALS_BEFORE_GIVING_UP: usize = 1_000;

/// A device's latest report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    pub device_id: String,
    /// When the server received it, in Unix milliseconds.
    pub at: i64,
    #[serde(flatten)]
    pub metrics: SystemMetrics,
    /// Orders readings across devices, so event sockets send each once.
    #[serde(skip)]
    pub sequence: u64,
}

/// Resource usage at one time, or averaged over a period starting then.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Point {
    pub at: i64,
    pub cpu_percent: f64,
    /// The busiest reading's CPU load; the load itself for a single reading.
    pub cpu_percent_max: f64,
    pub memory_used_bytes: i64,
    pub memory_total_bytes: i64,
    pub network_received_bytes_per_second: i64,
    pub network_sent_bytes_per_second: i64,
    pub storage_used_bytes: i64,
    pub storage_total_bytes: i64,
}

fn signed(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl Point {
    fn of(at: i64, metrics: &SystemMetrics) -> Self {
        let (storage_used, storage_total) = metrics.storage();
        Self {
            at,
            cpu_percent: f64::from(metrics.cpu_percent),
            cpu_percent_max: f64::from(metrics.cpu_percent),
            memory_used_bytes: signed(metrics.memory_used_bytes),
            memory_total_bytes: signed(metrics.memory_total_bytes),
            network_received_bytes_per_second: signed(metrics.network_received_bytes_per_second),
            network_sent_bytes_per_second: signed(metrics.network_sent_bytes_per_second),
            storage_used_bytes: signed(storage_used),
            storage_total_bytes: signed(storage_total),
        }
    }
}

/// Points of one period, each weighted by the milliseconds it covers. Loads
/// and rates average; capacities and storage take the latest point.
#[derive(Debug, Clone, Copy)]
struct Totals {
    start: i64,
    covered_ms: i64,
    cpu: f64,
    memory_used: f64,
    received: f64,
    sent: f64,
    last: Point,
}

impl Totals {
    fn new(start: i64, point: &Point, covered_ms: i64) -> Self {
        let mut totals = Self {
            start,
            covered_ms: 0,
            cpu: 0.0,
            memory_used: 0.0,
            received: 0.0,
            sent: 0.0,
            last: *point,
        };
        totals.add(point, covered_ms);
        totals
    }

    fn add(&mut self, point: &Point, covered_ms: i64) {
        let weight = covered_ms as f64;
        self.covered_ms += covered_ms;
        self.cpu += point.cpu_percent * weight;
        self.memory_used += point.memory_used_bytes as f64 * weight;
        self.received += point.network_received_bytes_per_second as f64 * weight;
        self.sent += point.network_sent_bytes_per_second as f64 * weight;
        let busiest = self.last.cpu_percent_max.max(point.cpu_percent_max);
        self.last = *point;
        self.last.cpu_percent_max = busiest;
    }

    fn point(&self) -> Point {
        let covered = Ord::max(self.covered_ms, 1) as f64;
        Point {
            at: self.start,
            cpu_percent: self.cpu / covered,
            memory_used_bytes: (self.memory_used / covered).round() as i64,
            network_received_bytes_per_second: (self.received / covered).round() as i64,
            network_sent_bytes_per_second: (self.sent / covered).round() as i64,
            ..self.last
        }
    }
}

/// What the history view covers, and how finely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Range {
    /// Every reading of the last [`LIVE_WINDOW_MS`], from memory.
    Live,
    Hour,
    Day,
    Week,
}

impl Range {
    /// How far back the range reaches.
    pub fn span_ms(self) -> i64 {
        match self {
            Self::Live => LIVE_WINDOW_MS,
            Self::Hour => HOUR_MS,
            Self::Day => DAY_MS,
            Self::Week => HISTORY_MS,
        }
    }

    /// How long each point covers: a few hundred points at most. Live
    /// points are readings, which cover the time since the previous one:
    /// this long while the device is watched, as when its page is open, and
    /// up to an idle interval before.
    pub fn step_ms(self) -> i64 {
        match self {
            Self::Live => WATCHED_INTERVAL_MS,
            Self::Hour => MINUTE_MS,
            Self::Day => 5 * MINUTE_MS,
            Self::Week => 30 * MINUTE_MS,
        }
    }
}

/// A reading in the live window, in 40 bytes where a [`Point`] takes 72: an
/// offset for its time, the load as the Agent's `f32`, the amounts unsigned
/// as reported, and capacities kept apart, as they rarely change.
#[derive(Debug, Clone, Copy)]
struct LivePoint {
    /// Milliseconds after its window's [`LiveWindow::base`].
    offset: u32,
    cpu_percent: f32,
    memory_used_bytes: u64,
    network_received_bytes_per_second: u64,
    network_sent_bytes_per_second: u64,
    storage_used_bytes: u64,
}

/// The capacities of the points from the one at `from` on.
#[derive(Debug, Clone, Copy)]
struct Capacities {
    from: u32,
    memory_total_bytes: u64,
    storage_total_bytes: u64,
}

/// A device's readings of the last [`LIVE_WINDOW_MS`], oldest first, which
/// make the same [`Point`]s as the readings themselves.
#[derive(Debug, Default)]
struct LiveWindow {
    /// The Unix milliseconds that offsets count from.
    base: i64,
    points: VecDeque<LivePoint>,
    /// Oldest first; the first applies to the first point.
    capacities: VecDeque<Capacities>,
}

impl LiveWindow {
    fn at(&self, point: &LivePoint) -> i64 {
        self.base + i64::from(point.offset)
    }

    /// Adds a reading taken at `at`, after the others, and drops those that
    /// leave the window.
    fn push(&mut self, at: i64, metrics: &SystemMetrics) {
        while let Some(oldest) = self.points.front().map(|point| self.at(point))
            && oldest <= at - LIVE_WINDOW_MS
        {
            self.points.pop_front();
        }
        // A device no longer watched gives back the room its readings every
        // few seconds took as they leave the window: a quarter full, or back
        // to the idle room's worth, it keeps twice what it holds.
        let (held, room) = (self.points.len(), self.points.capacity());
        if room > 2 * LIVE_ROOM && (held * 4 <= room || held <= LIVE_ROOM) {
            self.points.shrink_to(Ord::max(LIVE_ROOM, held * 2));
        }
        match self.points.front().map(|point| point.offset) {
            None => {
                self.base = at;
                self.capacities.clear();
                if self.points.capacity() == 0 {
                    self.points.reserve_exact(LIVE_ROOM);
                    self.capacities.reserve_exact(1);
                }
            }
            Some(first) => {
                while self
                    .capacities
                    .get(1)
                    .is_some_and(|next| next.from <= first)
                {
                    self.capacities.pop_front();
                }
                if u32::try_from(at - self.base).is_err() {
                    // After 49 days of one connection: count from the oldest
                    // point, which is within the window.
                    for point in &mut self.points {
                        point.offset -= first;
                    }
                    for capacities in &mut self.capacities {
                        capacities.from = capacities.from.saturating_sub(first);
                    }
                    self.base += i64::from(first);
                }
            }
        }
        let offset = u32::try_from(at - self.base).unwrap_or(u32::MAX);
        let (storage_used, storage_total) = metrics.storage();
        if !self.capacities.back().is_some_and(|last| {
            (last.memory_total_bytes, last.storage_total_bytes)
                == (metrics.memory_total_bytes, storage_total)
        }) {
            self.capacities.push_back(Capacities {
                from: offset,
                memory_total_bytes: metrics.memory_total_bytes,
                storage_total_bytes: storage_total,
            });
        }
        self.points.push_back(LivePoint {
            offset,
            cpu_percent: metrics.cpu_percent,
            memory_used_bytes: metrics.memory_used_bytes,
            network_received_bytes_per_second: metrics.network_received_bytes_per_second,
            network_sent_bytes_per_second: metrics.network_sent_bytes_per_second,
            storage_used_bytes: storage_used,
        });
    }

    /// The points taken after `after`, oldest first.
    fn points(&self, after: i64) -> Vec<Point> {
        let mut changes = self.capacities.iter().peekable();
        let mut current = changes.next().copied();
        let mut points = Vec::with_capacity(self.points.len());
        for point in &self.points {
            while let Some(next) = changes.next_if(|next| next.from <= point.offset) {
                current = Some(*next);
            }
            let at = self.at(point);
            let Some(capacities) = current.filter(|_| at > after) else {
                continue;
            };
            points.push(Point {
                at,
                cpu_percent: f64::from(point.cpu_percent),
                cpu_percent_max: f64::from(point.cpu_percent),
                memory_used_bytes: signed(point.memory_used_bytes),
                memory_total_bytes: signed(capacities.memory_total_bytes),
                network_received_bytes_per_second: signed(point.network_received_bytes_per_second),
                network_sent_bytes_per_second: signed(point.network_sent_bytes_per_second),
                storage_used_bytes: signed(point.storage_used_bytes),
                storage_total_bytes: signed(capacities.storage_total_bytes),
            });
        }
        points
    }

    fn held(&self) -> Held {
        Held {
            points: self.points.len(),
            bytes: size_of::<Self>()
                + self.points.capacity() * size_of::<LivePoint>()
                + self.capacities.capacity() * size_of::<Capacities>(),
        }
    }
}

/// What the live view holds for a device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    pub points: usize,
    /// The bytes its window takes with its room for more.
    pub bytes: usize,
}

#[derive(Debug, Default)]
struct Device {
    latest: Option<Reading>,
    /// The volumes the Agent last sent on this connection: none until it
    /// sends a list, as for a computer whose disks can't be read.
    volumes: Vec<VolumeUsage>,
    live: LiveWindow,
    minute: Option<Totals>,
}

#[derive(Debug, Default)]
struct State {
    devices: HashMap<String, Device>,
    sequence: u64,
}

/// Finished minutes waiting for the writer, at most one per device and
/// minute: PostgreSQL refuses an upsert that names a row twice.
#[derive(Debug)]
struct Pending {
    /// By minute first, so the oldest are dropped first.
    minutes: BTreeMap<(i64, String), Totals>,
    limit: usize,
    /// Minutes dropped for room since the last stored batch.
    dropped: u64,
}

impl Pending {
    fn new(limit: usize) -> Self {
        Self {
            minutes: BTreeMap::new(),
            limit,
            dropped: 0,
        }
    }

    /// Queues a minute, merging it into one already waiting for the device,
    /// as the upsert merges it into a stored one.
    fn insert(&mut self, device_id: &str, minute: Totals) {
        match self.minutes.entry((minute.start, device_id.to_owned())) {
            Entry::Occupied(mut waiting) => {
                waiting.get_mut().add(&minute.point(), minute.covered_ms);
            }
            Entry::Vacant(slot) => {
                slot.insert(minute);
                self.trim();
            }
        }
    }

    fn trim(&mut self) {
        while self.minutes.len() > self.limit {
            self.minutes.pop_first();
            if self.dropped == 0 {
                tracing::warn!(
                    limit = self.limit,
                    "the database is not keeping up with resource usage history; dropping the oldest minutes"
                );
            }
            self.dropped += 1;
        }
    }

    fn take(&mut self) -> Vec<Queued> {
        std::mem::take(&mut self.minutes).into_iter().collect()
    }

    /// Queues again minutes a failed write took, under the ones queued since.
    fn restore(&mut self, minutes: Vec<Queued>) {
        let newer = std::mem::replace(&mut self.minutes, minutes.into_iter().collect());
        for ((_, device_id), minute) in newer {
            self.insert(&device_id, minute);
        }
        self.trim();
    }

    /// Dropped minutes since the last call, once the database caught up.
    fn caught_up(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }
}

/// What one flush stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flushed {
    /// Device minutes written.
    pub minutes: usize,
    pub statements: usize,
    /// Minutes the database refused on their own while others were stored.
    pub refused: usize,
}

#[derive(Debug, Default)]
struct Writer {
    /// Whether the last flush failed, so an outage is logged once.
    failing: bool,
    /// Statements tried for a single minute after a refused batch.
    tried_alone: u64,
}

#[derive(Debug, Clone)]
pub struct Metrics {
    database: Database,
    state: Arc<Mutex<State>>,
    pending: Arc<Mutex<Pending>>,
    /// Held while writing, so a device's history isn't deleted between a
    /// flush taking its minutes and storing them.
    writer: Arc<tokio::sync::Mutex<Writer>>,
    /// What the website's event sockets send.
    feed: Arc<feed::Feed>,
    /// The devices the event sockets watch.
    watched: Watched,
    /// Watched devices whose screen thumbnail was just replaced, for the
    /// event sockets watching them.
    thumbnails: tokio::sync::broadcast::Sender<Arc<str>>,
    /// How long an Agent keeps reporting at the watched interval after its
    /// device stops being watched.
    unwatched_linger: Duration,
}

impl Metrics {
    pub fn new(database: Database) -> Self {
        Self::with_limit(database, MAX_PENDING_MINUTES)
    }

    fn with_limit(database: Database, limit: usize) -> Self {
        Self {
            database,
            state: Arc::default(),
            pending: Arc::new(Mutex::new(Pending::new(limit))),
            writer: Arc::default(),
            feed: Arc::default(),
            watched: Watched::default(),
            thumbnails: tokio::sync::broadcast::Sender::new(feed::THUMBNAIL_BACKLOG),
            unwatched_linger: UNWATCHED_LINGER,
        }
    }

    /// Lingers `linger` instead of [`UNWATCHED_LINGER`] before slowing an
    /// Agent whose device stopped being watched; for tests.
    pub fn set_unwatched_linger(&mut self, linger: Duration) {
        self.unwatched_linger = linger;
    }

    /// Follows whether the device is watched, for its Agent connection to
    /// tell the Agent how often to report.
    pub fn reporting_interval(&self, device_id: &str) -> ReportingInterval {
        ReportingInterval::new(self.watched.listen(device_id), self.unwatched_linger)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        // As for `state`.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a report from the device's connected Agent, and queues the
    /// previous minute for storing when this one starts the next. A report
    /// without volumes has the device's last list.
    pub fn record(&self, device_id: &str, report: MetricsReport) {
        let finished = self.record_at(device_id, report.sanitized(), now_ms());
        if let Some(finished) = finished {
            self.pending().insert(device_id, finished);
        }
    }

    fn record_at(&self, device_id: &str, mut report: MetricsReport, at: i64) -> Option<Totals> {
        let mut state = self.state();
        state.sequence += 1;
        let sequence = state.sequence;
        if !state.devices.contains_key(device_id) {
            state
                .devices
                .insert(device_id.to_owned(), Device::default());
        }
        let device = state.devices.get_mut(device_id)?;
        // Kept even from a reading dropped for coming too soon, as the Agent
        // does not send them again until it reads them again.
        if let Some(volumes) = report.volumes.take() {
            device.volumes = volumes;
        }
        let previous = device.latest.as_ref().map(|latest| latest.at);
        if previous.is_some_and(|previous| at - previous < MIN_SPACING_MS) {
            return None;
        }
        let covered = covered_ms(previous, at);
        let metrics = report.with_volumes(device.volumes.clone());
        let point = Point::of(at, &metrics);
        device.live.push(at, &metrics);
        // The previous reading's copy of the id is reused.
        let device_id = device
            .latest
            .take()
            .map_or_else(|| device_id.to_owned(), |latest| latest.device_id);
        device.latest = Some(Reading {
            device_id,
            at,
            metrics,
            sequence,
        });
        let start = at - at.rem_euclid(MINUTE_MS);
        match &mut device.minute {
            Some(minute) if minute.start == start => {
                minute.add(&point, covered);
                None
            }
            minute => minute.replace(Totals::new(start, &point, covered)),
        }
    }

    /// The device's Agent is offline: its live readings are dropped and its
    /// unfinished minute is queued for storing.
    pub fn disconnected(&self, device_id: &str) {
        let device = self.state().devices.remove(device_id);
        if let Some(minute) = device.and_then(|device| device.minute) {
            self.pending().insert(device_id, minute);
        }
    }

    /// The device was deleted: its readings and history are dropped.
    pub async fn forget(&self, device_id: &str) -> db::Result<()> {
        self.state().devices.remove(device_id);
        let _writing = self.writer.lock().await;
        self.pending()
            .minutes
            .retain(|(_, waiting), _| waiting != device_id);
        self.database
            .execute(
                &Query::delete()
                    .from_table(DeviceMetrics::Table)
                    .and_where(Expr::col(DeviceMetrics::DeviceId).eq(device_id))
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    /// The latest reading of every device that reported after `sequence`,
    /// and the sequence to ask from next.
    pub fn since(&self, sequence: u64) -> (Vec<Reading>, u64) {
        let state = self.state();
        let readings = state
            .devices
            .values()
            .filter_map(|device| device.latest.as_ref())
            .filter(|reading| reading.sequence > sequence)
            .cloned()
            .collect();
        (readings, state.sequence)
    }

    /// The latest reading of every watched device that reported after
    /// `sequence`, and the sequence to ask from next. It looks through the
    /// watched devices or the online ones, whichever are fewer.
    fn watched_since(&self, sequence: u64) -> (Vec<Reading>, u64) {
        let state = self.state();
        let watched = self.watched.lock();
        let newer = |reading: &&Reading| reading.sequence > sequence;
        let readings = if watched::watched_count(&watched) < state.devices.len() {
            watched::watched_ids(&watched)
                .filter_map(|id| state.devices.get(id)?.latest.as_ref())
                .filter(newer)
                .cloned()
                .collect()
        } else {
            state
                .devices
                .iter()
                .filter(|(id, _)| watched::is_watched(&watched, id))
                .filter_map(|(_, device)| device.latest.as_ref())
                .filter(newer)
                .cloned()
                .collect()
        };
        (readings, state.sequence)
    }

    /// The latest reading of each of the devices that has one.
    fn latest_readings<'a>(&self, device_ids: impl IntoIterator<Item = &'a str>) -> Vec<Reading> {
        let state = self.state();
        device_ids
            .into_iter()
            .filter_map(|id| state.devices.get(id)?.latest.clone())
            .collect()
    }

    /// The sequence to ask [`Metrics::since`] from to get only readings
    /// recorded after now.
    pub fn sequence(&self) -> u64 {
        self.state().sequence
    }

    /// What the live view holds for the device, while its Agent is online.
    pub fn held(&self, device_id: &str) -> Option<Held> {
        self.state()
            .devices
            .get(device_id)
            .map(|device| device.live.held())
    }

    /// The device's latest reading, while its Agent is online.
    pub fn latest(&self, device_id: &str) -> Option<Reading> {
        self.state()
            .devices
            .get(device_id)
            .and_then(|device| device.latest.clone())
    }

    /// The device's usage over `range`, oldest first.
    pub async fn history(&self, device_id: &str, range: Range) -> db::Result<Vec<Point>> {
        let now = now_ms();
        if range == Range::Live {
            return Ok(self
                .state()
                .devices
                .get(device_id)
                .map(|device| device.live.points(now - LIVE_WINDOW_MS))
                .unwrap_or_default());
        }
        let step = range.step_ms();
        let from = now - range.span_ms();
        let rows: Vec<Row> = self
            .database
            .fetch_all(
                &Query::select()
                    .columns(ROW_COLUMNS[1..].iter().copied())
                    .from(DeviceMetrics::Table)
                    .and_where(Expr::col(DeviceMetrics::DeviceId).eq(device_id))
                    .and_where(Expr::col(DeviceMetrics::Minute).gte(from - from.rem_euclid(step)))
                    .order_by(DeviceMetrics::Minute, Order::Asc)
                    .to_owned(),
            )
            .await?;
        Ok(buckets(rows, step))
    }

    /// Starts the task that stores the finished minutes every
    /// [`FLUSH_INTERVAL`].
    pub fn spawn_writer(&self) -> HistoryWriter {
        let metrics = self.clone();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(FLUSH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // A failure is logged, and its minutes wait for the next.
                        let _ = metrics.flush().await;
                    }
                    _ = &mut stopped => break,
                }
            }
            let _ = metrics.flush().await;
        });
        HistoryWriter { stop, task }
    }

    /// Stores every finished minute waiting, in one transaction. The minutes
    /// of a failed flush wait for the next.
    pub async fn flush(&self) -> db::Result<Flushed> {
        let mut writer = self.writer.lock().await;
        let minutes = self.pending().take();
        if minutes.is_empty() {
            return Ok(Flushed::default());
        }
        match self.write(&mut writer, minutes).await {
            Ok(flushed) => {
                if std::mem::take(&mut writer.failing) {
                    tracing::info!("storing resource usage history again");
                }
                let dropped = self.pending().caught_up();
                if dropped > 0 {
                    tracing::warn!(
                        dropped,
                        "minutes of resource usage history were dropped while the database was behind"
                    );
                }
                Ok(flushed)
            }
            Err((error, unwritten)) => {
                self.pending().restore(unwritten);
                if std::mem::replace(&mut writer.failing, true) {
                    tracing::debug!(%error, "still could not store resource usage history");
                } else {
                    tracing::warn!(%error, "could not store resource usage history; retrying");
                }
                Err(error)
            }
        }
    }

    /// Writes the minutes, or fails with those not written.
    async fn write(
        &self,
        writer: &mut Writer,
        minutes: Vec<Queued>,
    ) -> Result<Flushed, (sqlx::Error, Vec<Queued>)> {
        match self.write_together(&minutes).await {
            Ok(statements) => {
                return Ok(Flushed {
                    minutes: minutes.len(),
                    statements,
                    refused: 0,
                });
            }
            Err(Failed::Statement(sqlx::Error::Database(_))) => {}
            Err(Failed::Statement(error) | Failed::Transaction(error)) => {
                return Err((error, minutes));
            }
        }
        // The database refused a statement, which rolled back the rest: store
        // each minute on its own, so a minute it refuses loses only itself.
        let mut flushed = Flushed::default();
        let mut refused = Vec::new();
        let mut refusal = None;
        let mut rest = minutes.into_iter();
        while let Some(minute) = rest.next() {
            writer.tried_alone += 1;
            match self
                .database
                .execute(&upsert(std::slice::from_ref(&minute)))
                .await
            {
                Ok(_) => {
                    flushed.minutes += 1;
                    flushed.statements += 1;
                }
                Err(error @ sqlx::Error::Database(_)) => {
                    refused.push(minute);
                    if flushed.minutes == 0 && refused.len() == REFUSALS_BEFORE_GIVING_UP {
                        refused.extend(rest);
                        return Err((refusal.unwrap_or(error), refused));
                    }
                    refusal.get_or_insert(error);
                }
                Err(error) => {
                    refused.push(minute);
                    refused.extend(rest);
                    return Err((error, refused));
                }
            }
        }
        match refusal {
            // Refusing every minute is about the database, not the minutes:
            // they wait, as a failed flush's do.
            Some(error) if flushed.minutes == 0 => Err((error, refused)),
            Some(error) => {
                tracing::warn!(
                    refused = refused.len(),
                    %error,
                    "the database refused minutes of resource usage history; they are dropped"
                );
                flushed.refused = refused.len();
                Ok(flushed)
            }
            None => Ok(flushed),
        }
    }

    /// Writes the minutes in one transaction, a chunk per statement, and
    /// returns how many statements that took.
    async fn write_together(&self, minutes: &[Queued]) -> Result<usize, Failed> {
        let mut transaction = self.database.begin().await.map_err(Failed::Transaction)?;
        let mut statements = 0;
        for chunk in minutes.chunks(ROWS_PER_STATEMENT) {
            transaction
                .execute(&upsert(chunk))
                .await
                .map_err(Failed::Statement)?;
            statements += 1;
        }
        transaction.commit().await.map_err(Failed::Transaction)?;
        Ok(statements)
    }
}

/// A finished minute waiting for the writer, by its start and device.
type Queued = ((i64, String), Totals);

enum Failed {
    /// An insert failed.
    Statement(sqlx::Error),
    /// Beginning or committing the transaction failed.
    Transaction(sqlx::Error),
}

/// The task storing finished minutes.
pub struct HistoryWriter {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl HistoryWriter {
    /// Stores the minutes waiting, then ends the task.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

/// Inserts the minutes, merging each into one already stored for its device
/// and minute, as when a reconnect within the minute stores it again, by the
/// time each covers.
fn upsert(minutes: &[Queued]) -> InsertStatement {
    let weighted = |column: &str| {
        Expr::cust(format!(
            "(device_metrics.{column} * device_metrics.covered_ms + excluded.{column} * excluded.covered_ms) \
             / (device_metrics.covered_ms + excluded.covered_ms)"
        ))
    };
    let mut statement = Query::insert()
        .into_table(DeviceMetrics::Table)
        .columns(ROW_COLUMNS)
        .to_owned();
    for ((start, device_id), minute) in minutes {
        let point = minute.point();
        statement.values_panic([
            device_id.as_str().into(),
            (*start).into(),
            minute.covered_ms.into(),
            point.cpu_percent.into(),
            point.cpu_percent_max.into(),
            point.memory_used_bytes.into(),
            point.memory_total_bytes.into(),
            point.network_received_bytes_per_second.into(),
            point.network_sent_bytes_per_second.into(),
            point.storage_used_bytes.into(),
            point.storage_total_bytes.into(),
        ]);
    }
    statement.on_conflict(
        OnConflict::columns([DeviceMetrics::DeviceId, DeviceMetrics::Minute])
            .values([
                (DeviceMetrics::CpuPercent, weighted("cpu_percent")),
                (
                    DeviceMetrics::MemoryUsedBytes,
                    weighted("memory_used_bytes"),
                ),
                (
                    DeviceMetrics::NetworkReceivedBytesPerSecond,
                    weighted("network_received_bytes_per_second"),
                ),
                (
                    DeviceMetrics::NetworkSentBytesPerSecond,
                    weighted("network_sent_bytes_per_second"),
                ),
                (
                    DeviceMetrics::CpuPercentMax,
                    Expr::cust(
                        "CASE WHEN excluded.cpu_percent_max > device_metrics.cpu_percent_max \
                         THEN excluded.cpu_percent_max ELSE device_metrics.cpu_percent_max END",
                    ),
                ),
                (
                    DeviceMetrics::CoveredMs,
                    Expr::cust("device_metrics.covered_ms + excluded.covered_ms"),
                ),
            ])
            .update_columns([
                DeviceMetrics::MemoryTotalBytes,
                DeviceMetrics::StorageUsedBytes,
                DeviceMetrics::StorageTotalBytes,
            ])
            .to_owned(),
    );
    statement
}

const ROW_COLUMNS: [DeviceMetrics; 11] = [
    DeviceMetrics::DeviceId,
    DeviceMetrics::Minute,
    DeviceMetrics::CoveredMs,
    DeviceMetrics::CpuPercent,
    DeviceMetrics::CpuPercentMax,
    DeviceMetrics::MemoryUsedBytes,
    DeviceMetrics::MemoryTotalBytes,
    DeviceMetrics::NetworkReceivedBytesPerSecond,
    DeviceMetrics::NetworkSentBytesPerSecond,
    DeviceMetrics::StorageUsedBytes,
    DeviceMetrics::StorageTotalBytes,
];

#[derive(Debug, sqlx::FromRow)]
struct Row {
    minute: i64,
    covered_ms: i64,
    cpu_percent: f64,
    cpu_percent_max: f64,
    memory_used_bytes: i64,
    memory_total_bytes: i64,
    network_received_bytes_per_second: i64,
    network_sent_bytes_per_second: i64,
    storage_used_bytes: i64,
    storage_total_bytes: i64,
}

/// Minute rows, in order, merged into points `step` long.
fn buckets(rows: Vec<Row>, step: i64) -> Vec<Point> {
    let mut points: Vec<Totals> = Vec::new();
    for row in rows {
        let point = Point {
            at: row.minute,
            cpu_percent: row.cpu_percent,
            cpu_percent_max: row.cpu_percent_max,
            memory_used_bytes: row.memory_used_bytes,
            memory_total_bytes: row.memory_total_bytes,
            network_received_bytes_per_second: row.network_received_bytes_per_second,
            network_sent_bytes_per_second: row.network_sent_bytes_per_second,
            storage_used_bytes: row.storage_used_bytes,
            storage_total_bytes: row.storage_total_bytes,
        };
        let start = row.minute - row.minute.rem_euclid(step);
        match points.last_mut() {
            Some(totals) if totals.start == start => totals.add(&point, row.covered_ms),
            _ => points.push(Totals::new(start, &point, row.covered_ms)),
        }
    }
    points.iter().map(Totals::point).collect()
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::VolumeUsage;

    use super::*;

    /// A report with its volumes.
    fn sample(cpu: f32, memory: u64) -> MetricsReport {
        MetricsReport::new(usage(cpu, memory), true)
    }

    fn usage(cpu: f32, memory: u64) -> SystemMetrics {
        SystemMetrics {
            cpu_percent: cpu,
            memory_used_bytes: memory,
            memory_total_bytes: 100,
            network_received_bytes_per_second: memory * 10,
            network_sent_bytes_per_second: 1,
            uptime_seconds: 60,
            volumes: vec![VolumeUsage {
                name: "C:".into(),
                total_bytes: 1000,
                free_bytes: 1000 - memory,
            }],
        }
    }

    async fn store() -> Metrics {
        Metrics::new(
            Database::connect("sqlite::memory:", 1)
                .await
                .expect("an in-memory database opens"),
        )
    }

    #[tokio::test]
    async fn readings_fill_the_live_window_and_finish_minutes() {
        let metrics = store().await;
        let minute = 1_000 * MINUTE_MS;
        assert!(metrics.record_at("a", sample(10.0, 10), minute).is_none());
        // Too soon after the last reading.
        assert!(
            metrics
                .record_at("a", sample(99.0, 99), minute + SECOND_MS)
                .is_none()
        );
        assert!(
            metrics
                .record_at("a", sample(30.0, 30), minute + 5 * SECOND_MS)
                .is_none()
        );
        let finished = metrics
            .record_at("a", sample(50.0, 50), minute + MINUTE_MS)
            .expect("the next minute finishes the first");
        assert_eq!(finished.start, minute);
        // The connection's first reading, and the five seconds to the next.
        assert_eq!(finished.covered_ms, FIRST_READING_COVERS_MS + 5 * SECOND_MS);
        let point = finished.point();
        assert_eq!(point.at, minute);
        assert_eq!(point.cpu_percent, 20.0);
        assert_eq!(point.cpu_percent_max, 30.0);
        assert_eq!(point.memory_used_bytes, 20);
        assert_eq!(point.network_received_bytes_per_second, 200);
        assert_eq!(point.storage_used_bytes, 30, "the latest storage");
        assert_eq!(point.storage_total_bytes, 1000);

        let state = metrics.state();
        let device = &state.devices["a"];
        assert_eq!(device.live.points.len(), 3);
        assert_eq!(device.latest.as_ref().unwrap().metrics.cpu_percent, 50.0);
    }

    #[tokio::test]
    async fn the_live_window_drops_old_readings() {
        let metrics = store().await;
        for second in (0..=LIVE_WINDOW_MS / SECOND_MS + 60).step_by(5) {
            metrics.record_at("a", sample(1.0, 1), second * SECOND_MS);
        }
        let state = metrics.state();
        let points = state.devices["a"].live.points(i64::MIN);
        assert_eq!(
            points.len() as i64,
            LIVE_WINDOW_MS / (5 * SECOND_MS),
            "readings older than the window are gone"
        );
        assert!(points.last().unwrap().at - points[0].at < LIVE_WINDOW_MS);
    }

    /// A small deterministic generator, so failures repeat.
    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    fn random_reading(random: &mut Random) -> SystemMetrics {
        let mut volumes = vec![VolumeUsage {
            name: "C:".into(),
            total_bytes: 512 << 30,
            free_bytes: random.below(512 << 30),
        }];
        // Now and then a drive comes or goes, or memory changes.
        if random.below(20) == 0 {
            volumes.push(VolumeUsage {
                name: "D:".into(),
                total_bytes: random.next(),
                free_bytes: random.next(),
            });
        }
        SystemMetrics {
            cpu_percent: f32::from_bits(random.next() as u32 & 0x3fff_ffff).min(100.0),
            memory_used_bytes: random.next() >> random.below(64),
            memory_total_bytes: if random.below(30) == 0 {
                random.next()
            } else {
                16 << 30
            },
            network_received_bytes_per_second: random.next() >> random.below(64),
            network_sent_bytes_per_second: u64::MAX - random.below(3),
            uptime_seconds: 60,
            volumes,
        }
    }

    #[tokio::test]
    async fn the_live_window_gives_back_the_points_of_its_readings() {
        let metrics = store().await;
        let mut random = Random(7);
        let mut at = 1_000 * MINUTE_MS;
        let mut expected = VecDeque::new();
        for _ in 0..2_000 {
            at += [2, 5, 5, 5, 6, 20, 400, 1_000][random.below(8) as usize] * SECOND_MS;
            let reading = random_reading(&mut random);
            expected.push_back(Point::of(at, &reading));
            while expected
                .front()
                .is_some_and(|point: &Point| point.at <= at - LIVE_WINDOW_MS)
            {
                expected.pop_front();
            }
            metrics.record_at("a", MetricsReport::new(reading, true), at);
            let state = metrics.state();
            let live = &state.devices["a"].live;
            assert_eq!(live.points(i64::MIN), Vec::from(expected.clone()));
            let after = at - random.below(LIVE_WINDOW_MS as u64) as i64;
            assert_eq!(
                live.points(after),
                expected
                    .iter()
                    .filter(|point| point.at > after)
                    .copied()
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn the_live_window_outlasts_its_offsets() {
        let mut live = LiveWindow::default();
        let mut expected = VecDeque::new();
        let start = 1_000 * MINUTE_MS;
        let mut at = start;
        // Past the 49 days a `u32` of milliseconds counts.
        while at < start + i64::from(u32::MAX) + 2 * LIVE_WINDOW_MS {
            let reading = usage((at / SECOND_MS % 100) as f32, (at / SECOND_MS) as u64 % 997);
            expected.push_back(Point::of(at, &reading));
            if expected.len() > 30 {
                expected.pop_front();
            }
            live.push(at, &reading);
            at += 30 * SECOND_MS;
        }
        assert!(live.base > start, "offsets count from a later time");
        assert_eq!(live.points(i64::MIN), Vec::from(expected));
    }

    #[tokio::test]
    async fn a_live_window_holds_its_points_in_40_bytes_each() {
        assert_eq!(size_of::<LivePoint>(), 40);
        let metrics = store().await;
        // Unwatched, a reading a minute.
        let mut at = 0;
        let report = |at: &mut i64, every: i64, for_ms: i64| {
            let end = *at + for_ms;
            while *at < end {
                metrics.record_at("a", sample(1.0, 1), *at);
                *at += every;
            }
        };
        report(&mut at, IDLE_INTERVAL_MS, 2 * LIVE_WINDOW_MS);
        let held = metrics.held("a").unwrap();
        assert_eq!(held.points, 15);
        // The room for 19 points and one set of capacities, which it never
        // outgrew.
        assert_eq!(LIVE_ROOM, 19);
        let idle = size_of::<LiveWindow>() + 19 * 40 + size_of::<Capacities>();
        assert_eq!((held.bytes, idle), (idle, 856));
        // Watched, it grows to hold a reading every five seconds.
        report(&mut at, WATCHED_INTERVAL_MS, LIVE_WINDOW_MS);
        let held = metrics.held("a").unwrap();
        assert_eq!(held.points, 180);
        assert!(held.bytes >= idle + (180 - 19) * 40, "{held:?}");
        // Unwatched again, it gives most of that back once they are gone.
        report(&mut at, IDLE_INTERVAL_MS, 2 * LIVE_WINDOW_MS);
        let held = metrics.held("a").unwrap();
        assert_eq!(held.points, 15);
        assert!(held.bytes <= idle + LIVE_ROOM * 40, "{held:?}");
        metrics.disconnected("a");
        assert_eq!(metrics.held("a"), None);
    }

    #[tokio::test]
    async fn readings_without_volumes_have_the_devices_last_list() {
        let metrics = store().await;
        let without = |cpu: f32| MetricsReport::new(usage(cpu, 10), false);
        // Before any list: none, as for a computer whose disks can't be read.
        metrics.record_at("a", without(1.0), 0);
        let latest = metrics.latest("a").unwrap();
        assert!(latest.metrics.volumes.is_empty());
        assert_eq!(latest.metrics.storage(), (0, 0));
        metrics.record_at("a", sample(2.0, 10), 5 * SECOND_MS);
        let listed = metrics.latest("a").unwrap().metrics.volumes;
        assert_eq!(listed.len(), 1);
        metrics.record_at("a", without(3.0), 10 * SECOND_MS);
        let latest = metrics.latest("a").unwrap();
        assert_eq!(
            (latest.metrics.cpu_percent, &latest.metrics.volumes),
            (3.0, &listed)
        );
        let point = metrics.state().devices["a"].live.points(i64::MIN)[2];
        assert_eq!(
            (point.storage_used_bytes, point.storage_total_bytes),
            (10, 1000)
        );
        // A list in a reading dropped for coming too soon is still taken.
        let mut changed = usage(4.0, 10);
        changed.volumes[0].name = "D:".into();
        metrics.record_at("a", MetricsReport::new(changed, true), 11 * SECOND_MS);
        metrics.record_at("a", without(5.0), 15 * SECOND_MS);
        let latest = metrics.latest("a").unwrap();
        assert_eq!(latest.metrics.cpu_percent, 5.0);
        assert_eq!(latest.metrics.volumes[0].name, "D:");
        // A new connection starts without a list.
        metrics.disconnected("a");
        metrics.record_at("a", without(6.0), 20 * SECOND_MS);
        assert!(metrics.latest("a").unwrap().metrics.volumes.is_empty());
    }

    #[test]
    fn readings_cover_the_time_since_the_previous_one_within_bounds() {
        assert_eq!(covered_ms(None, 1_000), FIRST_READING_COVERS_MS);
        assert_eq!(covered_ms(Some(0), 5 * SECOND_MS), 5 * SECOND_MS);
        assert_eq!(covered_ms(Some(0), MINUTE_MS), MINUTE_MS);
        assert_eq!(covered_ms(Some(0), 10 * MINUTE_MS), 2 * MINUTE_MS);
        assert_eq!(covered_ms(Some(5), 5), 1);
    }

    #[tokio::test]
    async fn a_minute_that_mixes_spacings_stores_the_time_weighted_average() {
        let metrics = store().await;
        let minute = 1_000 * MINUTE_MS;
        // Unwatched: a reading a minute.
        metrics.record_at("a", sample(0.0, 0), minute - 2 * MINUTE_MS + 10 * SECOND_MS);
        metrics.record_at("a", sample(0.0, 0), minute - MINUTE_MS + 10 * SECOND_MS);
        // This minute's first reading covers the minute before it, at 80%;
        // then someone opens the device's page: nine readings five seconds
        // apart, at 20%.
        metrics.record_at("a", sample(80.0, 80), minute + 10 * SECOND_MS);
        for n in 1..=9 {
            metrics.record_at("a", sample(20.0, 20), minute + (10 + 5 * n) * SECOND_MS);
        }
        let finished = metrics
            .record_at("a", sample(20.0, 20), minute + MINUTE_MS)
            .unwrap();
        assert_eq!(finished.covered_ms, (60 + 45) * SECOND_MS);
        let point = finished.point();
        // Not (80 + 9 × 20) / 10 = 26, as counting readings would have it.
        let weighted = (80.0 * 60.0 + 20.0 * 45.0) / 105.0;
        assert!((point.cpu_percent - weighted).abs() < 1e-9, "{point:?}");
        assert_eq!(point.memory_used_bytes, weighted.round() as i64);
        assert_eq!(point.cpu_percent_max, 80.0);
    }

    #[tokio::test]
    async fn a_watched_minute_counts_as_much_as_an_unwatched_one() {
        let metrics = store().await;
        let start = 1_000 * MINUTE_MS;
        let mut minutes = Vec::new();
        let mut record = |at: i64, cpu: f32| {
            minutes.extend(metrics.record_at("a", sample(cpu, 10), at));
        };
        // Nine minutes unwatched at 10%, one a minute.
        record(start - 30 * SECOND_MS, 10.0);
        for n in 0..9 {
            record(start + n * MINUTE_MS + 30 * SECOND_MS, 10.0);
        }
        // The tenth watched at 90%: twelve readings.
        for n in 0..12 {
            record(start + 9 * MINUTE_MS + (2 + 5 * n) * SECOND_MS, 90.0);
        }
        record(start + 10 * MINUTE_MS + 30 * SECOND_MS, 0.0);
        let rows = minutes
            .iter()
            .filter(|minute| minute.start >= start)
            .map(|minute| {
                let point = minute.point();
                Row {
                    minute: minute.start,
                    covered_ms: minute.covered_ms,
                    cpu_percent: point.cpu_percent,
                    cpu_percent_max: point.cpu_percent_max,
                    memory_used_bytes: point.memory_used_bytes,
                    memory_total_bytes: point.memory_total_bytes,
                    network_received_bytes_per_second: point.network_received_bytes_per_second,
                    network_sent_bytes_per_second: point.network_sent_bytes_per_second,
                    storage_used_bytes: point.storage_used_bytes,
                    storage_total_bytes: point.storage_total_bytes,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 10);
        // Each minute covers a minute, watched or not; the tenth's first
        // reading covers the 32 seconds since the ninth minute's.
        assert!(rows[..9].iter().all(|row| row.covered_ms == MINUTE_MS));
        assert_eq!(rows[9].covered_ms, (32 + 11 * 5) * SECOND_MS);
        let points = buckets(rows, 30 * MINUTE_MS);
        assert_eq!(points.len(), 1);
        let weighted = (10.0 * 9.0 * 60.0 + 90.0 * 87.0) / (9.0 * 60.0 + 87.0);
        // Counting readings would make it (9 × 10 + 12 × 90) / 21 = 55.7.
        assert!(
            (points[0].cpu_percent - weighted).abs() < 1e-9,
            "{:?}",
            points[0]
        );
        assert!(points[0].cpu_percent < 22.0);
    }

    #[tokio::test]
    async fn since_returns_each_new_reading_once() {
        let metrics = store().await;
        metrics.record_at("a", sample(1.0, 1), 0);
        metrics.record_at("b", sample(2.0, 2), 0);
        let (readings, sequence) = metrics.since(0);
        assert_eq!(readings.len(), 2);
        let (readings, unchanged) = metrics.since(sequence);
        assert!(readings.is_empty());
        assert_eq!(unchanged, sequence);
        metrics.record_at("b", sample(3.0, 3), 10 * SECOND_MS);
        let (readings, _) = metrics.since(sequence);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].device_id, "b");
    }

    #[tokio::test]
    async fn the_scan_returns_each_new_reading_of_a_watched_device_once() {
        let metrics = store().await;
        let mut socket = watched::WatchSet::new(metrics.watched.clone());
        socket.replace(["a", "b"].map(String::from).into());
        metrics.record_at("a", sample(1.0, 1), 0);
        metrics.record_at("b", sample(2.0, 2), 0);
        metrics.record_at("unwatched", sample(2.0, 2), 0);
        let (readings, sequence) = metrics.watched_since(0);
        assert_eq!(readings.len(), 2);
        assert!(
            readings
                .iter()
                .all(|reading| reading.device_id != "unwatched")
        );
        let (readings, unchanged) = metrics.watched_since(sequence);
        assert!(readings.is_empty());
        assert_eq!(unchanged, sequence);
        // Fewer watched devices than online ones, and more, find the same.
        for extra in 0..10 {
            metrics.record_at(&format!("x{extra}"), sample(1.0, 1), 0);
        }
        let many = (0..20)
            .map(|n| format!("w{n}"))
            .chain(["a".into(), "b".into()]);
        let mut wide = watched::WatchSet::new(metrics.watched.clone());
        assert_eq!(metrics.watched_since(0).0.len(), 2);
        wide.replace(many.collect());
        assert_eq!(metrics.watched_since(0).0.len(), 2);
        metrics.record_at("b", sample(3.0, 3), 10 * SECOND_MS);
        let (readings, _) = metrics.watched_since(sequence);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].device_id, "b");
        let json = serde_json::to_value(&readings[0]).unwrap();
        assert_eq!(json["cpu_percent"], 3.0);
        assert_eq!(json["at"], 10 * SECOND_MS);
        assert!(json.get("sequence").is_none());
    }

    async fn stored_coverage(metrics: &Metrics) -> Vec<i64> {
        let rows: Vec<(i64,)> = metrics
            .database
            .fetch_all(
                &Query::select()
                    .column(DeviceMetrics::CoveredMs)
                    .from(DeviceMetrics::Table)
                    .to_owned(),
            )
            .await
            .unwrap();
        rows.into_iter().map(|(covered,)| covered).collect()
    }

    fn minute(start: i64, cpu: f64) -> Totals {
        Totals::new(start * MINUTE_MS, &Point::of(0, &usage(cpu as f32, 10)), 1)
    }

    #[test]
    fn waiting_minutes_merge_per_device_and_minute() {
        let mut pending = Pending::new(10);
        pending.insert("a", minute(1, 10.0));
        pending.insert("a", minute(1, 30.0));
        pending.insert("b", minute(1, 50.0));
        let waiting = pending.take();
        assert_eq!(waiting.len(), 2);
        let ((start, device_id), merged) = &waiting[0];
        assert_eq!((*start, device_id.as_str()), (MINUTE_MS, "a"));
        assert_eq!(merged.covered_ms, 2);
        assert_eq!(merged.point().cpu_percent, 20.0);
        assert_eq!(merged.point().cpu_percent_max, 30.0);
        assert!(pending.take().is_empty());
    }

    #[test]
    fn the_wait_is_bounded_by_dropping_the_oldest_minutes() {
        let mut pending = Pending::new(3);
        for start in 0..5 {
            pending.insert("a", minute(start, 1.0));
        }
        assert_eq!(pending.caught_up(), 2);
        assert_eq!(pending.caught_up(), 0, "counted once per incident");
        let starts = |pending: &mut Pending| -> Vec<i64> {
            let waiting = pending.take();
            let starts = waiting
                .iter()
                .map(|((start, _), _)| start / MINUTE_MS)
                .collect();
            pending.restore(waiting);
            starts
        };
        assert_eq!(starts(&mut pending), [2, 3, 4]);

        // A failed flush's minutes come back under the newer ones and the
        // bound, merging with any queued since.
        let failed = pending.take();
        pending.insert("a", minute(4, 1.0));
        pending.insert("a", minute(5, 1.0));
        pending.restore(failed);
        assert_eq!(pending.caught_up(), 1);
        let waiting = pending.take();
        assert_eq!(waiting.len(), 3);
        assert_eq!(waiting[0].0.0, 3 * MINUTE_MS);
        assert_eq!(waiting[1].1.covered_ms, 2, "minute 4 merged");
    }

    #[tokio::test]
    async fn a_failed_flush_keeps_its_minutes_for_the_next() {
        // No tables yet, so storing fails.
        let metrics = store().await;
        metrics.record_at("a", sample(10.0, 10), 1_000 * MINUTE_MS);
        metrics.disconnected("a");
        assert!(metrics.flush().await.is_err());
        assert!(metrics.writer.lock().await.failing);
        assert_eq!(metrics.pending().minutes.len(), 1);

        metrics.database.migrate().await.unwrap();
        let flushed = metrics.flush().await.unwrap();
        assert_eq!(
            flushed,
            Flushed {
                minutes: 1,
                statements: 1,
                refused: 0
            }
        );
        assert!(!metrics.writer.lock().await.failing);
        assert_eq!(stored_coverage(&metrics).await, [FIRST_READING_COVERS_MS]);
    }

    #[tokio::test]
    async fn a_database_refusing_everything_is_tried_alone_only_so_often() {
        // No tables yet, so every statement is refused.
        let metrics = store().await;
        let waiting = REFUSALS_BEFORE_GIVING_UP + 500;
        for device in 0..waiting {
            let device_id = format!("device-{device}");
            metrics.record_at(&device_id, sample(10.0, 10), 1_000 * MINUTE_MS);
            metrics.disconnected(&device_id);
        }
        assert!(metrics.flush().await.is_err());
        assert_eq!(
            metrics.writer.lock().await.tried_alone,
            REFUSALS_BEFORE_GIVING_UP as u64
        );
        assert_eq!(metrics.pending().minutes.len(), waiting, "all of them wait");

        metrics.database.migrate().await.unwrap();
        let flushed = metrics.flush().await.unwrap();
        assert_eq!((flushed.minutes, flushed.statements), (waiting, 1));
        assert_eq!(stored_coverage(&metrics).await.len(), waiting);
    }

    #[tokio::test]
    async fn the_writer_stores_what_waits_when_it_stops() {
        let metrics = store().await;
        metrics.database.migrate().await.unwrap();
        let writer = metrics.spawn_writer();
        metrics.record_at("a", sample(10.0, 10), 1_000 * MINUTE_MS);
        metrics.disconnected("a");
        writer.stop().await;
        assert!(metrics.pending().minutes.is_empty());
        assert_eq!(stored_coverage(&metrics).await, [FIRST_READING_COVERS_MS]);
    }

    #[test]
    fn rows_merge_into_weighted_buckets() {
        let row = |minute: i64, covered_ms: i64, cpu: f64| Row {
            minute: minute * MINUTE_MS,
            covered_ms,
            cpu_percent: cpu,
            cpu_percent_max: cpu + 5.0,
            memory_used_bytes: 10,
            memory_total_bytes: 100,
            network_received_bytes_per_second: 0,
            network_sent_bytes_per_second: 0,
            storage_used_bytes: minute,
            storage_total_bytes: 1000,
        };
        let points = buckets(
            vec![row(0, 12, 10.0), row(1, 4, 50.0), row(5, 12, 0.0)],
            5 * MINUTE_MS,
        );
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].at, 0);
        assert_eq!(points[0].cpu_percent, 20.0);
        assert_eq!(points[0].cpu_percent_max, 55.0);
        assert_eq!(points[0].storage_used_bytes, 1);
        assert_eq!(points[1].at, 5 * MINUTE_MS);
    }

    #[test]
    fn ranges_have_a_few_hundred_points_at_most() {
        for range in [Range::Live, Range::Hour, Range::Day, Range::Week] {
            assert!(range.span_ms() / range.step_ms() <= 360, "{range:?}");
        }
    }
}
