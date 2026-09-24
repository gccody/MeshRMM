use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::SystemInformation::{
    ComputerNameDnsHostname, GetComputerNameExW, GetSystemWindowsDirectoryW,
};
use windows::Win32::UI::Shell::{
    FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::{PCWSTR, PWSTR, w};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::private_directory;
use crate::service::{LEGACY_SERVICE_NAME, SERVICE_NAME};

const ENROLLMENT_MAGIC: &[u8] = b"MESHRMM-BOOTSTRAP-V1";
const CONFIG_LENGTH_BYTES: usize = 8;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

#[derive(Debug, Deserialize)]
struct InstallerBootstrap {
    server: String,
    install_token: String,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Serialize)]
struct RedeemInstallerRequest {
    name: String,
    redemption_key: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProvisionedAgentConfig {
    server: String,
    device_id: String,
    agent_token: String,
    #[serde(default = "default_update_manifest_url")]
    update_manifest_url: String,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    json_logs: bool,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
}

fn default_update_manifest_url() -> String {
    meshrmm_self_update::DEFAULT_MANIFEST_URL.to_owned()
}

pub fn launch_if_embedded() -> anyhow::Result<bool> {
    let executable = std::env::current_exe().context("could not locate the Agent installer")?;
    let bytes = std::fs::read(&executable).context("could not read the Agent installer")?;
    if parse_embedded(&bytes)?.is_none() {
        return Ok(false);
    }

    let operation = wide(OsStr::new("runas"));
    let executable_wide = wide(executable.as_os_str());
    let parameters = wide(OsStr::new("--install"));
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(executable_wide.as_ptr()),
            PCWSTR(parameters.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        bail!(
            "administrator approval was not granted (ShellExecute error {})",
            result.0 as isize
        );
    }
    Ok(true)
}

pub fn install_and_notify() -> anyhow::Result<()> {
    match install() {
        Ok(notice) => {
            let mut text = String::from(
                "MeshRMM Agent installed successfully. The LocalSystem service is running. You can now delete the downloaded installer.",
            );
            if let Some(notice) = notice {
                text.push_str("\n\n");
                text.push_str(&notice);
            }
            message(&text, false);
            Ok(())
        }
        Err(error) => {
            message(
                &format!("MeshRMM Agent installation failed:\n\n{error:#}"),
                true,
            );
            Err(error)
        }
    }
}

/// Starts an independent LocalSystem helper before the service worker exits. The helper can then
/// stop and remove the service without trying to delete the executable that is currently running.
pub fn schedule_uninstall() -> anyhow::Result<()> {
    let source = std::env::current_exe().context("could not locate the Agent executable")?;
    let helper = stage_uninstall_helper(&program_data()?, &source)?;
    let helper_directory = helper
        .parent()
        .context("the uninstall helper has no parent directory")?;
    Command::new(&helper)
        .arg("--uninstall")
        // The desktop worker normally runs from the install directory. Do not let the helper
        // inherit that working directory or Windows will keep the otherwise-empty directory
        // in use while the helper tries to remove it.
        .current_dir(helper_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("failed to start uninstall helper {}", helper.display()))?;
    Ok(())
}

/// The helper runs as LocalSystem, so it is copied into a new administrator-only directory. A
/// shared temporary folder can be pre-created by a standard user, who could then replace the
/// executable or plant DLLs beside it. The directory sits beside the Agent data it deletes, and
/// the helper removes it through `schedule_helper_cleanup`.
fn stage_uninstall_helper(program_data: &Path, source: &Path) -> anyhow::Result<PathBuf> {
    let helper_directory = program_data.join(format!(
        "MeshRMM-uninstall-{}",
        uuid::Uuid::new_v4().simple()
    ));
    private_directory::create_new(&helper_directory)?;
    let helper = helper_directory.join("meshrmm-agent-uninstall.exe");
    std::fs::copy(source, &helper)
        .with_context(|| format!("failed to create uninstall helper {}", helper.display()))?;
    Ok(helper)
}

pub fn uninstall() -> anyhow::Result<()> {
    // Give the worker enough time to flush its coordinator acknowledgement before stopping the
    // service terminates that worker process.
    sleep(Duration::from_secs(1));
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("administrator access is required to uninstall the Agent service")?;
    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    for service_name in [SERVICE_NAME, LEGACY_SERVICE_NAME] {
        if let Ok(service) = manager.open_service(service_name, service_access) {
            stop_service(&service)?;
            service
                .delete()
                .context("failed to unregister the Agent service")?;
        }
    }
    drop(manager);

    let install_directory = program_files()?.join("MeshRMM").join("Agent");
    let config_directory = program_data()?.join("MeshRMM").join("Agent");
    remove_directory_if_present(&config_directory)?;
    remove_directory_if_present(&install_directory)?;
    remove_empty_parent(&config_directory);
    remove_empty_parent(&install_directory);
    remove_legacy_directories()?;
    schedule_helper_cleanup()?;
    Ok(())
}

/// Installs or repairs the Agent, returning a notice for the user when legacy state was skipped.
fn install() -> anyhow::Result<Option<String>> {
    let source_path = std::env::current_exe().context("could not locate the Agent installer")?;
    let source_bytes = std::fs::read(&source_path).context("could not read the Agent installer")?;
    let embedded = parse_embedded(&source_bytes)?
        .context("this executable does not contain a MeshRMM Agent enrollment")?;
    let bootstrap = validate_bootstrap(embedded.bootstrap)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("administrator access is required to install the Agent service")?;
    let service_access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::STOP
        | ServiceAccess::START
        | ServiceAccess::CHANGE_CONFIG;
    let existing_service = manager.open_service(SERVICE_NAME, service_access).ok();
    let legacy_service = manager
        .open_service(LEGACY_SERVICE_NAME, service_access | ServiceAccess::DELETE)
        .ok();

    let program_files = program_files()?;
    let program_data = program_data()?;
    let install_directory = program_files.join("MeshRMM").join("Agent");
    let data_root = program_data.join("MeshRMM");
    let config_directory = data_root.join("Agent");
    std::fs::create_dir_all(&install_directory)
        .with_context(|| format!("failed to create {}", install_directory.display()))?;
    // The credential and the SYSTEM update helper live here, and standard users may create
    // folders under ProgramData, so secure the whole chain before reading or writing anything.
    secure_or_replace_directory(&data_root)?;
    secure_or_replace_directory(&config_directory)?;
    private_directory::secure_contents(&config_directory)?;

    let machine_name = machine_name()?;
    let recovery_path = config_directory.join("enrollment-recovery.json");
    let recovery_key = match std::fs::read_to_string(&recovery_path) {
        Ok(key) => key,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let key = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            std::fs::write(&recovery_path, &key)?;
            key
        }
        Err(error) => return Err(error).context("could not read enrollment recovery key"),
    };
    let config_path = config_directory.join("agent.json");
    // Repair preserves the installed identity, including legacy installations.
    let mut notice = None;
    let previous_config = if config_path.exists() {
        Some(std::fs::read(&config_path)?)
    } else {
        // Nothing re-secured the legacy directory, so a configuration that another account could
        // have planted, for example one naming its own server, is ignored and the endpoint
        // enrolls as a new device instead.
        let legacy = program_data
            .join("PulseRMM")
            .join("Agent")
            .join("agent.json");
        match private_directory::read_protected_file(&legacy) {
            Ok(config) => config,
            Err(error) => {
                let untrusted = error.downcast::<private_directory::UntrustedPath>()?;
                notice = Some(format!(
                    "The previous PulseRMM configuration was not imported because {untrusted}. This endpoint was enrolled as a new device."
                ));
                None
            }
        }
    };
    let provisioned_config = if let Some(config) = previous_config {
        serde_json::from_slice::<ProvisionedAgentConfig>(&config)?
    } else {
        let pending = config_directory.join("enrollment-pending.json");
        if pending.exists() {
            serde_json::from_slice::<ProvisionedAgentConfig>(&std::fs::read(&pending)?)?
        } else {
            let config = redeem_installer(&bootstrap, machine_name, recovery_key)?;
            replace_file(&pending, &serde_json::to_vec(&config)?)?;
            config
        }
    };
    if provisioned_config.server.trim_end_matches('/') != bootstrap.server.trim_end_matches('/') {
        bail!(
            "this endpoint is already enrolled with another server; uninstall it before enrolling in a different company"
        );
    }
    let agent_path = install_directory.join("meshrmm-agent.exe");
    let mut rollback = InstallationRollback {
        files: vec![
            (agent_path.clone(), read_optional(&agent_path)?),
            (config_path.clone(), read_optional(&config_path)?),
        ],
        restart: Vec::new(),
        committed: false,
    };
    for (name, service) in [
        (SERVICE_NAME, existing_service.as_ref()),
        (LEGACY_SERVICE_NAME, legacy_service.as_ref()),
    ] {
        if let Some(service) = service
            && service.query_status()?.current_state == ServiceState::Running
        {
            rollback.restart.push(name);
        }
    }
    let config_bytes = serde_json::to_vec_pretty(&provisioned_config)
        .context("failed to encode the provisioned Agent configuration")?;
    if let Some(service) = existing_service.as_ref() {
        stop_service(service)?;
    }
    if let Some(service) = legacy_service.as_ref() {
        stop_service(service)?;
    }

