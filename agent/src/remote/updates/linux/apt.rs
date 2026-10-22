//! APT, on Debian and Ubuntu. A scan runs `apt-get update` and then
//! simulates `apt-get dist-upgrade`, whose `Inst` lines name each package it
//! would upgrade, the versions, and the archives the new one comes from. An
//! update from a `-security` archive is a security update. Ubuntu's phased
//! updates that this computer isn't due for yet are left out, as an upgrade
//! would leave them out.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{PendingUpdate, UpdateCategory, UpdateInstallItem};

use super::super::Progress;
use super::super::process::{Output, find, run};
use super::{needs_reboot, outcomes, packages};

const REFRESH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SIMULATE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
/// How long APT waits for another program, such as unattended-upgrades, to
/// let go of the package database.
const LOCK_WAIT_SECONDS: &str = "600";

/// Keeps package scripts from asking questions nobody can answer, and keeps
/// needrestart from restarting services, among them the Agent running this.
const ENVIRONMENT: [(&str, &str); 4] = [
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("APT_LISTCHANGES_FRONTEND", "none"),
    ("NEEDRESTART_SUSPEND", "1"),
    ("UCF_FORCE_CONFFOLD", "1"),
];

fn apt_get() -> anyhow::Result<PathBuf> {
    find(&["apt-get"]).context("apt-get isn't installed")
}

/// Runs apt-get with `args` after the options every run uses.
fn apt(args: &[&str], timeout: Duration) -> anyhow::Result<Output> {
    let lock = format!("DPkg::Lock::Timeout={LOCK_WAIT_SECONDS}");
    let mut all = vec!["-q", "-o", &lock];
    all.extend_from_slice(args);
    run(&apt_get()?, &all, &ENVIRONMENT, timeout)
}

/// Why apt-get failed, in words.
fn failure(what: &str, output: &Output) -> anyhow::Error {
    let complaint = output.complaint();
    if complaint.contains("Could not get lock") || complaint.contains("Unable to acquire") {
        anyhow::anyhow!(
            "{what}: another program, such as unattended-upgrades, is using the package manager; try again later"
        )
    } else {
        anyhow::anyhow!("{what}: {complaint}")
    }
}

pub(super) fn scan() -> anyhow::Result<Vec<PendingUpdate>> {
    let refresh = apt(&["update"], REFRESH_TIMEOUT)?;
    if !refresh.success() {
        return Err(failure("Could not refresh the package lists", &refresh));
    }
    let simulation = apt(
        &["-s", "-o", "Debug::NoLocking=1", "dist-upgrade"],
        SIMULATE_TIMEOUT,
    )?;
    if !simulation.success() {
        return Err(failure("Could not work out the upgrade", &simulation));
    }
    let mut updates = parse_simulation(&simulation.stdout);
    // Download sizes are a nicety: an upgrade still works without them.
    if !updates.is_empty()
        && let Ok(uris) = apt(
            &[
                "-qq",
                "--print-uris",
                "-o",
                "Debug::NoLocking=1",
                "dist-upgrade",
            ],
            SIMULATE_TIMEOUT,
        )
        && uris.success()
    {
        let sizes = parse_sizes(&uris.stdout);
        for update in &mut updates {
            let (name, _) = split_qualifier(&update.id);
            update.size_bytes = sizes.get(name).copied();
        }
    }
    Ok(updates)
}

pub(super) fn install(
    chosen: &[&PendingUpdate],
    everything: bool,
    progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    progress.report(
        format!("Downloading and installing {}", packages(chosen.len())),
        None,
    );
    let mut args = vec![
        "-y",
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
    ];
    if everything {
        args.push("dist-upgrade");
    } else {
        args.extend(["install", "--only-upgrade"]);
        args.extend(chosen.iter().map(|update| update.id.as_str()));
    }
    let output = apt(&args, INSTALL_TIMEOUT)?;
    if !output.success() {
        let complaint = output.complaint();
        if complaint.contains("Could not get lock") || complaint.contains("Unable to acquire") {
            return Err(failure("Could not install the updates", &output));
        }
    }
    let installed = installed_versions()?;
    let by_id = chosen
        .iter()
        .map(|update| {
            let (name, _) = split_qualifier(&update.id);
            let versions = installed.get(name).cloned().unwrap_or_default();
            (update.id.clone(), versions)
        })
        .collect();
    Ok(outcomes(chosen, &by_id, &output))
}

