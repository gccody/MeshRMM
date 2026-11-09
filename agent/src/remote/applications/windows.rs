//! Applications on Windows: winget and Chocolatey, installer files, and the
//! programs Programs and Features lists.
//!
//! winget comes with the App Installer package, which registers it for
//! signed-in users only, so the SYSTEM service runs the `winget.exe` in the
//! newest `Microsoft.DesktopAppInstaller_*` folder under `WindowsApps`, from
//! that folder so its libraries load. Installs ask for machine scope, and
//! fall back to the installer's default where a package has no machine
//! installer.
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use meshrmm_protocol::{
    APPLICATION_SCAN_TIMEOUT_SECONDS, ItemOutcome, PackageKey, PackageRef, PackageSource,
    ScannedPackage,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_WOW64_32KEY,
    KEY_WOW64_64KEY, REG_SAM_FLAGS,
};

use super::process::{self, Ran};
use super::{Attempt, ITEM_TIMEOUT, Verdict, extension, parse};
use crate::remote::system_info::windows::registry::Key;

const SCAN_TIMEOUT: Duration = Duration::from_secs(APPLICATION_SCAN_TIMEOUT_SECONDS);
/// How long looking up the ID a package is listed by may take.
const SHOW_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
/// winget's arguments for running without anyone to answer it.
const WINGET_QUIET: [&str; 2] = ["--accept-source-agreements", "--disable-interactivity"];
/// winget: no installer matches this device, such as no machine-scope one.
const NO_APPLICABLE_INSTALLER: i64 = 0x8A15_0010;
const ALREADY_INSTALLED: [i64; 2] = [0x8A15_0061, 0x8A15_010D];
const NOTHING_TO_UPDATE: i64 = 0x8A15_002B;
const NOT_INSTALLED: i64 = 0x8A15_0014;
const RESTART_TO_FINISH: [i64; 2] = [0x8A15_0109, 0x8A15_010B];

/// A package manager on this computer.
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command.args(args);
        if self.source == PackageSource::Winget {
            // Commands take these; `--version` takes nothing else.
            if args.first().is_some_and(|first| !first.starts_with('-')) {
                command.args(WINGET_QUIET);
            }
            if let Some(folder) = self.program.parent() {
                command.current_dir(folder);
            }
        }
        // Chocolatey's progress and colours are for terminals.
        command.env("NO_COLOR", "1");
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> anyhow::Result<Ran> {
        process::run(self.command(args), timeout)
    }
}

/// The App Installer folder whose `winget.exe` to run, from the folders
/// under `WindowsApps`: the newest version for this computer's
/// architecture, else the newest of any.
fn pick_winget(names: &[String], architecture: &str) -> Option<String> {
    let mut candidates = names
        .iter()
        .filter_map(|name| {
            let mut parts = name.split('_');
            (parts.next()? == "Microsoft.DesktopAppInstaller").then_some(())?;
            let version = parts.next()?;
            let arch = parts.next()?;
            Some((arch.eq_ignore_ascii_case(architecture), version, name))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| meshrmm_protocol::compare_versions(left.1, right.1))
    });
    candidates.last().map(|(_, _, name)| (*name).clone())
}

fn winget() -> Option<PathBuf> {
    let program_files =
        std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    let apps = Path::new(&program_files).join("WindowsApps");
    let names = std::fs::read_dir(&apps)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| apps.join(name).join("winget.exe").is_file())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let architecture = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    if let Some(folder) = pick_winget(&names, architecture) {
        return Some(apps.join(folder).join("winget.exe"));
    }
    // An Agent run in the console, as a user, has winget's alias.
    let local = std::env::var_os("LOCALAPPDATA")?;
    let alias = Path::new(&local).join(r"Microsoft\WindowsApps\winget.exe");
    alias.exists().then_some(alias)
}

fn chocolatey() -> Option<PathBuf> {
    let program_data = std::env::var_os("ChocolateyInstall")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("ProgramData").map(|data| Path::new(&data).join("chocolatey")))
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\chocolatey"));
    let choco = program_data.join(r"bin\choco.exe");
    choco.is_file().then_some(choco)
}

/// The package managers this computer has.
pub(super) fn tools() -> Vec<Tool> {
    let mut tools = Vec::new();
    if let Some(program) = winget() {
        tools.push(Tool {
            source: PackageSource::Winget,
            program,
        });
    }
    if let Some(program) = chocolatey() {
        tools.push(Tool {
            source: PackageSource::Chocolatey,
            program,
        });
    }
    tools
}