    let agent_path = install_directory.join("meshrmm-agent.exe");
    let config_path = config_directory.join("agent.json");
    replace_file(&agent_path, embedded.executable)?;
    replace_file(&config_path, &config_bytes)?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("MeshRMM Agent"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: agent_path,
        launch_arguments: vec![
            OsString::from("--service"),
            OsString::from("--config"),
            config_path.into_os_string(),
        ],
        dependencies: vec![],
        account_name: Some(OsString::from("LocalSystem")),
        account_password: None,
    };
    let service = match existing_service {
        Some(service) => {
            service
                .change_config(&service_info)
                .context("failed to update the Agent service")?;
            service
        }
        None => manager
            .create_service(&service_info, service_access)
            .context("failed to register the Agent service")?,
    };
    service
        .set_description("MeshRMM LocalSystem supervisor for persistent remote access")
        .context("failed to set the Agent service description")?;
    service
        .update_failure_actions(ServiceFailureActions {
            reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86_400)),
            reboot_msg: None,
            command: None,
            actions: Some(vec![restart_after(5), restart_after(15), restart_after(60)]),
        })
        .context("failed to configure Agent service recovery")?;
    service
        .set_failure_actions_on_non_crash_failures(true)
        .context("failed to enable Agent service recovery")?;
    let start_result = service
        .start::<&OsStr>(&[])
        .context("failed to start the Agent service")
        .and_then(|()| wait_for_state(&service, ServiceState::Running, Duration::from_secs(20)));
    if let Err(error) = start_result {
        if let Some(legacy_service) = legacy_service.as_ref() {
            let _ = legacy_service.start::<&OsStr>(&[]);
        }
        return Err(error);
    }
    rollback.committed = true;
    let _ = std::fs::remove_file(config_directory.join("enrollment-pending.json"));
    let _ = std::fs::remove_file(&recovery_path);
    if let Some(legacy_service) = legacy_service {
        legacy_service
            .delete()
            .context("failed to unregister the legacy Agent service")?;
        remove_legacy_directories()?;
    }
    Ok(notice)
}

