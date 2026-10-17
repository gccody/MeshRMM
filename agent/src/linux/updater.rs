//! Automatic updates of the installed Linux Agent.
//!
//! The coordinator checks the release manifest on the same schedule as the
//! other platforms, and stages a newer Agent beside the installed one once
//! its SHA-256 and release signature check out. It starts the new Agent's
//! `--apply-update` in a transient systemd unit, which outlives the service:
//! that process waits for the coordinator to stop, swaps the executable,
//! restarts the service, and puts the previous executable back if the new
//! coordinator does not stay up.
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
use crate::update_policy::{UpdateAttempts, UpdateSchedule, read_attempts};

const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
const SCHEDULE_TICK: Duration = Duration::from_secs(30);
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;
/// The coordinator ends remote sessions and runs their close actions first.
const COORDINATOR_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the new coordinator must keep running for the update to count.
const HEALTHY_AFTER: Duration = Duration::from_secs(15);
const ATTEMPTS_FILE: &str = "update-attempts.json";

/// This build's release target.
#[cfg(target_arch = "x86_64")]
const TARGET: &str = meshrmm_self_update::AGENT_LINUX_X64;
#[cfg(target_arch = "aarch64")]
const TARGET: &str = meshrmm_self_update::AGENT_LINUX_ARM64;

/// Checks for updates until one is staged, then stops the coordinator so the
/// new Agent can replace it.
pub(crate) async fn run(config: Config, link: Arc<ServiceLink>) {
    tokio::time::sleep(FIRST_CHECK_DELAY).await;
    let mut schedule = UpdateSchedule::new(Instant::now());
    loop {
        if schedule.due(Instant::now(), link.session_active()) {
            let check_config = config.clone();
            match tokio::task::spawn_blocking(move || stage(&check_config)).await {
                Ok(Ok(Some(version))) => {
                    tracing::info!(%version, "stopping the Agent to install an update");
                    link.stop_for_update(version);
                    return;
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) => tracing::warn!(error = ?error, "Agent update check failed"),
                Err(error) => tracing::warn!(%error, "Agent update check stopped"),
            }
        }
        tokio::time::sleep(SCHEDULE_TICK).await;
    }
}

/// Downloads and verifies a newer Agent and starts its installer. Returns the
/// staged version.
fn stage(config: &Config) -> anyhow::Result<Option<String>> {
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
    let updates = super::installer::config_directory()?.join("updates");
    std::fs::create_dir_all(&updates)?;
    let attempts_path = updates.join(ATTEMPTS_FILE);
    let Some(attempt) = UpdateAttempts::next(
        read_attempts(&attempts_path).as_ref(),
        &release.version,
        unix_seconds(),
    ) else {
        tracing::warn!(version = %release.version, "skipping an Agent update that already failed repeatedly");
        return Ok(None);
    };
    super::installer::replace_file(&attempts_path, &serde_json::to_vec(&attempt)?)?;
    tracing::info!(version = %release.version, "downloading an Agent update");
    let executable = read_limited(
        http.get(&release.url)
            .call()
            .context("could not download the Agent update")?
            .into_body()
            .into_reader(),
        MAX_UPDATE_BYTES,
    )?;
    release.verify(&executable)?;
    // Beside the installed Agent, so the swap is a rename on one file system
    // and the new file gets the folder's SELinux label.
    let staged = Path::new(INSTALL_DIRECTORY).join(format!("meshrmm-agent-{}", release.version));
    super::installer::replace_file(&staged, &executable)?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    start_installer(&staged)?;
    Ok(Some(release.version))
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
pub fn apply(staged: &Path, coordinator: libc::pid_t) -> anyhow::Result<()> {
    let deadline = Instant::now() + COORDINATOR_EXIT_TIMEOUT;
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(coordinator, 0) } == 0 {
        if Instant::now() >= deadline {
            // SAFETY: as above; SIGKILL ends the stuck coordinator.
            unsafe { libc::kill(coordinator, libc::SIGKILL) };
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let previous = Path::new(INSTALL_DIRECTORY).join("meshrmm-agent.previous");
    let _ = std::fs::remove_file(&previous);
    std::fs::rename(EXECUTABLE, &previous).context("could not set the installed Agent aside")?;
    if let Err(error) = std::fs::rename(staged, EXECUTABLE) {
        let _ = std::fs::rename(&previous, EXECUTABLE);
        restart();
        return Err(error).context("could not install the Agent update");
    }
    restart();
    if stays_running() {
        println!("Agent {CURRENT_VERSION} installed");
        let _ = std::fs::remove_file(&previous);
        return Ok(());
    }
    let failed = Path::new(INSTALL_DIRECTORY).join("meshrmm-agent.failed");
    let _ = std::fs::rename(EXECUTABLE, &failed);
    std::fs::rename(&previous, EXECUTABLE).context("could not restore the previous Agent")?;
    restart();
    bail!("the updated Agent did not stay running; restored the previous Agent")
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

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_downloads_up_to_the_limit() {
        assert_eq!(read_limited(&b"abc"[..], 3).unwrap(), b"abc");
        assert!(read_limited(&b"abcd"[..], 3).is_err());
    }
}
