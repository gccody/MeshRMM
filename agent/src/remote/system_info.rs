//! The computer's operating system, hardware, network and installed
//! software, collected a little after the Agent starts, about every
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
//!
//! The applications are most of a snapshot and seldom change, so a snapshot
//! leaves them out when they are the list the server said it holds
//! (`AgentCommand::StoredApplications`), and names that list's hash
//! instead. The snapshots the Agent collects on its own fall at random
//! points ([`next_collection`]), so a fleet that started together, after an
//! update or a power cut, doesn't send together.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{
    AgentStatusMessage, MAX_SYSTEM_INFO_BYTES, SYSTEM_INFO_INTERVAL_SECONDS, SystemInfo,
    applications_hash, clean_text,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
pub(crate) mod windows;

#[cfg(target_os = "linux")]
use self::linux as platform;
#[cfg(target_os = "macos")]
use self::macos as platform;
#[cfg(windows)]
use self::windows as platform;

/// How often the Agent collects on its own, at most.
pub const INTERVAL: Duration = Duration::from_secs(SYSTEM_INFO_INTERVAL_SECONDS);
/// The share of [`INTERVAL`] a snapshot the Agent collects on its own may
/// come early by, at random, so devices that collected together drift apart
/// and a snapshot is never older than the interval.
const JITTER: f64 = 0.1;
/// The soonest the first snapshot is collected after the Agent starts, so it
/// doesn't compete with connecting and the first sessions.
const FIRST_DELAY: Duration = Duration::from_secs(10);
/// The latest the first snapshot is collected after the Agent starts: a
/// fleet of 100,000 that starts together sends about 28 a second. The server
/// asks at once for one it is missing or that is old.
const FIRST_SPREAD: Duration = Duration::from_secs(60 * 60);
/// A collection still running after this is given up on, so one stuck on a
/// hung system service or file system doesn't stop every later snapshot.
const COLLECT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

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

/// An IPv4 address of one of the computer's interfaces that are up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalAddress {
    /// The interface's name, such as `en0`, `eth0` or `Ethernet`.
    pub(crate) interface: String,
    pub(crate) address: std::net::Ipv4Addr,
    pub(crate) prefix_length: u8,
}

/// The IPv4 addresses of the interfaces that are up, without loopback ones,
/// as they are now. Blocks briefly, so call it off the async runtime.
pub(crate) fn local_ipv4_addresses() -> anyhow::Result<Vec<LocalAddress>> {
    platform::local_ipv4_addresses()
}