fn read_optional(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("could not back up existing installation"),
    }
}

struct InstallationRollback {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
    restart: Vec<&'static str>,
    committed: bool,
}

impl Drop for InstallationRollback {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(manager) =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        {
            if let Ok(service) = manager.open_service(
                SERVICE_NAME,
                ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
            ) {
                let _ = stop_service(&service);
            }
            for (path, original) in &self.files {
                let result = match original {
                    Some(bytes) => replace_file(path, bytes),
                    None => match std::fs::remove_file(path) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(error.into()),
                    },
                };
                if let Err(error) = result {
                    tracing::error!(%error, "installation rollback failed");
                }
            }
            for name in &self.restart {
                if let Ok(service) = manager.open_service(name, ServiceAccess::START) {
                    let _ = service.start::<&OsStr>(&[]);
                }
            }
        }
    }
}

fn remove_legacy_directories() -> anyhow::Result<()> {
    let install_directory = program_files()?.join("PulseRMM").join("Agent");
    let config_directory = program_data()?.join("PulseRMM").join("Agent");
    remove_directory_if_present(&config_directory)?;
    remove_directory_if_present(&install_directory)?;
    remove_empty_parent(&config_directory);
    remove_empty_parent(&install_directory);
    Ok(())
}

fn stop_service(service: &windows_service::service::Service) -> anyhow::Result<()> {
    if service.query_status()?.current_state != ServiceState::Stopped {
        service
            .stop()
            .context("failed to stop the existing Agent service")?;
        wait_for_state(service, ServiceState::Stopped, Duration::from_secs(20))?;
    }
    Ok(())
}

