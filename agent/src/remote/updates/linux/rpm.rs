//! DNF and YUM, on Fedora and Red Hat Enterprise Linux and its rebuilds. A
//! scan runs `check-update`, which refreshes the repositories' metadata and
//! lists each package with a newer version, and reads the advisories that
//! cover them (`updateinfo`, or `advisory` in DNF 5) to tell security
//! updates, their severity and their release date. Installed versions come
//! from rpm.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{
    PendingUpdate, UpdateCategory, UpdateInstallItem, UpdateManager, UpdateSeverity,
};

use super::super::Progress;
use super::super::process::{Output, find, run};
use super::{needs_reboot, outcomes, packages};

const CHECK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const ADVISORY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const QUERY_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
/// `check-update`'s exit code when there are updates.
const UPDATES_AVAILABLE: i32 = 100;

/// The tool and whether it is DNF 5, whose commands and output differ.
fn tool(manager: UpdateManager) -> anyhow::Result<(PathBuf, bool)> {
    if manager == UpdateManager::Yum {
        return Ok((find(&["yum"]).context("yum isn't installed")?, false));
    }
    if let Some(dnf5) = find(&["dnf5"]) {
        return Ok((dnf5, true));
    }
    Ok((find(&["dnf"]).context("dnf isn't installed")?, false))
}

fn failure(what: &str, output: &Output) -> anyhow::Error {
    anyhow::anyhow!("{what}: {}", output.complaint())
}

pub(super) fn scan(manager: UpdateManager) -> anyhow::Result<Vec<PendingUpdate>> {
    let (tool, dnf5) = tool(manager)?;
    let mut check = vec!["-q", "check-update"];
    if manager == UpdateManager::Dnf {
        check.push("--refresh");
    }
    let output = run(&tool, &check, &[], CHECK_TIMEOUT)?;
    let mut updates = match output.code {
        Some(0) => return Ok(Vec::new()),
        Some(UPDATES_AVAILABLE) => parse_check_update(&output.stdout),
        _ => return Err(failure("Could not check for updates", &output)),
    };
    // Without advisories every update is an ordinary one; the list stands.
    let advisories = if dnf5 {
        run(
            &tool,
            &["-q", "advisory", "list", "--updates"],
            &[],
            ADVISORY_TIMEOUT,
        )
    } else if manager == UpdateManager::Yum {
        run(
            &tool,
            &["-q", "updateinfo", "list", "updates"],
            &[],
            ADVISORY_TIMEOUT,
        )
    } else {
        run(
            &tool,
            &["-q", "updateinfo", "list", "--updates"],
            &[],
            ADVISORY_TIMEOUT,
        )
    };
    match advisories {
        Ok(output) if output.success() => {
            classify(&mut updates, &parse_advisories(&output.stdout));
        }
        Ok(output) => tracing::warn!(
            error = output.complaint(),
            "could not read the update advisories"
        ),
        Err(error) => tracing::warn!(error = ?error, "could not read the update advisories"),
    }
    if let Ok(installed) = installed_versions() {
        for update in &mut updates {
            update.current_version = installed
                .get(&update.id)
                .and_then(|versions| versions.last().cloned());
        }
    }
    Ok(updates)
}

pub(super) fn install(
    manager: UpdateManager,
    chosen: &[&PendingUpdate],
    everything: bool,
    progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    let (tool, _) = tool(manager)?;
    progress.report(
        format!("Downloading and installing {}", packages(chosen.len())),
        None,
    );
    let mut args = vec![
        "-y",
        if manager == UpdateManager::Yum {
            "update"
        } else {
            "upgrade"
        },
    ];
    if !everything {
        args.extend(chosen.iter().map(|update| update.id.as_str()));
    }
    let output = run(&tool, &args, &[], INSTALL_TIMEOUT)?;
    let installed = installed_versions()?;
    Ok(outcomes(chosen, &installed, &output))
}

/// The versions of every installed package, by `name.arch`, oldest
/// installed first, written as `check-update` writes them: with the epoch
/// only when it isn't 0.
fn installed_versions() -> anyhow::Result<HashMap<String, Vec<String>>> {
    let rpm = find(&["rpm"]).context("rpm isn't installed")?;
    let output = run(
        &rpm,
        &[
            "-qa",
            "--qf",
            "%{INSTALLTIME} %{NAME}.%{ARCH} %|EPOCH?{%{EPOCH}:}:{}|%{VERSION}-%{RELEASE}\\n",
        ],
        &[],
        QUERY_TIMEOUT,
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
    let mut rows = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let time: i64 = fields.next()?.parse().ok()?;
            let id = fields.next()?;
            let version = fields.next()?;
            let version = version.strip_prefix("0:").unwrap_or(version);
            Some((time, id.to_owned(), version.to_owned()))
        })
        .collect::<Vec<_>>();
    rows.sort();
    let mut installed: HashMap<String, Vec<String>> = HashMap::new();
    for (_, id, version) in rows {
        installed.entry(id).or_default().push(version);
    }
    installed
}

