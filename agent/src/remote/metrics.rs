//! The computer's CPU, memory, network and storage use, sampled every
//! [`INTERVAL`] and reported to the server on the control connection, so the
//! dashboard can show it.
//!
//! The coordinator owns the control connection, so it reports: the Windows
//! service's SYSTEM worker and the Mac's root launchd daemon. Readings come
//! straight from the operating system. CPU load and network rates are averages
//! since the previous sample. Volumes can sit on slow disks, so they are read
//! at most every [`VOLUME_REFRESH`] and the last list is reported in between.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meshrmm_protocol::{METRICS_INTERVAL_SECONDS, SystemMetrics, VolumeUsage};
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior, interval_at};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

/// How often the Agent samples and reports.
pub const INTERVAL: Duration = Duration::from_secs(METRICS_INTERVAL_SECONDS);
/// How long a volume list is reused before the disks are asked again.
const VOLUME_REFRESH: Duration = Duration::from_secs(60);

/// Processor time, in any unit, split into busy and idle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CpuTime {
    busy: u64,
    idle: u64,
}

/// The busy share of the processor time between two readings, from 0 to 100.
fn cpu_percent(elapsed: CpuTime) -> f32 {
    let total = elapsed.busy.saturating_add(elapsed.idle);
    if total == 0 {
        return 0.0;
    }
    (elapsed.busy as f64 * 100.0 / total as f64).clamp(0.0, 100.0) as f32
}

/// One network interface's byte counters since it came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Interface {
    id: u32,
    received: u64,
    sent: u64,
}

/// Bytes received and sent per second, from each interface's counters
/// `elapsed` apart. An interface that is new, or whose counters went back
/// because it was reset, adds nothing until its next reading.
fn network_rates(previous: &[Interface], next: &[Interface], elapsed: Duration) -> (u64, u64) {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return (0, 0);
    }
    let previous: HashMap<u32, &Interface> = previous.iter().map(|row| (row.id, row)).collect();
    let (received, sent) = next
        .iter()
        .filter_map(|row| Some((row, previous.get(&row.id)?)))
        .fold((0u64, 0u64), |(received, sent), (row, old)| {
            (
                received.saturating_add(row.received.saturating_sub(old.received)),
                sent.saturating_add(row.sent.saturating_sub(old.sent)),
            )
        });
    (
        (received as f64 / seconds).round() as u64,
        (sent as f64 / seconds).round() as u64,
    )
}

/// Largest first, so truncating the list keeps the volumes that matter most.
fn order_volumes(volumes: &mut [VolumeUsage]) {
    volumes.sort_by(|first, second| {
        second
            .total_bytes
            .cmp(&first.total_bytes)
            .then_with(|| first.name.cmp(&second.name))
    });
}

/// Physical memory in use and installed, in bytes.
struct Memory {
    used: u64,
    total: u64,
}

/// Takes samples, keeping the counters each one is measured against. A part
/// that cannot be read is reported as zero, and the rest of the sample still
/// goes out.
pub struct Sampler {
    cpu: Option<platform::CpuTicks>,
    network: Option<(Instant, Vec<Interface>)>,
    volumes: Vec<VolumeUsage>,
    volumes_read: Option<Instant>,
    /// The parts whose last reading failed. A failure is logged as a warning
    /// once, not every few seconds.
    failing: HashSet<&'static str>,
}

impl Sampler {
    /// Reads the counters once, so the first sample already has rates.
    pub fn new() -> Self {
        let mut sampler = Self {
            cpu: None,
            network: None,
            volumes: Vec::new(),
            volumes_read: None,
            failing: HashSet::new(),
        };
        sampler.cpu = sampler.read("cpu", platform::cpu_ticks());
        sampler.network = sampler
            .read("network", platform::interfaces())
            .map(|interfaces| (Instant::now(), interfaces));
        sampler
    }

