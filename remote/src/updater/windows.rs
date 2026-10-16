use std::ffi::OsString;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::windows::{PreviousProcess, current_process_created};
use meshrmm_self_update::{CLIENT_WINDOWS_X64, CURRENT_VERSION, UpdateManifest};

use super::download;
use crate::config::Config;
use crate::launch_status::{self, LaunchStatus};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 256 * 1024 * 1024;
/// The viewer exits as soon as the helper starts, so this only allows for a slow shutdown.
const VIEWER_EXIT_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINATED_VIEWER_TIMEOUT: Duration = Duration::from_secs(10);
const HELPER_DIRECTORY_PREFIX: &str = "client-update-";

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
    launch_status::report(LaunchStatus::CheckingForUpdate);
    let http = crate::http::client_builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create client update HTTP client")?;
    let manifest_bytes = download(
        &http,
        &config.update_manifest_url,
        MAX_MANIFEST_BYTES,
        |_| {},
    )
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
    let executable = download(&http, &release.url, MAX_UPDATE_BYTES, |progress| {
        launch_status::report(progress.status(&release.version))
    })
    .await
    .context("failed to download the client update")?;
    release.verify(&executable)?;
    if !executable.starts_with(b"MZ") {
        bail!("downloaded client update is not a Windows executable");
    }
    launch_status::report(LaunchStatus::InstallingUpdate {
        version: release.version.clone(),
    });

    let current = std::env::current_exe().context("could not locate the client executable")?;
    let parent = current
        .parent()
        .context("client executable has no parent directory")?;
    let suffix = unique_suffix();
    let staged = parent.join(format!(
        "meshrmm-remote-{}.update-{suffix}.exe",
        release.version
    ));
    let helper_directory = helpers_directory().join(format!("{HELPER_DIRECTORY_PREFIX}{suffix}"));
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
/// installed and removes the staged executable. A later viewer removes the helper itself; see
/// [`remove_stale_files`].
pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    let [target, staged, process_id, launch_arguments @ ..] = arguments.as_slice() else {
        bail!(
            "the client update helper requires target and staged executable paths and the viewer process ID"
        );
    };
    let target = PathBuf::from(target);
    let staged = PathBuf::from(staged);
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
    result
}

/// Removes update helpers and staged executables that earlier updates left behind. A running
/// executable cannot be deleted, so the helper that has just relaunched this viewer may be left
/// for a later call.
pub fn remove_stale_files() {
    let installed = std::env::current_exe().ok();
    let installed_directory = installed.as_deref().and_then(Path::parent);
    remove_stale_files_in(&helpers_directory(), installed_directory);
}

fn remove_stale_files_in(helpers_directory: &Path, installed_directory: Option<&Path>) {
    let mut removed = 0;
    for entry in std::fs::read_dir(helpers_directory)
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir())
            || !entry
                .file_name()
                .to_string_lossy()
                .starts_with(HELPER_DIRECTORY_PREFIX)
        {
            continue;
        }
        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => tracing::debug!(
                path = %entry.path().display(),
                %error,
                "left a client update helper that is still in use"
            ),
        }
    }
    let staged = installed_directory
        .map(std::fs::read_dir)
        .into_iter()
        .flatten()
        .flatten()
        .flatten();
    for entry in staged {
        if !entry.file_type().is_ok_and(|kind| kind.is_file())
            || !is_staged_update(&entry.file_name().to_string_lossy())
        {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) => tracing::debug!(
                path = %entry.path().display(),
                %error,
                "left a staged client update that is still in use"
            ),
        }
    }
    if removed > 0 {
        tracing::info!(removed, "removed files left by earlier client updates");
    }
}

fn helpers_directory() -> PathBuf {
    std::env::temp_dir().join("MeshRMM")
}

fn is_staged_update(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("meshrmm-remote-") && name.contains(".update-") && name.ends_with(".exe")
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
    let created_before =
        current_process_created().context("could not read the client update helper start time")?;
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
    let Some(process) = PreviousProcess::open(process_id, image, created_before) else {
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

    fn ping_image() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32")
            .join("PING.EXE")
    }

    fn ping(count: u32) -> (PathBuf, std::process::Child) {
        let ping = ping_image();
        let child = ping_from(&ping, count);
        (ping, child)
    }

    fn ping_from(image: &Path, count: u32) -> std::process::Child {
        Command::new(image)
            .args(["127.0.0.1", "-n", &count.to_string()])
            .stdout(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap()
    }

    #[test]
    fn recognizes_only_staged_updates() {
        assert!(is_staged_update(
            "meshrmm-remote-0.3.1.update-10188-1787741428068.exe"
        ));
        assert!(is_staged_update("MeshRMM-Remote-0.3.1.Update-1-2.EXE"));
        assert!(!is_staged_update("meshrmm-remote.exe"));
        assert!(!is_staged_update("meshrmm-remote.exe.previous"));
        assert!(!is_staged_update("remote.json"));
    }

    #[test]
    fn removes_leftover_update_files_only() {
        let root =
            std::env::temp_dir().join(format!("meshrmm-client-update-cleanup-{}", unique_suffix()));
        let helpers = root.join("helpers");
        let installed = root.join("installed");
        for directory in ["client-update-1-2", "client-update-3-4", "recordings"] {
            std::fs::create_dir_all(helpers.join(directory)).unwrap();
        }
        std::fs::write(
            helpers.join("client-update-1-2").join("update-helper.exe"),
            b"MZ",
        )
        .unwrap();
        std::fs::write(helpers.join("client-update-5-6.exe"), b"MZ").unwrap();
        std::fs::create_dir_all(&installed).unwrap();
        for name in [
            "meshrmm-remote.exe",
            "meshrmm-remote-0.3.1.update-1-2.exe",
            "remote.json",
        ] {
            std::fs::write(installed.join(name), b"MZ").unwrap();
        }
        // A running helper keeps its executable, and so its directory, in use.
        let in_use = helpers.join("client-update-7-8");
        std::fs::create_dir_all(&in_use).unwrap();
        std::fs::copy(ping_image(), in_use.join("update-helper.exe")).unwrap();
        let mut helper = ping_from(&in_use.join("update-helper.exe"), 60);

        remove_stale_files_in(&helpers, Some(&installed));
        let names = |directory: &Path| {
            let mut names = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(
            names(&helpers),
            ["client-update-5-6.exe", "client-update-7-8", "recordings"]
        );
        assert_eq!(names(&installed), ["meshrmm-remote.exe", "remote.json"]);

        helper.kill().unwrap();
        helper.wait().unwrap();
        remove_stale_files_in(&helpers, Some(&installed));
        assert_eq!(names(&helpers), ["client-update-5-6.exe", "recordings"]);
        std::fs::remove_dir_all(&root).unwrap();
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
        let helper_started = current_process_created().unwrap();
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
}
