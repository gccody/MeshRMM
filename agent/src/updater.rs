use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::{AGENT_WINDOWS_X64, CURRENT_VERSION, UpdateManifest};
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SERVICE_ALREADY_RUNNING, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;
use windows_service::service::{Service, ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::private_directory;
use crate::remote::config::Config;
use crate::service::service_name_for_path;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;
/// A failed update restarts the previous Agent, which checks for updates again at once, so a
/// release that cannot be installed is only retried this many times per window.
const MAX_ATTEMPTS_PER_VERSION: u32 = 3;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
const ATTEMPTS_FILE: &str = "update-attempts.json";
/// The stopping service waits up to 5 seconds for each tray and 25 seconds for its coordinator to
/// end remote sessions and run their close actions.
const PREVIOUS_INSTANCE_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINATED_INSTANCE_TIMEOUT: Duration = Duration::from_secs(15);
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// An update that stops right after reporting Running is rolled back too.
const UPDATE_STARTUP_GRACE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub fn check_and_schedule(config: &Config) -> anyhow::Result<bool> {
    let http = http_agent();
    let manifest_bytes = download(&http, &config.update_manifest_url, MAX_MANIFEST_BYTES)
        .context("failed to download the Agent update manifest")?;
    let manifest = UpdateManifest::parse(&manifest_bytes)?;
    let Some(release) = manifest.newer_release(AGENT_WINDOWS_X64, CURRENT_VERSION)? else {
        return Ok(false);
    };

    let update_directory = prepare_update_directory(
        config
            .config_path
            .parent()
            .context("Agent configuration has no parent directory")?,
    )
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
        return Ok(false);
    };

    tracing::info!(
        current_version = CURRENT_VERSION,
        release_version = %release.version,
        attempt = attempt.attempts,
        "downloading automatic Agent update"
    );
    let executable = download(&http, &release.url, MAX_UPDATE_BYTES)
        .context("failed to download the Agent update")?;
    release.verify(&executable)?;
    if !executable.starts_with(b"MZ") {
        bail!("downloaded Agent update is not a Windows executable");
    }

    let current = std::env::current_exe().context("could not locate the Agent executable")?;
    let suffix = unique_suffix();
    let staged = update_directory.join(format!("agent-{}-{suffix}.exe", release.version));
    let helper = update_directory.join(format!("update-helper-{suffix}.exe"));
    let scheduled = write_new_file(&staged, &executable)
        .and_then(|()| {
            std::fs::copy(&current, &helper).with_context(|| {
                format!("failed to create Agent update helper {}", helper.display())
            })
        })
        .and_then(|_| {
            crate::installer::replace_file(&attempts_path, &serde_json::to_vec(&attempt)?)
                .context("failed to record the Agent update attempt")
        })
        .and_then(|()| {
            let mut command = Command::new(&helper);
            command
                .arg("--apply-agent-update")
                .arg(&current)
                .arg(&staged)
                // The helper waits for this process to exit before replacing its executable.
                .arg(std::process::id().to_string());
            if config.json_logs {
                command.arg("--json-logs");
            }
            command
                .current_dir(&update_directory)
                .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
                .spawn()
                .with_context(|| {
                    format!("failed to start Agent update helper {}", helper.display())
                })
        });
    if let Err(error) = scheduled {
        remove_if_present(&staged);
        remove_if_present(&helper);
        return Err(error);
    }
    tracing::info!(
        release_version = %release.version,
        helper = %helper.display(),
        staged = %staged.display(),
        "started the Agent update helper"
    );
    Ok(true)
}

/// Runs as a copy of the previous Agent after the service that staged the update stops. Whatever
/// fails after that, the helper leaves an Agent executable installed and tries to start the
/// service again, so a failed update does not leave the endpoint offline until it reboots.
pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    let [target, staged, process_id, options @ ..] = arguments.as_slice() else {
        bail!(
            "the Agent update helper requires target and staged executable paths and the service process ID"
        );
    };
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
        "Agent update helper started"
    );

    let result = install_update(service_name_for_path(&target), &target, &staged, process_id);
    match &result {
        Ok(()) => tracing::info!("Agent update installed and the updated service is running"),
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

/// Replaces the Agent once its previous instance has exited, and restores and restarts the
/// previous executable when the update cannot be installed or does not start.
fn install_update(
    service_name: &str,
    target: &Path,
    staged: &Path,
    process_id: u32,
) -> anyhow::Result<()> {
    if let Err(error) = stop_instance(service_name, target, Some(process_id)) {
        return Err(restart_installed(
            service_name,
            error.context("the previous Agent service instance did not exit"),
        ));
    }
    tracing::info!("the previous Agent service instance exited; replacing its executable");

    let backup = target.with_extension("exe.previous");
    if let Err(error) = replace_executable(target, staged, &backup) {
        return Err(restart_installed(service_name, error));
    }
    tracing::info!(target = %target.display(), "installed the Agent update; starting the service");

    let Err(start_error) = start_and_confirm(service_name, UPDATE_STARTUP_GRACE) else {
        remove_if_present(&backup);
        return Ok(());
    };
    tracing::error!(
        error = ?start_error,
        "the updated Agent service did not stay running; restoring the previous executable"
    );
    let start_error = start_error.context("the updated Agent service did not stay running");
    let error = match stop_instance(service_name, target, None)
        .and_then(|()| restore_backup(target, &backup))
    {
        Ok(()) => start_error,
        Err(restore_error) => start_error.context(format!(
            "the previous executable could not be restored: {restore_error:#}"
        )),
    };
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
    let status = service.query_status()?;
    let process_id = process_id.or(status.process_id).filter(|&id| id != 0);
    let process = process_id.and_then(|id| InstanceProcess::open(id, image));
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
    process: Option<&InstanceProcess>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = service.query_status()?.current_state;
        let exited = process.is_none_or(InstanceProcess::has_exited);
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

/// The process that ran an Agent service instance.
struct InstanceProcess(HANDLE);

impl InstanceProcess {
    /// Opens the process only while it runs `image`, so a recycled process ID is never waited
    /// on or terminated.
    fn open(process_id: u32, image: &Path) -> Option<Self> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                process_id,
            )
        }
        .ok()?;
        let process = Self(handle);
        let mut name = vec![0_u16; 32_768];
        let mut length = name.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process.0,
                PROCESS_NAME_WIN32,
                PWSTR(name.as_mut_ptr()),
                &mut length,
            )
        }
        .ok()?;
        let name = PathBuf::from(OsString::from_wide(&name[..length as usize]));
        name.as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&image.as_os_str().to_string_lossy())
            .then_some(process)
    }

    fn has_exited(&self) -> bool {
        (unsafe { WaitForSingleObject(self.0, 0) }) == WAIT_OBJECT_0
    }

    fn terminate(&self) -> windows::core::Result<()> {
        unsafe { TerminateProcess(self.0, 1) }
    }
}

