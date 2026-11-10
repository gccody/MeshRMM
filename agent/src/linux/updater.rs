//! Automatic updates of the installed Linux Agent.
//!
//! The coordinator checks the release manifest on the same schedule as the
//! other platforms, and stages a newer Agent beside the installed one once
//! its SHA-256 and release signature check out. It starts the new Agent's
//! `--apply-update` in a transient systemd unit, which outlives the service:
//! that process waits for the coordinator to stop, swaps the executable,
//! restarts the service, and puts the previous executable back if the new
//! coordinator does not stay up or does not connect to the server within
//! [`CONFIRM_TIMEOUT`]. See [`crate::update_files`] for how the swap, and the
//! removal of what earlier updates left, work.
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_self_update::{CURRENT_VERSION, UpdateManifest};

use super::installer::{EXECUTABLE, INSTALL_DIRECTORY, SERVICE, systemctl};
use crate::remote::config::Config;
use crate::remote::service_link::ServiceLink;
use crate::update_files::{self, InstallLock};
use crate::update_policy::{
    CONFIRM_TIMEOUT, Confirmation, NEXT_CHECK_FILE, NextCheck, UpdateAttempts, UpdateSchedule,
    random_bits, read_attempts, read_next_check, unix_seconds,
};

const SCHEDULE_TICK: Duration = Duration::from_secs(30);
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;
/// The coordinator ends remote sessions and runs their close actions first.
const COORDINATOR_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the installer waits for the coordinator's lock once the coordinator has exited.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the new coordinator must keep running before the installer waits for it to connect,
/// so one that crashes at once is rolled back without waiting for [`CONFIRM_TIMEOUT`].
const HEALTHY_AFTER: Duration = Duration::from_secs(15);
const ATTEMPTS_FILE: &str = "update-attempts.json";
/// A staged release beside the installed Agent is named this and its version. After the swap
/// the name holds the previous build, and after a rollback the failed one.
const STAGED_PREFIX: &str = "meshrmm-agent-";

/// This build's release target.
#[cfg(target_arch = "x86_64")]
const TARGET: &str = meshrmm_self_update::AGENT_LINUX_X64;
#[cfg(target_arch = "aarch64")]
const TARGET: &str = meshrmm_self_update::AGENT_LINUX_ARM64;

/// Checks for updates until one is staged, then stops the coordinator so the
/// new Agent can replace it.
pub(crate) async fn run(config: Config, link: Arc<ServiceLink>) {
    let _ = tokio::task::spawn_blocking(|| {
        if let Err(error) = updates_directory().and_then(|updates| {
            remove_leftovers_unless_installing(&updates, Path::new(INSTALL_DIRECTORY))
        }) {
            tracing::warn!(error = ?error, "could not remove what earlier Agent updates left");
        }
    })
    .await;
    let record = updates_directory()
        .ok()
        .and_then(|updates| read_next_check(&updates.join(NEXT_CHECK_FILE)));
    let mut schedule = UpdateSchedule::new(
        Instant::now(),
        unix_seconds(),
        record.as_ref(),
        random_bits(),
    );
    // A release a check found, waiting for the Agent to be free to stop.
    let mut staged: Option<Staged> = None;
    loop {
        // The installer of this build can put the previous one back until it has connected,
        // so this build does not replace itself meanwhile.
        if staged.is_none() && !confirming() && schedule.due(Instant::now(), link.busy()) {
            let check_config = config.clone();
            let succeeded = match tokio::task::spawn_blocking(move || stage(&check_config)).await {
                Ok(Ok(Some(update))) => {
                    staged = Some(update);
                    None
                }
                Ok(Ok(None)) => Some(true),
                Ok(Err(error)) => {
                    tracing::warn!(error = ?error, "Agent update check failed");
                    Some(false)
                }
                Err(error) => {
                    tracing::warn!(%error, "Agent update check stopped");
                    Some(false)
                }
            };
            if let Some(succeeded) = succeeded
                && let Some(next) =
                    schedule.checked(Instant::now(), unix_seconds(), succeeded, random_bits())
            {
                record_next_check(&next);
            }
        }
        if let Some(update) = staged.take_if(|_| schedule.install_now(Instant::now(), link.busy()))
        {
            let version = update.version.clone();
            match tokio::task::spawn_blocking(move || update.start()).await {
                Ok(Ok(())) => {
                    tracing::info!(%version, "stopping the Agent to install an update");
                    link.stop_for_update(version);
                    return;
                }
                Ok(Err(error)) => {
                    tracing::warn!(error = ?error, "could not start the Agent update")
                }
                Err(error) => tracing::warn!(%error, "starting the Agent update stopped"),
            }
            schedule.checked(Instant::now(), unix_seconds(), false, random_bits());
        }
        tokio::time::sleep(SCHEDULE_TICK).await;
    }
}

