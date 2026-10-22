//! Installed packages: dpkg's and pacman's databases are read directly; rpm,
//! Flatpak and snap are asked, since their databases have no stable format.
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use meshrmm_protocol::{Application, ApplicationSource};

use super::super::{Problems, unix, unix_ms};

const DPKG_STATUS: &str = "/var/lib/dpkg/status";
const DPKG_INFO: &str = "/var/lib/dpkg/info";
const PACMAN_LOCAL: &str = "/var/lib/pacman/local";
/// Where rpm keeps its database: newer distributions under /usr, older ones
/// under /var.
const RPM_DATABASES: &[&str] = &["/usr/lib/sysimage/rpm", "/var/lib/rpm"];
/// Querying every package's header takes a second or two on most systems.
const RPM_TIMEOUT: Duration = Duration::from_secs(60);
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
const RPM_FORMAT: &str =
    "%{NAME}\\t%{EPOCHNUM}\\t%{VERSION}-%{RELEASE}\\t%{ARCH}\\t%{VENDOR}\\t%{INSTALLTIME}\\n";

/// Every package source the computer has. One that fails is a problem; the
/// others still count.
pub(super) fn applications(problems: &mut Problems) -> Vec<Application> {
    let mut applications = Vec::new();
    if Path::new(DPKG_STATUS).is_file() {
        applications.extend(
            problems
                .take("applications (dpkg)", dpkg())
                .unwrap_or_default(),
        );
    }
    if Path::new(PACMAN_LOCAL).is_dir() {
        applications.extend(
            problems
                .take("applications (pacman)", pacman())
                .unwrap_or_default(),
        );
    }
    if let Some(rpm) = unix::find_tool("rpm")
        && RPM_DATABASES.iter().any(|path| Path::new(path).is_dir())
    {
        let listed = unix::run_tool(&rpm, &["-qa", "--qf", RPM_FORMAT], RPM_TIMEOUT)
            .map(|output| parse_rpm(&output));
        applications.extend(
            problems
                .take("applications (rpm)", listed)
                .unwrap_or_default(),
        );
    }
    if let Some(flatpak) = unix::find_tool("flatpak") {
        let listed = unix::run_tool(
            &flatpak,
            &["list", "--app", "--columns=name,application,version,arch"],
            TOOL_TIMEOUT,
        )
        .map(|output| parse_flatpak(&output));
        applications.extend(
            problems
                .take("applications (flatpak)", listed)
                .unwrap_or_default(),
        );
    }
    if let Some(snap) = unix::find_tool("snap") {
        let listed = unix::run_tool(&snap, &["list", "--unicode=never"], TOOL_TIMEOUT)
            .map(|output| parse_snap(&output));
        applications.extend(
            problems
                .take("applications (snap)", listed)
                .unwrap_or_default(),
        );
    }
    applications
}

fn dpkg() -> anyhow::Result<Vec<Application>> {
    let status = std::fs::read_to_string(DPKG_STATUS).context(DPKG_STATUS)?;
    let mut packages = parse_dpkg(&status);
    // dpkg records no install time; the file list it writes then stands in.
    for package in &mut packages {
        let info = Path::new(DPKG_INFO);
        let mut lists = vec![info.join(format!("{}.list", package.name))];
        if let Some(architecture) = &package.architecture {
            lists.push(info.join(format!("{}:{architecture}.list", package.name)));
        }
        package.installed_at_unix_ms = lists
            .iter()
            .find_map(|list| std::fs::metadata(list).and_then(|m| m.modified()).ok())
            .and_then(unix_ms);
    }
    Ok(packages)
}

/// The installed packages in dpkg's status file.
fn parse_dpkg(status: &str) -> Vec<Application> {
    status
        .split("\n\n")
        .filter_map(|stanza| {
            let field = |name: &str| {
                stanza.lines().find_map(|line| {
                    let value = line.strip_prefix(name)?.strip_prefix(':')?.trim();
                    (!value.is_empty()).then(|| value.to_owned())
                })
            };
            let state = field("Status")?;
            if state.split_whitespace().nth(2) != Some("installed") {
                return None;
            }
            Some(Application {
                name: field("Package")?,
                version: field("Version"),
                publisher: field("Maintainer").map(|maintainer| without_email(&maintainer)),
                architecture: field("Architecture"),
                installed_at_unix_ms: None,
                source: ApplicationSource::Dpkg,
            })
        })
        .collect()
}

