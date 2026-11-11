//! The computer's CPU, memory, network and storage use, sampled as often as
//! the server asks and reported to it on the control connection, so the
//! dashboard can show it.
//!
//! The coordinator owns the control connection, so it reports: the Windows
//! service's SYSTEM worker, the Mac's root launchd daemon and the Linux root
//! systemd service. Readings come
//! straight from the operating system. CPU load and network rates are averages
//! since the previous sample, however long ago that was. Volumes can sit on
//! slow disks, so they are read at most every [`VOLUME_REFRESH`], and one that
//! does not answer is left out (see [`VolumeReader`]). A report carries them
//! only after they were read again, or when this connection has not had them
//! yet ([`Reports`]).
//!
//! The server says how often to report ([`AgentCommand::SetMetricsInterval`]):
//! every few seconds while someone watches the device, about once a minute
//! otherwise. Each connection starts at [`IDLE_INTERVAL`] until it says.
//!
//! [`AgentCommand::SetMetricsInterval`]: meshrmm_protocol::AgentCommand::SetMetricsInterval
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use meshrmm_protocol::{
    METRICS_IDLE_INTERVAL_SECONDS, METRICS_WATCHED_INTERVAL_SECONDS, MetricsReport, SystemMetrics,
    VolumeUsage,
};
use tokio::task::JoinHandle;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

/// How often the Agent reports on a connection until the server says.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(METRICS_IDLE_INTERVAL_SECONDS);
/// The shortest interval the Agent reports at, whatever it is told: what
/// the server asks for while someone watches.
const SHORTEST_INTERVAL: Duration = Duration::from_secs(METRICS_WATCHED_INTERVAL_SECONDS);
/// The longest interval the Agent reports at, whatever it is told, so the
/// server always hears of the computer every few minutes.
const LONGEST_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// How long a volume list is reused before the disks are asked again.
const VOLUME_REFRESH: Duration = Duration::from_secs(60);
/// How long one volume may take to give its size and free space. A healthy
/// disk answers at once, from what the kernel already holds.
const VOLUME_TIMEOUT: Duration = Duration::from_secs(3);

/// When the computer started, in Unix milliseconds by its clock at `now`,
/// from how long it has been up. `None` when the uptime can't be read.
pub(crate) fn booted_at_unix_ms(now: i64) -> Option<i64> {
    match platform::uptime_seconds() {
        Ok(seconds) => Some(now.saturating_sub(i64::try_from(seconds).ok()?.saturating_mul(1000))),
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not read how long the computer has been up"
            );
            None
        }
    }
}

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

/// A volume's size and free space, in bytes.
type Usage = anyhow::Result<(u64, u64)>;

/// Asks each volume for its usage on a thread of its own, waiting at most
/// [`VOLUME_TIMEOUT`]. A disk that never answers, such as one that is
/// failing or an iSCSI disk whose target went away, blocks the call that
/// asks it, and would otherwise stop every later sample. It is left out,
/// and not asked again until its stuck call returns, so at most one thread
/// waits on each.
struct VolumeReader {
    usage: Arc<dyn Fn(&str) -> Usage + Send + Sync>,
    timeout: Duration,
    /// The volumes whose call has not returned, with where it will answer.
    hanging: HashMap<String, mpsc::Receiver<Usage>>,
}

impl VolumeReader {
    fn new(timeout: Duration, usage: impl Fn(&str) -> Usage + Send + Sync + 'static) -> Self {
        Self {
            usage: Arc::new(usage),
            timeout,
            hanging: HashMap::new(),
        }
    }

