use std::ffi::OsStr;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use meshrmm_self_update::{AGENT_WINDOWS_X64, CURRENT_VERSION, UpdateManifest};
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::remote::config::Config;
use crate::service::service_name_for_path;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_UPDATE_BYTES: u64 = 256 * 1024 * 1024;

pub fn check_and_schedule(config: &Config) -> anyhow::Result<bool> {
    let http = http_agent();
    let manifest_bytes = download(&http, &config.update_manifest_url, MAX_MANIFEST_BYTES)
        .context("failed to download the Agent update manifest")?;
    let manifest = UpdateManifest::parse(&manifest_bytes)?;
    let Some(release) = manifest.newer_release(AGENT_WINDOWS_X64, CURRENT_VERSION)? else {
        return Ok(false);
    };

    tracing::info!(
        current_version = CURRENT_VERSION,
        release_version = %release.version,
        "downloading automatic Agent update"
    );
    let executable = download(&http, &release.url, MAX_UPDATE_BYTES)
        .context("failed to download the Agent update")?;
    release.verify(&executable)?;
    if !executable.starts_with(b"MZ") {
        bail!("downloaded Agent update is not a Windows executable");
    }

    let current = std::env::current_exe().context("could not locate the Agent executable")?;
    let update_directory = config
        .config_path
        .parent()
        .context("Agent configuration has no parent directory")?
        .join("updates");
    std::fs::create_dir_all(&update_directory).with_context(|| {
        format!(
            "failed to create Agent update directory {}",
            update_directory.display()
        )
    })?;
    let suffix = unique_suffix();
    let staged = update_directory.join(format!("agent-{}-{suffix}.exe", release.version));
    let helper = update_directory.join(format!("update-helper-{suffix}.exe"));
    write_new_file(&staged, &executable)?;
    std::fs::copy(&current, &helper)
        .with_context(|| format!("failed to create Agent update helper {}", helper.display()))?;
    Command::new(&helper)
        .arg("--apply-agent-update")
        .arg(&current)
        .arg(&staged)
        .current_dir(&update_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("failed to start Agent update helper {}", helper.display()))?;
    Ok(true)
}

pub fn apply_scheduled_update() -> anyhow::Result<()> {
    let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
    if arguments.len() != 2 {
        bail!("the Agent update helper requires target and staged executable paths");
    }
    let target = PathBuf::from(&arguments[0]);
    let staged = PathBuf::from(&arguments[1]);
    let helper = std::env::current_exe().context("could not locate the Agent update helper")?;
    let helper_directory = helper
        .parent()
        .context("Agent update helper has no parent directory")?
        .to_owned();

    wait_until_replaceable(&target, Duration::from_secs(60))?;
    let backup = target.with_extension("exe.previous");
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&target, &backup)
        .with_context(|| format!("failed to back up installed Agent {}", target.display()))?;
    if let Err(error) = std::fs::rename(&staged, &target) {
        let _ = std::fs::rename(&backup, &target);
        return Err(error)
            .with_context(|| format!("failed to install Agent update {}", target.display()));
    }

    let service_name = service_name_for_path(&target);
    if let Err(update_error) = start_service_and_wait(service_name) {
        let _ = stop_service_if_running(service_name);
        let _ = std::fs::remove_file(&target);
        std::fs::rename(&backup, &target)
            .context("the Agent update failed and the previous executable could not be restored")?;
        start_service_and_wait(service_name).context(
            "the Agent update failed; the previous executable was restored but could not be restarted",
        )?;
        return Err(update_error).context("the updated Agent service did not start");
    }

    let _ = std::fs::remove_file(backup);
    schedule_cleanup(&helper, &helper_directory)?;
    Ok(())
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

fn wait_until_replaceable(target: &Path, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let probe = target.with_extension("exe.update-probe");
        match std::fs::rename(target, &probe) {
            Ok(()) => {
                std::fs::rename(&probe, target)
                    .context("failed to restore Agent after update readiness probe")?;
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => sleep(Duration::from_millis(250)),
            Err(error) => {
                return Err(error).context("timed out waiting for the Agent service to exit");
            }
        }
    }
}

fn start_service_and_wait(service_name: &str) -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        service_name,
        ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    service.start::<&OsStr>(&[])?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let state = service.query_status()?.current_state;
        if state == ServiceState::Running {
            return Ok(());
        }
        if state == ServiceState::Stopped || Instant::now() >= deadline {
            bail!("Agent service stopped before the update was confirmed running");
        }
        sleep(Duration::from_millis(250));
    }
}

fn stop_service_if_running(service_name: &str) -> anyhow::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        service_name,
        ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn schedule_cleanup(helper: &Path, helper_directory: &Path) -> anyhow::Result<()> {
    let working_directory = helper_directory
        .parent()
        .context("Agent update directory has no parent directory")?;
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