impl Drop for InstanceProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Update attempts for the release most recently offered, kept in the private update directory.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct UpdateAttempts {
    version: String,
    attempts: u32,
    first_attempt_unix: u64,
}

impl UpdateAttempts {
    /// The record to store before trying `version`, or `None` once it has used its attempts in
    /// the current window.
    fn next(previous: Option<&Self>, version: &str, now: u64) -> Option<Self> {
        match previous {
            Some(previous)
                if previous.version == version
                    && now
                        .checked_sub(previous.first_attempt_unix)
                        .is_some_and(|elapsed| elapsed < ATTEMPT_WINDOW.as_secs()) =>
            {
                (previous.attempts < MAX_ATTEMPTS_PER_VERSION).then(|| Self {
                    version: version.to_owned(),
                    attempts: previous.attempts + 1,
                    first_attempt_unix: previous.first_attempt_unix,
                })
            }
            _ => Some(Self {
                version: version.to_owned(),
                attempts: 1,
                first_attempt_unix: now,
            }),
        }
    }
}

fn read_attempts(path: &Path) -> Option<UpdateAttempts> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The helper starts before the Agent's logging and exits right after its last step, so it
/// appends to the Agent log synchronously rather than through the bounded asynchronous writer.
fn initialize_helper_log(helper_directory: &Path, json: bool) {
    let Some(config_directory) = helper_directory.parent() else {
        return;
    };
    let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_directory.join("agent.log"))
    else {
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

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(true)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

fn download(http: &ureq::Agent, url: &str, maximum: u64) -> anyhow::Result<Vec<u8>> {
    let response = http.get(url).call()?;
    response
        .into_body()
        .into_with_config()
        .limit(maximum)
        .read_to_vec()
        .context("download exceeded its size limit or could not be read")
}

fn write_new_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create staged update {}", path.display()))?;
    std::io::Write::write_all(&mut file, contents)
        .with_context(|| format!("failed to write staged update {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush staged update {}", path.display()))
}

fn schedule_cleanup(helper: &Path, helper_directory: &Path) -> anyhow::Result<()> {
    let working_directory = helper_directory
        .parent()
        .context("Agent update directory has no parent directory")?;
    crate::installer::helper_cleanup_command(helper, helper_directory, working_directory)
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

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn limits_attempts_for_the_same_release() {
        let first = UpdateAttempts::next(None, "1.2.0", 1_000).unwrap();
        assert_eq!(first.attempts, 1);
        let second = UpdateAttempts::next(Some(&first), "1.2.0", 1_060).unwrap();
        let third = UpdateAttempts::next(Some(&second), "1.2.0", 1_120).unwrap();
        assert_eq!((third.attempts, third.first_attempt_unix), (3, 1_000));
        assert_eq!(UpdateAttempts::next(Some(&third), "1.2.0", 1_180), None);
        assert_eq!(
            UpdateAttempts::next(Some(&third), "1.2.0", 1_000 + DAY - 1),
            None
        );
    }

    #[test]
    fn retries_after_the_window_or_for_another_release() {
        let exhausted = UpdateAttempts {
            version: "1.2.0".to_owned(),
            attempts: MAX_ATTEMPTS_PER_VERSION,
            first_attempt_unix: 1_000,
        };
        let later = UpdateAttempts::next(Some(&exhausted), "1.2.0", 1_000 + DAY).unwrap();
        assert_eq!((later.attempts, later.first_attempt_unix), (1, 1_000 + DAY));
        let newer = UpdateAttempts::next(Some(&exhausted), "1.2.1", 1_100).unwrap();
        assert_eq!((newer.attempts, newer.version.as_str()), (1, "1.2.1"));
        // A clock set back before the recorded attempt must not block updates until it catches up.
        let rewound = UpdateAttempts::next(Some(&exhausted), "1.2.0", 10).unwrap();
        assert_eq!(rewound.attempts, 1);
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
}