fn pacman() -> anyhow::Result<Vec<Application>> {
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(PACMAN_LOCAL).context(PACMAN_LOCAL)? {
        let description = entry?.path().join("desc");
        // The directory also holds ALPM_DB_VERSION.
        let Ok(text) = std::fs::read_to_string(&description) else {
            continue;
        };
        packages.extend(parse_pacman(&text));
    }
    Ok(packages)
}

/// A package's `desc` file in pacman's local database.
fn parse_pacman(description: &str) -> Option<Application> {
    let field = |name: &str| {
        let heading = format!("%{name}%");
        let mut lines = description.lines();
        lines.find(|line| line.trim() == heading)?;
        lines
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Some(Application {
        name: field("NAME")?,
        version: field("VERSION"),
        publisher: field("PACKAGER").map(|packager| without_email(&packager)),
        architecture: field("ARCH"),
        installed_at_unix_ms: field("INSTALLDATE")
            .and_then(|seconds| seconds.parse::<i64>().ok())
            .map(|seconds| seconds * 1000),
        source: ApplicationSource::Pacman,
    })
}

/// rpm's answer to [`RPM_FORMAT`]. rpm's own signing keys are listed as
/// `gpg-pubkey` packages, which aren't software.
fn parse_rpm(output: &str) -> Vec<Application> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            let [name, epoch, version, architecture, vendor, installed] = fields[..] else {
                return None;
            };
            if name.is_empty() || name == "gpg-pubkey" {
                return None;
            }
            let known =
                |value: &str| (!value.is_empty() && value != "(none)").then(|| value.to_owned());
            let version = match epoch {
                "" | "0" | "(none)" => version.to_owned(),
                epoch => format!("{epoch}:{version}"),
            };
            Some(Application {
                name: name.to_owned(),
                version: Some(version),
                publisher: known(vendor),
                architecture: known(architecture),
                installed_at_unix_ms: installed.parse::<i64>().ok().map(|seconds| seconds * 1000),
                source: ApplicationSource::Rpm,
            })
        })
        .collect()
}

/// `flatpak list --columns=name,application,version,arch`, which prints a
/// header only to a terminal.
fn parse_flatpak(output: &str) -> Vec<Application> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').map(str::trim).collect();
            let [name, application, version, architecture] = fields[..] else {
                return None;
            };
            if name == "Name" && application == "Application ID" {
                return None;
            }
            let name = if name.is_empty() { application } else { name };
            let known = |value: &str| (!value.is_empty()).then(|| value.to_owned());
            Some(Application {
                name: known(name)?,
                version: known(version),
                publisher: None,
                architecture: known(architecture),
                installed_at_unix_ms: None,
                source: ApplicationSource::Flatpak,
            })
        })
        .collect()
}

/// `snap list --unicode=never`: name, version, revision, tracking, publisher
/// (with `*` or `**` for verified ones) and notes, under a header.
fn parse_snap(output: &str) -> Vec<Application> {
    output
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [name, version, _revision, _tracking, publisher, ..] = fields[..] else {
                return None;
            };
            if fields
                .get(5)
                .is_some_and(|notes| notes.contains("disabled"))
            {
                return None;
            }
            let publisher = publisher.trim_end_matches('*');
            Some(Application {
                name: name.to_owned(),
                version: Some(version.to_owned()),
                publisher: (!publisher.is_empty() && publisher != "-")
                    .then(|| publisher.to_owned()),
                architecture: None,
                installed_at_unix_ms: None,
                source: ApplicationSource::Snap,
            })
        })
        .collect()
}