fn remove_directory_if_present(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove Agent data at {}", path.display()))
        }
    }
}

fn remove_empty_parent(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

fn schedule_helper_cleanup() -> anyhow::Result<()> {
    let helper = std::env::current_exe().context("could not locate the uninstall helper")?;
    let helper_directory = helper
        .parent()
        .context("the uninstall helper has no parent directory")?;
    let cleanup_working_directory = helper_directory
        .parent()
        .context("the uninstall helper directory has no parent directory")?;
    let cleanup = format!(
        "ping.exe 127.0.0.1 -n 3 >NUL & del /f /q \"{}\" & rmdir /q \"{}\"",
        helper.display(),
        helper_directory.display()
    );
    Command::new("cmd.exe")
        .args(["/D", "/S", "/C"])
        .arg(cleanup)
        // The cleanup process must not keep the helper directory open while removing it.
        .current_dir(cleanup_working_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .context("failed to schedule uninstall helper cleanup")?;
    Ok(())
}

fn wait_for_state(
    service: &windows_service::service::Service,
    expected: ServiceState,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = service
            .query_status()
            .context("failed to query Agent service status")?;
        if status.current_state == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "the Agent service did not reach {expected:?} within {} seconds",
                timeout.as_secs()
            );
        }
        sleep(Duration::from_millis(250));
    }
}

fn restart_after(seconds: u64) -> ServiceAction {
    ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(seconds),
    }
}

/// Secures an Agent data directory. One owned by another account is moved aside and replaced by a
/// new private directory: resetting its ACL in place would not revoke a WRITE_DAC handle its
/// owner opened beforehand, and nothing that account left inside can be trusted.
fn secure_or_replace_directory(path: &Path) -> anyhow::Result<()> {
    let Err(error) = private_directory::secure(path) else {
        return Ok(());
    };
    if error
        .downcast_ref::<private_directory::UntrustedOwner>()
        .is_none()
    {
        return Err(error);
    }
    let name = path
        .file_name()
        .with_context(|| format!("{} has no directory name", path.display()))?;
    let mut quarantine_name = name.to_owned();
    quarantine_name.push(format!(".untrusted-{}", uuid::Uuid::new_v4().simple()));
    let quarantine = path.with_file_name(quarantine_name);
    let source = wide(path.as_os_str());
    let destination = wide(quarantine.as_os_str());
    unsafe {
        windows::Win32::Storage::FileSystem::MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            windows::Win32::Storage::FileSystem::MOVE_FILE_FLAGS(0),
        )
    }
    .with_context(|| {
        format!("{error}, and it could not be moved aside; close programs using it or delete it")
    })?;
    // Nothing reads the quarantined copy, so anything left behind only costs disk space.
    let _ = std::fs::remove_dir_all(&quarantine);
    private_directory::create_new(path)
}

pub(crate) fn replace_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.new",
        path.extension().and_then(OsStr::to_str).unwrap_or("tmp")
    ));
    std::fs::write(&temporary, contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    let file = std::fs::OpenOptions::new().write(true).open(&temporary)?;
    file.sync_all()?;
    drop(file);
    let source = wide(temporary.as_os_str());
    let destination = wide(path.as_os_str());
    unsafe {
        windows::Win32::Storage::FileSystem::MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(destination.as_ptr()),
            windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| format!("failed to replace {}", path.display()))
}

/// Whether `path` is the configuration directory an installer created under ProgramData.
pub(crate) fn is_managed_config_directory(path: &Path) -> anyhow::Result<bool> {
    let program_data = program_data()?;
    let path = path.to_string_lossy();
    Ok(["MeshRMM", "PulseRMM"].into_iter().any(|product| {
        program_data
            .join(product)
            .join("Agent")
            .to_string_lossy()
            .eq_ignore_ascii_case(path.trim_end_matches(['\\', '/']))
    }))
}