    /// The computer's use since the previous sample. Blocks while it reads
    /// volumes, so run it off the async runtime.
    pub fn sample(&mut self) -> SystemMetrics {
        let cpu_percent = match self.read("cpu", platform::cpu_ticks()) {
            Some(ticks) => {
                let percent = self
                    .cpu
                    .as_ref()
                    .map_or(0.0, |previous| cpu_percent(ticks.since(previous)));
                self.cpu = Some(ticks);
                percent
            }
            None => 0.0,
        };
        let (network_received_bytes_per_second, network_sent_bytes_per_second) =
            match self.read("network", platform::interfaces()) {
                Some(interfaces) => {
                    let now = Instant::now();
                    let rates = self.network.as_ref().map_or((0, 0), |(read, previous)| {
                        network_rates(previous, &interfaces, now.duration_since(*read))
                    });
                    self.network = Some((now, interfaces));
                    rates
                }
                None => (0, 0),
            };
        let memory = self
            .read("memory", platform::memory())
            .unwrap_or(Memory { used: 0, total: 0 });
        let uptime_seconds = self.read("uptime", platform::uptime_seconds()).unwrap_or(0);
        SystemMetrics {
            cpu_percent,
            memory_used_bytes: memory.used,
            memory_total_bytes: memory.total,
            network_received_bytes_per_second,
            network_sent_bytes_per_second,
            uptime_seconds,
            volumes: self.volumes(),
        }
    }

    /// The local volumes, read again once the cached list is
    /// [`VOLUME_REFRESH`] old. A failed read keeps the last list.
    fn volumes(&mut self) -> Vec<VolumeUsage> {
        if self
            .volumes_read
            .is_none_or(|read| read.elapsed() >= VOLUME_REFRESH)
        {
            self.volumes_read = Some(Instant::now());
            if let Some(mut volumes) = self.read("volumes", platform::volumes()) {
                order_volumes(&mut volumes);
                self.volumes = volumes;
            }
        }
        self.volumes.clone()
    }

    /// `result`'s value, noting whether reading `part` failed.
    fn read<T>(&mut self, part: &'static str, result: anyhow::Result<T>) -> Option<T> {
        match result {
            Ok(value) => {
                if self.failing.remove(part) {
                    tracing::info!(part, "resource usage can be read again");
                }
                Some(value)
            }
            Err(error) if self.failing.insert(part) => {
                tracing::warn!(part, error = ?error, "could not read resource usage");
                None
            }
            Err(error) => {
                tracing::debug!(part, error = ?error, "resource usage still unavailable");
                None
            }
        }
    }
}

/// Samples every [`INTERVAL`] while the Agent runs, one sample at a time and
/// off the signaling task.
pub struct Reports {
    timer: Interval,
    sampler: Arc<Mutex<Sampler>>,
    task: Option<JoinHandle<SystemMetrics>>,
}

