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

use super::{Attempt, ITEM_TIMEOUT, Verdict, extension, parse};
use crate::remote::process::{self, Ran};

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
            .env("LC_ALL", locale())
            .env("LANG", locale())
            .current_dir("/");
        process::run(command, timeout)
    }
}

/// The locale Flatpak and snap run in: `C.UTF-8` where the device has it,
/// so their messages keep their quotation marks and non-ASCII names, else
/// `C`. Both are English, which is what the output is read as.
fn locale() -> &'static str {
    static LOCALE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    LOCALE.get_or_init(|| {
        let listed = find("locale").and_then(|program| {
            let mut command = Command::new(program);
            command.arg("-a").env("LC_ALL", "C").current_dir("/");
            process::run(command, Duration::from_secs(30)).ok()
        });
        if listed.is_some_and(|ran| ran.success() && parse::has_c_utf8(&ran.stdout)) {
            "C.UTF-8"
        } else {
            "C"
        }
    })
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
            let mut packages = if listed.success() {
                parse::snap_list(&listed.stdout)
            } else if listed.stderr.contains("No snaps") {
                Vec::new()
            } else {
                anyhow::bail!("snap list failed: {}", listed.complaint());
            };
            // snapd says which are applications; without its answer, the
            // notes have left out the system's snaps already.
            match snapd_applications() {
                Ok(applications) => packages.retain(|package| applications.contains(&package.id)),
                Err(error) => tracing::debug!(
                    error = format!("{error:#}"),
                    "could not ask snapd which snaps are applications"
                ),
            }
            let refresh = checked(
                tool.run(&["refresh", "--list", "--unicode=never"], SCAN_TIMEOUT)?,
                "snap refresh --list",
            )?;
            // Updates of snaps that aren't listed, such as bases, aren't
            // applications' updates.
            let updates = parse::snap_updates(&refresh.stdout)
                .into_iter()
                .filter(|(id, _)| packages.iter().any(|package| &package.id == id))
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

/// The names of the snaps that are applications, from snapd's REST API on
/// its socket: `GET /v2/snaps`, over HTTP/1.0 so the answer is the body
/// as is, to the end of the connection.
fn snapd_applications() -> anyhow::Result<std::collections::HashSet<String>> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use anyhow::Context as _;

    const SNAPD_TIMEOUT: Duration = Duration::from_secs(30);
    /// snapd's answer for a few hundred snaps is well under this.
    const MOST: u64 = 16 * 1024 * 1024;
    let mut stream =
        UnixStream::connect("/run/snapd.socket").context("could not connect to snapd")?;
    stream.set_read_timeout(Some(SNAPD_TIMEOUT))?;
    stream.set_write_timeout(Some(SNAPD_TIMEOUT))?;
    stream
        .write_all(b"GET /v2/snaps HTTP/1.0\r\nHost: snapd\r\nAccept: application/json\r\n\r\n")?;
    let mut answer = Vec::new();
    stream.take(MOST).read_to_end(&mut answer)?;
    let answer = String::from_utf8_lossy(&answer);
    let (head, body) = answer
        .split_once("\r\n\r\n")
        .context("snapd's answer has no body")?;
    anyhow::ensure!(
        head.lines()
            .next()
            .is_some_and(|status| status.split_whitespace().nth(1) == Some("200")),
        "snapd answered {}",
        head.lines().next().unwrap_or_default()
    );
    parse::snapd_applications(body).context("snapd's answer isn't its list of snaps")
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
            let first = attempt(tool, &["install", "--system", "--noninteractive", "-y", id]);
            // With several remotes that have it, such as Fedora's own and
            // Flathub, Flatpak asks which to use, and fails without a
            // terminal. Name one: Flathub first, then the others in the
            // installation's order, past those that don't have it.
            if first.outcome != meshrmm_protocol::ItemOutcome::Failed
                || !first.output.contains("Multiple remotes")
            {
                return first;
            }
            for remote in flatpak_remotes(tool) {
                let tried = attempt(
                    tool,
                    &["install", "--system", "--noninteractive", "-y", &remote, id],
                );
                if !tried.output.contains("Nothing matches") {
                    return tried;
                }
            }
            first
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

/// The system installation's remotes, Flathub first.
fn flatpak_remotes(tool: &Tool) -> Vec<String> {
    let Ok(ran) = tool.run(&["remotes", "--system", "--columns=name"], SCAN_TIMEOUT) else {
        return Vec::new();
    };
    if !ran.success() {
        return Vec::new();
    }
    let mut remotes = parse::flatpak_remotes(&ran.stdout);
    remotes.sort_by_key(|remote| remote != "flathub");
    remotes
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