/// `Name <address>` as `Name`.
fn without_email(person: &str) -> String {
    match person.split_once('<') {
        Some((name, _)) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => person.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpkg_lists_installed_packages_only() {
        let status = "Package: openssl\nStatus: install ok installed\nPriority: optional\nArchitecture: amd64\nMaintainer: Ubuntu Developers <ubuntu-devel-discuss@lists.ubuntu.com>\nVersion: 3.0.13-0ubuntu3.5\nDescription: Secure Sockets Layer toolkit\n This package contains the openssl binary.\n .\n Version: not a field\n\n\
Package: removed-tool\nStatus: deinstall ok config-files\nArchitecture: amd64\nVersion: 1.0\n\n\
Package: libc6\nStatus: hold ok installed\nArchitecture: i386\nMulti-Arch: same\nMaintainer: GNU Libc Maintainers <debian-glibc@lists.debian.org>\nVersion: 2.39-0ubuntu8.4\n";
        let packages = parse_dpkg(status);
        assert_eq!(packages.len(), 2, "{packages:?}");
        assert_eq!(packages[0].name, "openssl");
        assert_eq!(packages[0].version.as_deref(), Some("3.0.13-0ubuntu3.5"));
        assert_eq!(packages[0].publisher.as_deref(), Some("Ubuntu Developers"));
        assert_eq!(packages[0].architecture.as_deref(), Some("amd64"));
        assert_eq!(packages[1].name, "libc6");
        assert_eq!(packages[1].architecture.as_deref(), Some("i386"));
        assert!(
            packages
                .iter()
                .all(|package| package.source == ApplicationSource::Dpkg)
        );
    }

    #[test]
    fn pacman_descriptions_give_every_field() {
        let description = "%NAME%\nfirefox\n\n%VERSION%\n131.0.3-1\n\n%BASE%\nfirefox\n\n%DESC%\nStandalone web browser\n\n%ARCH%\nx86_64\n\n%BUILDDATE%\n1728900000\n\n%INSTALLDATE%\n1729000000\n\n%PACKAGER%\nJan Alexander Steffens (heftig) <heftig@archlinux.org>\n\n";
        let package = parse_pacman(description).unwrap();
        assert_eq!(package.name, "firefox");
        assert_eq!(package.version.as_deref(), Some("131.0.3-1"));
        assert_eq!(package.architecture.as_deref(), Some("x86_64"));
        assert_eq!(
            package.publisher.as_deref(),
            Some("Jan Alexander Steffens (heftig)")
        );
        assert_eq!(package.installed_at_unix_ms, Some(1_729_000_000_000));
        assert_eq!(parse_pacman("%DESC%\nno name\n"), None);
    }

    #[test]
    fn rpm_output_keeps_epochs_and_skips_keys() {
        let output = "bash\t0\t5.1.8-9.el9\tx86_64\tAlmaLinux\t1718000000\n\
gpg-pubkey\t0\tb86b3716-61e69f29\t(none)\t(none)\t1718000001\n\
NetworkManager\t1\t1.46.0-19.el9_4\tx86_64\tAlmaLinux\t1718000002\n\
local-tool\t0\t1.0-1\tnoarch\t(none)\t1718000003\n\
broken line\n";
        let packages = parse_rpm(output);
        assert_eq!(packages.len(), 3, "{packages:?}");
        assert_eq!(packages[0].version.as_deref(), Some("5.1.8-9.el9"));
        assert_eq!(packages[0].publisher.as_deref(), Some("AlmaLinux"));
        assert_eq!(packages[0].installed_at_unix_ms, Some(1_718_000_000_000));
        assert_eq!(packages[1].version.as_deref(), Some("1:1.46.0-19.el9_4"));
        assert_eq!(packages[2].publisher, None);
        assert_eq!(packages[2].architecture.as_deref(), Some("noarch"));
    }

    #[test]
    fn flatpak_lists_applications() {
        let output = "Name\tApplication ID\tVersion\tArch\n\
Firefox\torg.mozilla.firefox\t131.0.3\tx86_64\n\
\tcom.example.NoName\t\tx86_64\n";
        let applications = parse_flatpak(output);
        assert_eq!(applications.len(), 2, "{applications:?}");
        assert_eq!(applications[0].name, "Firefox");
        assert_eq!(applications[0].version.as_deref(), Some("131.0.3"));
        assert_eq!(applications[1].name, "com.example.NoName");
        assert_eq!(applications[1].version, None);
    }

    #[test]
    fn snap_lists_enabled_snaps_with_their_publisher() {
        let output = "Name      Version          Rev    Tracking         Publisher   Notes\n\
core22    20240904         1621   latest/stable    canonical**  base\n\
firefox   131.0.3-1        5091   latest/stable/…  mozilla**    -\n\
lxd       5.21.2-2f4ba6b   30131  5.21/stable      canonical**  disabled\n\
hello     2.10             42     -                -            -\n";
        let snaps = parse_snap(output);
        let names: Vec<_> = snaps.iter().map(|snap| snap.name.as_str()).collect();
        assert_eq!(names, ["core22", "firefox", "hello"]);
        assert_eq!(snaps[0].publisher.as_deref(), Some("canonical"));
        assert_eq!(snaps[1].version.as_deref(), Some("131.0.3-1"));
        assert_eq!(snaps[2].publisher, None);
    }

    #[test]
    fn email_addresses_are_dropped_from_names() {
        assert_eq!(
            without_email("Fedora Project <bugs@fedora>"),
            "Fedora Project"
        );
        assert_eq!(without_email("<only@address>"), "<only@address>");
        assert_eq!(without_email("Plain"), "Plain");
    }
}
