use std::ffi::OsStr;
use std::io::{Read, Seek};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::CURRENT_VERSION;
use meshrmm_self_update::windows::{
    PreviousProcess, current_process_created, helper_cleanup_command,
};
use tracing_subscriber::EnvFilter;
use windows::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING;
use windows_service::service::{Service, ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::private_directory;
use crate::remote::config::Config;
use crate::service::service_name_for_path;
use crate::update_policy::{
    CONFIRM_TIMEOUT, Confirmation, MAX_ATTEMPTS_PER_VERSION, NEXT_CHECK_FILE, NextCheck,
    UpdateAttempts, read_attempts, read_next_check, unix_seconds,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;
const ATTEMPTS_FILE: &str = "update-attempts.json";
/// The stopping service waits up to 5 seconds for each tray and 25 seconds for its coordinator to
/// end remote sessions and run their close actions.
const PREVIOUS_INSTANCE_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINATED_INSTANCE_TIMEOUT: Duration = Duration::from_secs(15);
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// An update that stops right after reporting Running is rolled back at once, rather than once
/// it has failed to connect for [`CONFIRM_TIMEOUT`].
const UPDATE_STARTUP_GRACE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// This build's release target.
#[cfg(target_arch = "x86_64")]
const TARGET: &str = meshrmm_self_update::AGENT_WINDOWS_X64;
#[cfg(target_arch = "aarch64")]
const TARGET: &str = meshrmm_self_update::AGENT_WINDOWS_ARM64;

/// A downloaded and verified release, with the helper that installs it, waiting for the Agent
/// to be free to stop. Files left by one that never starts are removed when the service next
/// starts (see [`remove_stale_files`]).
pub struct StagedUpdate {
    version: String,
    current: PathBuf,
    staged: PathBuf,
    helper: PathBuf,
    update_directory: PathBuf,
    json_logs: bool,
}

impl StagedUpdate {
    /// Starts the helper and returns the version it installs, after which the service must stop.
    pub fn start(self) -> anyhow::Result<String> {
        let mut command = Command::new(&self.helper);
        command
            .arg("--apply-agent-update")
            .arg(&self.current)
            .arg(&self.staged)
            // The helper waits for this process to exit before replacing its executable.
            .arg(std::process::id().to_string())
            // And for this version's coordinator to connect before it commits the update.
            .arg(&self.version);
        if self.json_logs {
            command.arg("--json-logs");
        }
        let started = command
            .current_dir(&self.update_directory)
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start Agent update helper {}",
                    self.helper.display()
                )
            });
        if let Err(error) = started {
            remove_if_present(&self.staged);
            remove_if_present(&self.helper);
            return Err(error);
        }
        tracing::info!(
            release_version = %self.version,
            helper = %self.helper.display(),
            staged = %self.staged.display(),
            "started the Agent update helper"
        );
        Ok(self.version)
    }
}

/// Downloads and stages a newer release, if there is one; [`StagedUpdate::start`] installs it.
pub fn check_and_stage(config: &Config) -> anyhow::Result<Option<StagedUpdate>> {
    let http = http_agent();
    let manifest = crate::update_check::fetch_manifest(&http, config)?;
    let config_directory = config
        .config_path
        .parent()
        .context("Agent configuration has no parent directory")?;
    let Some(release) = manifest.newer_release(TARGET, CURRENT_VERSION)? else {
        return Ok(None);
    };

    let update_directory = prepare_update_directory(config_directory)
        .context("failed to prepare a private Agent update directory")?;
    let attempts_path = update_directory.join(ATTEMPTS_FILE);
    let previous = read_attempts(&attempts_path);
    let Some(attempt) = UpdateAttempts::next(previous.as_ref(), &release.version, unix_seconds())
    else {
        tracing::warn!(
            current_version = CURRENT_VERSION,
            release_version = %release.version,
            attempts = MAX_ATTEMPTS_PER_VERSION,
            "skipping an automatic Agent update that already failed repeatedly; it is retried after 24 hours"
        );
        return Ok(None);
    };

    tracing::info!(
        current_version = CURRENT_VERSION,
        release_version = %release.version,
        attempt = attempt.attempts,
        "downloading automatic Agent update"
    );
    let current = std::env::current_exe().context("could not locate the Agent executable")?;
    let suffix = unique_suffix();
    let staged = update_directory.join(format!("agent-{}-{suffix}.exe", release.version));
    let helper = update_directory.join(format!("update-helper-{suffix}.exe"));
    let body = http
        .get(&release.url)
        .call()
        .context("failed to download the Agent update")?
        .into_body()
        .into_reader();
    let verified = stage_executable(body, &staged, &release, || {
        // Only a complete download uses an attempt, so one cut short is retried on the backoff.
        crate::installer::replace_file(&attempts_path, &serde_json::to_vec(&attempt)?)
            .context("failed to record the Agent update attempt")
    })?;
    // The verified build stays open, so nothing can change it, until the helper exists.
    if let Err(error) = std::fs::copy(&current, &helper) {
        remove_if_present(&helper);
        return Err(error)
            .with_context(|| format!("failed to create Agent update helper {}", helper.display()));
    }
    let staged = verified.keep();
    Ok(Some(StagedUpdate {
        version: release.version,
        current,
        staged,
        helper,
        update_directory,
        json_logs: config.json_logs,
    }))
}

