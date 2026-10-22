//! The computer's operating system, hardware, network and installed
//! software, collected a little after the Agent connects, every
//! [`INTERVAL`] after that, and when the server asks, and sent to the server
//! on the control connection as one snapshot.
//!
//! The coordinator collects, as it reports resource usage: the Windows
//! service's SYSTEM worker, the Mac's root launchd daemon and the Linux root
//! systemd service, so every user's applications and the firmware details are
//! readable. Collecting reads the registry, IOKit, `/sys`, `/proc` and the
//! package databases directly, and only runs a tool where no file format is
//! stable (rpm, flatpak, snap). It runs on a blocking thread, one snapshot at
//! a time, so the connection never waits for it. A part that cannot be read
//! is left empty and named in the snapshot's problems; the rest still goes
//! out.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{
    MAX_SYSTEM_INFO_BYTES, SYSTEM_INFO_INTERVAL_SECONDS, SystemInfo, clean_text,
};
use tokio::task::JoinHandle;
use tokio::time::{Interval, MissedTickBehavior, interval_at};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

/// How often the Agent collects on its own.
pub const INTERVAL: Duration = Duration::from_secs(SYSTEM_INFO_INTERVAL_SECONDS);
/// The first snapshot waits this long after the Agent starts, so it doesn't
/// compete with connecting and the first sessions.
const FIRST_DELAY: Duration = Duration::from_secs(10);

/// The parts of a snapshot that could not be read.
#[derive(Debug, Default)]
pub(crate) struct Problems(Vec<String>);

impl Problems {
    /// `result`'s value, noting why `part` could not be read if it failed.
    pub(crate) fn take<T>(&mut self, part: &str, result: anyhow::Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.note(part, &error);
                None
            }
        }
    }

    pub(crate) fn note(&mut self, part: &str, error: &anyhow::Error) {
        tracing::warn!(
            part,
            error = format!("{error:#}"),
            "could not collect part of the system information"
        );
        self.0.push(clean_text(&format!("{part}: {error:#}")));
    }
}

/// Unix milliseconds of `time`, or `None` before 1970.
pub(crate) fn unix_ms(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
}

/// Collects a snapshot. Blocks for as long as reading the package databases
/// takes, so run it off the async runtime.
pub fn collect() -> SystemInfo {
    let started = std::time::Instant::now();
    let mut problems = Problems::default();
    let mut info = platform::collect(&mut problems);
    info.collected_at_unix_ms = unix_ms(SystemTime::now()).unwrap_or_default();
    info.problems.append(&mut problems.0);
    let info = fit(info.sanitized());
    tracing::info!(
        applications = info.applications.len(),
        problems = info.problems.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "collected the system information"
    );
    info
}

/// The snapshot with applications dropped from its end until its JSON fits
/// in [`MAX_SYSTEM_INFO_BYTES`].
fn fit(mut info: SystemInfo) -> SystemInfo {
    let size = |info: &SystemInfo| serde_json::to_vec(info).map_or(usize::MAX, |json| json.len());
    let listed = info.applications.len();
    while size(&info) > MAX_SYSTEM_INFO_BYTES && !info.applications.is_empty() {
        let keep = info.applications.len() * 9 / 10;
        info.applications.truncate(keep);
    }
    if info.applications.len() < listed {
        info.problems.push(format!(
            "applications: only the first {} of {listed} fit in a snapshot",
            info.applications.len()
        ));
    }
    info
}

/// Collects every [`INTERVAL`], and at once when the server asks, one
/// snapshot at a time and off the signaling task.
pub struct Collector {
    timer: Interval,
    task: Option<JoinHandle<SystemInfo>>,
    requested: bool,
}

impl Collector {
    /// Created once per Agent process, so reconnecting doesn't collect again
    /// before the interval ends. The server asks for a snapshot when the one
    /// it has is old.
    pub fn new() -> Self {
        let mut timer = interval_at(tokio::time::Instant::now() + FIRST_DELAY, INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            timer,
            task: None,
            requested: false,
        }
    }

    /// The server asked for a new snapshot. One being collected already
    /// answers it.
    pub fn request(&mut self) {
        if self.task.is_none() {
            self.requested = true;
        }
    }

    /// Completes with the next snapshot. Cancel safe: a snapshot that is
    /// still being collected is kept for the next call.
    pub async fn next(&mut self) -> SystemInfo {
        loop {
            if let Some(task) = &mut self.task {
                let result = task.await;
                self.task = None;
                match result {
                    Ok(info) => return info,
                    Err(error) => {
                        tracing::warn!(error = %error, "collecting the system information failed")
                    }
                }
            }
            if self.requested {
                self.requested = false;
                // The next periodic snapshot is an interval after this one.
                self.timer.reset();
            } else {
                self.timer.tick().await;
            }
            self.task = Some(tokio::task::spawn_blocking(collect));
        }
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{Application, ApplicationSource};

    use super::*;

    #[test]
    fn oversized_snapshots_lose_applications_from_the_end() {
        let application = |index: usize| Application {
            name: format!("{index:06} {}", "x".repeat(200)),
            version: Some("1.0".into()),
            publisher: Some("p".repeat(200)),
            architecture: None,
            installed_at_unix_ms: None,
            source: ApplicationSource::Dpkg,
        };
        let info = SystemInfo {
            applications: (0..15_000).map(application).collect(),
            ..Default::default()
        };
        let fitted = fit(info);
        assert!(serde_json::to_vec(&fitted).unwrap().len() <= MAX_SYSTEM_INFO_BYTES);
        assert!(fitted.applications.len() < 15_000);
        assert!(fitted.applications[0].name.starts_with("000000"));
        assert_eq!(fitted.problems.len(), 1);
        let small = fit(SystemInfo::default());
        assert!(small.problems.is_empty());
    }

    #[test]
    fn problems_name_the_part() {
        let mut problems = Problems::default();
        assert_eq!(problems.take("disks", Ok(3)), Some(3));
        assert_eq!(
            problems.take::<u8>("serial number", Err(anyhow::anyhow!("access denied\n"))),
            None
        );
        assert_eq!(problems.0, ["serial number: access denied"]);
    }

    /// Collects this computer's real snapshot, and checks what every
    /// platform can always read.
    #[test]
    fn real_snapshots_are_plausible() {
        let info = collect();
        println!("{}", serde_json::to_string_pretty(&info).unwrap());
        assert!(info.collected_at_unix_ms > 0);
        assert!(!info.os.id.is_empty(), "{:?}", info.os);
        assert!(!info.os.name.is_empty(), "{:?}", info.os);
        assert!(!info.os.version.is_empty(), "{:?}", info.os);
        assert!(!info.os.architecture.is_empty(), "{:?}", info.os);
        assert!(info.os.boot_time_unix_ms.is_some(), "{:?}", info.os);
        assert!(info.hardware.memory_bytes > 0, "{:?}", info.hardware);
        let processor = info.hardware.processor.as_ref().expect("a processor");
        assert!(processor.threads >= processor.cores && processor.cores > 0);
        assert!(!info.network.hostname.is_empty());
        assert!(
            info.network
                .adapters
                .iter()
                .any(|adapter| !adapter.addresses.is_empty()),
            "{:?}",
            info.network.adapters
        );
        assert!(!info.applications.is_empty());
        assert_eq!(info.clone().sanitized(), info);
    }
}
