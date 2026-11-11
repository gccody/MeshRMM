//! The system resource usage an Agent reports on its control connection.
//!
//! The server sets how often each Agent reports
//! ([`crate::AgentCommand::SetMetricsInterval`]): every
//! [`METRICS_WATCHED_INTERVAL_SECONDS`] while a website tab shows the
//! device's live readings, and every [`METRICS_IDLE_INTERVAL_SECONDS`]
//! otherwise, which is also what an Agent reports at on each connection
//! until the server says.
use serde::{Deserialize, Serialize};

/// How often an Agent reports while someone watches the device's readings.
/// It is also the shortest interval an Agent accepts.
pub const METRICS_WATCHED_INTERVAL_SECONDS: u64 = 5;
/// How often an Agent reports while nobody watches the device's readings,
/// and on each connection until the server sets an interval.
pub const METRICS_IDLE_INTERVAL_SECONDS: u64 = 60;
/// The most volumes one report lists; an Agent reports its largest.
pub const MAX_METRIC_VOLUMES: usize = 16;
/// The longest volume name, in characters, a report carries.
pub const MAX_VOLUME_NAME_CHARS: usize = 64;

/// One sample of a computer's resource usage, as the server keeps and shows
/// it: with the device's volumes. Rates and the CPU load are averages over
/// the time since the Agent's previous sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// Busy time of all logical processors together, from 0 to 100.
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    /// Bytes per second through the physical network adapters.
    pub network_received_bytes_per_second: u64,
    pub network_sent_bytes_per_second: u64,
    pub uptime_seconds: u64,
    /// Local fixed volumes, at most [`MAX_METRIC_VOLUMES`].
    #[serde(default)]
    pub volumes: Vec<VolumeUsage>,
}

/// One sample as an Agent sends it ([`crate::AgentStatusMessage::Metrics`]):
/// a [`SystemMetrics`] whose volumes travel only in the first report on each
/// connection and in the first report after the Agent read them again
/// (about once a minute). A report without them means they are unchanged:
/// the server keeps the device's last list and shows it with every reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsReport {
    pub cpu_percent: f32,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub network_received_bytes_per_second: u64,
    pub network_sent_bytes_per_second: u64,
    pub uptime_seconds: u64,
    /// Local fixed volumes, at most [`MAX_METRIC_VOLUMES`], or `None` for
    /// the list last sent on this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<VolumeUsage>>,
}

/// A local volume's capacity, as a drive letter (`C:`) or mount point (`/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsage {
    pub name: String,
    pub total_bytes: u64,
    /// Space available to programs, not counting what only the system may use.
    pub free_bytes: u64,
}

/// A CPU load a peer sent, brought into range.
fn sane_percent(percent: f32) -> f32 {
    if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

/// A volume list a peer sent, without empty volumes or control characters,
/// and bounded.
pub fn sanitized_volumes(mut volumes: Vec<VolumeUsage>) -> Vec<VolumeUsage> {
    volumes.retain(|volume| volume.total_bytes > 0);
    volumes.truncate(MAX_METRIC_VOLUMES);
    for volume in &mut volumes {
        volume.free_bytes = volume.free_bytes.min(volume.total_bytes);
        volume.name = volume
            .name
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_VOLUME_NAME_CHARS)
            .collect();
    }
    volumes
}

impl MetricsReport {
    /// The report of `metrics`, with its volumes or without them.
    pub fn new(metrics: SystemMetrics, with_volumes: bool) -> Self {
        Self {
            cpu_percent: metrics.cpu_percent,
            memory_used_bytes: metrics.memory_used_bytes,
            memory_total_bytes: metrics.memory_total_bytes,
            network_received_bytes_per_second: metrics.network_received_bytes_per_second,
            network_sent_bytes_per_second: metrics.network_sent_bytes_per_second,
            uptime_seconds: metrics.uptime_seconds,
            volumes: with_volumes.then_some(metrics.volumes),
        }
    }

    /// The report with impossible values brought into range and any volume
    /// list bounded, as [`SystemMetrics::sanitized`].
    pub fn sanitized(mut self) -> Self {
        self.cpu_percent = sane_percent(self.cpu_percent);
        self.memory_used_bytes = self.memory_used_bytes.min(self.memory_total_bytes);
        self.volumes = self.volumes.map(sanitized_volumes);
        self
    }

    /// The sample with `volumes`, the device's list, in place of any it
    /// carries.
    pub fn with_volumes(self, volumes: Vec<VolumeUsage>) -> SystemMetrics {
        SystemMetrics {
            cpu_percent: self.cpu_percent,
            memory_used_bytes: self.memory_used_bytes,
            memory_total_bytes: self.memory_total_bytes,
            network_received_bytes_per_second: self.network_received_bytes_per_second,
            network_sent_bytes_per_second: self.network_sent_bytes_per_second,
            uptime_seconds: self.uptime_seconds,
            volumes,
        }
    }
}

impl SystemMetrics {
    /// The sample with impossible values brought into range and its volume
    /// list bounded, so whatever a peer sent is safe to store and show.
    pub fn sanitized(mut self) -> Self {
        self.cpu_percent = sane_percent(self.cpu_percent);
        self.memory_used_bytes = self.memory_used_bytes.min(self.memory_total_bytes);
        self.volumes = sanitized_volumes(self.volumes);
        self
    }