/// ProgramData as Windows registers it. The elevated installer inherits the environment of the
/// user who launched it, so a user-defined `ProgramData` variable could redirect where it writes
/// the credential and the SYSTEM helpers. FOLDERID_ProgramData is not enough either: it expands
/// the registered `%SystemDrive%\ProgramData` with the caller's `SystemDrive` variable, so that
/// drive is taken from the Windows directory the kernel reports instead.
pub(crate) fn program_data() -> anyhow::Result<PathBuf> {
    let registered = registered_program_data()?;
    let mut windows = vec![0_u16; 260];
    let length = unsafe { GetSystemWindowsDirectoryW(Some(&mut windows)) } as usize;
    if length == 0 || length >= windows.len() {
        bail!("Windows did not provide its system directory");
    }
    let windows = PathBuf::from(OsString::from_wide(&windows[..length]));
    let Some(Component::Prefix(drive)) = windows.components().next() else {
        bail!("the Windows directory {} has no drive", windows.display());
    };
    const SYSTEM_DRIVE: &str = "%SystemDrive%";
    let expanded = match registered.get(..SYSTEM_DRIVE.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(SYSTEM_DRIVE) => {
            let mut path = drive.as_os_str().to_owned();
            path.push(&registered[SYSTEM_DRIVE.len()..]);
            PathBuf::from(path)
        }
        _ => PathBuf::from(&registered),
    };
    if expanded.as_os_str().to_string_lossy().contains('%') || !expanded.is_absolute() {
        bail!("the registered ProgramData directory {registered} cannot be resolved");
    }
    Ok(expanded)
}

fn registered_program_data() -> anyhow::Result<String> {
    let key = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList");
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut size = 0;
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            w!("ProgramData"),
            flags,
            None,
            None,
            Some(&mut size),
        )
    }
    .ok()
    .context("Windows did not register the ProgramData directory")?;
    let mut value = vec![0_u16; (size as usize).div_ceil(2)];
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            w!("ProgramData"),
            flags,
            None,
            Some(value.as_mut_ptr().cast()),
            Some(&mut size),
        )
    }
    .ok()
    .context("Windows did not register the ProgramData directory")?;
    let length = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    String::from_utf16(&value[..length]).context("the registered ProgramData directory is invalid")
}

/// The native Program Files directory as Windows registers it, for the same reason. Unlike
/// ProgramData, this known folder is stored as a literal path.
fn program_files() -> anyhow::Result<PathBuf> {
    let path = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None) }
        .context("Windows did not provide the Program Files directory")?;
    let value = OsString::from_wide(unsafe { path.as_wide() });
    unsafe { CoTaskMemFree(Some(path.0.cast_const().cast())) };
    if value.is_empty() {
        bail!("Windows did not provide the Program Files directory");
    }
    Ok(PathBuf::from(value))
}

/// Where the Agent keeps its WebRTC identity, beside its other ProgramData state.
pub(crate) fn identity_directory() -> anyhow::Result<PathBuf> {
    Ok(program_data()?
        .join("MeshRMM")
        .join("Agent")
        .join("identity"))
}

fn validate_bootstrap(config: &[u8]) -> anyhow::Result<InstallerBootstrap> {
    let bootstrap: InstallerBootstrap = serde_json::from_slice(config)
        .context("the embedded Agent installer authorization is invalid JSON")?;
    let server = url::Url::parse(&bootstrap.server)
        .context("the embedded Agent installer server URL is invalid")?;
    if server.scheme() != "https" || server.host_str().is_none() {
        bail!("the Agent installer requires an HTTPS server URL");
    }
    if bootstrap.install_token.len() < 32
        || !bootstrap
            .install_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("the embedded Agent installer authorization is invalid");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the Windows system clock is before the Unix epoch")?
        .as_millis() as u64;
    if bootstrap.expires_at_unix_ms <= now {
        bail!("this Agent installer authorization has expired; download a new installer");
    }
    Ok(bootstrap)
}

