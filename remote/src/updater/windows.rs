use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::{CLIENT_WINDOWS_X64, CURRENT_VERSION, UpdateManifest};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;

use crate::config::Config;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 256 * 1024 * 1024;
/// The viewer exits as soon as the helper starts, so this only allows for a slow shutdown.
const VIEWER_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINATED_VIEWER_TIMEOUT: Duration = Duration::from_secs(10);

pub fn is_helper_invocation() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--apply-client-update")
}

pub async fn check_and_schedule(config: &Config) -> anyhow::Result<bool> {
    if !config.auto_update {
        tracing::info!("automatic viewer updates disabled by local configuration");
        return Ok(false);
    }
    if std::env::var_os("MESHRMM_UPDATE_READY_FILE").is_some() {
        return Ok(false);
    }
    let http = crate::http::client_builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create client update HTTP client")?;
    let manifest_bytes = download(&http, &config.update_manifest_url, MAX_MANIFEST_BYTES)
        .await
        .context("failed to download the client update manifest")?;
    let manifest = UpdateManifest::parse(&manifest_bytes)?;
    let Some(release) = manifest.newer_release(CLIENT_WINDOWS_X64, CURRENT_VERSION)? else {
        return Ok(false);
    };

    tracing::info!(
        current_version = CURRENT_VERSION,
        release_version = %release.version,
        "downloading client update for this launch"
    );
    let executable = download(&http, &release.url, MAX_UPDATE_BYTES)
        .await
        .context("failed to download the client update")?;
    release.verify(&executable)?;
    if !executable.starts_with(b"MZ") {
        bail!("downloaded client update is not a Windows executable");
    }

    let current = std::env::current_exe().context("could not locate the client executable")?;
    let parent = current
        .parent()
        .context("client executable has no parent directory")?;
    let suffix = unique_suffix();
    let staged = parent.join(format!(
        "meshrmm-remote-{}.update-{suffix}.exe",
        release.version
    ));
    let helper_directory = std::env::temp_dir()
        .join("MeshRMM")
        .join(format!("client-update-{suffix}"));
    let scheduled = write_new_file(&staged, &executable)
        .and_then(|()| start_helper(config, &current, &staged, &helper_directory));
    if let Err(error) = scheduled {
        remove_if_present(&staged);
        let _ = std::fs::remove_dir_all(&helper_directory);
        return Err(error);
    }
    Ok(true)
}

fn start_helper(
    config: &Config,
    current: &Path,
    staged: &Path,
    helper_directory: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(helper_directory).with_context(|| {
        format!(
            "failed to create client update helper directory {}",
            helper_directory.display()
        )
    })?;
    let helper = helper_directory.join("update-helper.exe");
    std::fs::copy(current, &helper)
        .with_context(|| format!("failed to create client update helper {}", helper.display()))?;
    let launch_arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    Command::new(&helper)
        .env(
            "MESHRMM_SESSION_BOOTSTRAP",
            serde_json::to_string(
                &config
                    .bootstrap
                    .as_ref()
                    .context("missing launch session")?,
            )?,
        )
        .arg("--apply-client-update")
        .arg(current)
        .arg(staged)
        // The helper waits for this process to exit before replacing its executable.
        .arg(std::process::id().to_string())
        .args(launch_arguments)
        .current_dir(helper_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("failed to start client update helper {}", helper.display()))?;
    Ok(())
}

/// Runs as a copy of the previous viewer. Whatever fails, it relaunches the viewer that is left
/// installed, removes the staged executable, and schedules its own removal.
pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    let [target, staged, process_id, launch_arguments @ ..] = arguments.as_slice() else {
        bail!(
            "the client update helper requires target and staged executable paths and the viewer process ID"
        );
    };
    let target = PathBuf::from(target);
    let staged = PathBuf::from(staged);
    let helper = std::env::current_exe().context("could not locate the client update helper")?;
    let helper_directory = helper
        .parent()
        .context("client update helper has no parent directory")?
        .to_owned();
    tracing::info!(
        target = %target.display(),
        staged = %staged.display(),
        viewer_process_id = ?process_id,
        "client update helper started"
    );

    let mut result = process_id
        .to_str()
        .and_then(|value| value.parse::<u32>().ok())
        .context("the client update helper received an invalid viewer process ID")
        .and_then(|process_id| install_update(&target, &staged, process_id, launch_arguments));
    if let Err(error) = &result
        && target.exists()
    {
        tracing::warn!(error = ?error, "client update failed; relaunching the installed viewer");
        if let Err(relaunch_error) = launch(&target, launch_arguments) {
            result = result.with_context(|| {
                format!("the restored viewer could not be relaunched either: {relaunch_error:#}")
            });
        }
    }
    remove_if_present(&staged);
    let cleanup = schedule_cleanup(&helper, &helper_directory);
    if let Err(error) = &cleanup {
        tracing::warn!(error = ?error, "could not schedule removal of the client update helper");
    }
    result.and(cleanup)
}

