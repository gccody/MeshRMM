//! Installation of the Linux Agent, and where it keeps its state.
//!
//! The executable lives in `/opt/meshrmm`, and a systemd service runs it as
//! the root coordinator, which starts the session helpers itself. The
//! installed Agent keeps its state in `/var/lib/meshrmm-agent`, which only
//! root can read; a console Agent run by a user for development keeps it in
//! that user's `~/.local/state/meshrmm/agent`.
//!
//! KDE's KWin lends its screencast interface only to programs a desktop file
//! names, so the installer adds one for the Agent.
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

const SYSTEM_CONFIG_DIRECTORY: &str = "/var/lib/meshrmm-agent";
pub(crate) const INSTALL_DIRECTORY: &str = "/opt/meshrmm";
pub(crate) const EXECUTABLE: &str = "/opt/meshrmm/meshrmm-agent";
pub(crate) const SERVICE: &str = "meshrmm-agent.service";
const UNIT: &str = "/etc/systemd/system/meshrmm-agent.service";
const DESKTOP_FILE: &str = "/usr/share/applications/meshrmm-agent.desktop";

/// Replaces `path` atomically with a file only its owner can read.
pub(crate) fn replace_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.new",
        path.extension().and_then(OsStr::to_str).unwrap_or("tmp")
    ));
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace {}", path.display()))
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

pub(crate) fn config_directory() -> anyhow::Result<PathBuf> {
    let directory = if running_as_root() {
        PathBuf::from(SYSTEM_CONFIG_DIRECTORY)
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Path::new(&home).join(".local/state/meshrmm/agent")
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    Ok(directory)
}

pub(crate) fn identity_directory() -> anyhow::Result<PathBuf> {
    Ok(config_directory()?.join("identity"))
}

/// Installs or repairs the Agent from the running copy, enrolling it with the
/// hex-encoded installer authorization the dashboard's install command passes.
pub fn install(authorization: &str) -> anyhow::Result<()> {
    require_root()?;
    anyhow::ensure!(
        Path::new("/run/systemd/system").is_dir(),
        "the MeshRMM Agent needs systemd, which this computer does not run"
    );
    let bootstrap = crate::enrollment::validate_bootstrap(
        &decode_hex(authorization.trim())
            .context("the installer authorization is not valid hexadecimal")?,
    )?;
    let config_directory = config_directory()?;
    let config_path = config_directory.join("agent.json");
    let config = match std::fs::read(&config_path) {
        // Repair keeps the enrolled identity.
        Ok(existing) => {
            serde_json::from_slice::<crate::enrollment::ProvisionedAgentConfig>(&existing)
                .context("the installed Agent configuration is invalid")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let recovery_key = crate::enrollment::recovery_key(&config_directory)?;
            crate::enrollment::redeem_once(
                &config_directory,
                &bootstrap,
                computer_name()?,
                recovery_key,
            )?
        }
        Err(error) => {
            return Err(error).context("could not read the installed Agent configuration");
        }
    };
    if config.server.trim_end_matches('/') != bootstrap.server.trim_end_matches('/') {
        bail!(
            "this computer is already enrolled with another server; uninstall the Agent before enrolling it with this one"
        );
    }

    let _ = systemctl(&["stop", SERVICE]);
    install_executable()?;
    replace_file(&config_path, &serde_json::to_vec_pretty(&config)?)?;
    write_public_file(Path::new(UNIT), unit(&config_path).as_bytes())?;
    write_public_file(Path::new(DESKTOP_FILE), desktop_file().as_bytes())?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", SERVICE])?;
    crate::enrollment::finish(&config_directory);
    println!(
        "MeshRMM Agent installed. It runs as the {SERVICE} systemd service; `sudo {EXECUTABLE} --uninstall` removes it."
    );
    Ok(())
}

/// Removes the Agent. Stopping the service comes last, since the service may
/// be what started this.
pub fn uninstall() -> anyhow::Result<()> {
    require_root()?;
    let _ = systemctl(&["disable", SERVICE]);
    for path in [UNIT, DESKTOP_FILE] {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("could not remove {path}"));
            }
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(INSTALL_DIRECTORY);
    let _ = std::fs::remove_dir_all(SYSTEM_CONFIG_DIRECTORY);
    let _ = systemctl(&["stop", SERVICE]);
    let _ = systemctl(&["daemon-reload"]);
    Ok(())
}

