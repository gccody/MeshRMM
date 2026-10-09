//! Devices' resource usage, as their Agents report it every few seconds on
//! the control connection.
//!
//! Each online device's latest reading and its readings from the last
//! [`LIVE_WINDOW_MS`] stay in memory for the website's live view. Each
//! minute's readings are averaged into one `device_metrics` row when the
//! next minute's first reading arrives or the Agent disconnects, and rows are
//! kept for [`HISTORY_MS`]. Readings are stamped with the server's clock, so
//! an Agent's wrong clock can't misplace them.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use meshrmm_protocol_types::SystemMetrics;
use sea_query::{Expr, ExprTrait, OnConflict, Order, Query};
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

#[derive(Debug, Clone)]
pub struct Metrics {
    database: Database,
    state: Arc<Mutex<State>>,
}

impl Metrics {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            state: Arc::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a reading from the device's connected Agent, and stores the
    /// previous minute when this one starts the next.
    pub async fn record(&self, device_id: &str, metrics: SystemMetrics) {
        let finished = self.record_at(device_id, metrics.sanitized(), now_ms());
        if let Some(finished) = finished {
            self.store(device_id, &finished).await;
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
    /// unfinished minute is stored.
    pub async fn disconnected(&self, device_id: &str) {
        let device = self.state().devices.remove(device_id);
        if let Some(minute) = device.and_then(|device| device.minute) {
            self.store(device_id, &minute).await;
        }
    }

    /// The device was deleted: its readings and history are dropped.
    pub async fn forget(&self, device_id: &str) -> db::Result<()> {
        self.state().devices.remove(device_id);
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

    async fn store(&self, device_id: &str, minute: &Totals) {
        let point = minute.point();
        // A reconnect within the minute stores it again; the two merge.
        let weighted = |column: &str| {
            Expr::cust(format!(
                "(device_metrics.{column} * device_metrics.samples + excluded.{column} * excluded.samples) \
                 / (device_metrics.samples + excluded.samples)"
            ))
        };
        let stored = self
            .database
            .execute(
                &Query::insert()
                    .into_table(DeviceMetrics::Table)
                    .columns(ROW_COLUMNS)
                    .values_panic([
                        device_id.into(),
                        minute.start.into(),
                        minute.samples.into(),
                        point.cpu_percent.into(),
                        point.cpu_percent_max.into(),
                        point.memory_used_bytes.into(),
                        point.memory_total_bytes.into(),
                        point.network_received_bytes_per_second.into(),
                        point.network_sent_bytes_per_second.into(),
                        point.storage_used_bytes.into(),
                        point.storage_total_bytes.into(),
                    ])
                    .on_conflict(
                        OnConflict::columns([DeviceMetrics::DeviceId, DeviceMetrics::Minute])
                            .values([
                                (DeviceMetrics::CpuPercent, weighted("cpu_percent")),
                                (DeviceMetrics::MemoryUsedBytes, weighted("memory_used_bytes")),
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
                    )
                    .to_owned(),
            )
            .await;
        if let Err(error) = stored {
            tracing::warn!(device_id, %error, "could not store a minute of the device's resource usage");
        }
    }
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