/// Replaces the viewer once the process that scheduled the update has exited, and restores the
/// previous executable when the update cannot be installed or does not start.
fn install_update(
    target: &Path,
    staged: &Path,
    process_id: u32,
    launch_arguments: &[OsString],
) -> anyhow::Result<()> {
    // The helper was started by that viewer, so the viewer was created before it.
    let created_before = ViewerProcess::creation_time(unsafe { GetCurrentProcess() })
        .context("could not read the client update helper start time")?;
    wait_for_viewer_exit(
        process_id,
        target,
        created_before,
        VIEWER_EXIT_TIMEOUT,
        TERMINATED_VIEWER_TIMEOUT,
    )?;
    tracing::info!("the previous viewer exited; replacing it");

    let backup = target.with_extension("exe.previous");
    remove_if_present(&backup);
    std::fs::rename(target, &backup)
        .with_context(|| format!("failed to back up client {}", target.display()))?;
    if let Err(error) = std::fs::rename(staged, target) {
        std::fs::rename(&backup, target).context("could not restore the previous viewer")?;
        tracing::warn!("restored the previous viewer after the update could not be installed");
        return Err(error)
            .with_context(|| format!("failed to install client update {}", target.display()));
    }

    if let Err(update_error) = launch(target, launch_arguments) {
        let _ = std::fs::remove_file(target);
        std::fs::rename(&backup, target).context(
            "the client update failed and the previous executable could not be restored",
        )?;
        tracing::warn!("restored the previous viewer after the update could not be launched");
        return Err(update_error).context("the updated client could not be launched");
    }

    remove_if_present(&backup);
    tracing::info!(target = %target.display(), "installed and launched the client update");
    Ok(())
}

async fn download(http: &reqwest::Client, url: &str, maximum: usize) -> anyhow::Result<Vec<u8>> {
    let mut response = http.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        bail!("download exceeds the {maximum}-byte size limit");
    }
    let mut contents = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > maximum.saturating_sub(contents.len()) {
            bail!("download exceeds the {maximum}-byte size limit");
        }
        contents.extend_from_slice(&chunk);
    }
    Ok(contents)
}

fn write_new_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create staged client update {}", path.display()))?;
    std::io::Write::write_all(&mut file, contents)
        .with_context(|| format!("failed to write staged client update {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush staged client update {}", path.display()))
}

/// Waits for the viewer that scheduled the update to exit. Windows lets a running executable be
/// renamed, so a successful rename does not show that the viewer has released it, and relaunching
/// while it is still closing could race it. The viewer has already committed to exiting, so one
/// that does not exit in time is terminated.
fn wait_for_viewer_exit(
    process_id: u32,
    image: &Path,
    created_before: u64,
    timeout: Duration,
    terminated_timeout: Duration,
) -> anyhow::Result<()> {
    let Some(process) = ViewerProcess::open(process_id, image, created_before) else {
        return Ok(());
    };
    if process.wait(timeout) {
        return Ok(());
    }
    tracing::warn!(
        process_id,
        "the previous viewer did not exit in time; terminating it"
    );
    process
        .terminate()
        .context("failed to terminate the previous viewer")?;
    if !process.wait(terminated_timeout) {
        bail!("the previous viewer is still running after it was terminated");
    }
    Ok(())
}

/// The viewer process that scheduled an update.
struct ViewerProcess(HANDLE);

impl ViewerProcess {
    /// Opens the process only while it runs `image` and was created before `created_before`, so
    /// a recycled process ID, including one reused by a newer viewer, is never waited on or
    /// terminated. `None` means the viewer has exited.
    fn open(process_id: u32, image: &Path, created_before: u64) -> Option<Self> {
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
        let name = OsString::from_wide(&name[..length as usize]);
        let same_image = name
            .to_string_lossy()
            .eq_ignore_ascii_case(&image.as_os_str().to_string_lossy());
        let created = Self::creation_time(process.0).ok()?;
        (same_image && created < created_before).then_some(process)
    }

