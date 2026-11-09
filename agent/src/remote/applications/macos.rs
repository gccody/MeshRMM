//! Applications on a Mac: Homebrew formulae and casks, and installer
//! packages.
//!
//! Homebrew refuses to run as root, so the Agent runs `brew` as the user who
//! owns the Homebrew installation, through `sudo -n` (which never asks for
//! a password), with Homebrew's automatic update, cleanup and hints off. A
//! cask whose installer itself needs an administrator's password fails, as
//! it would for that user without a terminal.
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use meshrmm_protocol::{
    APPLICATION_SCAN_TIMEOUT_SECONDS, PackageKey, PackageRef, PackageSource, ScannedPackage,
};

use super::process::{self, Ran};
use super::{Attempt, ITEM_TIMEOUT, Verdict, extension, parse};

const SCAN_TIMEOUT: Duration = Duration::from_secs(APPLICATION_SCAN_TIMEOUT_SECONDS);
/// Where Homebrew installs itself on Apple silicon and on Intel Macs.
const BREW: [&str; 2] = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"];
const ENVIRONMENT: [&str; 6] = [
    "HOMEBREW_NO_AUTO_UPDATE=1",
    "HOMEBREW_NO_INSTALL_CLEANUP=1",
    "HOMEBREW_NO_ENV_HINTS=1",
    "HOMEBREW_NO_ANALYTICS=1",
    "HOMEBREW_NO_COLOR=1",
    "NONINTERACTIVE=1",
];

/// Homebrew, and the user it runs as.
pub(super) struct Tool {
    program: PathBuf,
    owner: u32,
}

impl Tool {
    pub(super) fn source(&self) -> PackageSource {
        PackageSource::Homebrew
    }

    pub(super) fn handles(&self, source: PackageSource) -> bool {
        matches!(
            source,
            PackageSource::Homebrew | PackageSource::HomebrewCask
        )
    }

    fn command(&self, args: &[&str]) -> anyhow::Result<Command> {
        // SAFETY: geteuid has no preconditions.
        let root = unsafe { libc::geteuid() } == 0;
        let mut command = if root {
            anyhow::ensure!(
                self.owner != 0,
                "Homebrew belongs to root, and Homebrew doesn't run as root"
            );
            let mut command = Command::new("/usr/bin/sudo");
            command.args([
                "-n",
                "-u",
                &format!("#{}", self.owner),
                "-H",
                "/usr/bin/env",
            ]);
            command.args(ENVIRONMENT);
            command.arg(&self.program);
            command
        } else {
            let mut command = Command::new(&self.program);
            for pair in ENVIRONMENT {
                if let Some((name, value)) = pair.split_once('=') {
                    command.env(name, value);
                }
            }
            command
        };
        command.args(args).current_dir("/private/tmp");
        Ok(command)
    }

    fn run(&self, args: &[&str], timeout: Duration) -> anyhow::Result<Ran> {
        process::run(self.command(args)?, timeout)
    }
}

pub(super) fn tools() -> Vec<Tool> {
    BREW.iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .and_then(|program| {
            Some(Tool {
                owner: std::fs::metadata(program).ok()?.uid(),
                program: program.to_owned(),
            })
        })
        .into_iter()
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
    let version = tool
        .run(&["--version"], SCAN_TIMEOUT)?
        .stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Homebrew "))
        .map(str::to_owned);
    // Learn of new versions; a failure (no network) still lists what's known.
    if let Ok(ran) = tool.run(&["update", "--quiet"], SCAN_TIMEOUT)
        && !ran.success()
    {
        tracing::warn!(complaint = ran.complaint(), "brew update failed");
    }
    let formulae = checked(
        tool.run(&["list", "--formula", "--versions"], SCAN_TIMEOUT)?,
        "brew list",
    )?;
    let casks = checked(
        tool.run(&["list", "--cask", "--versions"], SCAN_TIMEOUT)?,
        "brew list --cask",
    )?;
    let outdated = checked(
        tool.run(&["outdated", "--json=v2"], SCAN_TIMEOUT)?,
        "brew outdated",
    )?;
    let mut installed = parse::homebrew_list(&formulae.stdout, PackageSource::Homebrew);
    installed.extend(parse::homebrew_list(
        &casks.stdout,
        PackageSource::HomebrewCask,
    ));
    Ok((
        version,
        parse::merge(installed, parse::homebrew_outdated(&outdated.stdout)?),
    ))
}

fn kind(source: PackageSource) -> &'static str {
    if source == PackageSource::HomebrewCask {
        "--cask"
    } else {
        "--formula"
    }
}

fn attempt(tool: &Tool, args: &[&str]) -> Attempt {
    Attempt::of(tool.run(args, ITEM_TIMEOUT), Verdict::by_code)
}

/// Homebrew installs a formula's or cask's current version; a version the
/// library pins can't be chosen.
// Homebrew's names and taps are lowercase; it is given them so.
pub(super) fn install(tool: &Tool, package: &PackageRef) -> Attempt {
    let id = package.id.to_ascii_lowercase();
    attempt(tool, &["install", kind(package.source), &id])
}

pub(super) fn update(tool: &Tool, package: &ScannedPackage) -> Attempt {
    attempt(tool, &["upgrade", kind(package.source), &package.id])
}

pub(super) fn uninstall(tool: &Tool, package: &PackageKey) -> Attempt {
    let id = package.id.to_ascii_lowercase();
    attempt(tool, &["uninstall", kind(package.source), &id])
}

/// Runs an installer package with `installer` onto the startup disk (its
/// arguments after the usual ones), or a shell script with its arguments.
pub(super) fn run_installer(path: &Path, arguments: &str) -> Attempt {
    let arguments = match parse::split_arguments(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return Attempt::failed(format!("{error:#}")),
    };
    let mut command = match extension(path).as_str() {
        "pkg" | "mpkg" => {
            let mut command = Command::new("/usr/sbin/installer");
            command.arg("-pkg").arg(path).args(["-target", "/"]);
            command
        }
        "sh" | "command" => {
            let mut command = Command::new("/bin/sh");
            command.arg(path);
            command
        }
        other => {
            return Attempt::failed(format!(
                "macOS runs .pkg installers and .sh scripts, not .{other} files"
            ));
        }
    };
    command.args(arguments).current_dir("/private/tmp");
    Attempt::of(process::run(command, ITEM_TIMEOUT), Verdict::by_code)
}

pub(super) fn run_command(line: &str) -> Attempt {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", line]).current_dir("/private/tmp");
    Attempt::of(process::run(command, ITEM_TIMEOUT), Verdict::by_code)
}

pub(super) fn uninstall_inventory(_name: &str, _version: Option<&str>) -> Attempt {
    Attempt::failed("Programs are uninstalled from the inventory on Windows only.")
}

pub(super) fn registry_exists(_key: &str, _value: Option<&str>) -> bool {
    false
}