/// The packages in `check-update` output: lines of `name.arch version
/// repository`, which DNF 4 wraps after a long name, under an `Upgrades`
/// heading in DNF 5. Obsoleting packages are left out: an upgrade of
/// everything installs them, and they are listed with what they replace.
fn parse_check_update(text: &str) -> Vec<PendingUpdate> {
    let mut updates = Vec::new();
    let mut carried: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("Obsoleting") || line.starts_with("Security:") {
            break;
        }
        let mut fields = line
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if let Some(name) = carried.take() {
            fields.insert(0, name);
        }
        match fields.as_slice() {
            [name] if name.contains('.') && line == line.trim_start() => {
                carried = Some(name.clone());
            }
            [id, version, repository] if id.contains('.') => {
                let name = id.rsplit_once('.').map_or(id.as_str(), |(name, _)| name);
                updates.push(PendingUpdate {
                    id: id.clone(),
                    title: name.to_owned(),
                    kb: None,
                    version: Some(version.clone()),
                    current_version: None,
                    category: UpdateCategory::Other,
                    severity: None,
                    size_bytes: None,
                    reboot_required: needs_reboot(name),
                    released_at_unix_ms: None,
                    source: Some(repository.clone()),
                });
            }
            _ => {}
        }
    }
    updates
}

/// An advisory that covers a package's new version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Advisory {
    id: String,
    security: bool,
    severity: Option<UpdateSeverity>,
    /// `name.arch` of the package it covers, and the version.
    package: String,
    version: String,
    issued_at_unix_ms: Option<i64>,
}

/// Advisories, from DNF 4's `updateinfo list` (`ALSA-2026:85958
/// Moderate/Sec.  openssh-9.9p1-13.el9_8.alma.1.x86_64`, or `bugfix` in
/// place of the severity) or DNF 5's `advisory list` (`FEDORA-2026-… security
/// Important kernel-core-7.2.11-200.fc44.x86_64 2026-10-17 01:03:51`).
fn parse_advisories(text: &str) -> Vec<Advisory> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let (id, kind, severity, nevra, issued) = match fields.as_slice() {
                [id, kind, nevra] => {
                    let (severity, security) = match kind.split_once('/') {
                        Some((severity, "Sec.")) => (UpdateSeverity::from_vendor(severity), true),
                        _ => (None, *kind == "security"),
                    };
                    (*id, security, severity, *nevra, None)
                }
                [id, kind, severity, nevra, date, time] if *id != "Name" => (
                    *id,
                    *kind == "security",
                    UpdateSeverity::from_vendor(severity),
                    *nevra,
                    parse_time(date, time),
                ),
                _ => return None,
            };
            let (package, version) = split_nevra(nevra)?;
            Some(Advisory {
                id: id.to_owned(),
                security: kind,
                severity,
                package,
                version,
                issued_at_unix_ms: issued,
            })
        })
        .collect()
}

/// `name.arch` and `[epoch:]version-release` of `name-[epoch:]version-release.arch`.
fn split_nevra(nevra: &str) -> Option<(String, String)> {
    let (rest, arch) = nevra.rsplit_once('.')?;
    let (rest, release) = rest.rsplit_once('-')?;
    let (name, version) = rest.rsplit_once('-')?;
    Some((format!("{name}.{arch}"), format!("{version}-{release}")))
}

/// Marks updates that an advisory covers: security ones as such, with the
/// advisory's severity, and every one with the advisory as its source. Of
/// several advisories for a package, the one for its new version wins, then
/// a security one.
fn classify(updates: &mut [PendingUpdate], advisories: &[Advisory]) {
    for update in updates {
        let best = advisories
            .iter()
            .filter(|advisory| advisory.package == update.id)
            .max_by_key(|advisory| {
                (
                    Some(&advisory.version) == update.version.as_ref(),
                    advisory.security,
                    advisory.severity.map(std::cmp::Reverse),
                )
            });
        let Some(advisory) = best else {
            continue;
        };
        if advisory.security {
            update.category = UpdateCategory::Security;
            update.severity = advisory.severity;
        }
        update.source = Some(advisory.id.clone());
        update.released_at_unix_ms = advisory.issued_at_unix_ms;
    }
}