    fn creation_time(process: HANDLE) -> windows::core::Result<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }?;
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    /// Returns whether the process exited within `timeout`.
    fn wait(&self, timeout: Duration) -> bool {
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        (unsafe { WaitForSingleObject(self.0, milliseconds) }) == WAIT_OBJECT_0
    }

    fn terminate(&self) -> windows::core::Result<()> {
        unsafe { TerminateProcess(self.0, 1) }
    }
}

impl Drop for ViewerProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn remove_if_present(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "could not remove file"),
    }
}

fn launch(target: &Path, arguments: &[OsString]) -> anyhow::Result<()> {
    let mut command = Command::new(target);
    command.args(arguments);
    super::launch_verified(command)
}

fn schedule_cleanup(helper: &Path, helper_directory: &Path) -> anyhow::Result<()> {
    let working_directory = helper_directory
        .parent()
        .context("client update helper directory has no parent directory")?;
    cleanup_command(helper, helper_directory, working_directory)
        .spawn()
        .context("failed to schedule client update cleanup")?;
    Ok(())
}

/// Builds a detached `cmd.exe` that waits about two seconds for `helper` to exit, then deletes it
/// and its now-empty directory. `working_directory` must be outside `helper_directory`.
fn cleanup_command(helper: &Path, helper_directory: &Path, working_directory: &Path) -> Command {
    let cleanup = format!(
        "ping.exe 127.0.0.1 -n 3 >NUL & del /f /q \"{}\" & rmdir /q \"{}\"",
        helper.display(),
        helper_directory.display()
    );
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C"])
        // `arg` would escape the inner quotes as \", which cmd does not understand. With /S, cmd
        // removes only the outer pair of quotes.
        .raw_arg(format!("\"{cleanup}\""))
        .current_dir(working_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    command
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

    fn ping(count: u32) -> (PathBuf, std::process::Child) {
        let ping = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("PING.EXE");
        let child = Command::new(&ping)
            .args(["127.0.0.1", "-n", &count.to_string()])
            .stdout(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        (ping, child)
    }

    #[test]
    fn waits_for_the_viewer_to_exit() {
        let (image, mut child) = ping(3);
        let started = std::time::Instant::now();
        wait_for_viewer_exit(
            child.id(),
            &image,
            u64::MAX,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(child.try_wait().unwrap().unwrap().success());
    }

    #[test]
    fn terminates_a_viewer_that_does_not_exit() {
        let (image, mut child) = ping(60);
        wait_for_viewer_exit(
            child.id(),
            &image,
            u64::MAX,
            Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(1));
    }

    #[test]
    fn ignores_a_recycled_process_id() {
        let (image, mut child) = ping(60);
        let other_image = image.with_file_name("meshrmm-remote.exe");
        let helper_started = ViewerProcess::creation_time(unsafe { GetCurrentProcess() }).unwrap();
        for (image, created_before) in [(&other_image, u64::MAX), (&image, helper_started)] {
            let started = std::time::Instant::now();
            wait_for_viewer_exit(
                child.id(),
                image,
                created_before,
                Duration::from_millis(200),
                Duration::from_secs(5),
            )
            .unwrap();
            assert!(started.elapsed() < Duration::from_millis(200));
        }
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn cleanup_deletes_the_helper_and_its_directory() {
        let parent = std::env::temp_dir().join(format!("meshrmm cleanup {}", unique_suffix()));
        let helper_directory = parent.join("client-update test");
        std::fs::create_dir_all(&helper_directory).unwrap();
        let helper = helper_directory.join("update-helper.exe");
        std::fs::write(&helper, b"MZ helper").unwrap();

        let mut command = cleanup_command(&helper, &helper_directory, &parent);
        // cmd receives the quoted paths unescaped inside one outer pair of quotes.
        let cleanup = format!(
            "\"ping.exe 127.0.0.1 -n 3 >NUL & del /f /q \"{}\" & rmdir /q \"{}\"\"",
            helper.display(),
            helper_directory.display()
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["/D", "/S", "/C", cleanup.as_str()].map(std::ffi::OsStr::new)
        );
        let status = command.spawn().unwrap().wait().unwrap();
        assert!(status.success());
        assert!(!helper_directory.exists());
        std::fs::remove_dir(&parent).unwrap();
    }
}