/// When the last successful check said the next one is due.
pub fn scheduled_check(config_directory: &Path) -> Option<NextCheck> {
    read_next_check(&config_directory.join("updates").join(NEXT_CHECK_FILE))
}

/// Whether the helper that installed this build is still waiting for it to connect, so it must
/// not start an update of its own.
pub fn confirming(config_directory: &Path) -> bool {
    crate::update_policy::confirming(&config_directory.join("updates"), unix_seconds())
}

/// Records that the coordinator connected, which confirms an update the helper is installing.
pub fn record_connected(config_directory: &Path) -> anyhow::Result<()> {
    let updates = config_directory.join("updates");
    private_directory::secure(&updates)?;
    crate::update_policy::record_connected(&updates)
}

/// Writes when the next check is due, so a restart keeps its place in the schedule.
pub fn record_next_check(config_directory: &Path, next: &NextCheck) {
    let updates = config_directory.join("updates");
    let written = private_directory::secure(&updates).and_then(|()| {
        crate::installer::replace_file(&updates.join(NEXT_CHECK_FILE), &serde_json::to_vec(next)?)
    });
    if let Err(error) = written {
        tracing::warn!(error = ?error, "could not record when the next Agent update check is due");
    }
}

/// Runs as a copy of the previous Agent after the service that staged the update stops. Whatever
/// fails after that, the helper leaves an Agent executable installed and tries to start the
/// service again, so a failed update does not leave the endpoint offline until it reboots. The
/// update is committed only once the new build's coordinator has connected to the server.
pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    let [target, staged, process_id, version, options @ ..] = arguments.as_slice() else {
        bail!(
            "the Agent update helper requires target and staged executable paths, the service process ID and the release version"
        );
    };
    let version = version
        .to_str()
        .filter(|version| meshrmm_protocol::is_release_version(version))
        .context("the Agent update helper received an invalid release version")?;
    let target = PathBuf::from(target);
    let staged = PathBuf::from(staged);
    let process_id = process_id
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .context("the Agent update helper received an invalid service process ID")?;
    let helper = std::env::current_exe().context("could not locate the Agent update helper")?;
    let helper_directory = helper
        .parent()
        .context("Agent update helper has no parent directory")?
        .to_owned();
    initialize_helper_log(
        &helper_directory,
        options.iter().any(|option| option == "--json-logs"),
    );
    tracing::info!(
        helper = %helper.display(),
        target = %target.display(),
        staged = %staged.display(),
        service_process_id = process_id,
        release_version = version,
        "Agent update helper started"
    );

    let release = Release {
        version,
        updates: &helper_directory,
    };
    let result = install_update(
        service_name_for_path(&target),
        &target,
        &staged,
        process_id,
        &release,
    );
    match &result {
        Ok(()) => tracing::info!(
            release_version = version,
            "Agent update installed: the updated Agent connected to the server"
        ),
        Err(error) => tracing::error!(error = ?error, "Agent update failed"),
    }
    remove_if_present(&staged);
    if let Err(error) = schedule_cleanup(&helper, &helper_directory) {
        tracing::warn!(error = ?error, "could not schedule Agent update helper cleanup");
    }
    result
}