    /// Used space across every reported volume, and their total capacity.
    pub fn storage(&self) -> (u64, u64) {
        self.volumes.iter().fold((0, 0), |(used, total), volume| {
            (
                used.saturating_add(volume.total_bytes - volume.free_bytes.min(volume.total_bytes)),
                total.saturating_add(volume.total_bytes),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SystemMetrics {
        SystemMetrics {
            cpu_percent: 12.5,
            memory_used_bytes: 6,
            memory_total_bytes: 16,
            network_received_bytes_per_second: 1_000,
            network_sent_bytes_per_second: 200,
            uptime_seconds: 3_600,
            volumes: vec![VolumeUsage {
                name: "C:".into(),
                total_bytes: 100,
                free_bytes: 40,
            }],
        }
    }

    #[test]
    fn samples_use_plain_json() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "cpu_percent": 12.5,
                "memory_used_bytes": 6,
                "memory_total_bytes": 16,
                "network_received_bytes_per_second": 1000,
                "network_sent_bytes_per_second": 200,
                "uptime_seconds": 3600,
                "volumes": [{ "name": "C:", "total_bytes": 100, "free_bytes": 40 }]
            })
        );
        assert_eq!(
            serde_json::from_value::<SystemMetrics>(json).unwrap(),
            sample()
        );
    }

    #[test]
    fn reports_leave_out_volumes_they_do_not_carry() {
        let without = MetricsReport::new(sample(), false);
        let json = serde_json::to_value(&without).unwrap();
        assert!(json.get("volumes").is_none(), "{json}");
        assert_eq!(
            serde_json::from_value::<MetricsReport>(json).unwrap(),
            without
        );
        let with = MetricsReport::new(sample(), true);
        assert_eq!(with.volumes.as_deref(), Some(&sample().volumes[..]));
        assert_eq!(with.with_volumes(Vec::new()).volumes, vec![]);
        assert_eq!(without.with_volumes(sample().volumes), sample());
    }

    #[test]
    fn leaving_out_three_volumes_halves_a_report() {
        let volume = |name: &str| VolumeUsage {
            name: name.into(),
            total_bytes: 1_000_204_886_016,
            free_bytes: 412_316_860_416,
        };
        let typical = SystemMetrics {
            cpu_percent: 12.345_678,
            memory_used_bytes: 9_663_676_416,
            memory_total_bytes: 17_179_869_184,
            network_received_bytes_per_second: 1_234_567,
            network_sent_bytes_per_second: 98_765,
            uptime_seconds: 1_234_567,
            volumes: vec![volume("C:"), volume("D:"), volume("E:")],
        };
        let size = |with_volumes: bool| {
            serde_json::to_string(&crate::AgentStatusMessage::Metrics {
                metrics: MetricsReport::new(typical.clone(), with_volumes),
            })
            .unwrap()
            .len()
        };
        assert_eq!((size(false), size(true)), (225, 441));
    }

    #[test]
    fn sanitizing_a_report_bounds_its_values_and_any_volumes() {
        let mut wild = MetricsReport::new(sample(), true);
        wild.cpu_percent = f32::INFINITY;
        wild.memory_used_bytes = 99;
        wild.volumes = Some(vec![VolumeUsage {
            name: "D:".into(),
            total_bytes: 0,
            free_bytes: 0,
        }]);
        let clean = wild.sanitized();
        assert_eq!(clean.cpu_percent, 0.0);
        assert_eq!(clean.memory_used_bytes, 16);
        assert_eq!(clean.volumes, Some(vec![]));
        let mut none = MetricsReport::new(sample(), false);
        none.cpu_percent = -3.0;
        let clean = none.sanitized();
        assert_eq!((clean.cpu_percent, clean.volumes), (0.0, None));
    }

    #[test]
    fn sanitizing_bounds_every_value() {
        let mut wild = sample();
        wild.cpu_percent = f32::NAN;
        wild.memory_used_bytes = 99;
        wild.volumes = (0..40)
            .map(|index| VolumeUsage {
                name: format!("{}\n{}", "x".repeat(100), index),
                total_bytes: index,
                free_bytes: index * 2,
            })
            .collect();
        let clean = wild.sanitized();
        assert_eq!(clean.cpu_percent, 0.0);
        assert_eq!(clean.memory_used_bytes, 16);
        assert_eq!(clean.volumes.len(), MAX_METRIC_VOLUMES);
        assert!(clean.volumes.iter().all(|volume| volume.total_bytes > 0
            && volume.free_bytes == volume.total_bytes
            && volume.name.chars().count() == MAX_VOLUME_NAME_CHARS
            && !volume.name.contains('\n')));
        let mut hot = sample();
        hot.cpu_percent = 250.0;
        assert_eq!(hot.sanitized().cpu_percent, 100.0);
    }

    #[test]
    fn storage_sums_used_and_total_space() {
        let mut metrics = sample();
        metrics.volumes.push(VolumeUsage {
            name: "D:".into(),
            total_bytes: 50,
            free_bytes: 50,
        });
        assert_eq!(metrics.storage(), (60, 150));
        assert_eq!(
            SystemMetrics {
                volumes: vec![],
                ..sample()
            }
            .storage(),
            (0, 0)
        );
    }
}