/// Each installed package's versions, by name, from dpkg's database.
fn installed_versions() -> anyhow::Result<HashMap<String, Vec<String>>> {
    let query = find(&["dpkg-query"]).context("dpkg-query isn't installed")?;
    let output = run(
        &query,
        &["-W", "-f", "${Package} ${Version} ${db:Status-Abbrev}\\n"],
        &[],
        SIMULATE_TIMEOUT,
    )?;
    if !output.success() {
        bail!(
            "Could not read the installed packages: {}",
            output.complaint()
        );
    }
    Ok(parse_installed(&output.stdout))
}

fn parse_installed(text: &str) -> HashMap<String, Vec<String>> {
    let mut installed: HashMap<String, Vec<String>> = HashMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(name), Some(version), Some(status)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // `ii`: wanted and installed.
        if status.starts_with("ii") {
            installed
                .entry(name.to_owned())
                .or_default()
                .push(version.to_owned());
        }
    }
    installed
}

/// A package's name without an architecture qualifier such as `:i386`.
fn split_qualifier(id: &str) -> (&str, Option<&str>) {
    match id.split_once(':') {
        Some((name, architecture)) => (name, Some(architecture)),
        None => (id, None),
    }
}

/// The upgrades in `apt-get -s dist-upgrade` output, from lines such as
/// `Inst git [1:2.53.0-1] (1:2.53.0-1.1 Ubuntu:26.04/resolute-updates,
/// Ubuntu:26.04/resolute-security [amd64])`. Packages newly installed as
/// dependencies have no old version and aren't updates of their own.
fn parse_simulation(text: &str) -> Vec<PendingUpdate> {
    text.lines().filter_map(parse_inst).collect()
}

fn parse_inst(line: &str) -> Option<PendingUpdate> {
    let rest = line.strip_prefix("Inst ")?;
    let (name, rest) = rest.split_once(' ')?;
    let rest = rest.trim_start();
    let old = rest.strip_prefix('[')?;
    let (current, rest) = old.split_once(']')?;
    let open = rest.find('(')?;
    let inside = &rest[open + 1..];
    let inside = &inside[..inside.find(')')?];
    let (version, origins) = inside.split_once(' ')?;
    let origins = match origins.rfind('[') {
        Some(bracket) => &origins[..bracket],
        None => origins,
    };
    let pockets = origins
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .collect::<Vec<_>>();
    let security = pockets.iter().find(|origin| is_security(origin));
    let source = security.or(pockets.first()).map(|origin| {
        origin
            .split_once('/')
            .map_or(*origin, |(_, pocket)| pocket)
            .to_owned()
    });
    Some(PendingUpdate {
        id: name.to_owned(),
        title: name.to_owned(),
        kb: None,
        version: Some(version.to_owned()),
        current_version: Some(current.to_owned()),
        category: if security.is_some() {
            UpdateCategory::Security
        } else {
            UpdateCategory::Other
        },
        severity: None,
        size_bytes: None,
        reboot_required: needs_reboot(name),
        released_at_unix_ms: None,
        source,
    })
}

/// Whether an archive, such as `Ubuntu:24.04/noble-security` or
/// `Debian-Security:13/stable-security`, is a security archive.
fn is_security(origin: &str) -> bool {
    origin.starts_with("Debian-Security:")
        || origin
            .split_once('/')
            .is_some_and(|(_, pocket)| pocket.ends_with("-security"))
}