/// A program's failure as an error, unless `accept` says its code is fine.
fn checked(ran: Ran, what: &str, accept: &[i64]) -> anyhow::Result<Ran> {
    if ran.success() || ran.code.is_some_and(|code| accept.contains(&code)) {
        return Ok(ran);
    }
    anyhow::bail!("{what} failed: {}", explain(&ran))
}

/// Why a winget or Chocolatey run failed, in words.
fn explain(ran: &Ran) -> String {
    let known = ran
        .code
        .and_then(|code| u32::try_from(code).ok())
        .and_then(parse::winget_exit_code);
    match (known, ran.code) {
        (Some(meaning), Some(code)) => format!("{meaning} ({})", process::describe_code(code)),
        _ => ran.complaint(),
    }
}

pub(super) fn scan(tool: &Tool) -> anyhow::Result<(Option<String>, Vec<ScannedPackage>)> {
    match tool.source {
        PackageSource::Winget => {
            let version = tool
                .run(&["--version"], SCAN_TIMEOUT)
                .ok()
                .and_then(|ran| parse::winget_version_text(&ran.stdout));
            let ran = checked(
                tool.run(&["list"], SCAN_TIMEOUT)?,
                "winget list",
                &[NOT_INSTALLED],
            )?;
            if ran.code == Some(NOT_INSTALLED) {
                return Ok((version, Vec::new()));
            }
            let packages = parse::winget_list(&ran.stdout).map_err(anyhow::Error::msg)?;
            Ok((version, packages))
        }
        _ => {
            let version = tool
                .run(&["--version"], SCAN_TIMEOUT)
                .ok()
                .map(|ran| ran.stdout.trim().to_owned())
                .filter(|version| meshrmm_protocol::valid_version(version));
            // Chocolatey 1 lists the community repository unless told to
            // list what is installed; Chocolatey 2 lists only that.
            let old = version
                .as_deref()
                .is_some_and(|version| version.starts_with("0.") || version.starts_with("1."));
            let list: &[&str] = if old {
                &["list", "-l", "-r"]
            } else {
                &["list", "-r"]
            };
            let listed = checked(tool.run(list, SCAN_TIMEOUT)?, "choco list", &[])?;
            let outdated = checked(
                tool.run(&["outdated", "-r"], SCAN_TIMEOUT)?,
                "choco outdated",
                // With enhanced exit codes, 2 means some are outdated.
                &[2],
            )?;
            Ok((
                version,
                parse::merge(
                    parse::chocolatey_list(&listed.stdout),
                    parse::chocolatey_outdated(&outdated.stdout),
                ),
            ))
        }
    }
}

/// What a winget install, upgrade or uninstall's exit means.
fn winget_verdict(ran: &Ran) -> Verdict {
    match ran.code {
        Some(0) => Verdict::Done,
        Some(code) if RESTART_TO_FINISH.contains(&code) => Verdict::DoneRestart,
        Some(code) if ALREADY_INSTALLED.contains(&code) => {
            Verdict::Unchanged("It is installed already.".to_owned())
        }
        Some(NOTHING_TO_UPDATE) => {
            Verdict::Unchanged("No newer version applies to this device.".to_owned())
        }
        Some(NOT_INSTALLED) => Verdict::Unchanged("It isn't installed.".to_owned()),
        _ => Verdict::Failed(explain(ran)),
    }
}

fn chocolatey_verdict(ran: &Ran) -> Verdict {
    match ran.code {
        Some(0) => Verdict::Done,
        Some(3010 | 1641) => Verdict::DoneRestart,
        _ => Verdict::Failed(ran.complaint()),
    }
}

/// The ID winget's sources write `id` as, when they write it differently:
/// winget's `--exact` matches case, while the library ignores it.
fn winget_listed_id(tool: &Tool, id: &str) -> Option<String> {
    let ran = tool.run(&["show", "--id", id], SHOW_TIMEOUT).ok()?;
    if !ran.success() {
        return None;
    }
    parse::winget_shown_id(&ran.stdout)
        .filter(|listed| listed != id && listed.eq_ignore_ascii_case(id))
}