/// A newer Agent, downloaded, verified and staged beside the installed one.
struct Staged {
    version: String,
    executable: std::path::PathBuf,
    /// Taken before staging; see [`crate::update_files`].
    lock: InstallLock,
}

impl Staged {
    /// Starts its installer, which waits for the coordinator to stop.
    fn start(self) -> anyhow::Result<()> {
        start_installer(&self.executable)?;
        // Released as this process exits, which the installer waits for.
        self.lock.hold_until_exit();
        Ok(())
    }
}

/// Removes the staged releases and swapped-out builds earlier updates left in
/// `install_directory`, unless an installer holds the lock in `updates` and so may still need
/// them.
fn remove_leftovers_unless_installing(
    updates: &Path,
    install_directory: &Path,
) -> anyhow::Result<()> {
    if let Some(_lock) = InstallLock::try_take(updates)? {
        remove_leftovers(install_directory);
    }
    Ok(())
}

fn remove_leftovers(install_directory: &Path) {
    let removed = update_files::remove_leftovers(install_directory, STAGED_PREFIX);
    if removed > 0 {
        tracing::info!(removed, "removed files left by earlier Agent updates");
    }
}

/// Downloads, verifies and stages a newer Agent, if there is one.
fn stage(config: &Config) -> anyhow::Result<Option<Staged>> {
    let http = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(600)))
        .tls_config(crate::enrollment::https_tls_config())
        .build()
        .new_agent();
    let manifest = read_limited(
        http.get(&config.update_manifest_url)
            .call()
            .context("could not download the update manifest")?
            .into_body()
            .into_reader(),
        MAX_MANIFEST_BYTES,
    )?;
    let manifest = UpdateManifest::parse(&manifest)?;
    let Some(release) = manifest.newer_release(TARGET, CURRENT_VERSION)? else {
        return Ok(None);
    };
    let updates = updates_directory()?;
    let attempts_path = updates.join(ATTEMPTS_FILE);
    let Some(attempt) = UpdateAttempts::next(
        read_attempts(&attempts_path).as_ref(),
        &release.version,
        unix_seconds(),
    ) else {
        tracing::warn!(version = %release.version, "skipping an Agent update that already failed repeatedly");
        return Ok(None);
    };
    // Held until the installer takes over, so nothing removes what is staged meanwhile.
    let lock =
        InstallLock::try_take(&updates)?.context("an Agent update is still being installed")?;
    remove_leftovers(Path::new(INSTALL_DIRECTORY));
    tracing::info!(version = %release.version, "downloading an Agent update");
    let executable = read_limited(
        http.get(&release.url)
            .call()
            .context("could not download the Agent update")?
            .into_body()
            .into_reader(),
        MAX_UPDATE_BYTES,
    )?;
    // Only a complete download uses an attempt, so one cut short is retried on the backoff.
    super::installer::replace_file(&attempts_path, &serde_json::to_vec(&attempt)?)?;
    release.verify(&executable)?;
    // Beside the installed Agent, so the swap is a rename on one file system
    // and the new file gets the folder's SELinux label.
    let staged = Path::new(INSTALL_DIRECTORY).join(format!("{STAGED_PREFIX}{}", release.version));
    super::installer::replace_file(&staged, &executable)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    Ok(Some(Staged {
        version: release.version,
        executable: staged,
        lock,
    }))
}

fn updates_directory() -> anyhow::Result<std::path::PathBuf> {
    let directory = super::installer::config_directory()?.join("updates");
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// Whether the installer of this build is still waiting for it to connect.
fn confirming() -> bool {
    updates_directory()
        .is_ok_and(|updates| crate::update_policy::confirming(&updates, unix_seconds()))
}

/// Records that the coordinator connected, which confirms an update being installed.
pub(crate) fn record_connected() -> anyhow::Result<()> {
    crate::update_policy::record_connected(&updates_directory()?)
}

/// Writes when the next check is due, so a restart keeps its place in the schedule.
fn record_next_check(next: &NextCheck) {
    let written = updates_directory().and_then(|updates| {
        super::installer::replace_file(&updates.join(NEXT_CHECK_FILE), &serde_json::to_vec(next)?)
    });
    if let Err(error) = written {
        tracing::warn!(error = ?error, "could not record when the next Agent update check is due");
    }
}

fn read_limited(reader: impl Read, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= limit, "the download is too large");
    Ok(bytes)
}

/// Starts the staged Agent's `--apply-update` in its own systemd unit; a
/// child of the service would be stopped with it.
fn start_installer(staged: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("systemd-run")
        .args([
            "--unit",
            &format!("meshrmm-agent-update-{}", unix_seconds()),
            "--collect",
            "--quiet",
        ])
        .arg(staged)
        .arg("--apply-update")
        .arg(staged)
        .arg(std::process::id().to_string())
        .status()
        .context("could not start the Agent update")?;
    anyhow::ensure!(
        status.success(),
        "systemd-run could not start the Agent update"
    );
    Ok(())
}

