use std::ffi::OsString;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::{CLIENT_WINDOWS_X64, UpdateManifest};

use crate::config::Config;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 256 * 1024 * 1024;

pub fn is_helper_invocation() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--apply-client-update")
}

pub async fn check_and_schedule(config: &Config) -> anyhow::Result<bool> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create client update HTTP client")?;
    let manifest_bytes = download(&http, &config.update_manifest_url, MAX_MANIFEST_BYTES)
        .await
        .context("failed to download the client update manifest")?;
    let manifest = UpdateManifest::parse(&manifest_bytes)?;
    let Some(release) = manifest.newer_release(CLIENT_WINDOWS_X64, env!("CARGO_PKG_VERSION"))?
    else {
        return Ok(false);
    };

    tracing::info!(
        current_version = env!("CARGO_PKG_VERSION"),
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
    write_new_file(&staged, &executable)?;

    let helper_directory = std::env::temp_dir()
        .join("MeshRMM")
        .join(format!("client-update-{suffix}"));
    std::fs::create_dir_all(&helper_directory).with_context(|| {
        format!(
            "failed to create client update helper directory {}",
            helper_directory.display()
        )
    })?;
    let helper = helper_directory.join("update-helper.exe");
    std::fs::copy(&current, &helper)
        .with_context(|| format!("failed to create client update helper {}", helper.display()))?;
    let launch_arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    Command::new(&helper)
        .arg("--apply-client-update")
        .arg(&current)
        .arg(&staged)
        .args(launch_arguments)
        .current_dir(&helper_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("failed to start client update helper {}", helper.display()))?;
    Ok(true)
}

pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    if arguments.len() < 2 {
        bail!("the client update helper requires target and staged executable paths");
    }
    let target = PathBuf::from(&arguments[0]);
    let staged = PathBuf::from(&arguments[1]);
    let launch_arguments = arguments.into_iter().skip(2).collect::<Vec<OsString>>();
    let helper = std::env::current_exe().context("could not locate the client update helper")?;
    let helper_directory = helper
        .parent()
        .context("client update helper has no parent directory")?
        .to_owned();

    wait_until_replaceable(&target, Duration::from_secs(60))?;
    let backup = target.with_extension("exe.previous");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&target, &backup)
        .with_context(|| format!("failed to back up client {}", target.display()))?;
    if let Err(error) = std::fs::rename(&staged, &target) {
        let _ = std::fs::rename(&backup, &target);
        return Err(error)
            .with_context(|| format!("failed to install client update {}", target.display()));
    }

    if let Err(update_error) = launch(&target, &launch_arguments) {
        let _ = std::fs::remove_file(&target);
        std::fs::rename(&backup, &target).context(
            "the client update failed and the previous executable could not be restored",
        )?;
        launch(&target, &launch_arguments).context(
            "the client update failed; the previous executable was restored but could not be relaunched",
        )?;
        return Err(update_error).context("the updated client could not be launched");
    }

    let _ = std::fs::remove_file(backup);
    schedule_cleanup(&helper, &helper_directory)?;
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

fn wait_until_replaceable(target: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let probe = target.with_extension("exe.update-probe");
        match std::fs::rename(target, &probe) {
            Ok(()) => {
                std::fs::rename(&probe, target)
                    .context("failed to restore client after update readiness probe")?;
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(200)),
            Err(error) => {
                return Err(error).context("timed out waiting for the client to exit");
            }
        }
    }
}

fn launch(target: &Path, arguments: &[OsString]) -> anyhow::Result<()> {
    Command::new(target)
        .args(arguments)
        .current_dir(
            target
                .parent()
                .context("client executable has no parent directory")?,
        )
        .spawn()
        .with_context(|| format!("failed to relaunch updated client {}", target.display()))?;
    Ok(())
}

fn schedule_cleanup(helper: &Path, helper_directory: &Path) -> anyhow::Result<()> {
    let working_directory = helper_directory
        .parent()
        .context("client update helper directory has no parent directory")?;
    let cleanup = format!(
        "ping.exe 127.0.0.1 -n 3 >NUL & del /f /q \"{}\" & rmdir /q \"{}\"",
        helper.display(),
        helper_directory.display()
    );
    Command::new("cmd.exe")
        .args(["/D", "/S", "/C"])
        .arg(cleanup)
        .current_dir(working_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .context("failed to schedule client update cleanup")?;
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