/// Removes helpers and staged executables that earlier updates left behind, for example when an
/// older helper failed before deleting itself. A helper that is still running cannot be deleted,
/// so the one that has just started this service is left to finish and remove itself.
pub fn remove_stale_files(config_directory: &Path) {
    let update_directory = config_directory.join("updates");
    match std::fs::symlink_metadata(&update_directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            tracing::warn!(
                path = %update_directory.display(),
                "the Agent update directory is not a plain directory; leaving it unchanged"
            );
            return;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(%error, "could not inspect the Agent update directory");
            return;
        }
    }
    let entries = match std::fs::read_dir(&update_directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "could not list the Agent update directory");
            return;
        }
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || !is_update_file(&entry.file_name().to_string_lossy())
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => tracing::debug!(
                path = %entry.path().display(),
                %error,
                "left an Agent update file that is still in use"
            ),
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed files left by earlier Agent updates");
    }
}

fn is_update_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".exe") && (name.starts_with("update-helper-") || name.starts_with("agent-"))
}

/// The release a helper installs, and the private update directory its coordinator confirms it in.
struct Release<'a> {
    version: &'a str,
    updates: &'a Path,
}

/// Replaces the Agent once its previous instance has exited, and restores and restarts the
/// previous executable when the update cannot be installed, does not start, or its coordinator
/// does not connect to the server within [`CONFIRM_TIMEOUT`].
fn install_update(
    service_name: &str,
    target: &Path,
    staged: &Path,
    process_id: u32,
    release: &Release,
) -> anyhow::Result<()> {
    if let Err(error) = stop_instance(service_name, target, Some(process_id)) {
        return Err(restart_installed(
            service_name,
            error.context("the previous Agent service instance did not exit"),
        ));
    }
    tracing::info!("the previous Agent service instance exited; replacing its executable");

    // Before the new build starts, so only its own connection confirms it, and it holds back
    // updates of its own meanwhile.
    let confirmation = match Confirmation::begin(
        release.updates,
        release.version,
        Instant::now(),
        unix_seconds(),
    ) {
        Ok(confirmation) => confirmation,
        Err(error) => {
            return Err(restart_installed(
                service_name,
                error.context("could not prepare to confirm the Agent update"),
            ));
        }
    };
    let backup = target.with_extension("exe.previous");
    if let Err(error) = replace_executable(target, staged, &backup) {
        drop(confirmation);
        return Err(restart_installed(service_name, error));
    }
    tracing::info!(target = %target.display(), "installed the Agent update; starting the service");

    let proved = start_and_confirm(service_name, UPDATE_STARTUP_GRACE)
        .context("the updated Agent service did not stay running")
        .and_then(|()| {
            tracing::info!(
                timeout_minutes = CONFIRM_TIMEOUT.as_secs() / 60,
                "the updated Agent service is running; waiting for its coordinator to connect to the server"
            );
            if confirmation.wait(Instant::now, sleep) {
                Ok(())
            } else {
                bail!(
                    "the updated Agent did not connect to the server within {} minutes",
                    CONFIRM_TIMEOUT.as_secs() / 60
                )
            }
        });
    let Err(update_error) = proved else {
        remove_if_present(&backup);
        return Ok(());
    };
    tracing::error!(
        error = ?update_error,
        "the updated Agent did not prove it works; restoring the previous executable"
    );
    let error = match stop_instance(service_name, target, None)
        .and_then(|()| restore_backup(target, &backup))
    {
        Ok(()) => update_error,
        Err(restore_error) => update_error.context(format!(
            "the previous executable could not be restored: {restore_error:#}"
        )),
    };
    drop(confirmation);
    Err(restart_installed(service_name, error))
}

