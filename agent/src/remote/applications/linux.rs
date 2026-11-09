//! Applications on Linux: Flatpaks in the system installation, snaps, and
//! package and script installers. Distribution packages are patch
//! management's (docs/patch-management.md), so APT, DNF and pacman packages
//! aren't listed here; a custom `.deb` or `.rpm` installs through them.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use meshrmm_protocol::{
    APPLICATION_SCAN_TIMEOUT_SECONDS, PackageKey, PackageRef, PackageSource, ScannedPackage,
};

use super::process::{self, Ran};
use super::{Attempt, ITEM_TIMEOUT, Verdict, extension, parse};

const SCAN_TIMEOUT: Duration = Duration::from_secs(APPLICATION_SCAN_TIMEOUT_SECONDS);
const FOLDERS: [&str; 5] = [
    "/usr/bin",
    "/usr/sbin",
    "/bin",
    "/usr/local/bin",
    "/snap/bin",
];

fn find(name: &str) -> Option<PathBuf> {
    FOLDERS
        .iter()
        .map(|folder| Path::new(folder).join(name))
        .find(|path| path.is_file())
}

/// Flatpak or snap.
pub(super) struct Tool {
    source: PackageSource,
    program: PathBuf,
}

impl Tool {
    pub(super) fn source(&self) -> PackageSource {
        self.source
    }

    pub(super) fn handles(&self, source: PackageSource) -> bool {
        self.source == source
    }

    fn run(&self, args: &[&str], timeout: Duration) -> anyhow::Result<Ran> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .current_dir("/");
        process::run(command, timeout)
    }
}

pub(super) fn tools() -> Vec<Tool> {
    [
        ("flatpak", PackageSource::Flatpak),
        ("snap", PackageSource::Snap),
    ]
    .into_iter()
    .filter_map(|(name, source)| {
        // snap is useless without its daemon, as in containers.
        if source == PackageSource::Snap && !Path::new("/run/snapd.socket").exists() {
            return None;
        }
        Some(Tool {
            source,
            program: find(name)?,
        })
    })
    .collect()
}

fn checked(ran: Ran, what: &str) -> anyhow::Result<Ran> {
    if ran.success() {
        Ok(ran)
    } else {
        anyhow::bail!("{what} failed: {}", ran.complaint())
    }
}

pub(super) fn scan(tool: &Tool) -> anyhow::Result<(Option<String>, Vec<ScannedPackage>)> {
    match tool.source {
        PackageSource::Flatpak => {
            let version = tool
                .run(&["--version"], SCAN_TIMEOUT)?
                .stdout
                .trim()
                .strip_prefix("Flatpak ")
                .map(str::to_owned);
            let listed = checked(
                tool.run(
                    &[
                        "list",
                        "--system",
                        "--app",
                        "--columns=application,name,version",
                    ],
                    SCAN_TIMEOUT,
                )?,
                "flatpak list",
            )?;
            let mut packages = parse::flatpak_list(&listed.stdout);
            let updates = checked(
                tool.run(
                    &[
                        "remote-ls",
                        "--system",
                        "--updates",
                        "--app",
                        "--columns=application,version",
                    ],
                    SCAN_TIMEOUT,
                )?,
                "flatpak remote-ls",
            )?;
            for (id, version) in parse::flatpak_updates(&updates.stdout) {
                if let Some(package) = packages.iter_mut().find(|package| package.id == id) {
                    // An update of the build alone keeps the version.
                    package.available_version =
                        Some(version.unwrap_or_else(|| "a newer build".to_owned()));
                }
            }
            Ok((version, packages))
        }
        _ => {
            let version = tool
                .run(&["version"], SCAN_TIMEOUT)?
                .stdout
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .map(str::to_owned);
            let listed = tool.run(&["list", "--unicode=never"], SCAN_TIMEOUT)?;
            // With no snaps installed, `snap list` says so and fails.
            let packages = if listed.success() {
                parse::snap_list(&listed.stdout)
            } else if listed.stderr.contains("No snaps") {
                Vec::new()
            } else {
                anyhow::bail!("snap list failed: {}", listed.complaint());
            };
            let refresh = checked(
                tool.run(&["refresh", "--list", "--unicode=never"], SCAN_TIMEOUT)?,
                "snap refresh --list",
            )?;
            let updates = parse::snap_updates(&refresh.stdout)
                .into_iter()
                .map(|(id, version)| ScannedPackage {
                    source: PackageSource::Snap,
                    name: id.clone(),
                    id,
                    version: None,
                    available_version: Some(version),
                })
                .collect();
            Ok((version, parse::merge(packages, updates)))
        }
    }
}

fn attempt(tool: &Tool, args: &[&str]) -> Attempt {
    Attempt::of(tool.run(args, ITEM_TIMEOUT), Verdict::by_code)
}