fn winget_install(tool: &Tool, id: &str, version: Option<&str>) -> Attempt {
    let mut args = vec![
        "install",
        "--id",
        id,
        "--exact",
        "--silent",
        "--accept-package-agreements",
    ];
    if let Some(version) = version {
        args.extend(["--version", version]);
    }
    let machine = [args.as_slice(), &["--scope", "machine"]].concat();
    let ran = tool.run(&machine, ITEM_TIMEOUT);
    if ran
        .as_ref()
        .is_ok_and(|ran| ran.code == Some(NO_APPLICABLE_INSTALLER))
    {
        // The package has no machine-scope installer; use its default.
        return Attempt::of(tool.run(&args, ITEM_TIMEOUT), winget_verdict);
    }
    Attempt::of(ran, winget_verdict)
}

pub(super) fn install(tool: &Tool, package: &PackageRef) -> Attempt {
    let id = package.id.as_str();
    match tool.source {
        PackageSource::Winget => {
            let version = package.version.as_deref();
            let attempt = winget_install(tool, id, version);
            if attempt.exit_code == Some(NOT_INSTALLED)
                && let Some(listed) = winget_listed_id(tool, id)
            {
                return winget_install(tool, &listed, version);
            }
            attempt
        }
        _ => {
            let mut args = vec!["install", id, "-y", "--no-progress", "-r"];
            if let Some(version) = &package.version {
                args.extend(["--version", version]);
            }
            Attempt::of(tool.run(&args, ITEM_TIMEOUT), chocolatey_verdict)
        }
    }
}

pub(super) fn update(tool: &Tool, package: &ScannedPackage) -> Attempt {
    let id = package.id.as_str();
    match tool.source {
        PackageSource::Winget => Attempt::of(
            tool.run(
                &[
                    "upgrade",
                    "--id",
                    id,
                    "--exact",
                    "--silent",
                    "--accept-package-agreements",
                    "--include-unknown",
                ],
                ITEM_TIMEOUT,
            ),
            winget_verdict,
        ),
        _ => Attempt::of(
            tool.run(&["upgrade", id, "-y", "--no-progress", "-r"], ITEM_TIMEOUT),
            chocolatey_verdict,
        ),
    }
}

pub(super) fn uninstall(tool: &Tool, package: &PackageKey) -> Attempt {
    let id = package.id.as_str();
    match tool.source {
        PackageSource::Winget => Attempt::of(
            tool.run(
                &["uninstall", "--id", id, "--exact", "--silent"],
                ITEM_TIMEOUT,
            ),
            winget_verdict,
        ),
        _ => Attempt::of(
            tool.run(
                &["uninstall", id, "-y", "--no-progress", "-r"],
                ITEM_TIMEOUT,
            ),
            chocolatey_verdict,
        ),
    }
}