/// Moves the installed executable aside and the staged one into its place, putting the previous
/// executable back if the update cannot be installed.
fn replace_executable(target: &Path, staged: &Path, backup: &Path) -> anyhow::Result<()> {
    // A backup kept by an earlier update would otherwise block the move below.
    remove_if_present(backup);
    std::fs::rename(target, backup)
        .with_context(|| format!("failed to back up installed Agent {}", target.display()))?;
    tracing::info!(backup = %backup.display(), "backed up the installed Agent executable");
    let installed = std::fs::rename(staged, target)
        .with_context(|| format!("failed to install Agent update {}", target.display()))
        .and_then(|()| {
            // Signed-in users start the tray from this executable, but a rename keeps the
            // administrator-only DACL it had in the private update directory.
            private_directory::inherit_parent_security(target)
                .context("failed to let signed-in users run the updated Agent")
        });
    let Err(error) = installed else {
        return Ok(());
    };
    match restore_backup(target, backup) {
        Ok(()) => Err(error),
        Err(restore_error) => Err(error.context(format!(
            "the previous executable could not be restored: {restore_error:#}"
        ))),
    }
}

fn restore_backup(target: &Path, backup: &Path) -> anyhow::Result<()> {
    std::fs::rename(backup, target).with_context(|| {
        format!(
            "failed to restore the previous Agent executable {}",
            target.display()
        )
    })?;
    tracing::info!(target = %target.display(), "restored the previous Agent executable");
    // An executable installed by an older updater may still carry the private update DACL.
    if let Err(error) = private_directory::inherit_parent_security(target) {
        tracing::warn!(error = ?error, "the restored Agent may not be runnable by signed-in users");
    }
    Ok(())
}

/// Starts whichever executable is installed after a failed update and reports the outcome with
/// the update error. Nothing remains to fall back to, and the restarted Agent may stop again at
/// once to retry the update, so it only has to reach Running.
fn restart_installed(service_name: &str, error: anyhow::Error) -> anyhow::Error {
    tracing::error!(error = ?error, "the Agent update failed; restarting the installed Agent");
    match start_and_confirm(service_name, Duration::ZERO) {
        Ok(()) => {
            tracing::info!("the installed Agent service is running again");
            error.context("the Agent update failed; the previously installed Agent is running")
        }
        Err(restart_error) => {
            tracing::error!(error = ?restart_error, "could not restart the Agent service");
            error.context(format!(
                "the Agent update failed and the service could not be restarted: {restart_error:#}"
            ))
        }
    }
}

/// Stops the service and waits until the SCM reports it stopped and the process that ran it has
/// exited. A running executable can still be renamed, so only the process exit shows that the
/// old instance is gone, and starting the service while that instance is stopping fails. An
/// instance that does not exit in time is terminated, since it already committed to stopping.
fn stop_instance(service_name: &str, image: &Path, process_id: Option<u32>) -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        service_name,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    let queried_at = meshrmm_self_update::windows::now();
    let status = service.query_status()?;
    // The instance that scheduled the update started this helper, so it was created first. One
    // the SCM reports now was created before the query. Either way a recycled ID is skipped.
    let (process_id, created_before) = match process_id {
        Some(id) => (Some(id), current_process_created()?),
        None => (status.process_id, queried_at),
    };
    let process_id = process_id.filter(|&id| id != 0);
    let process = process_id.and_then(|id| PreviousProcess::open(id, image, created_before));
    if !matches!(
        status.current_state,
        ServiceState::Stopped | ServiceState::StopPending
    ) {
        // The instance that staged the update may not have reported StopPending yet, and asking
        // it to stop again is harmless.
        let _ = service.stop();
    }
    let Err(error) = wait_for_exit(&service, process.as_ref(), PREVIOUS_INSTANCE_TIMEOUT) else {
        return Ok(());
    };
    let Some(process) = process.as_ref().filter(|process| !process.has_exited()) else {
        return Err(error);
    };
    tracing::warn!(
        error = ?error,
        process_id,
        "the Agent service instance did not exit in time; terminating it"
    );
    process
        .terminate()
        .context("failed to terminate the Agent service instance")?;
    wait_for_exit(&service, Some(process), TERMINATED_INSTANCE_TIMEOUT)
}

fn wait_for_exit(
    service: &Service,
    process: Option<&PreviousProcess>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = service.query_status()?.current_state;
        let exited = process.is_none_or(PreviousProcess::has_exited);
        if state == ServiceState::Stopped && exited {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let process = if exited { "exited" } else { "is still running" };
            bail!(
                "after {} seconds the Agent service is {state:?} and its process {process}",
                timeout.as_secs()
            );
        }
        sleep(POLL_INTERVAL);
    }
}