impl Reports {
    /// Created once per Agent process, so a reconnect does not lose the
    /// counters the next sample is measured against.
    pub fn new() -> Self {
        let sampler = Sampler::new();
        // The sampler was just primed, so the first sample is due one
        // interval from now. After a disconnect, a missed sample is taken
        // as soon as the connection is back.
        let mut timer = interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            timer,
            sampler: Arc::new(Mutex::new(sampler)),
            task: None,
        }
    }

    /// Completes with the next sample. Cancel safe: a sample that is still
    /// being taken is kept for the next call.
    pub async fn next(&mut self) -> SystemMetrics {
        loop {
            if let Some(task) = &mut self.task {
                let result = task.await;
                self.task = None;
                match result {
                    Ok(metrics) => return metrics,
                    Err(error) => tracing::warn!(error = %error, "resource usage sample failed"),
                }
            }
            self.timer.tick().await;
            let sampler = Arc::clone(&self.sampler);
            self.task = Some(tokio::task::spawn_blocking(move || {
                sampler
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .sample()
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(id: u32, received: u64, sent: u64) -> Interface {
        Interface { id, received, sent }
    }

    fn volume(name: &str, total_bytes: u64) -> VolumeUsage {
        VolumeUsage {
            name: name.into(),
            total_bytes,
            free_bytes: 0,
        }
    }

    #[test]
    fn cpu_load_is_the_busy_share() {
        assert_eq!(cpu_percent(CpuTime { busy: 1, idle: 3 }), 25.0);
        assert_eq!(cpu_percent(CpuTime { busy: 7, idle: 0 }), 100.0);
        assert_eq!(cpu_percent(CpuTime { busy: 0, idle: 9 }), 0.0);
        // No time passed.
        assert_eq!(cpu_percent(CpuTime::default()), 0.0);
        assert_eq!(
            cpu_percent(CpuTime {
                busy: u64::MAX,
                idle: u64::MAX
            }),
            100.0
        );
    }

    #[test]
    fn network_rates_average_each_direction_over_the_interval() {
        let previous = [interface(1, 1_000, 100), interface(2, 0, 0)];
        let next = [interface(1, 11_000, 600), interface(2, 5_000, 1_000)];
        assert_eq!(
            network_rates(&previous, &next, Duration::from_secs(5)),
            (3_000, 300)
        );
        assert_eq!(network_rates(&previous, &next, Duration::ZERO), (0, 0));
    }

    #[test]
    fn reset_or_new_interfaces_add_nothing() {
        let previous = [interface(1, 1_000, 1_000), interface(2, 500, 500)];
        // Interface 1 was reset, 2 went away and 3 came up.
        let next = [interface(1, 10, 2_000), interface(3, 9_000, 9_000)];
        assert_eq!(
            network_rates(&previous, &next, Duration::from_secs(1)),
            (0, 1_000)
        );
    }

    #[test]
    fn volumes_are_ordered_largest_first() {
        let mut volumes = vec![
            volume("D:", 10),
            volume("/Volumes/Backup", 500),
            volume("C:", 500),
            volume("/", 100),
        ];
        order_volumes(&mut volumes);
        let names: Vec<_> = volumes.iter().map(|volume| volume.name.as_str()).collect();
        assert_eq!(names, ["/Volumes/Backup", "C:", "/", "D:"]);
    }

    #[test]
    fn real_samples_are_plausible() {
        let mut sampler = Sampler::new();
        std::thread::sleep(Duration::from_millis(500));
        let first = sampler.sample();
        std::thread::sleep(Duration::from_millis(500));
        let second = sampler.sample();
        println!("{first:#?}\n{second:#?}");
        for metrics in [&first, &second] {
            assert!(metrics.memory_total_bytes > 0, "{metrics:?}");
            assert!(metrics.memory_used_bytes > 0, "{metrics:?}");
            assert!(
                metrics.memory_used_bytes <= metrics.memory_total_bytes,
                "{metrics:?}"
            );
            assert!((0.0..=100.0).contains(&metrics.cpu_percent), "{metrics:?}");
            assert!(metrics.uptime_seconds > 0, "{metrics:?}");
            assert!(!metrics.volumes.is_empty(), "{metrics:?}");
            for volume in &metrics.volumes {
                assert!(!volume.name.is_empty(), "{volume:?}");
                assert!(volume.total_bytes > 0, "{volume:?}");
                assert!(volume.free_bytes <= volume.total_bytes, "{volume:?}");
            }
            assert_eq!(metrics.clone().sanitized(), *metrics);
        }
        assert!(sampler.failing.is_empty(), "{:?}", sampler.failing);
        // The cached list is reused within the refresh interval.
        assert_eq!(first.volumes, second.volumes);
        assert!(
            first
                .volumes
                .windows(2)
                .all(|pair| pair[0].total_bytes >= pair[1].total_bytes)
        );
        // Most computers have a processor busy at least now and then; a
        // second reading proves the rate is measured, not left at zero.
        let busy = (0..20).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            sampler.sample().cpu_percent > 0.0
        });
        assert!(busy, "processor load never rose above zero");
    }
}