fn machine_name() -> anyhow::Result<String> {
    let mut buffer = vec![0_u16; 256];
    let mut length = buffer.len() as u32;
    unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(PWSTR(buffer.as_mut_ptr())),
            &mut length,
        )
    }
    .context("failed to read the Windows computer name")?;
    let name = String::from_utf16(&buffer[..length as usize])
        .context("the Windows computer name is not valid UTF-16")?;
    let name = name.trim().to_owned();
    if name.is_empty() || name.len() > 120 {
        bail!("the Windows computer name must contain between 1 and 120 characters");
    }
    Ok(name)
}

fn redeem_installer(
    bootstrap: &InstallerBootstrap,
    machine_name: String,
    redemption_key: String,
) -> anyhow::Result<ProvisionedAgentConfig> {
    let endpoint = format!(
        "{}/v1/agent-installers/redeem",
        bootstrap.server.trim_end_matches('/')
    );
    let http = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut response = http
        .post(&endpoint)
        .header(
            "Authorization",
            &format!("Bearer {}", bootstrap.install_token),
        )
        .send_json(&RedeemInstallerRequest {
            name: machine_name,
            redemption_key,
        })
        .context("failed to contact the MeshRMM Agent enrollment service")?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response
            .body_mut()
            .read_json::<ApiError>()
            .map(|body| body.error)
            .unwrap_or_else(|_| "the Agent enrollment service rejected the installer".to_owned());
        bail!("Agent enrollment failed with HTTP {status}: {detail}");
    }
    let config = response
        .body_mut()
        .read_json::<ProvisionedAgentConfig>()
        .context("the Agent enrollment service returned an invalid configuration")?;
    if config.device_id.is_empty() || config.agent_token.is_empty() || config.server.is_empty() {
        bail!("the Agent enrollment service returned an incomplete configuration");
    }
    meshrmm_self_update::validate_manifest_url(&config.update_manifest_url)
        .context("the Agent enrollment service returned an invalid update manifest URL")?;
    Ok(config)
}

struct EmbeddedInstaller<'a> {
    executable: &'a [u8],
    bootstrap: &'a [u8],
}

fn parse_embedded(bytes: &[u8]) -> anyhow::Result<Option<EmbeddedInstaller<'_>>> {
    let trailer_size = CONFIG_LENGTH_BYTES + ENROLLMENT_MAGIC.len();
    if bytes.len() < trailer_size || !bytes.ends_with(ENROLLMENT_MAGIC) {
        return Ok(None);
    }
    let length_offset = bytes.len() - trailer_size;
    let config_length = u64::from_le_bytes(
        bytes[length_offset..length_offset + CONFIG_LENGTH_BYTES]
            .try_into()
            .expect("length slice has a fixed size"),
    );
    let config_length =
        usize::try_from(config_length).context("embedded enrollment is too large")?;
    if config_length > length_offset {
        bail!("embedded Agent enrollment length is invalid");
    }
    let config_offset = length_offset - config_length;
    Ok(Some(EmbeddedInstaller {
        executable: &bytes[..config_offset],
        bootstrap: &bytes[config_offset..length_offset],
    }))
}