fn start_and_confirm(service_name: &str, grace: Duration) -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        service_name,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    match service.start::<&OsStr>(&[]) {
        Ok(()) => {}
        // Service recovery may have started it already.
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING.0 as i32) => {}
        Err(error) => return Err(error).context("failed to start the Agent service"),
    }
    let deadline = Instant::now() + START_TIMEOUT;
    let process_id = loop {
        let status = service.query_status()?;
        match status.current_state {
            ServiceState::Running => break status.process_id,
            ServiceState::Stopped => bail!("the Agent service stopped while starting"),
            _ if Instant::now() >= deadline => bail!(
                "the Agent service did not start within {} seconds",
                START_TIMEOUT.as_secs()
            ),
            _ => sleep(POLL_INTERVAL),
        }
    };
    let settled = Instant::now() + grace;
    while Instant::now() < settled {
        sleep(POLL_INTERVAL);
        let status = service.query_status()?;
        if status.current_state != ServiceState::Running || status.process_id != process_id {
            bail!("the Agent service stopped shortly after starting");
        }
    }
    Ok(())
}

/// The helper starts before the Agent's logging and exits right after its last step, so it
/// appends to the Agent log synchronously rather than through the bounded asynchronous writer.
fn initialize_helper_log(helper_directory: &Path, json: bool) {
    let Some(config_directory) = helper_directory.parent() else {
        return;
    };
    let Ok(log) = meshrmm_log_file::RotatingFile::open(config_directory.join("agent.log")) else {
        return;
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(log))
        .with_ansi(false);
    let _ = if json {
        builder.json().try_init()
    } else {
        builder.try_init()
    };
}

fn remove_if_present(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::info!(path = %path.display(), "removed Agent update file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not remove Agent update file");
        }
    }
}

/// The helper staged here runs as LocalSystem, so every directory that could rename or replace it
/// must be administrator-only. The installer-managed ProgramData chain is secured as a whole; the
/// parents of a custom configuration directory are not the Agent's to change.
fn prepare_update_directory(config_directory: &Path) -> anyhow::Result<PathBuf> {
    if crate::installer::is_managed_config_directory(config_directory)? {
        let data_root = config_directory
            .parent()
            .context("Agent configuration directory has no parent directory")?;
        private_directory::secure(data_root)?;
        private_directory::secure(config_directory)?;
        // Existing installations were protected with icacls, which left extra ACEs in place.
        private_directory::secure_contents(config_directory)?;
    }
    let update_directory = config_directory.join("updates");
    private_directory::secure(&update_directory)?;
    Ok(update_directory)
}

pub(crate) use crate::enrollment::https_tls_config;

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(true)
        .tls_config(https_tls_config())
        .build()
        .new_agent()
}