/// Uninstalls from a separate systemd unit, which outlives the service that
/// the uninstallation stops; a child of the service would be stopped with it.
pub fn schedule_uninstall() -> anyhow::Result<()> {
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let status = Command::new("systemd-run")
        .args(["--unit", "meshrmm-agent-uninstall", "--collect", "--quiet"])
        .arg(executable)
        .arg("--uninstall")
        .status()
        .context("could not start the Agent uninstaller")?;
    anyhow::ensure!(
        status.success(),
        "systemd-run could not start the Agent uninstaller"
    );
    Ok(())
}

fn require_root() -> anyhow::Result<()> {
    if !running_as_root() {
        bail!("installing or removing the MeshRMM Agent requires root; run it with sudo");
    }
    Ok(())
}

/// Writes a file root owns and everyone can read, as systemd and the desktop
/// expect.
fn write_public_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    replace_file(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

/// Copies the running executable into place, unless it already runs from there.
fn install_executable() -> anyhow::Result<()> {
    let running = std::fs::canonicalize(std::env::current_exe()?)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(INSTALL_DIRECTORY)
        .with_context(|| format!("failed to create {INSTALL_DIRECTORY}"))?;
    std::fs::set_permissions(INSTALL_DIRECTORY, std::fs::Permissions::from_mode(0o755))?;
    if running == Path::new(EXECUTABLE) {
        return Ok(());
    }
    // Staged beside the target, so the rename is atomic and the new file
    // gets the folder's SELinux label.
    let staged = Path::new(INSTALL_DIRECTORY).join("meshrmm-agent.new");
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(&running, &staged)
        .with_context(|| format!("could not copy the Agent to {}", staged.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staged, EXECUTABLE).with_context(|| format!("could not install {EXECUTABLE}"))
}

pub(crate) fn systemctl(arguments: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("systemctl")
        .args(arguments)
        .output()
        .context("could not run systemctl")?;
    anyhow::ensure!(
        output.status.success(),
        "systemctl {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The computer's name: the pretty host name when it has one, else the
/// host name.
fn computer_name() -> anyhow::Result<String> {
    let pretty = std::fs::read_to_string("/etc/machine-info")
        .ok()
        .and_then(|info| {
            info.lines().find_map(|line| {
                Some(
                    line.strip_prefix("PRETTY_HOSTNAME=")?
                        .trim()
                        .trim_matches('"')
                        .to_owned(),
                )
            })
        })
        .filter(|name| !name.is_empty());
    let name = match pretty {
        Some(name) => name,
        None => {
            let mut buffer = [0_u8; 256];
            // SAFETY: the buffer is writable for its length.
            if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("could not read the host name");
            }
            let length = buffer
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(buffer.len());
            String::from_utf8_lossy(&buffer[..length]).into_owned()
        }
    };
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 120,
        "the computer name must contain between 1 and 120 characters"
    );
    Ok(name)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

fn unit(config: &Path) -> String {
    format!(
        "[Unit]
Description=MeshRMM Agent
Documentation=https://github.com/gccody/MeshRMM
Wants=network-online.target
After=network-online.target systemd-logind.service

[Service]
ExecStart={EXECUTABLE} --service --config {}
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
",
        config.display()
    )
}

/// Names the Agent's executable for KWin, which lends its screencast
/// interface only to programs a desktop file lists it for.
fn desktop_file() -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=MeshRMM Agent
Comment=Remote support for this computer
Exec={EXECUTABLE}
NoDisplay=true
X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,org_kde_kwin_fake_input
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_hex_authorization() {
        assert_eq!(decode_hex("7b7d").unwrap(), b"{}");
        assert!(decode_hex("7b7").is_none());
        assert!(decode_hex("zz").is_none());
    }

    #[test]
    fn the_service_runs_the_installed_agent() {
        let unit = unit(Path::new("/var/lib/meshrmm-agent/agent.json"));
        assert!(unit.contains(
            "ExecStart=/opt/meshrmm/meshrmm-agent --service --config /var/lib/meshrmm-agent/agent.json"
        ));
        assert!(unit.contains("Restart=always"));
        assert!(desktop_file().contains("Exec=/opt/meshrmm/meshrmm-agent\n"));
    }

    #[test]
    fn has_a_computer_name() {
        let name = computer_name().unwrap();
        assert!(!name.is_empty());
    }
}