/// `%SystemRoot%\System32\<name>`, by the kernel's Windows directory.
fn system32(name: &str) -> PathBuf {
    crate::win32::windows_directory()
        .unwrap_or_else(|_| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join(name)
}

/// Runs an installer file with its arguments: Windows Installer packages
/// through `msiexec` (`/qn /norestart` unless the library gives arguments),
/// programs directly, and PowerShell and batch scripts through their
/// interpreters.
pub(super) fn run_installer(path: &Path, arguments: &str) -> Attempt {
    let kind = extension(path);
    if !matches!(kind.as_str(), "msi" | "exe" | "ps1" | "cmd" | "bat") {
        return Attempt::failed(format!(
            "Windows runs .msi, .exe, .ps1, .cmd and .bat installers, not .{kind} files"
        ));
    }
    installing(
        || installer_command(path, &kind, arguments),
        Verdict::by_code,
    )
}

/// Windows Installer does one install at a time. A program that exits with
/// 1618, another installation in progress, is run again a few times, half a
/// minute apart, while the run's time lasts.
fn installing(command: impl Fn() -> Command, judge: impl Fn(&Ran) -> Verdict) -> Attempt {
    const TRIES: usize = 4;
    const APART: Duration = Duration::from_secs(30);
    for _ in 1..TRIES {
        let ran = process::run(command(), ITEM_TIMEOUT);
        if !ran
            .as_ref()
            .is_ok_and(|ran| ran.code == Some(INSTALL_IN_PROGRESS))
        {
            return Attempt::of(ran, &judge);
        }
        tracing::info!("another installation is in progress; trying again shortly");
        std::thread::sleep(APART);
    }
    Attempt::of(process::run(command(), ITEM_TIMEOUT), &judge)
}

/// Windows Installer's "another installation is in progress".
const INSTALL_IN_PROGRESS: i64 = 1618;

fn installer_command(path: &Path, kind: &str, arguments: &str) -> Command {
    let quoted = format!("\"{}\"", path.display());
    let mut command = match kind {
        "msi" => {
            let mut command = Command::new(system32("msiexec.exe"));
            let arguments = if arguments.trim().is_empty() {
                "/qn /norestart"
            } else {
                arguments
            };
            command.raw_arg(format!("/i {quoted} {arguments}"));
            command
        }
        "exe" => {
            let mut command = Command::new(path);
            if !arguments.trim().is_empty() {
                command.raw_arg(arguments);
            }
            command
        }
        "ps1" => {
            let mut command = Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"));
            command.raw_arg(format!(
                "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {quoted} {arguments}"
            ));
            command
        }
        "cmd" | "bat" => {
            let mut command = Command::new(system32("cmd.exe"));
            command.raw_arg(format!("/d /s /c \"{quoted} {arguments}\""));
            command
        }
        _ => Command::new(path),
    };
    if let Some(folder) = path.parent() {
        command.current_dir(folder);
    }
    command
}

/// Runs a command line with `cmd.exe`, as a library entry's uninstall
/// command.
pub(super) fn run_command(line: &str) -> Attempt {
    installing(
        || {
            let mut command = Command::new(system32("cmd.exe"));
            command.raw_arg(format!("/d /s /c \"{line}\""));
            command.current_dir(system32(""));
            command
        },
        Verdict::by_code,
    )
}

/// An installed program's Uninstall entry.
#[derive(Debug)]
struct Entry {
    key: String,
    quiet: Option<String>,
    uninstall: Option<String>,
    windows_installer: bool,
}

/// The Uninstall entries of programs installed for every user named `name`
/// (and `version`, if given), in both registry views.
fn entries(name: &str, version: Option<&str>) -> Vec<Entry> {
    let mut found = Vec::new();
    for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
        let Ok(uninstall) = Key::open(HKEY_LOCAL_MACHINE, UNINSTALL, view) else {
            continue;
        };
        for key in uninstall.subkeys() {
            let Ok(entry) = uninstall.child(&key, view) else {
                continue;
            };
            let matches = entry
                .string("DisplayName")
                .is_some_and(|display| display.trim().eq_ignore_ascii_case(name.trim()))
                && version.is_none_or(|version| {
                    entry
                        .string("DisplayVersion")
                        .is_some_and(|installed| installed.trim() == version.trim())
                });
            if matches {
                found.push(Entry {
                    quiet: entry
                        .string("QuietUninstallString")
                        .filter(|text| !text.trim().is_empty()),
                    uninstall: entry
                        .string("UninstallString")
                        .filter(|text| !text.trim().is_empty()),
                    windows_installer: entry.dword("WindowsInstaller") == Some(1),
                    key,
                });
            }
        }
    }
    found
}

/// The command that removes an entry's program without asking anything:
/// its quiet uninstaller, else Windows Installer with its product code.
fn quiet_command(entry: &Entry) -> Option<Command> {
    if let Some(quiet) = &entry.quiet {
        let mut command = Command::new(system32("cmd.exe"));
        command.raw_arg(format!("/d /s /c \"{quiet}\""));
        return Some(command);
    }
    let code = if entry.windows_installer && parse::is_product_code(&entry.key) {
        Some(entry.key.clone())
    } else {
        entry.uninstall.as_deref().and_then(parse::msi_product_code)
    }?;
    let mut command = Command::new(system32("msiexec.exe"));
    command.args(["/x", &code, "/qn", "/norestart"]);
    Some(command)
}

