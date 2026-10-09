//! The system resource usage an Agent reports on its control connection.
use serde::{Deserialize, Serialize};

/// How often an Agent samples and reports its resource usage.
pub const METRICS_INTERVAL_SECONDS: u64 = 5;
/// The most volumes one report lists; an Agent reports its largest.
pub const MAX_METRIC_VOLUMES: usize = 16;
/// The longest volume name, in characters, a report carries.
pub const MAX_VOLUME_NAME_CHARS: usize = 64;

/// One sample of a computer's resource usage. Rates and the CPU load are
/// averages over the time since the Agent's previous sample.
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

/// A local volume's capacity, as a drive letter (`C:`) or mount point (`/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsage {
    pub name: String,
    pub total_bytes: u64,
    /// Space available to programs, not counting what only the system may use.
    pub free_bytes: u64,
}

impl SystemMetrics {
    /// The sample with impossible values brought into range and its volume
    /// list bounded, so whatever a peer sent is safe to store and show.
    pub fn sanitized(mut self) -> Self {
        self.cpu_percent = if self.cpu_percent.is_finite() {
            self.cpu_percent.clamp(0.0, 100.0)
        } else {
            0.0
        };
        self.memory_used_bytes = self.memory_used_bytes.min(self.memory_total_bytes);
        self.volumes.retain(|volume| volume.total_bytes > 0);
        self.volumes.truncate(MAX_METRIC_VOLUMES);
        for volume in &mut self.volumes {
            volume.free_bytes = volume.free_bytes.min(volume.total_bytes);
            volume.name = volume
                .name
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_VOLUME_NAME_CHARS)
                .collect();
        }
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