    /// The usage of each of `mounts` that answers in time and has space.
    fn read(&mut self, mounts: Vec<String>) -> Vec<VolumeUsage> {
        // A call that returned at last frees its volume to be asked again.
        self.hanging
            .retain(|_, answer| matches!(answer.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let mut volumes = Vec::new();
        for name in mounts {
            if self.hanging.contains_key(&name) {
                continue;
            }
            let (answer, answered) = mpsc::sync_channel(1);
            let (usage, mount) = (Arc::clone(&self.usage), name.clone());
            let asked = std::thread::Builder::new()
                .name("meshrmm-volume-usage".into())
                .spawn(move || {
                    let _ = answer.send(usage(&mount));
                });
            if let Err(error) = asked {
                tracing::debug!(volume = %name, %error, "could not ask a volume for its usage");
                continue;
            }
            match answered.recv_timeout(self.timeout) {
                Ok(Ok((total_bytes, free_bytes))) if total_bytes > 0 => {
                    volumes.push(VolumeUsage {
                        name,
                        total_bytes,
                        free_bytes,
                    });
                }
                Ok(Ok(_)) => {}
                // Such as a BitLocker drive that is still locked.
                Ok(Err(error)) => {
                    tracing::debug!(volume = %name, error = ?error, "could not read a volume's usage");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(volume = %name, timeout = ?self.timeout, "a volume did not give its usage in time; it is left out until it answers");
                    self.hanging.insert(name, answered);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
            }
        }
        volumes
    }
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
    volume_reader: VolumeReader,
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
            volume_reader: VolumeReader::new(VOLUME_TIMEOUT, platform::volume_usage),
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
    pub fn sample(&mut self) -> Sample {
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
        let volumes_read = self.read_volumes();
        Sample {
            metrics: SystemMetrics {
                cpu_percent,
                memory_used_bytes: memory.used,
                memory_total_bytes: memory.total,
                network_received_bytes_per_second,
                network_sent_bytes_per_second,
                uptime_seconds,
                volumes: self.volumes.clone(),
            },
            volumes_read,
        }
    }

    /// Reads the local volumes again once the cached list is
    /// [`VOLUME_REFRESH`] old, and returns whether it did. A failed read
    /// keeps the last list.
    fn read_volumes(&mut self) -> bool {
        if self
            .volumes_read
            .is_some_and(|read| read.elapsed() < VOLUME_REFRESH)
        {
            return false;
        }
        self.volumes_read = Some(Instant::now());
        let Some(mounts) = self.read("volumes", platform::volume_mounts()) else {
            return false;
        };
        let mut volumes = self.volume_reader.read(mounts);
        order_volumes(&mut volumes);
        self.volumes = volumes;
        true
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

/// One sample, and whether its volumes were read for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub metrics: SystemMetrics,
    /// The volumes were read again for this sample rather than reused.
    pub volumes_read: bool,
}

/// The interval `seconds` asks for, within what the Agent accepts.
fn accepted(seconds: u32) -> Duration {
    Duration::from_secs(u64::from(seconds)).clamp(SHORTEST_INTERVAL, LONGEST_INTERVAL)
}

type TakeSample = Box<dyn FnMut() -> Sample + Send>;
type Fraction = Box<dyn FnMut() -> f64 + Send>;

/// Samples while the Agent runs, one sample at a time and off the signaling
/// task, every interval the server set. The first sample after the Agent
/// starts, and the first after the interval changes, is due after a random
/// part of the interval, so a fleet does not report in step: not computers
/// that start together, nor every Agent told the same interval in the same
/// second, as after the server restarts.
pub struct Reports {
    interval: Duration,
    /// When the next sample is taken.
    due: tokio::time::Instant,
    /// A random fraction of 1, for the first sample's delay.
    fraction: Fraction,
    take: Arc<Mutex<TakeSample>>,
    task: Option<JoinHandle<Sample>>,
    /// Whether the next report carries the volumes: this connection has not
    /// had them since they were last read.
    volumes_owed: bool,
}

impl Reports {
    /// Created once per Agent process, so a reconnect does not lose the
    /// counters the next sample is measured against.
    pub fn new() -> Self {
        let mut sampler = Sampler::new();
        Self::with(
            move || sampler.sample(),
            || meshrmm_signaling_client::random_fraction().unwrap_or(0.5),
        )
    }

    fn with(
        take: impl FnMut() -> Sample + Send + 'static,
        fraction: impl FnMut() -> f64 + Send + 'static,
    ) -> Self {
        let mut reports = Self {
            interval: IDLE_INTERVAL,
            due: tokio::time::Instant::now(),
            fraction: Box::new(fraction),
            take: Arc::new(Mutex::new(Box::new(take))),
            task: None,
            volumes_owed: true,
        };
        // The sampler was just primed, so any delay measures something.
        let delay = reports.random_part();
        reports.due += delay;
        reports
    }

    /// A random part of the interval.
    fn random_part(&mut self) -> Duration {
        let fraction = (self.fraction)();
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.interval.mul_f64(fraction)
    }

    /// The interval the Agent reports at now.
    #[cfg(test)]
    fn interval(&self) -> Duration {
        self.interval
    }

    /// A new connection: its first report carries the volumes, and it is
    /// reported to at [`IDLE_INTERVAL`] until the server says otherwise.
    pub fn connected(&mut self) {
        self.volumes_owed = true;
        self.set_interval(IDLE_INTERVAL);
    }

    /// Reports every `seconds` the server asked for, within what the Agent
    /// accepts. The interval it already has changes nothing.
    pub fn set_interval_seconds(&mut self, seconds: u32) {
        self.set_interval(accepted(seconds));
    }

    fn set_interval(&mut self, interval: Duration) {
        if interval == self.interval {
            return;
        }
        tracing::debug!(?interval, "resource usage reporting interval changed");
        self.interval = interval;
        self.due = tokio::time::Instant::now() + self.random_part();
    }

    /// Completes with the next report. Cancel safe: a sample that is still
    /// being taken is kept for the next call. After a disconnect, a sample
    /// that came due is taken as soon as the connection is back.
    pub async fn next(&mut self) -> MetricsReport {
        loop {
            if let Some(task) = &mut self.task {
                let result = task.await;
                self.task = None;
                match result {
                    Ok(sample) => {
                        self.volumes_owed |= sample.volumes_read;
                        return MetricsReport::new(sample.metrics, self.volumes_owed).sanitized();
                    }
                    Err(error) => tracing::warn!(error = %error, "resource usage sample failed"),
                }
            }
            tokio::time::sleep_until(self.due).await;
            // One interval after this sample, however late it is.
            self.due = tokio::time::Instant::now() + self.interval;
            let take = Arc::clone(&self.take);
            self.task = Some(tokio::task::spawn_blocking(move || {
                (take.lock().unwrap_or_else(|error| error.into_inner()))()
            }));
        }
    }

    /// Notes whether `report` went into the send queue. Once one carrying
    /// the volumes did, reports leave them out until they are read again; a
    /// report that was dropped did not deliver them.
    pub fn reported(&mut self, report: &MetricsReport, queued: bool) {
        if queued && report.volumes.is_some() {
            self.volumes_owed = false;
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
    fn a_volume_that_does_not_answer_is_left_out_until_it_does() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (release, gate) = mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&asked);
        let mut reader = VolumeReader::new(Duration::from_secs(1), move |mount| {
            if mount == "/hung" {
                counted.fetch_add(1, Ordering::SeqCst);
                // Blocks until the test lets it go, like a dead disk.
                let _ = gate.lock().unwrap().recv();
            }
            Ok((100, 40))
        });
        let mounts = || vec!["/".to_owned(), "/hung".to_owned()];
        let names = |volumes: Vec<VolumeUsage>| {
            volumes
                .into_iter()
                .map(|volume| volume.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(reader.read(mounts())), ["/"]);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // Still stuck, so not asked again: no second thread waits on it.
        assert_eq!(names(reader.read(mounts())), ["/"]);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        drop(release);
        // Waits for the stuck call to answer and its thread to end.
        for answer in reader.hanging.values() {
            while answer.recv().is_ok() {}
        }
        assert_eq!(names(reader.read(mounts())), ["/", "/hung"]);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn real_samples_are_plausible() {
        let mut sampler = Sampler::new();
        std::thread::sleep(Duration::from_millis(500));
        let first = sampler.sample();
        std::thread::sleep(Duration::from_millis(500));
        let second = sampler.sample();
        println!("{first:#?}\n{second:#?}");
        // The first sample reads the volumes, and the second reuses them.
        assert!(first.volumes_read && !second.volumes_read);
        let (first, second) = (first.metrics, second.metrics);
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
        // The test keeps a processor busy itself, since an idle virtual
        // machine can go seconds without a busy tick; a second reading
        // proves the rate is measured, not left at zero.
        let busy = (0..20).any(|_| {
            let started = std::time::Instant::now();
            while started.elapsed() < Duration::from_millis(100) {
                std::hint::black_box(started.elapsed());
            }
            sampler.sample().metrics.cpu_percent > 0.0
        });
        assert!(busy, "processor load never rose above zero");
    }

    /// Reports from samples whose CPU load counts them, and whose volumes
    /// were read when `read` says, with the fraction `fraction` for every
    /// random delay.
    fn fake(read: impl Fn(u32) -> bool + Send + 'static, fraction: f64) -> Reports {
        let mut taken = 0;
        Reports::with(
            move || {
                taken += 1;
                Sample {
                    metrics: SystemMetrics {
                        cpu_percent: taken as f32,
                        memory_used_bytes: 1,
                        memory_total_bytes: 2,
                        network_received_bytes_per_second: 0,
                        network_sent_bytes_per_second: 0,
                        uptime_seconds: 1,
                        volumes: vec![volume("C:", 100)],
                    },
                    volumes_read: read(taken),
                }
            },
            move || fraction,
        )
    }

    /// The next report, how long after `since` it came, and its sample's
    /// number.
    async fn next(reports: &mut Reports, since: tokio::time::Instant) -> (Duration, u32, bool) {
        let report = reports.next().await;
        reports.reported(&report, true);
        (
            since.elapsed(),
            report.cpu_percent as u32,
            report.volumes.is_some(),
        )
    }

    fn seconds(seconds: f64) -> Duration {
        Duration::from_millis((seconds * 1000.0) as u64)
    }

    #[tokio::test(start_paused = true)]
    async fn reports_start_idle_a_random_part_of_the_interval_in() {
        let start = tokio::time::Instant::now();
        let mut reports = fake(|_| false, 0.25);
        assert_eq!(reports.interval(), IDLE_INTERVAL);
        assert_eq!(next(&mut reports, start).await.0, seconds(15.0));
        assert_eq!(next(&mut reports, start).await.0, seconds(75.0));
        assert_eq!(next(&mut reports, start).await.0, seconds(135.0));
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_interval_starts_at_a_random_part_of_itself() {
        let mut reports = fake(|_| false, 0.5);
        let start = tokio::time::Instant::now();
        assert_eq!(next(&mut reports, start).await.0, seconds(30.0));
        // Watched: a reading within the new interval, then one each interval.
        let changed = tokio::time::Instant::now();
        reports.set_interval_seconds(5);
        assert_eq!(reports.interval(), seconds(5.0));
        assert_eq!(next(&mut reports, changed).await.0, seconds(2.5));
        assert_eq!(next(&mut reports, changed).await.0, seconds(7.5));
        // Told the same again, the timer stays where it was.
        tokio::time::advance(seconds(1.0)).await;
        reports.set_interval_seconds(5);
        assert_eq!(next(&mut reports, changed).await.0, seconds(12.5));
        // Back to idle: half of the minute in.
        let changed = tokio::time::Instant::now();
        reports.set_interval_seconds(60);
        assert_eq!(next(&mut reports, changed).await.0, seconds(30.0));
        assert_eq!(next(&mut reports, changed).await.0, seconds(90.0));
    }

    #[tokio::test(start_paused = true)]
    async fn the_interval_is_kept_within_what_the_agent_accepts() {
        let mut reports = fake(|_| false, 0.0);
        reports.set_interval_seconds(0);
        assert_eq!(reports.interval(), SHORTEST_INTERVAL);
        reports.set_interval_seconds(1);
        assert_eq!(reports.interval(), SHORTEST_INTERVAL);
        reports.set_interval_seconds(u32::MAX);
        assert_eq!(reports.interval(), LONGEST_INTERVAL);
        reports.set_interval_seconds(120);
        assert_eq!(reports.interval(), seconds(120.0));
        // A new connection reports at the idle interval until told.
        reports.connected();
        assert_eq!(reports.interval(), IDLE_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn volumes_go_once_per_connection_and_after_each_read() {
        // Read with samples 1 and 4.
        let mut reports = fake(|taken| taken == 1 || taken == 4, 0.0);
        let start = tokio::time::Instant::now();
        let sent = |(_, sample, volumes)| (sample, volumes);
        assert_eq!(sent(next(&mut reports, start).await), (1, true));
        assert_eq!(sent(next(&mut reports, start).await), (2, false));
        // A new connection gets them in its first report, though the
        // sampler reused them.
        reports.connected();
        assert_eq!(sent(next(&mut reports, start).await), (3, true));
        assert_eq!(sent(next(&mut reports, start).await), (4, true));
        // A report the send queue dropped did not deliver them.
        reports.connected();
        let dropped = reports.next().await;
        assert!(dropped.volumes.is_some());
        reports.reported(&dropped, false);
        assert_eq!(sent(next(&mut reports, start).await), (6, true));
        assert_eq!(sent(next(&mut reports, start).await), (7, false));
        // A dropped report without them owes nothing.
        let dropped = reports.next().await;
        reports.reported(&dropped, false);
        assert_eq!(sent(next(&mut reports, start).await), (9, false));
    }
}
