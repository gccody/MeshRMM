use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use windows::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW, SW_SHOWNORMAL,
};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::service::SERVICE_NAME;

const ENROLLMENT_MAGIC: &[u8] = b"PULSERMM-BOOTSTRAP-V1";
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
    pulsermm_self_update::DEFAULT_MANIFEST_URL.to_owned()
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
        Ok(()) => {
            message(
                "PulseRMM Agent installed successfully. The LocalSystem service is running. You can now delete the downloaded installer.",
                false,
            );
            Ok(())
        }
        Err(error) => {
            message(
                &format!("PulseRMM Agent installation failed:\n\n{error:#}"),
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
    let helper_directory = std::env::temp_dir().join("PulseRMM");
    std::fs::create_dir_all(&helper_directory).with_context(|| {
        format!(
            "failed to create uninstall helper directory {}",
            helper_directory.display()
        )
    })?;
    let helper = helper_directory.join(format!(
        "uninstall-{}-{}.exe",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    std::fs::copy(&source, &helper)
        .with_context(|| format!("failed to create uninstall helper {}", helper.display()))?;
    Command::new(&helper)
        .arg("--uninstall")
        // The desktop worker normally runs from the install directory. Do not let the helper
        // inherit that working directory or Windows will keep the otherwise-empty directory
        // in use while the helper tries to remove it.
        .current_dir(&helper_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .with_context(|| format!("failed to start uninstall helper {}", helper.display()))?;
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    // Give the worker enough time to flush its coordinator acknowledgement before stopping the
    // service terminates that worker process.
    sleep(Duration::from_secs(1));
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("administrator access is required to uninstall the Agent service")?;
    let service_access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    if let Ok(service) = manager.open_service(SERVICE_NAME, service_access) {
        stop_service(&service)?;
        service
            .delete()
            .context("failed to unregister the Agent service")?;
    }
    drop(manager);

    let install_directory = required_system_directory("ProgramFiles")?
        .join("PulseRMM")
        .join("Agent");
    let config_directory = required_system_directory("ProgramData")?
        .join("PulseRMM")
        .join("Agent");
    remove_directory_if_present(&config_directory)?;
    remove_directory_if_present(&install_directory)?;
    remove_empty_parent(&config_directory);
    remove_empty_parent(&install_directory);
    schedule_helper_cleanup()?;
    Ok(())
}

fn install() -> anyhow::Result<()> {
    let source_path = std::env::current_exe().context("could not locate the Agent installer")?;
    let source_bytes = std::fs::read(&source_path).context("could not read the Agent installer")?;
    let embedded = parse_embedded(&source_bytes)?
        .context("this executable does not contain a PulseRMM Agent enrollment")?;
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

    let program_files = required_system_directory("ProgramFiles")?;
    let program_data = required_system_directory("ProgramData")?;
    let install_directory = program_files.join("PulseRMM").join("Agent");
    let config_directory = program_data.join("PulseRMM").join("Agent");
    std::fs::create_dir_all(&install_directory)
        .with_context(|| format!("failed to create {}", install_directory.display()))?;
    std::fs::create_dir_all(&config_directory)
        .with_context(|| format!("failed to create {}", config_directory.display()))?;
    restrict_config_directory(&config_directory)?;

    let machine_name = machine_name()?;
    let provisioned_config = redeem_installer(&bootstrap, machine_name)?;
    let config_bytes = serde_json::to_vec_pretty(&provisioned_config)
        .context("failed to encode the provisioned Agent configuration")?;
    if let Some(service) = existing_service.as_ref() {
        stop_service(service)?;
    }

    let agent_path = install_directory.join("pulsermm-agent.exe");
    let config_path = config_directory.join("agent.json");
    replace_file(&agent_path, embedded.executable)?;
    replace_file(&config_path, &config_bytes)?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("PulseRMM Agent"),
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
        .set_description("PulseRMM LocalSystem supervisor for the active desktop Agent")
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
    service
        .start::<&OsStr>(&[])
        .context("failed to start the Agent service")?;
    wait_for_state(&service, ServiceState::Running, Duration::from_secs(20))?;
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

fn restrict_config_directory(path: &Path) -> anyhow::Result<()> {
    let output = Command::new("icacls.exe")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .output()
        .context("failed to start icacls while protecting the Agent credential")?;
    if !output.status.success() {
        let details = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            String::from_utf8_lossy(&output.stderr)
        };
        bail!(
            "icacls could not protect {}: {}",
            path.display(),
            details.trim()
        );
    }
    Ok(())
}

fn replace_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.new",
        path.extension().and_then(OsStr::to_str).unwrap_or("tmp")
    ));
    std::fs::write(&temporary, contents)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
    }
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed to move the new file into {}", path.display()))
}

fn required_system_directory(name: &str) -> anyhow::Result<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .with_context(|| format!("Windows did not provide the {name} directory"))
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
) -> anyhow::Result<ProvisionedAgentConfig> {
    let endpoint = format!(
        "{}/v1/agent-installers/redeem",
        bootstrap.server.trim_end_matches('/')
    );
    let http = ureq::Agent::config_builder()
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
        .send_json(&RedeemInstallerRequest { name: machine_name })
        .context("failed to contact the PulseRMM Agent enrollment service")?;
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
    pulsermm_self_update::validate_manifest_url(&config.update_manifest_url)
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
    let title = wide(OsStr::new("PulseRMM Agent Setup"));
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
}
