//! Devices' resource usage, as their Agents report it every few seconds on
//! the control connection.
//!
//! Each online device's latest reading and its readings from the last
//! [`LIVE_WINDOW_MS`] stay in memory for the website's live view. Each
//! minute's readings are averaged into one `device_metrics` row when the
//! next minute's first reading arrives or the Agent disconnects, and rows are
//! kept for [`HISTORY_MS`]. Readings are stamped with the server's clock, so
//! an Agent's wrong clock can't misplace them.
//!
//! Finished minutes wait in memory and the writer task stores them every
//! [`FLUSH_INTERVAL`], all in one transaction, so a minute reaches the history
//! within that long. Writing each on its own would be one statement per
//! device per minute: 167 a second for 10,000 Agents.
use std::{
    collections::{BTreeMap, HashMap, VecDeque, btree_map::Entry},
    sync::{Arc, Mutex},
    time::Duration,
};

use meshrmm_protocol_types::SystemMetrics;
use sea_query::{Expr, ExprTrait, InsertStatement, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};

use crate::{
    db::{self, Database, tables::DeviceMetrics},
    time::{DAY_MS, HOUR_MS, MINUTE_MS, SECOND_MS, now_ms},
};

/// How far back the live view reaches.
pub const LIVE_WINDOW_MS: i64 = 15 * MINUTE_MS;
/// How long per-minute history is kept.
pub const HISTORY_MS: i64 = 7 * DAY_MS;
/// Readings closer together than this are dropped, which bounds what an
/// Agent that reports too often can make the server keep.
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

/// Points of one period, weighted by how many readings each stands for.
/// Loads and rates average; capacities and storage take the latest point.
#[derive(Debug, Clone, Copy)]
struct Totals {
    start: i64,
    samples: i64,
    cpu: f64,
    memory_used: f64,
    received: f64,
    sent: f64,
    last: Point,
}

impl Totals {
    fn new(start: i64, point: &Point, samples: i64) -> Self {
        let mut totals = Self {
            start,
            samples: 0,
            cpu: 0.0,
            memory_used: 0.0,
            received: 0.0,
            sent: 0.0,
            last: *point,
        };
        totals.add(point, samples);
        totals
    }

    fn add(&mut self, point: &Point, samples: i64) {
        let weight = samples as f64;
        self.samples += samples;
        self.cpu += point.cpu_percent * weight;
        self.memory_used += point.memory_used_bytes as f64 * weight;
        self.received += point.network_received_bytes_per_second as f64 * weight;
        self.sent += point.network_sent_bytes_per_second as f64 * weight;
        let busiest = self.last.cpu_percent_max.max(point.cpu_percent_max);
        self.last = *point;
        self.last.cpu_percent_max = busiest;
    }