/// Installs a Flatpak from the system installation's remotes, such as
/// Flathub, or a snap from its default channel. A snap that asks for
/// classic confinement, which the library chose, gets it.
pub(super) fn install(tool: &Tool, package: &PackageRef) -> Attempt {
    let id = package.id.as_str();
    match tool.source {
        PackageSource::Flatpak => {
            attempt(tool, &["install", "--system", "--noninteractive", "-y", id])
        }
        _ => {
            let first = attempt(tool, &["install", id]);
            if first.outcome == meshrmm_protocol::ItemOutcome::Failed
                && first.output.contains("classic confinement")
            {
                return attempt(tool, &["install", "--classic", id]);
            }
            first
        }
    }
}

pub(super) fn update(tool: &Tool, package: &ScannedPackage) -> Attempt {
    match tool.source {
        PackageSource::Flatpak => attempt(
            tool,
            &["update", "--system", "--noninteractive", "-y", &package.id],
        ),
        _ => attempt(tool, &["refresh", &package.id]),
    }
}

pub(super) fn uninstall(tool: &Tool, package: &PackageKey) -> Attempt {
    match tool.source {
        PackageSource::Flatpak => attempt(
            tool,
            &[
                "uninstall",
                "--system",
                "--noninteractive",
                "-y",
                &package.id,
            ],
        ),
        _ => attempt(tool, &["remove", &package.id]),
    }
}

/// Installs a `.deb` with APT (or dpkg), an `.rpm` with DNF, YUM, zypper
/// (or rpm), and runs `.sh`, `.run` and `.bin` installers: a program
/// (ELF) itself, a script with `sh`. The arguments follow.
pub(super) fn run_installer(path: &Path, arguments: &str) -> Attempt {
    let arguments = match parse::split_arguments(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return Attempt::failed(format!("{error:#}")),
    };
    let mut command = match extension(path).as_str() {
        "deb" => match (find("apt-get"), find("dpkg")) {
            (Some(apt), _) => {
                let mut command = Command::new(apt);
                command
                    .args([
                        "install",
                        "-y",
                        "-q",
                        "-o",
                        "Dpkg::Options::=--force-confold",
                    ])
                    .arg(path)
                    .env("DEBIAN_FRONTEND", "noninteractive");
                command
            }
            (None, Some(dpkg)) => {
                let mut command = Command::new(dpkg);
                command.arg("-i").arg(path);
                command
            }
            _ => return Attempt::failed("This device has no APT or dpkg to install a .deb with"),
        },
        "rpm" => {
            if let Some(manager) = find("dnf").or_else(|| find("yum")) {
                let mut command = Command::new(manager);
                command.args(["install", "-y"]).arg(path);
                command
            } else if let Some(zypper) = find("zypper") {
                let mut command = Command::new(zypper);
                command
                    .args(["--non-interactive", "install", "--allow-unsigned-rpm"])
                    .arg(path);
                command
            } else if let Some(rpm) = find("rpm") {
                let mut command = Command::new(rpm);
                command.arg("-i").arg(path);
                command
            } else {
                return Attempt::failed(
                    "This device has no DNF, YUM, zypper or rpm to install an .rpm with",
                );
            }
        }
        "sh" | "run" | "bin" => match executable(path) {
            Ok(true) => Command::new(path),
            Ok(false) => {
                let mut command = Command::new("/bin/sh");
                command.arg(path);
                command
            }
            Err(error) => return Attempt::failed(format!("{error:#}")),
        },
        other => {
            return Attempt::failed(format!(
                "Linux installs .deb and .rpm packages and runs .sh, .run and .bin installers, not .{other} files"
            ));
        }
    };
    command.args(arguments).current_dir("/");
    Attempt::of(process::run(command, ITEM_TIMEOUT), Verdict::by_code)
}

/// Whether the installer at `path` is a program, which is made executable
/// for its owner (the Agent) to run; otherwise it's a script for `sh`.
fn executable(path: &Path) -> anyhow::Result<bool> {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    use anyhow::Context as _;
    let mut head = [0_u8; 4];
    let read = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .context("could not read the installer")?;
    if !parse::is_elf(&head[..read]) {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .context("could not make the installer executable")?;
    Ok(true)
}

pub(super) fn run_command(line: &str) -> Attempt {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", line]).current_dir("/");
    Attempt::of(process::run(command, ITEM_TIMEOUT), Verdict::by_code)
}

pub(super) fn uninstall_inventory(_name: &str, _version: Option<&str>) -> Attempt {
    Attempt::failed("Programs are uninstalled from the inventory on Windows only.")
}

pub(super) fn registry_exists(_key: &str, _value: Option<&str>) -> bool {
    false
}
