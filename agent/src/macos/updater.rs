//! Automatic updates of the installed macOS Agent.
//!
//! The coordinator checks the release manifest on the same schedule as the
//! Windows service, stages a newer Agent once its SHA-256 and code signature
//! check out, and starts the new Agent's `--apply-update`. That process waits
//! for the coordinator to stop, swaps the app bundle, restarts the launchd
//! jobs, and puts the previous bundle back if the new coordinator does not
//! stay up or does not connect to the server within [`CONFIRM_TIMEOUT`]. See
//! [`crate::update_files`] for how the swap, and the removal of what earlier
//! updates left, work.
//!
//! An Agent signed with a Developer ID takes only the server's build signed
//! by the same team; an ad-hoc signed one takes the release build, which the
//! release key vouches for.
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_self_update::{AGENT_MACOS, CURRENT_VERSION, UpdateManifest, macos};

use super::installer::{APP, BUNDLE_IDENTIFIER, DAEMON_LABEL, HELPER_LABEL};
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
/// A release is staged in a folder of the update directory named this, its version and the
/// staging coordinator's process ID. After the swap the folder holds the previous bundle, and
/// after a rollback the failed one.
const STAGING_PREFIX: &str = "agent-";
/// In the update directory: the installer's output from the last update that failed.
const FAILED_LOG_FILE: &str = "failed-update.log";

