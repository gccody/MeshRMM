//! Automatic updates of the installed macOS Agent.
//!
//! The coordinator checks the release manifest on the same schedule as the
//! Windows service, stages a newer Agent once its SHA-256 and code signature
//! check out, and starts the new Agent's `--apply-update`. That process waits
//! for the coordinator to stop, swaps the app bundle, restarts the launchd
//! jobs, and puts the previous bundle back if the new coordinator does not
//! stay up.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_self_update::{AGENT_MACOS, CURRENT_VERSION, UpdateManifest};

use super::installer::{APP, DAEMON_LABEL, HELPER_LABEL, SUPPORT_DIRECTORY};
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

fn updates_directory() -> anyhow::Result<PathBuf> {
    let directory = super::installer::config_directory()?.join("updates");
    std::fs::create_dir_all(&directory)?;
    Ok(directory)
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
    let Some(release) =
        UpdateManifest::parse(&manifest)?.newer_release(AGENT_MACOS, CURRENT_VERSION)?
    else {
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
    super::installer::replace_file(&attempts_path, &serde_json::to_vec(&attempt)?)?;
    tracing::info!(version = %release.version, "downloading an Agent update");
    let archive = read_limited(
        http.get(&release.url)
            .call()
            .context("could not download the Agent update")?
            .into_body()
            .into_reader(),
        MAX_UPDATE_BYTES,
    )?;
    release.verify(&archive)?;
    let staging = updates.join(format!("agent-{}-{}", release.version, std::process::id()));
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
    verify_signature(&app, Path::new(APP))?;
    start_installer(&app, &staging)?;
    Ok(Some(release.version))
}

fn read_limited(reader: impl Read, limit: u64) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= limit, "the download is too large");
    Ok(bytes)
}

/// Requires a valid signature, and the installed Agent's developer team when
/// the installed Agent has one, so a download cannot swap in someone else's code.
fn verify_signature(staged: &Path, installed: &Path) -> anyhow::Result<()> {
    command(
        "/usr/bin/codesign",
        &[
            "--verify".as_ref(),
            "--strict".as_ref(),
            "--deep".as_ref(),
            staged.as_os_str(),
        ],
    )
    .context("the Agent update's code signature is invalid")?;
    if let Some(team) = team_identifier(installed)
        && team_identifier(staged).as_deref() != Some(team.as_str())
    {
        bail!("the Agent update is not signed by the installed Agent's developer team {team}");
    }
    Ok(())
}

fn team_identifier(app: &Path) -> Option<String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=2"])
        .arg(app)
        .output()
        .ok()?;
    // codesign describes the signature on standard error.
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_owned)
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
    // The new Agent secures its own bundle, whatever the Agent that staged
    // it did.
    super::installer::secure_bundle(staged)?;
    let previous = Path::new(SUPPORT_DIRECTORY).join("MeshRMM Agent.app.previous");
    let _ = std::fs::remove_dir_all(&previous);
    std::fs::rename(APP, &previous).context("could not set the installed Agent aside")?;
    if let Err(error) = std::fs::rename(staged, APP) {
        let _ = std::fs::rename(&previous, APP);
        restart();
        return Err(error).context("could not install the Agent update");
    }
    restart();
    if stays_running() {
        println!("Agent {CURRENT_VERSION} installed");
        let _ = std::fs::remove_dir_all(&previous);
        if let Some(staging) = staged.parent() {
            let _ = std::fs::remove_dir_all(staging);
        }
        return Ok(());
    }
    let failed = Path::new(SUPPORT_DIRECTORY).join("MeshRMM Agent.app.failed");
    let _ = std::fs::remove_dir_all(&failed);
    let _ = std::fs::rename(APP, &failed);
    std::fs::rename(&previous, APP).context("could not restore the previous Agent")?;
    restart();
    bail!("the updated Agent did not stay running; restored the previous Agent")
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

    #[test]
    fn accepts_signed_updates_and_refuses_altered_ones() {
        let directory = std::env::temp_dir().join(format!("meshrmm-update-{}", std::process::id()));
        let app = directory.join("MeshRMM Agent.app");
        let executable = app.join("Contents/MacOS/meshrmm-agent");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::copy("/usr/bin/true", &executable).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            super::super::installer::info_plist(),
        )
        .unwrap();
        command(
            "/usr/bin/codesign",
            &[
                "--force".as_ref(),
                "--sign".as_ref(),
                "-".as_ref(),
                app.as_os_str(),
            ],
        )
        .unwrap();
        // An ad-hoc installed Agent has no team, so only the signature counts.
        verify_signature(&app, Path::new("/nonexistent/MeshRMM Agent.app")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&executable)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"tampered"))
            .unwrap();
        assert!(verify_signature(&app, Path::new("/nonexistent/MeshRMM Agent.app")).is_err());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn finds_no_team_for_an_unsigned_path() {
        assert_eq!(
            team_identifier(Path::new("/nonexistent/MeshRMM Agent.app")),
            None
        );
    }
}