/// Uninstalls a program Programs and Features lists for every user, by its
/// quiet uninstaller or its Windows Installer product code, and waits up to
/// two minutes for its entry to go, since some uninstallers hand the work to
/// a copy of themselves and exit at once.
pub(super) fn uninstall_inventory(name: &str, version: Option<&str>) -> Attempt {
    let found = entries(name, version);
    let Some(entry) = found.first() else {
        return Attempt::unchanged("It isn't installed for every user on this device.");
    };
    if quiet_command(entry).is_none() {
        return Attempt::failed(
            "It has no quiet uninstaller and isn't a Windows Installer package. Uninstall it with a script, or add it to the application library with an uninstall command.",
        );
    }
    let command = || {
        let mut command = quiet_command(entry).expect("checked above");
        command.current_dir(system32(""));
        command
    };
    let mut attempt = installing(command, |ran| match ran.code {
        // Windows Installer: the product isn't installed.
        Some(1605) => Verdict::Unchanged("It isn't installed.".to_owned()),
        _ => Verdict::by_code(ran),
    });
    if attempt.outcome == ItemOutcome::Succeeded {
        let started = Instant::now();
        while !entries(name, version).is_empty() {
            if started.elapsed() > Duration::from_secs(120) {
                attempt.outcome = ItemOutcome::Failed;
                attempt.error = Some(
                    "The uninstaller finished, but the program is still listed as installed."
                        .to_owned(),
                );
                break;
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    attempt
}

/// The root key and path of `HKLM\…` and the like.
fn registry_root(key: &str) -> Option<(HKEY, &str)> {
    let (root, path) = key.split_once('\\').unwrap_or((key, ""));
    let root = match root.to_ascii_uppercase().as_str() {
        "HKLM" | "HKEY_LOCAL_MACHINE" => HKEY_LOCAL_MACHINE,
        "HKCU" | "HKEY_CURRENT_USER" => HKEY_CURRENT_USER,
        "HKU" | "HKEY_USERS" => HKEY_USERS,
        "HKCR" | "HKEY_CLASSES_ROOT" => HKEY_CLASSES_ROOT,
        _ => return None,
    };
    Some((root, path.trim_matches('\\')))
}

/// Whether a registry key exists, in either view, with the string or
/// number value `value` if given.
pub(super) fn registry_exists(key: &str, value: Option<&str>) -> bool {
    let Some((root, path)) = registry_root(key) else {
        return false;
    };
    [KEY_WOW64_64KEY, KEY_WOW64_32KEY, REG_SAM_FLAGS(0)]
        .into_iter()
        .any(|view| {
            Key::open(root, path, view).is_ok_and(|key| {
                value.is_none_or(|value| key.string(value).is_some() || key.dword(value).is_some())
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_app_installer_for_this_architecture_runs_winget() {
        let names = [
            "Microsoft.DesktopAppInstaller_1.21.10120.0_x64__8wekyb3d8bbwe",
            "Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe",
            "Microsoft.DesktopAppInstaller_1.30.1.0_arm64__8wekyb3d8bbwe",
            "Microsoft.WindowsTerminal_1.21.0.0_x64__8wekyb3d8bbwe",
        ]
        .map(str::to_owned);
        assert_eq!(
            pick_winget(&names, "x64").as_deref(),
            Some("Microsoft.DesktopAppInstaller_1.29.380.0_x64__8wekyb3d8bbwe")
        );
        assert_eq!(
            pick_winget(&names, "arm64").as_deref(),
            Some("Microsoft.DesktopAppInstaller_1.30.1.0_arm64__8wekyb3d8bbwe")
        );
        assert_eq!(pick_winget(&names[3..], "x64"), None);
    }

    #[test]
    fn registry_keys_name_their_root() {
        assert!(
            registry_root(r"HKLM\SOFTWARE\Microsoft").is_some_and(|(root, path)| root
                == HKEY_LOCAL_MACHINE
                && path == r"SOFTWARE\Microsoft")
        );
        assert!(registry_root(r"HKEY_USERS\.DEFAULT").is_some());
        assert!(registry_root(r"SOFTWARE\Microsoft").is_none());
        assert!(registry_exists(
            r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion",
            Some("ProgramFilesDir")
        ));
        assert!(!registry_exists(r"HKLM\SOFTWARE\MeshRMM-no-such-key", None));
    }

    #[test]
    fn winget_exit_codes_say_what_happened() {
        let ran = |code| Ran {
            code: Some(code),
            stdout: "Found Git [Git.Git]".into(),
            stderr: String::new(),
        };
        assert!(matches!(winget_verdict(&ran(0)), Verdict::Done));
        assert!(matches!(
            winget_verdict(&ran(0x8A15_0061)),
            Verdict::Unchanged(_)
        ));
        assert!(matches!(
            winget_verdict(&ran(0x8A15_0109)),
            Verdict::DoneRestart
        ));
        let Verdict::Failed(reason) = winget_verdict(&ran(0x8A15_0017)) else {
            panic!("an unknown package fails");
        };
        assert_eq!(
            reason,
            "No package has this ID in winget's sources (code 0x8A150017)"
        );
    }

    /// Lists this computer's packages with the real winget and Chocolatey,
    /// read-only. Needs them installed.
    #[test]
    #[ignore]
    fn real_scan_is_plausible() {
        for tool in tools() {
            let (version, packages) = scan(&tool).unwrap();
            println!(
                "{} {version:?}: {} packages, {} outdated",
                tool.source.as_str(),
                packages.len(),
                packages.iter().filter(|package| package.outdated()).count()
            );
            assert!(!packages.is_empty());
        }
    }
}