/// Checks for updates until one is staged, then stops the coordinator so the
/// new Agent can replace it.
pub(crate) async fn run(config: Config, link: Arc<ServiceLink>) {
    let _ = tokio::task::spawn_blocking(|| {
        if let Err(error) =
            updates_directory().and_then(|updates| remove_leftovers_unless_installing(&updates))
        {
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

fn updates_directory() -> anyhow::Result<PathBuf> {
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

/// A newer Agent, downloaded, verified and unpacked in its staging folder.
struct Staged {
    version: String,
    app: PathBuf,
    staging: PathBuf,
    /// Taken before staging; see [`crate::update_files`].
    lock: InstallLock,
}

impl Staged {
    /// Starts its installer, which waits for the coordinator to stop.
    fn start(self) -> anyhow::Result<()> {
        start_installer(&self.app, &self.staging)?;
        // Released as this process exits, which the installer waits for.
        self.lock.hold_until_exit();
        Ok(())
    }
}

/// Removes the staging folders earlier updates left in `updates`, with the downloads and
/// bundles they hold, unless an installer holds the lock there and so may still need them.
fn remove_leftovers_unless_installing(updates: &Path) -> anyhow::Result<()> {
    if let Some(_lock) = InstallLock::try_take(updates)? {
        remove_leftovers(updates);
    }
    Ok(())
}

fn remove_leftovers(updates: &Path) {
    let removed = update_files::remove_leftovers(updates, STAGING_PREFIX);
    if removed > 0 {
        tracing::info!(removed, "removed files left by earlier Agent updates");
    }
}

/// Downloads, verifies and unpacks a newer Agent, if there is one.
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
    // Without this team's signature, macOS would also forget the Agent's
    // privacy permissions.
    let team = macos::team_identifier(Path::new(APP));
    let release = match &team {
        Some(_) => manifest.newer_developer_id_release(AGENT_MACOS, CURRENT_VERSION)?,
        None => manifest.newer_release(AGENT_MACOS, CURRENT_VERSION)?,
    };
    let Some(release) = release else {
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
    remove_leftovers(&updates);
    tracing::info!(version = %release.version, "downloading an Agent update");
    let archive = read_limited(
        http.get(&release.url)
            .call()
            .context("could not download the Agent update")?
            .into_body()
            .into_reader(),
        MAX_UPDATE_BYTES,
    )?;
    // Only a complete download uses an attempt, so one cut short is retried on the backoff.
    super::installer::replace_file(&attempts_path, &serde_json::to_vec(&attempt)?)?;
    release.verify(&archive)?;
    let staging = updates.join(format!(
        "{STAGING_PREFIX}{}-{}",
        release.version,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let archive_path = staging.join("agent.zip");
    std::fs::write(&archive_path, &archive)?;
    command(
        "/usr/bin/ditto",
        &[
            "-x".as_ref(),
            "-k".as_ref(),
            archive_path.as_os_str(),
            staging.as_os_str(),
        ],
    )?;
    let app = staging.join("MeshRMM Agent.app");
    match &team {
        Some(team) => macos::verify_developer_id(&app, BUNDLE_IDENTIFIER, team, &release.version)?,
        None => macos::verify_signature(&app)?,
    }
    Ok(Some(Staged {
        version: release.version,
        app,
        staging,
        lock,
    }))
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

fn start_installer(app: &Path, staging: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let log = std::fs::File::create(staging.join("apply.log"))?;
    let mut command = std::process::Command::new(app.join("Contents/MacOS/meshrmm-agent"));
    command
        .arg("--apply-update")
        .arg(app)
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // Its own session keeps launchd from stopping it with the coordinator.
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
        .spawn()
        .context("could not start the Agent update")?;
    Ok(())
}

/// Entry point of `--apply-update <staged app> <coordinator pid>`.
pub fn apply(staged: &Path, coordinator: u32) -> anyhow::Result<()> {
    let updates = updates_directory()?;
    let lock = InstallLock::queue(&updates)?;
    update_files::wait_for_coordinator(
        coordinator,
        COORDINATOR_EXIT_TIMEOUT,
        Instant::now,
        std::thread::sleep,
        || update_files::process_exists(coordinator),
        coordinator_pid,
        || {
            let job = format!("system/{DAEMON_LABEL}");
            if let Err(error) = command(
                "/bin/launchctl",
                &["kill".as_ref(), "SIGKILL".as_ref(), job.as_ref()],
            ) {
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
    // The folder holds the download, and the new bundle if it was never installed, the previous
    // one once the update is committed, or the failed one after a rollback.
    if let Some(staging) = staging_folder(staged, &updates) {
        let failed_log = updates.join(FAILED_LOG_FILE);
        update_files::discard_staging(staging, result.is_err().then_some(failed_log.as_path()));
    }
    result
}

/// The staging folder in `updates` that `staged` was unpacked in.
fn staging_folder<'a>(staged: &'a Path, updates: &Path) -> Option<&'a Path> {
    staged.parent().filter(|staging| {
        staging.parent() == Some(updates)
            && staging
                .file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(STAGING_PREFIX))
    })
}

/// Swaps the staged bundle in, and back out unless it proves it works.
fn install(staged: &Path, updates: &Path) -> anyhow::Result<()> {
    // The new Agent secures its own bundle, whatever the Agent that staged
    // it did.
    super::installer::secure_bundle(staged)?;
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
    if let Err(error) = update_files::exchange(staged, Path::new(APP)) {
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
            // Only for a build that is kept: the previous one, now where the staged one was,
            // keeps its permissions if the update is rolled back.
            if let Err(error) = super::installer::forget_lost_permissions(staged, Path::new(APP)) {
                tracing::warn!(error = ?error, "the Agent's privacy permissions may be out of date");
            }
            return Ok(());
        }
        "did not connect to the server in time"
    };
    tracing::error!(
        failure,
        "the updated Agent did not prove it works; restoring the previous Agent"
    );
    let restored = update_files::exchange(staged, Path::new(APP))
        .context("could not restore the previous Agent");
    drop(confirmation);
    restart();
    restored?;
    bail!("the updated Agent {failure}; restored the previous Agent")
}

fn restart() {
    for uid in super::installer::graphical_users() {
        let _ = command(
            "/bin/launchctl",
            &[
                "kickstart".as_ref(),
                "-k".as_ref(),
                format!("gui/{uid}/{HELPER_LABEL}").as_ref(),
            ],
        );
    }
    let _ = command(
        "/bin/launchctl",
        &[
            "kickstart".as_ref(),
            "-k".as_ref(),
            format!("system/{DAEMON_LABEL}").as_ref(),
        ],
    );
}

/// Whether the coordinator runs, and keeps the same process, for a while.
fn stays_running() -> bool {
    std::thread::sleep(Duration::from_secs(3));
    let first = coordinator_pid();
    std::thread::sleep(HEALTHY_AFTER);
    first.is_some() && coordinator_pid() == first
}

fn coordinator_pid() -> Option<u32> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(["print", &format!("system/{DAEMON_LABEL}")])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

fn command(program: &str, arguments: &[&std::ffi::OsStr]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .with_context(|| format!("could not run {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
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
    fn discards_only_staging_folders_in_the_update_directory() {
        let updates = Path::new("/Library/Application Support/MeshRMM/Agent/updates");
        let staging = updates.join(format!("{STAGING_PREFIX}1.2.3-41"));
        assert_eq!(
            staging_folder(&staging.join("MeshRMM Agent.app"), updates),
            Some(staging.as_path())
        );
        assert_eq!(
            staging_folder(Path::new("/Applications/MeshRMM Agent.app"), updates),
            None
        );
        assert_eq!(
            staging_folder(&updates.join("MeshRMM Agent.app"), updates),
            None
        );
    }
}
