use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::{CLIENT_MACOS_ARM64, CLIENT_MACOS_X64, UpdateManifest};

use crate::config::Config;

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 512 * 1024 * 1024;

pub fn is_helper_invocation() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--apply-client-update")
}

pub async fn check_and_schedule(
    config: &Config,
    launch_deep_link: Option<&str>,
) -> anyhow::Result<bool> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("failed to create client update HTTP client")?;
    let manifest_bytes = download(&http, &config.update_manifest_url, MAX_MANIFEST_BYTES)
        .await
        .context("failed to download the client update manifest")?;
    let manifest = UpdateManifest::parse(&manifest_bytes)?;
    let target = if cfg!(target_arch = "aarch64") {
        CLIENT_MACOS_ARM64
    } else {
        CLIENT_MACOS_X64
    };
    let Some(release) = manifest.newer_release(target, env!("CARGO_PKG_VERSION"))? else {
        return Ok(false);
    };

    tracing::info!(
        current_version = env!("CARGO_PKG_VERSION"),
        release_version = %release.version,
        "downloading signed macOS client update for this launch"
    );
    let archive = download(&http, &release.url, MAX_UPDATE_BYTES)
        .await
        .context("failed to download the macOS client update")?;
    release.verify(&archive)?;

    let executable = std::env::current_exe().context("could not locate the client executable")?;
    let app_bundle = app_bundle_for_executable(&executable)?;
    let suffix = unique_suffix();
    let helper_directory = std::env::temp_dir()
        .join("MeshRMM")
        .join(format!("client-update-{suffix}"));
    std::fs::create_dir_all(&helper_directory).with_context(|| {
        format!(
            "failed to create client update helper directory {}",
            helper_directory.display()
        )
    })?;
    let archive_path = helper_directory.join("client-update.zip");
    write_new_file(&archive_path, &archive)?;
    let helper = helper_directory.join("update-helper");
    std::fs::copy(&executable, &helper)
        .with_context(|| format!("failed to create client update helper {}", helper.display()))?;
    let mut permissions = std::fs::metadata(&helper)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions)?;

    let mut command = Command::new(&helper);
    command
        .arg("--apply-client-update")
        .arg(std::process::id().to_string())
        .arg(&app_bundle)
        .arg(&archive_path)
        .current_dir(&helper_directory);
    if let Some(link) = launch_deep_link {
        command.arg(link);
    }
    command
        .spawn()
        .with_context(|| format!("failed to start client update helper {}", helper.display()))?;
    Ok(true)
}

pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    if arguments.len() < 3 {
        bail!("the client update helper requires process, application, and archive arguments");
    }
    let old_process_id = arguments[0]
        .to_string_lossy()
        .parse::<u32>()
        .context("invalid old client process ID")?;
    let target = PathBuf::from(&arguments[1]);
    let archive = PathBuf::from(&arguments[2]);
    let launch_arguments = arguments.into_iter().skip(3).collect::<Vec<OsString>>();
    let helper = std::env::current_exe().context("could not locate the client update helper")?;
    let helper_directory = helper
        .parent()
        .context("client update helper has no parent directory")?
        .to_owned();

    wait_for_process_exit(old_process_id, Duration::from_secs(60))?;
    let extracted_directory = helper_directory.join("extracted");
    std::fs::create_dir(&extracted_directory)?;
    let output = Command::new("/usr/bin/ditto")
        .args([OsStr::new("-x"), OsStr::new("-k")])
        .arg(&archive)
        .arg(&extracted_directory)
        .output()
        .context("failed to extract the macOS client update")?;
    if !output.status.success() {
        bail!(
            "could not extract the macOS client update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let replacement = find_app_bundle(&extracted_directory)?;
    let replacement_executable = replacement.join("Contents/MacOS/meshrmm-remote");
    if !replacement_executable.is_file() {
        bail!("macOS client update does not contain the expected executable");
    }

    let backup = target.with_file_name(format!(
        "{}.previous",
        target
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("MeshRMM Remote.app")
    ));
    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("failed to remove stale backup {}", backup.display()))?;
    }
    std::fs::rename(&target, &backup)
        .with_context(|| format!("failed to back up client application {}", target.display()))?;
    if let Err(error) = std::fs::rename(&replacement, &target) {
        let _ = std::fs::rename(&backup, &target);
        return Err(error).context("failed to install macOS client update");
    }

    if let Err(update_error) = launch(&target, &launch_arguments) {
        let _ = std::fs::remove_dir_all(&target);
        std::fs::rename(&backup, &target)
            .context("the client update failed and the previous app could not be restored")?;
        launch(&target, &launch_arguments).context(
            "the client update failed; the previous app was restored but could not be relaunched",
        )?;
        return Err(update_error).context("the updated macOS client could not be launched");
    }

    let _ = std::fs::remove_dir_all(backup);
    let _ = std::fs::remove_dir_all(helper_directory);
    Ok(())
}

async fn download(http: &reqwest::Client, url: &str, maximum: usize) -> anyhow::Result<Vec<u8>> {
    let response = http.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        bail!("download exceeds the {maximum}-byte size limit");
    }
    let contents = response.bytes().await?;
    if contents.len() > maximum {
        bail!("download exceeds the {maximum}-byte size limit");
    }
    Ok(contents.to_vec())
}

fn app_bundle_for_executable(executable: &Path) -> anyhow::Result<PathBuf> {
    let bundle = executable
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("client is not running inside a macOS application bundle")?;
    if bundle.extension() != Some(OsStr::new("app")) {
        bail!("client is not running inside a macOS application bundle");
    }
    Ok(bundle.to_owned())
}

fn find_app_bundle(directory: &Path) -> anyhow::Result<PathBuf> {
    let applications = std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("app")))
        .collect::<Vec<_>>();
    match applications.as_slice() {
        [application] => Ok(application.clone()),
        _ => bail!("macOS client update must contain exactly one application bundle"),
    }
}

fn wait_for_process_exit(process_id: u32, timeout: Duration) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let running = unsafe { libc::kill(process_id as i32, 0) } == 0;
        if !running {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("timed out waiting for the client to exit");
        }
        sleep(Duration::from_millis(200));
    }
}

fn launch(target: &Path, arguments: &[OsString]) -> anyhow::Result<()> {
    Command::new("/usr/bin/open")
        .arg("-a")
        .arg(target)
        .arg("--args")
        .args(arguments)
        .spawn()
        .with_context(|| format!("failed to relaunch updated client {}", target.display()))?;
    Ok(())
}

fn write_new_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create client update {}", path.display()))?;
    std::io::Write::write_all(&mut file, contents)?;
    file.sync_all()?;
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