/// Unix milliseconds of a UTC `YYYY-MM-DD` `HH:MM:SS`.
fn parse_time(date: &str, time: &str) -> Option<i64> {
    let mut day = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (day.next()??, day.next()??, day.next()??);
    let mut clock = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (clock.next()??, clock.next()??, clock.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from 1970-01-01 to the date, from Howard Hinnant's algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year - era * 400;
    let of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let of_era_days = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = era * 146_097 + of_era_days - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From AlmaLinux 9 (DNF 4).
    const ALMA_CHECK: &str = "
kernel-core.x86_64                     5.14.0-687.59.1.el9_8              baseos
openssh.x86_64                         9.9p1-13.el9_8.alma.1              baseos
a-package-with-an-unusually-long-name-for-testing.noarch
                                       1.2-3.el9                          appstream
Obsoleting Packages
grub2-tools.x86_64                     1:2.06-110.el9                     baseos
    grub2-tools.x86_64                 1:2.06-104.el9                     @baseos
";

    /// From Fedora 44 (DNF 5).
    const FEDORA_CHECK: &str = "Upgrades
ImageMagick.x86_64                   1:7.1.2.34-1.fc44                      updates
kernel-core.x86_64                   7.2.11-200.fc44                        updates
vim-data.noarch                      2:9.2.1202-1.fc44                      updates
";

    const ALMA_ADVISORIES: &str = "\
ALSA-2026:83156 Important/Sec. kernel-core-5.14.0-687.58.1.el9_8.x86_64
ALSA-2026:86033 Important/Sec. kernel-core-5.14.0-687.59.1.el9_8.x86_64
ALSA-2026:85958 Moderate/Sec.  openssh-9.9p1-13.el9_8.alma.1.x86_64
ALBA-2026:1000 bugfix a-package-with-an-unusually-long-name-for-testing-1.2-3.el9.noarch
";

    const FEDORA_ADVISORIES: &str = "\
Name                   Type        Severity                                                   Package              Issued
FEDORA-2026-1a3e2d0ffe security    Important                       kernel-core-7.2.11-200.fc44.x86_64 2026-10-17 01:03:51
FEDORA-2026-d59fbe5923 enhancement None                             vim-data-2:9.2.1202-1.fc44.noarch 2026-10-18 01:17:00
FEDORA-2026-f708512ec9 security    None                          ImageMagick-1:7.1.2.34-1.fc44.x86_64 2026-10-21 01:02:14
";

    #[test]
    fn check_update_lists_packages_across_wrapped_lines() {
        let updates = parse_check_update(ALMA_CHECK);
        let ids = updates
            .iter()
            .map(|update| update.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "kernel-core.x86_64",
                "openssh.x86_64",
                "a-package-with-an-unusually-long-name-for-testing.noarch"
            ]
        );
        assert_eq!(updates[0].title, "kernel-core");
        assert!(updates[0].reboot_required);
        assert_eq!(updates[2].version.as_deref(), Some("1.2-3.el9"));
        assert_eq!(updates[2].source.as_deref(), Some("appstream"));
        let fedora = parse_check_update(FEDORA_CHECK);
        assert_eq!(fedora.len(), 3);
        assert_eq!(fedora[0].version.as_deref(), Some("1:7.1.2.34-1.fc44"));
    }

    #[test]
    fn advisories_mark_security_updates_and_their_severity() {
        let mut alma = parse_check_update(ALMA_CHECK);
        classify(&mut alma, &parse_advisories(ALMA_ADVISORIES));
        assert_eq!(alma[0].category, UpdateCategory::Security);
        assert_eq!(alma[0].severity, Some(UpdateSeverity::Important));
        assert_eq!(
            alma[0].source.as_deref(),
            Some("ALSA-2026:86033"),
            "the advisory for the new version"
        );
        assert_eq!(alma[1].severity, Some(UpdateSeverity::Moderate));
        assert_eq!(alma[2].category, UpdateCategory::Other);
        assert_eq!(alma[2].source.as_deref(), Some("ALBA-2026:1000"));

        let mut fedora = parse_check_update(FEDORA_CHECK);
        classify(&mut fedora, &parse_advisories(FEDORA_ADVISORIES));
        assert_eq!(fedora[0].category, UpdateCategory::Security);
        assert_eq!(fedora[0].severity, None);
        assert_eq!(fedora[1].severity, Some(UpdateSeverity::Important));
        assert_eq!(fedora[1].released_at_unix_ms, Some(1_792_199_031_000));
        assert_eq!(fedora[2].category, UpdateCategory::Other);
        assert_eq!(fedora[2].source.as_deref(), Some("FEDORA-2026-d59fbe5923"));
    }

    #[test]
    fn package_names_split_from_versions() {
        assert_eq!(
            split_nevra("ImageMagick-c++-1:7.1.2.34-1.fc44.x86_64"),
            Some(("ImageMagick-c++.x86_64".into(), "1:7.1.2.34-1.fc44".into()))
        );
        assert_eq!(split_nevra("nonsense"), None);
        assert_eq!(parse_time("1970-01-02", "00:00:01"), Some(86_401_000));
        assert_eq!(parse_time("2026-13-01", "00:00:00"), None);
    }

    #[test]
    fn installed_versions_drop_a_zero_epoch_and_keep_install_order() {
        let installed = parse_installed(
            "1792159225 kernel-core.x86_64 7.2.10-200.fc44\n\
             1776866351 kernel-core.x86_64 6.19.10-300.fc44\n\
             1776866351 vim-data.noarch 2:9.2.1100-1.fc44\n\
             1776866351 bash.x86_64 0:5.3-1.fc44\n",
        );
        assert_eq!(
            installed["kernel-core.x86_64"],
            ["6.19.10-300.fc44", "7.2.10-200.fc44"]
        );
        assert_eq!(installed["vim-data.noarch"], ["2:9.2.1100-1.fc44"]);
        assert_eq!(installed["bash.x86_64"], ["5.3-1.fc44"]);
    }
}