/// Entry point of `--apply-update <staged executable> <coordinator pid>`.
pub fn apply(staged: &Path, coordinator: u32) -> anyhow::Result<()> {
    let updates = updates_directory()?;
    let lock = InstallLock::queue(&updates)?;
    update_files::wait_for_coordinator(
        coordinator,
        COORDINATOR_EXIT_TIMEOUT,
        Instant::now,
        std::thread::sleep,
        || update_files::process_exists(coordinator),
        main_pid,
        || {
            if let Err(error) = systemctl(&["kill", "--signal=SIGKILL", SERVICE]) {
                tracing::warn!(error = ?error, "could not end the Agent");
            }
        },
    );
    let _lock = match lock.wait(LOCK_TIMEOUT) {
        Ok(lock) => lock,
        Err(error) => {
            restart();
            return Err(error);
        }
    };
    let result = install(staged, &updates);
    // The staged path holds the new build if it was never installed, the previous one once the
    // update is committed, and the failed one after a rollback.
    match std::fs::remove_file(staged) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!(%error, "could not remove the staged Agent");
        }
        _ => {}
    }
    result
}

/// Swaps the staged build in, and back out unless it proves it works.
fn install(staged: &Path, updates: &Path) -> anyhow::Result<()> {
    // Before the new build starts, so only its own connection confirms it, and it holds back
    // updates of its own meanwhile.
    let confirmation =
        match Confirmation::begin(updates, CURRENT_VERSION, Instant::now(), unix_seconds()) {
            Ok(confirmation) => confirmation,
            Err(error) => {
                restart();
                return Err(error).context("could not prepare to confirm the Agent update");
            }
        };
    if let Err(error) = update_files::exchange(staged, Path::new(EXECUTABLE)) {
        drop(confirmation);
        restart();
        return Err(error).context("could not install the Agent update");
    }
    restart();
    let failure = if !stays_running() {
        "did not stay running"
    } else {
        tracing::info!(
            timeout_minutes = CONFIRM_TIMEOUT.as_secs() / 60,
            "the updated Agent is running; waiting for it to connect to the server"
        );
        if confirmation.wait(Instant::now, std::thread::sleep) {
            println!("Agent {CURRENT_VERSION} installed");
            tracing::info!(
                release_version = CURRENT_VERSION,
                "Agent update installed: the updated Agent connected to the server"
            );
            return Ok(());
        }
        "did not connect to the server in time"
    };
    tracing::error!(
        failure,
        "the updated Agent did not prove it works; restoring the previous Agent"
    );
    let restored = update_files::exchange(staged, Path::new(EXECUTABLE))
        .context("could not restore the previous Agent");
    drop(confirmation);
    restart();
    restored?;
    bail!("the updated Agent {failure}; restored the previous Agent")
}

fn restart() {
    if let Err(error) = systemctl(&["restart", SERVICE]) {
        tracing::warn!(error = ?error, "could not restart the Agent service");
    }
}

/// Whether the service runs, and keeps the same process, for a while.
fn stays_running() -> bool {
    std::thread::sleep(Duration::from_secs(3));
    let first = main_pid();
    std::thread::sleep(HEALTHY_AFTER);
    first.is_some() && main_pid() == first
}

fn main_pid() -> Option<u32> {
    systemctl(&["show", "--property", "MainPID", "--value", SERVICE])
        .ok()?
        .parse()
        .ok()
        .filter(|&pid| pid != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_downloads_up_to_the_limit() {
        assert_eq!(read_limited(&b"abc"[..], 3).unwrap(), b"abc");
        assert!(read_limited(&b"abcd"[..], 3).is_err());
    }

    #[test]
    fn removes_what_earlier_updates_left_unless_an_installer_runs() {
        let root = std::env::temp_dir().join(format!(
            "meshrmm-linux-update-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let updates = root.join("updates");
        let install = root.join("opt");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::create_dir_all(&install).unwrap();
        for name in [
            "meshrmm-agent",
            &format!("{STAGED_PREFIX}1.2.3"),
            &format!("{STAGED_PREFIX}1.2.4.new"),
        ] {
            std::fs::write(install.join(name), name).unwrap();
        }
        let names = || {
            let mut names = std::fs::read_dir(&install)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        // An installer that runs, or waits for its build to connect, may still need them.
        let installer = InstallLock::try_take(&updates).unwrap().unwrap();
        remove_leftovers_unless_installing(&updates, &install).unwrap();
        assert_eq!(names().len(), 3);
        drop(installer);
        remove_leftovers_unless_installing(&updates, &install).unwrap();
        assert_eq!(names(), ["meshrmm-agent"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