/// Downloads the Agent update from `body` into `staged`, a new file in the private update
/// directory, and checks that it is the release and a Windows executable; see
/// [`crate::update_download`]. `complete` runs once the whole of it has arrived.
fn stage_executable(
    body: impl std::io::Read,
    staged: &Path,
    release: &meshrmm_self_update::Release,
    complete: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<crate::update_download::Verified> {
    let verified =
        crate::update_download::download(body, staged, MAX_UPDATE_BYTES, release, complete)?;
    let mut header = [0; 2];
    let mut file = verified.file();
    file.seek(std::io::SeekFrom::Start(0))
        .and_then(|_| file.read_exact(&mut header))
        .context("failed to read the downloaded Agent update")?;
    if header != *b"MZ" {
        bail!("downloaded Agent update is not a Windows executable");
    }
    Ok(verified)
}

fn schedule_cleanup(helper: &Path, helper_directory: &Path) -> anyhow::Result<()> {
    let working_directory = helper_directory
        .parent()
        .context("Agent update directory has no parent directory")?;
    helper_cleanup_command(helper, helper_directory, working_directory)
        .spawn()
        .context("failed to schedule Agent update cleanup")?;
    Ok(())
}

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update_download::tests::{Staging, crash_during_download, release_of_bytes};
    use meshrmm_signaling_client::test_support::TlsServer;
    use rustls::version::{TLS12, TLS13};

    #[test]
    fn downloads_offer_only_tls_13() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (tls12, tls13) = runtime.block_on(async {
            (
                TlsServer::start(&TLS12).await,
                TlsServer::start(&TLS13).await,
            )
        });
        let http = http_agent();
        let url = |server: &TlsServer| format!("https://127.0.0.1:{}/", server.address.port());

        let refused = format!("{:?}", http.get(url(&tls12)).call().unwrap_err());
        assert!(refused.contains("ProtocolVersion"), "{refused}");
        // TLS 1.3 gets as far as certificate verification, which rejects the
        // test server's self-signed certificate.
        let untrusted = format!("{:?}", http.get(url(&tls13)).call().unwrap_err());
        assert!(untrusted.contains("InvalidCertificate"), "{untrusted}");
    }

    #[test]
    fn recognizes_only_files_the_updater_stages() {
        assert!(is_update_file("update-helper-10188-1787741428068.exe"));
        assert!(is_update_file("agent-0.2.8-10188-1787741428068.exe"));
        assert!(is_update_file("Update-Helper-1.EXE"));
        assert!(!is_update_file(ATTEMPTS_FILE));
        assert!(!is_update_file("agent.json"));
        assert!(!is_update_file("meshrmm-agent.exe"));
    }

    #[test]
    fn removes_leftover_update_files_only() {
        let config = std::env::temp_dir().join(format!(
            "meshrmm-update-cleanup-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let updates = config.join("updates");
        std::fs::create_dir_all(updates.join("agent-directory.exe")).unwrap();
        for name in [
            "update-helper-1-2.exe",
            "agent-0.2.8-1-2.exe",
            ATTEMPTS_FILE,
        ] {
            std::fs::write(updates.join(name), b"MZ").unwrap();
        }
        remove_stale_files(&config);
        let mut remaining = std::fs::read_dir(&updates)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        remaining.sort();
        assert_eq!(remaining, ["agent-directory.exe", ATTEMPTS_FILE]);
        std::fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn removes_a_download_a_crash_cut_short_at_the_next_start() {
        let config = Staging::new();
        let updates = config.0.join("updates");
        std::fs::create_dir(&updates).unwrap();
        let name = format!("agent-1.2.3-{}.exe", unique_suffix());
        crash_during_download(&updates, &name);
        remove_stale_files(&config.0);
        assert!(std::fs::read_dir(&updates).unwrap().next().is_none());
    }

    #[test]
    fn stages_only_a_verified_windows_executable() {
        let staging = Staging::new();
        let build = b"MZ a Windows build".repeat(10_000);
        let path = staging
            .0
            .join(format!("agent-1.2.3-{}.exe", unique_suffix()));
        let verified =
            stage_executable(&build[..], &path, &release_of_bytes(&build), || Ok(())).unwrap();
        assert_eq!(verified.keep(), path);
        assert_eq!(std::fs::read(&path).unwrap(), build);
        std::fs::remove_file(&path).unwrap();
        // A signed build that is not an executable is refused, and removed.
        let script = b"#!/bin/sh".repeat(10_000);
        let error = stage_executable(&script[..], &path, &release_of_bytes(&script), || Ok(()))
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("not a Windows executable"));
        assert!(staging.names().is_empty(), "{:?}", staging.names());
    }

    #[test]
    fn a_staged_build_takes_the_private_update_directory_security() {
        if !private_directory::test_support::elevated() {
            return;
        }
        let config = private_directory::test_support::scratch("staged-update");
        let updates = prepare_update_directory(&config).unwrap();
        let build = b"MZ a Windows build".to_vec();
        let path = updates.join(format!("agent-1.2.3-{}.exe", unique_suffix()));
        let staged = stage_executable(&build[..], &path, &release_of_bytes(&build), || Ok(()))
            .unwrap()
            .keep();
        assert_eq!(
            private_directory::test_support::sddl_of(&staged),
            private_directory::test_support::INHERITED_FILE
        );
        private_directory::test_support::remove(&config);
    }
}
