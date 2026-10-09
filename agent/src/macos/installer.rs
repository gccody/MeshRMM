//! Installation of the macOS Agent, and where it keeps its state.
//!
//! The app bundle lives in `/Library/Application Support/MeshRMM`. A launchd
//! daemon runs the root coordinator, and a launchd agent runs a session helper
//! in every graphical session, the login window's included. The installed
//! Agent keeps its state in `/Library/Application Support/MeshRMM/Agent`,
//! which only root can read; a console Agent run by a user for development
//! keeps it in that user's own Application Support folder.
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;

const SYSTEM_CONFIG_DIRECTORY: &str = "/Library/Application Support/MeshRMM/Agent";

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

pub(crate) fn config_directory() -> anyhow::Result<PathBuf> {
    // SAFETY: geteuid has no preconditions.
    let directory = if unsafe { libc::geteuid() } == 0 {
        PathBuf::from(SYSTEM_CONFIG_DIRECTORY)
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Path::new(&home).join("Library/Application Support/MeshRMM/Agent")
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

pub(crate) const SUPPORT_DIRECTORY: &str = "/Library/Application Support/MeshRMM";
pub(crate) const APP: &str = "/Library/Application Support/MeshRMM/MeshRMM Agent.app";
const EXECUTABLE: &str = "Contents/MacOS/meshrmm-agent";
pub(crate) const DAEMON_LABEL: &str = "com.meshrmm.agent";
const DAEMON_PLIST: &str = "/Library/LaunchDaemons/com.meshrmm.agent.plist";
pub(crate) const HELPER_LABEL: &str = "com.meshrmm.agent.session";
const HELPER_PLIST: &str = "/Library/LaunchAgents/com.meshrmm.agent.session.plist";
/// Where the root coordinator accepts session helpers. Only root can create
/// files in `/var/run`, so no other process can take the name first.
pub(crate) const HELPER_SOCKET: &str = "/var/run/com.meshrmm.agent.sock";
const BUNDLE_IDENTIFIER: &str = "com.meshrmm.agent";

/// Installs or repairs the Agent from the running copy, enrolling it with the
/// hex-encoded installer authorization the dashboard's install command passes.
pub fn install(authorization: &str) -> anyhow::Result<()> {
    require_root()?;
    let bootstrap = crate::enrollment::validate_bootstrap(
        &decode_hex(authorization.trim())
            .context("the installer authorization is not valid hexadecimal")?,
    )?;
    create_directory(Path::new(SUPPORT_DIRECTORY), 0o755)?;
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
        anyhow::bail!(
            "this Mac is already enrolled with another server; uninstall the Agent before enrolling it with this one"
        );
    }

    stop_services();
    install_app()?;
    replace_file(&config_path, &serde_json::to_vec_pretty(&config)?)?;
    let executable = Path::new(APP).join(EXECUTABLE);
    write_root_file(
        Path::new(DAEMON_PLIST),
        daemon_plist(&executable, &config_path).as_bytes(),
    )?;
    write_root_file(
        Path::new(HELPER_PLIST),
        helper_plist(&executable).as_bytes(),
    )?;
    launchctl(&["bootstrap", "system", DAEMON_PLIST])?;
    // Users signed in now get their helper here; launchd starts it for later
    // sign-ins and for the login window.
    for uid in graphical_users() {
        let domain = format!("gui/{uid}");
        if let Err(error) = launchctl(&["bootstrap", &domain, HELPER_PLIST]) {
            eprintln!("Could not start the MeshRMM session helper for user {uid}: {error:#}");
        }
    }
    crate::enrollment::finish(&config_directory);
    println!(
        "MeshRMM Agent installed. Allow \"MeshRMM Agent\" under Screen & System Audio Recording, Accessibility and Input Monitoring in System Settings > Privacy & Security so technicians can see and control this Mac."
    );
    Ok(())
}

/// Removes the Agent. Stopping the coordinator comes last, since the
/// coordinator may be the process running this.
pub fn uninstall() -> anyhow::Result<()> {
    require_root()?;
    for uid in graphical_users() {
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{HELPER_LABEL}")]);
    }
    for path in [HELPER_PLIST, DAEMON_PLIST] {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("could not remove {path}"));
            }
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(SUPPORT_DIRECTORY);
    let _ = std::fs::remove_dir_all("/Library/Logs/MeshRMM");
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
    let _ = std::fs::remove_file(HELPER_SOCKET);
    Ok(())
}

/// Uninstalls from a separate process, which outlives the coordinator that
/// the uninstallation stops.
pub fn schedule_uninstall() -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--uninstall")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
        .spawn()
        .context("could not start the Agent uninstaller")?;
    Ok(())
}

fn require_root() -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        anyhow::bail!("installing or removing the MeshRMM Agent requires root; run it with sudo");
    }
    Ok(())
}