/// Collects a snapshot. Blocks for as long as reading the package databases
/// takes, so run it off the async runtime.
pub fn collect() -> SystemInfo {
    let started = std::time::Instant::now();
    let mut problems = Problems::default();
    let mut info = platform::collect(&mut problems);
    info.os.terminal_shells = super::terminal::available_shells();
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

/// How long after now the Agent next collects on its own: after it starts
/// (`first`), between [`FIRST_DELAY`] and [`FIRST_SPREAD`]; after another
/// snapshot, between [`INTERVAL`] less its [`JITTER`] and [`INTERVAL`]. A
/// random `fraction` in `[0, 1)`, drawn for each one, picks the point.
fn next_collection(first: bool, fraction: f64) -> Duration {
    let fraction = fraction.clamp(0.0, 1.0);
    if first {
        FIRST_DELAY + (FIRST_SPREAD - FIRST_DELAY).mul_f64(fraction)
    } else {
        INTERVAL - INTERVAL.mul_f64(JITTER * fraction)
    }
}

fn random_fraction() -> f64 {
    meshrmm_signaling_client::random_fraction().unwrap_or(0.5)
}

/// The status message for a snapshot, given the hash of the application list
/// the server holds (`stored`): without the applications when they are that
/// list.
fn report(system: SystemInfo, stored: Option<&str>) -> AgentStatusMessage {
    let mut system = Box::new(system);
    let unchanged = stored.filter(|stored| *stored == applications_hash(&system.applications));
    let applications_unchanged = unchanged.map(|hash| {
        system.applications = Vec::new();
        hash.to_owned()
    });
    AgentStatusMessage::SystemInfo {
        system,
        applications_unchanged,
    }
}

/// Collects about every [`INTERVAL`], and at once when the server asks, one
/// snapshot at a time and off the signaling task.
pub struct Collector {
    /// When the Agent next collects on its own.
    due: Instant,
    /// The collection running, and when it is given up on.
    task: Option<(JoinHandle<SystemInfo>, Instant)>,
    requested: bool,
    /// The hash of the application list this connection's server last said
    /// it holds.
    stored: Option<String>,
}

impl Collector {
    /// Created once per Agent process, so reconnecting doesn't collect again
    /// before the interval ends. The server asks for a snapshot when the one
    /// it has is old.
    pub fn new() -> Self {
        Self {
            due: Instant::now() + next_collection(true, random_fraction()),
            task: None,
            requested: false,
            stored: None,
        }
    }

    /// A connection started: its server says which application list it
    /// holds first thing, and until then every snapshot carries its list.
    pub fn connected(&mut self) {
        self.stored = None;
    }

    /// The server said which application list it holds.
    pub fn stored(&mut self, applications_hash: Option<String>) {
        self.stored = applications_hash;
    }

    /// The status message that sends `system`.
    pub fn report(&self, system: SystemInfo) -> AgentStatusMessage {
        report(system, self.stored.as_deref())
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
            if let Some((task, deadline)) = &mut self.task {
                let result = tokio::time::timeout_at(*deadline, task).await;
                self.task = None;
                match result {
                    Ok(Ok(info)) => return info,
                    Ok(Err(error)) => {
                        tracing::warn!(error = %error, "collecting the system information failed")
                    }
                    // The blocking thread can't be stopped; it ends when
                    // whatever it waits for answers.
                    Err(_) => tracing::warn!(
                        timeout_seconds = COLLECT_TIMEOUT.as_secs(),
                        "collecting the system information took too long"
                    ),
                }
            }
            if self.requested {
                self.requested = false;
            } else {
                tokio::time::sleep_until(self.due).await;
            }
            // The next one the Agent collects on its own is an interval
            // after this one, whoever asked for this one.
            let now = Instant::now();
            self.due = now + next_collection(false, random_fraction());
            self.task = Some((tokio::task::spawn_blocking(collect), now + COLLECT_TIMEOUT));
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

    fn listing(names: &[&str]) -> SystemInfo {
        SystemInfo {
            applications: names
                .iter()
                .map(|name| Application {
                    name: (*name).to_owned(),
                    version: Some("1.0".into()),
                    publisher: None,
                    architecture: None,
                    installed_at_unix_ms: None,
                    source: ApplicationSource::Dpkg,
                })
                .collect(),
            ..Default::default()
        }
        .sanitized()
    }

    /// How many applications a report sends, and the hash it names instead.
    fn sent(message: AgentStatusMessage) -> (usize, Option<String>) {
        let AgentStatusMessage::SystemInfo {
            system,
            applications_unchanged,
        } = message
        else {
            panic!("not a snapshot: {message:?}");
        };
        (system.applications.len(), applications_unchanged)
    }

    #[tokio::test]
    async fn only_a_list_the_server_does_not_hold_is_sent() {
        let first = listing(&["curl", "openssl"]);
        let hash = applications_hash(&first.applications);
        let mut collector = Collector::new();
        // Until the server says what it holds, the list goes out.
        assert_eq!(sent(collector.report(first.clone())), (2, None));

        // The server stored it: the same list is left out, and named.
        collector.stored(Some(hash.clone()));
        assert_eq!(
            sent(collector.report(first.clone())),
            (0, Some(hash.clone()))
        );
        // A changed one goes out whole, and once the server says it stored
        // that one, it is left out in turn.
        let second = listing(&["curl", "openssl", "vim"]);
        let second_hash = applications_hash(&second.applications);
        assert_eq!(sent(collector.report(second.clone())), (3, None));
        collector.stored(Some(second_hash.clone()));
        assert_eq!(
            sent(collector.report(second.clone())),
            (0, Some(second_hash))
        );

        // A server that holds another list, or none, gets the whole list.
        collector.stored(Some(hash.clone()));
        assert_eq!(sent(collector.report(second)), (3, None));
        collector.stored(None);
        assert_eq!(sent(collector.report(first.clone())), (2, None));
        // As does a new connection's, until it says what it holds.
        collector.stored(Some(hash));
        collector.connected();
        assert_eq!(sent(collector.report(first)), (2, None));
        // An empty list is a list like any other.
        collector.stored(Some(applications_hash(&[])));
        assert_eq!(
            sent(collector.report(listing(&[]))),
            (0, Some(applications_hash(&[])))
        );
    }

    #[test]
    fn the_agents_own_snapshots_are_spread_out() {
        assert_eq!(next_collection(true, 0.0), FIRST_DELAY);
        assert_eq!(
            next_collection(true, 0.5),
            FIRST_DELAY + (FIRST_SPREAD - FIRST_DELAY) / 2
        );
        assert!(next_collection(true, 0.999) < FIRST_SPREAD);
        assert!(next_collection(true, 0.999) > FIRST_SPREAD - Duration::from_secs(5));
        let earliest = INTERVAL - INTERVAL.mul_f64(JITTER);
        assert_eq!(next_collection(false, 0.0), INTERVAL);
        assert_eq!(
            next_collection(false, 0.5),
            INTERVAL - INTERVAL.mul_f64(JITTER / 2.0)
        );
        assert!(next_collection(false, 0.999) > earliest);
        // Two Agents' times differ with their random draws, and stay within
        // bounds whatever they are.
        assert_ne!(next_collection(false, 0.1), next_collection(false, 0.9));
        assert_ne!(next_collection(true, 0.1), next_collection(true, 0.9));
        for fraction in [-1.0, 2.0] {
            let first = next_collection(true, fraction);
            assert!((FIRST_DELAY..=FIRST_SPREAD).contains(&first), "{first:?}");
            let later = next_collection(false, fraction);
            assert!((earliest..=INTERVAL).contains(&later), "{later:?}");
        }
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
        // The server hashes the list it receives, sanitized again, as the
        // Agent hashes the list it sends.
        let json = serde_json::to_vec(&info).unwrap();
        let received: SystemInfo = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            applications_hash(&received.sanitized().applications),
            applications_hash(&info.applications)
        );
    }
}