/// Each package's download size, by name, from `apt-get --print-uris`
/// lines such as `'http://…/git_2.53_amd64.deb' git_2.53_amd64.deb 1234 SHA256:…`.
fn parse_sizes(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _url = fields.next()?;
            let file = fields.next()?;
            let size = fields.next()?.parse().ok()?;
            let name = file.split('_').next()?;
            Some((name.to_owned(), size))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From Ubuntu 24.04, Ubuntu 26.04 and Debian 13.
    const SIMULATION: &str = "\
NOTE: This is only a simulation!
Reading package lists...
The following upgrades have been deferred due to phasing:
  tmux ubuntu-pro-client ubuntu-pro-client-l10n
The following packages will be upgraded:
6 upgraded, 1 newly installed, 0 to remove and 3 not upgraded.
Inst libglib2.0-bin [2.80.0-6ubuntu3.9] (2.80.0-6ubuntu3.10 Ubuntu:24.04/noble-updates [amd64]) []
Inst linux-image-6.8.0-150-generic (6.8.0-150.150 Ubuntu:24.04/noble-updates, Ubuntu:24.04/noble-security [amd64])
Inst git [1:2.53.0-1ubuntu1] (1:2.53.0-1ubuntu1.1 Ubuntu:26.04/resolute-updates, Ubuntu:26.04/resolute-security [amd64])
Inst cloud-init-base [26.1-0ubuntu3~26.04.1] (26.2-0ubuntu1~26.04.1 Ubuntu:26.04/resolute-updates [all]) [cloud-init:amd64 ]
Inst bind9-host [1:9.20.29-1~deb13u1] (1:9.20.30-1~deb13u1 Debian-Security:13/stable-security [amd64]) []
Inst libc6:i386 [2.41-12] (2.41-12+deb13u1 Debian:13.2/stable [i386])
Conf git (1:2.53.0-1ubuntu1.1 Ubuntu:26.04/resolute-updates [amd64])
Remv oldpackage [1.0]
";

    #[test]
    fn simulations_list_upgrades_and_their_archives() {
        let updates = parse_simulation(SIMULATION);
        let ids = updates
            .iter()
            .map(|update| update.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "libglib2.0-bin",
                "git",
                "cloud-init-base",
                "bind9-host",
                "libc6:i386"
            ]
        );
        let glib = &updates[0];
        assert_eq!(glib.current_version.as_deref(), Some("2.80.0-6ubuntu3.9"));
        assert_eq!(glib.version.as_deref(), Some("2.80.0-6ubuntu3.10"));
        assert_eq!(glib.category, UpdateCategory::Other);
        assert_eq!(glib.source.as_deref(), Some("noble-updates"));
        let git = &updates[1];
        assert_eq!(git.category, UpdateCategory::Security);
        assert_eq!(git.source.as_deref(), Some("resolute-security"));
        assert_eq!(git.version.as_deref(), Some("1:2.53.0-1ubuntu1.1"));
        assert_eq!(updates[2].source.as_deref(), Some("resolute-updates"));
        let bind = &updates[3];
        assert_eq!(bind.category, UpdateCategory::Security);
        assert_eq!(bind.source.as_deref(), Some("stable-security"));
        let libc = &updates[4];
        assert!(libc.reboot_required);
        assert_eq!(libc.category, UpdateCategory::Other);
        assert!(!glib.reboot_required);
    }

    #[test]
    fn download_sizes_are_read_by_package() {
        let sizes = parse_sizes(
            "'http://archive.ubuntu.com/ubuntu/pool/main/s/snapd/snapd_2.77.1%2bubuntu24.04_amd64.deb' snapd_2.77.1+ubuntu24.04_amd64.deb 33191424 MD5Sum:551d960a536b6bb6069649d0d537fed6\n\
             'http://archive.ubuntu.com/ubuntu/pool/main/c/cloud-init/cloud-init_26.2-0ubuntu1%7e24.04.1_all.deb' cloud-init_26.2-0ubuntu1~24.04.1_all.deb 633708 MD5Sum:13a8dd406c7fb7c2f9d7f3dab203c324\n",
        );
        assert_eq!(sizes.get("snapd"), Some(&33_191_424));
        assert_eq!(sizes.get("cloud-init"), Some(&633_708));
    }

    #[test]
    fn installed_versions_skip_removed_packages() {
        let installed = parse_installed(
            "git 1:2.53.0-1ubuntu1.1 ii \nlibc6 2.41-12 ii \nlibc6 2.41-12 ii \noldpackage 1.0 rc \n",
        );
        assert_eq!(installed["git"], ["1:2.53.0-1ubuntu1.1"]);
        assert_eq!(installed["libc6"].len(), 2);
        assert!(!installed.contains_key("oldpackage"));
        assert_eq!(split_qualifier("libc6:i386"), ("libc6", Some("i386")));
    }
}