fn create_directory(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    // An existing directory may have been created with other permissions.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Writes a file root owns and everyone can read, as launchd requires.
fn write_root_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    replace_file(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

/// Copies the running app bundle into place, or wraps a bare development
/// binary in a minimal one.
fn install_app() -> anyhow::Result<()> {
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    let staged = Path::new(SUPPORT_DIRECTORY).join("MeshRMM Agent.app.new");
    let _ = std::fs::remove_dir_all(&staged);
    let bundle = executable.ancestors().nth(3).filter(|bundle| {
        bundle
            .extension()
            .is_some_and(|extension| extension == "app")
    });
    match bundle {
        Some(bundle) if bundle != Path::new(APP) => {
            let status = std::process::Command::new("/usr/bin/ditto")
                .arg(bundle)
                .arg(&staged)
                .status()
                .context("could not run ditto")?;
            anyhow::ensure!(status.success(), "could not copy {}", bundle.display());
        }
        // Reinstalling from the installed copy keeps it.
        Some(_) => return secure_bundle(Path::new(APP)),
        None => {
            let contents = staged.join("Contents");
            create_directory(&contents.join("MacOS"), 0o755)?;
            std::fs::copy(&executable, staged.join(EXECUTABLE))?;
            std::fs::write(contents.join("Info.plist"), info_plist())?;
        }
    }
    secure_bundle(&staged)?;
    let _ = std::fs::remove_dir_all(APP);
    std::fs::rename(&staged, APP).with_context(|| format!("could not install {APP}"))
}

/// Gives root everything in an app bundle and takes write access away from
/// everyone else, since the root coordinator runs its code. `ditto` keeps the
/// owner an archive or copy came with: the account that built the release.
pub(crate) fn secure_bundle(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {}", path.display()))?;
    std::os::unix::fs::lchown(path, Some(0), Some(0))
        .with_context(|| format!("could not give root {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(metadata.permissions().mode() & 0o755),
    )
    .with_context(|| format!("could not protect {}", path.display()))?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            secure_bundle(&entry?.path())?;
        }
    }
    Ok(())
}

fn stop_services() {
    for uid in graphical_users() {
        let _ = launchctl(&["bootout", &format!("gui/{uid}/{HELPER_LABEL}")]);
    }
    let _ = launchctl(&["bootout", &format!("system/{DAEMON_LABEL}")]);
}

fn launchctl(arguments: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new("/bin/launchctl")
        .args(arguments)
        .output()
        .context("could not run launchctl")?;
    anyhow::ensure!(
        output.status.success(),
        "launchctl {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Users with a graphical login session, from their loginwindow processes.
pub(crate) fn graphical_users() -> Vec<u32> {
    let Ok(output) = std::process::Command::new("/bin/ps")
        .args(["-axo", "uid=,comm="])
        .output()
    else {
        return Vec::new();
    };
    let mut users = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (uid, command) = line.trim().split_once(char::is_whitespace)?;
            command
                .trim()
                .ends_with("/loginwindow")
                .then(|| uid.parse::<u32>().ok())
                .flatten()
        })
        .filter(|&uid| uid != 0)
        .collect::<Vec<_>>();
    users.sort_unstable();
    users.dedup();
    users
}

fn computer_name() -> anyhow::Result<String> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        .context("could not read the computer name")?;
    let name = String::from_utf8(output.stdout)?.trim().to_owned();
    anyhow::ensure!(
        output.status.success() && !name.is_empty() && name.len() <= 120,
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

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn daemon_plist(executable: &Path, config: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{DAEMON_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--service</string>
        <string>--config</string>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>AbandonProcessGroup</key>
    <true/>
</dict>
</plist>
"#,
        escape(&executable.to_string_lossy()),
        escape(&config.to_string_lossy()),
    )
}

fn helper_plist(executable: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{HELPER_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--session-helper</string>
    </array>
    <key>LimitLoadToSessionType</key>
    <array>
        <string>Aqua</string>
        <string>LoginWindow</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>ThrottleInterval</key>
    <integer>5</integer>
</dict>
</plist>
"#,
        escape(&executable.to_string_lossy()),
    )
}

pub(crate) fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>meshrmm-agent</string>
    <key>CFBundleIdentifier</key>
    <string>{BUNDLE_IDENTIFIER}</string>
    <key>CFBundleName</key>
    <string>MeshRMM Agent</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>{}</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.3</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
"#,
        meshrmm_self_update::CURRENT_VERSION
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
    fn launchd_jobs_run_the_installed_agent() {
        let executable = Path::new(APP).join(EXECUTABLE);
        let daemon = daemon_plist(&executable, Path::new("/x/agent.json"));
        assert!(daemon.contains("<string>/Library/Application Support/MeshRMM/MeshRMM Agent.app/Contents/MacOS/meshrmm-agent</string>"));
        assert!(daemon.contains("<string>--service</string>"));
        let helper = helper_plist(&executable);
        assert!(helper.contains("<string>LoginWindow</string>"));
        assert!(helper.contains("<string>--session-helper</string>"));
        for plist in [daemon, helper, info_plist()] {
            let status = std::process::Command::new("/usr/bin/plutil")
                .args(["-lint", "-"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(plist.as_bytes())?;
                    child.wait()
                })
                .unwrap();
            assert!(status.success(), "{plist}");
        }
    }

    #[test]
    fn finds_graphical_users_without_root() {
        // Runs wherever tests run; only checks that parsing does not fail.
        let _ = graphical_users();
    }
}