    fn point(&self) -> Point {
        let samples = Ord::max(self.samples, 1) as f64;
        Point {
            at: self.start,
            cpu_percent: self.cpu / samples,
            memory_used_bytes: (self.memory_used / samples).round() as i64,
            network_received_bytes_per_second: (self.received / samples).round() as i64,
            network_sent_bytes_per_second: (self.sent / samples).round() as i64,
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

    /// How long each point covers: a few hundred points at most.
    pub fn step_ms(self) -> i64 {
        match self {
            Self::Live => {
                i64::try_from(meshrmm_protocol_types::METRICS_INTERVAL_SECONDS).unwrap_or(5)
                    * SECOND_MS
            }
            Self::Hour => MINUTE_MS,
            Self::Day => 5 * MINUTE_MS,
            Self::Week => 30 * MINUTE_MS,
        }
    }
}

#[derive(Debug, Default)]
struct Device {
    latest: Option<Reading>,
    recent: VecDeque<Point>,
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
                waiting.get_mut().add(&minute.point(), minute.samples);
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

    fn take(&mut self) -> Vec<((i64, String), Totals)> {
        std::mem::take(&mut self.minutes).into_iter().collect()
    }

    /// Queues again minutes a failed write took, under the ones queued since.
    fn restore(&mut self, minutes: Vec<((i64, String), Totals)>) {
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
}

#[derive(Debug, Clone)]
pub struct Metrics {
    database: Database,
    state: Arc<Mutex<State>>,
    pending: Arc<Mutex<Pending>>,
    /// Held while writing, so a device's history isn't deleted between a
    /// flush taking its minutes and storing them.
    writer: Arc<tokio::sync::Mutex<Writer>>,
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
        }
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

    /// Records a reading from the device's connected Agent, and queues the
    /// previous minute for storing when this one starts the next.
    pub fn record(&self, device_id: &str, metrics: SystemMetrics) {
        let finished = self.record_at(device_id, metrics.sanitized(), now_ms());
        if let Some(finished) = finished {
            self.pending().insert(device_id, finished);
        }
    }

    fn record_at(&self, device_id: &str, metrics: SystemMetrics, at: i64) -> Option<Totals> {
        let mut state = self.state();
        state.sequence += 1;
        let sequence = state.sequence;
        let device = state.devices.entry(device_id.to_owned()).or_default();
        if device
            .latest
            .as_ref()
            .is_some_and(|latest| at - latest.at < MIN_SPACING_MS)
        {
            return None;
        }
        let point = Point::of(at, &metrics);
        device.latest = Some(Reading {
            device_id: device_id.to_owned(),
            at,
            metrics,
            sequence,
        });
        device.recent.push_back(point);
        while device
            .recent
            .front()
            .is_some_and(|oldest| oldest.at <= at - LIVE_WINDOW_MS)
        {
            device.recent.pop_front();
        }
        let start = at - at.rem_euclid(MINUTE_MS);
        match &mut device.minute {
            Some(minute) if minute.start == start => {
                minute.add(&point, 1);
                None
            }
            minute => minute.replace(Totals::new(start, &point, 1)),
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
                .map(|device| {
                    device
                        .recent
                        .iter()
                        .filter(|point| point.at > now - LIVE_WINDOW_MS)
                        .copied()
                        .collect()
                })
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
        match self.write(minutes).await {
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
    async fn write(&self, minutes: Vec<Queued>) -> Result<Flushed, (sqlx::Error, Vec<Queued>)> {
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
            // Refusing every minute is about the database, not the minutes.
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
/// and minute, as when a reconnect within the minute stores it again.
fn upsert(minutes: &[Queued]) -> InsertStatement {
    let weighted = |column: &str| {
        Expr::cust(format!(
            "(device_metrics.{column} * device_metrics.samples + excluded.{column} * excluded.samples) \
             / (device_metrics.samples + excluded.samples)"
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
            minute.samples.into(),
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
                    DeviceMetrics::Samples,
                    Expr::cust("device_metrics.samples + excluded.samples"),
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
    DeviceMetrics::Samples,
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
    samples: i64,
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
            Some(totals) if totals.start == start => totals.add(&point, row.samples),
            _ => points.push(Totals::new(start, &point, row.samples)),
        }
    }
    points.iter().map(Totals::point).collect()
}

/// What the website's event socket sends about devices' resource usage.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MetricsEvent {
    Metrics { readings: Vec<Reading> },
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol_types::VolumeUsage;

    use super::*;

    fn sample(cpu: f32, memory: u64) -> SystemMetrics {
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
        assert_eq!(finished.samples, 2);
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
        assert_eq!(device.recent.len(), 3);
        assert_eq!(device.latest.as_ref().unwrap().metrics.cpu_percent, 50.0);
    }

    #[tokio::test]
    async fn the_live_window_drops_old_readings() {
        let metrics = store().await;
        for second in (0..=LIVE_WINDOW_MS / SECOND_MS + 60).step_by(5) {
            metrics.record_at("a", sample(1.0, 1), second * SECOND_MS);
        }
        let state = metrics.state();
        let recent = &state.devices["a"].recent;
        assert_eq!(
            recent.len() as i64,
            LIVE_WINDOW_MS / (5 * SECOND_MS),
            "readings older than the window are gone"
        );
        assert!(recent.back().unwrap().at - recent.front().unwrap().at < LIVE_WINDOW_MS);
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
        let json = serde_json::to_value(&readings[0]).unwrap();
        assert_eq!(json["cpu_percent"], 3.0);
        assert_eq!(json["at"], 10 * SECOND_MS);
        assert!(json.get("sequence").is_none());
    }

    async fn stored_samples(metrics: &Metrics) -> Vec<i64> {
        let rows: Vec<(i64,)> = metrics
            .database
            .fetch_all(
                &Query::select()
                    .column(DeviceMetrics::Samples)
                    .from(DeviceMetrics::Table)
                    .to_owned(),
            )
            .await
            .unwrap();
        rows.into_iter().map(|(samples,)| samples).collect()
    }

    fn minute(start: i64, cpu: f64) -> Totals {
        Totals::new(start * MINUTE_MS, &Point::of(0, &sample(cpu as f32, 10)), 1)
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
        assert_eq!(merged.samples, 2);
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
        assert_eq!(waiting[1].1.samples, 2, "minute 4 merged");
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
        assert_eq!(stored_samples(&metrics).await, [1]);
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
        assert_eq!(stored_samples(&metrics).await, [1]);
    }

    #[test]
    fn rows_merge_into_weighted_buckets() {
        let row = |minute: i64, samples: i64, cpu: f64| Row {
            minute: minute * MINUTE_MS,
            samples,
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