fn message(text: &str, error: bool) {
    let text = wide(OsStr::new(text));
    let title = wide(OsStr::new("MeshRMM Agent Setup"));
    let style = MB_OK
        | if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), style);
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_appended_enrollment_without_copying_it_into_installed_binary() {
        let executable = b"mock-pe-image";
        let config = br#"{"server":"https://example.com","install_token":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","expires_at_unix_ms":4102444800000}"#;
        let mut bytes = executable.to_vec();
        bytes.extend_from_slice(config);
        bytes.extend_from_slice(&(config.len() as u64).to_le_bytes());
        bytes.extend_from_slice(ENROLLMENT_MAGIC);

        let parsed = parse_embedded(&bytes).unwrap().unwrap();
        assert_eq!(parsed.executable, executable);
        assert_eq!(parsed.bootstrap, config);
        let bootstrap = validate_bootstrap(parsed.bootstrap).unwrap();
        assert_eq!(bootstrap.server, "https://example.com");
    }

    #[test]
    fn ignores_regular_agent_binary() {
        assert!(parse_embedded(b"mock-pe-image").unwrap().is_none());
    }

    #[test]
    fn system_directories_ignore_the_environment() {
        const EXPECTED: &str = "MESHRMM_TEST_SYSTEM_DIRECTORIES";
        let actual = format!(
            "{}|{}",
            program_data().unwrap().display(),
            program_files().unwrap().display()
        );
        if let Some(expected) = std::env::var_os(EXPECTED) {
            assert_eq!(actual, expected.to_string_lossy());
            return;
        }
        // Without a hostile environment the result matches the known folder.
        let known = unsafe {
            SHGetKnownFolderPath(
                &windows::Win32::UI::Shell::FOLDERID_ProgramData,
                KF_FLAG_DEFAULT,
                None,
            )
        }
        .unwrap();
        let known_path = PathBuf::from(OsString::from_wide(unsafe { known.as_wide() }));
        unsafe { CoTaskMemFree(Some(known.0.cast_const().cast())) };
        assert_eq!(program_data().unwrap(), known_path);
        // Run this test again in a child whose environment points every variable a user could
        // set at a directory they control.
        let decoy = std::env::temp_dir().join("meshrmm-decoy");
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "installer::tests::system_directories_ignore_the_environment",
                "--nocapture",
            ])
            .env(EXPECTED, &actual)
            .env("ProgramData", &decoy)
            .env("ALLUSERSPROFILE", &decoy)
            .env("ProgramFiles", &decoy)
            .env("ProgramW6432", &decoy)
            .env("SystemDrive", "Z:")
            .env("SystemRoot", &decoy)
            .env("windir", &decoy)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn recognizes_only_installer_managed_config_directories() {
        let program_data = program_data().unwrap();
        let managed = program_data.join("MeshRMM").join("Agent");
        assert!(is_managed_config_directory(&managed).unwrap());
        let upper = PathBuf::from(managed.to_string_lossy().to_uppercase() + "\\");
        assert!(is_managed_config_directory(&upper).unwrap());
        let legacy = program_data.join("PulseRMM").join("Agent");
        assert!(is_managed_config_directory(&legacy).unwrap());
        assert!(!is_managed_config_directory(&managed.join("updates")).unwrap());
        assert!(!is_managed_config_directory(Path::new(r"C:\MeshRMM\Agent")).unwrap());
    }

    #[test]
    fn stages_each_uninstall_helper_in_a_new_private_directory() {
        use crate::private_directory::test_support::*;
        if !elevated() {
            return;
        }
        let program_data = scratch("uninstall");
        let source = program_data.join("meshrmm-agent.exe");
        std::fs::write(&source, b"MZ helper").unwrap();

        let first = stage_uninstall_helper(&program_data, &source).unwrap();
        let second = stage_uninstall_helper(&program_data, &source).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"MZ helper");
        let directory = first.parent().unwrap();
        assert_ne!(directory, second.parent().unwrap());
        assert_eq!(directory.parent().unwrap(), program_data);
        assert!(
            directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("MeshRMM-uninstall-")
        );
        assert_eq!(sddl_of(directory), PRIVATE);
        remove(&program_data);
    }

    #[test]
    fn replaces_data_directory_owned_by_another_account() {
        use crate::private_directory::test_support::*;
        if !elevated() {
            return;
        }
        let data_root = scratch("squatted");
        let config_directory = data_root.join("Agent");
        std::fs::create_dir(&config_directory).unwrap();
        std::fs::write(config_directory.join("enrollment-pending.json"), b"{}").unwrap();
        set_sddl(&config_directory, "O:BUD:(A;OICI;FA;;;BU)(A;OICI;FA;;;BA)");

        secure_or_replace_directory(&config_directory).unwrap();
        assert_eq!(sddl_of(&config_directory), PRIVATE);
        assert_eq!(std::fs::read_dir(&config_directory).unwrap().count(), 0);
        let remaining = std::fs::read_dir(&data_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(remaining, [OsString::from("Agent")]);
        // A trusted directory keeps its contents.
        std::fs::write(config_directory.join("agent.json"), b"{}").unwrap();
        secure_or_replace_directory(&config_directory).unwrap();
        assert!(config_directory.join("agent.json").exists());
        remove(&data_root);
    }
}
