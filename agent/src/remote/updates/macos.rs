//! macOS updates, through `softwareupdate`.
//!
//! A scan runs `softwareupdate --list`, which asks Apple's servers, so it
//! needs the network and can take a minute or more. Each update is named by
//! its label, which is what an install passes back. Apple doesn't classify
//! updates, so the Agent does by title:
//!
//! - a macOS update to a newer major version than the running one is an
//!   [`UpdateCategory::Upgrade`];
//! - other macOS updates, Background Security Improvements and Rapid
//!   Security Responses are [`UpdateCategory::Security`], since every macOS
//!   point release carries security fixes;
//! - XProtect, MRT, Gatekeeper and other configuration data updates are
//!   [`UpdateCategory::Definition`];
//! - everything else, such as Safari or the Command Line Tools, is
//!   [`UpdateCategory::Other`].
//!
//! macOS has no flag that says a restart is pending, so `reboot_pending` is
//! always `false`; an update that needs one says so with `Action: restart`.
//!
//! Installs run `softwareupdate --install` with each label (or `--all`),
//! never `--restart`: the shared code restarts as asked. On Apple silicon,
//! installing macOS itself needs a volume owner's password, which the Agent
//! doesn't have, so such an install fails with an explanation.
use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use meshrmm_protocol::{
    PendingUpdate, UpdateCategory, UpdateInstallItem, UpdateItemOutcome, UpdateManager, UpdateScan,
};

use super::Progress;
use super::process::{Output, run};

const SOFTWAREUPDATE: &str = "/usr/sbin/softwareupdate";
const SCAN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

pub(super) fn scan() -> UpdateScan {
    let mut scan = UpdateScan {
        manager: Some(UpdateManager::Softwareupdate),
        ..Default::default()
    };
    match list() {
        Ok(updates) => scan.updates = updates,
        Err(error) => scan.error = Some(format!("{error:#}")),
    }
    scan
}

/// The pending updates, classified against the running macOS version.
fn list() -> anyhow::Result<Vec<PendingUpdate>> {
    let output = run(Path::new(SOFTWAREUPDATE), &["--list"], &[], SCAN_TIMEOUT)?;
    if let Some(error) = list_error(&output) {
        bail!(error);
    }
    let running = running_major();
    Ok(parse_list(&output.stdout)
        .into_iter()
        .map(|listed| listed.into_update(running))
        .collect())
}

/// Why `softwareupdate --list` failed, if it did.
fn list_error(output: &Output) -> Option<String> {
    let text = format!("{}\n{}", output.stdout, output.stderr);
    let lowered = text.to_lowercase();
    if lowered.contains("can't connect") || lowered.contains("cannot connect") {
        return Some(
            "Software Update could not reach Apple's update server. Check the Mac's internet connection."
                .to_owned(),
        );
    }
    if !output.success() {
        return Some(format!("softwareupdate failed: {}", output.complaint()));
    }
    None
}

/// The running macOS major version, such as 15.
fn running_major() -> Option<u32> {
    let output = run(
        Path::new("/usr/bin/sw_vers"),
        &["-productVersion"],
        &[],
        Duration::from_secs(30),
    )
    .ok()?;
    major(output.stdout.trim())
}

/// The major version of `text`, such as 14 for `14.4.1`.
fn major(text: &str) -> Option<u32> {
    text.split('.').next()?.trim().parse().ok()
}

/// An update as `softwareupdate --list` lists it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Listed {
    label: String,
    title: String,
    version: Option<String>,
    size_bytes: Option<u64>,
    restart: bool,
}

impl Listed {
    fn into_update(self, running_major: Option<u32>) -> PendingUpdate {
        let category = categorize(&self.title, self.version.as_deref(), running_major);
        PendingUpdate {
            id: self.label,
            title: self.title,
            kb: None,
            version: self.version,
            current_version: None,
            category,
            severity: None,
            size_bytes: self.size_bytes,
            reboot_required: self.restart,
            released_at_unix_ms: None,
            source: None,
        }
    }
}

/// The category of an update titled `title`; see the module's description.
fn categorize(title: &str, version: Option<&str>, running_major: Option<u32>) -> UpdateCategory {
    let lowered = title.to_lowercase();
    if [
        "xprotect",
        "mrt",
        "gatekeeper",
        "configuration data",
        "configdata",
    ]
    .iter()
    .any(|word| lowered.contains(word))
    {
        return UpdateCategory::Definition;
    }
    if lowered.contains("background security improvement")
        || lowered.contains("rapid security response")
    {
        return UpdateCategory::Security;
    }
    if lowered.starts_with("macos") {
        let offered = version.and_then(major).or_else(|| {
            title
                .split_whitespace()
                .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))
                .and_then(major)
        });
        return match (offered, running_major) {
            (Some(offered), Some(running)) if offered > running => UpdateCategory::Upgrade,
            _ => UpdateCategory::Security,
        };
    }
    UpdateCategory::Other
}

/// Reads `softwareupdate --list`'s output, in the format since macOS 10.15
/// (`* Label: …` then `Title: …, Version: …, Size: …KiB, …`) or the older
/// one (`* <label>` then `<title> (<version>), <size>K [recommended]`).
fn parse_list(text: &str) -> Vec<Listed> {
    let mut updates: Vec<Listed> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('*') {
            let rest = rest.trim();
            let label = rest.strip_prefix("Label:").unwrap_or(rest).trim();
            if !label.is_empty() {
                updates.push(Listed {
                    label: label.to_owned(),
                    ..Default::default()
                });
            }
            continue;
        }
        let Some(update) = updates.last_mut() else {
            continue;
        };
        if !update.title.is_empty() || trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Title:") || trimmed.contains(", Title:") {
            parse_fields(trimmed, update);
        } else if line.starts_with('\t') || line.starts_with(' ') {
            parse_old_details(trimmed, update);
        }
    }
    for update in &mut updates {
        if update.title.is_empty() {
            update.title = update.label.clone();
        }
    }
    updates
}

/// `Title: Safari, Version: 17.4, Size: 158494KiB, Recommended: YES, Action: restart,`
fn parse_fields(line: &str, update: &mut Listed) {
    // Titles may contain commas, so split only before a known field name.
    const FIELDS: [&str; 6] = [
        "Title:",
        "Version:",
        "Size:",
        "Recommended:",
        "Action:",
        "Deferred:",
    ];
    let mut starts = FIELDS
        .iter()
        .filter_map(|field| {
            let at = if line.starts_with(field) {
                Some(0)
            } else {
                line.find(&format!(", {field}")).map(|at| at + 2)
            }?;
            Some((at, *field))
        })
        .collect::<Vec<_>>();
    starts.sort();
    for (index, (at, field)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(line.len(), |(next, _)| *next);
        let value = line[at + field.len()..end]
            .trim()
            .trim_end_matches(',')
            .trim();
        match *field {
            "Title:" => update.title = value.to_owned(),
            "Version:" if !value.is_empty() => update.version = Some(value.to_owned()),
            "Size:" => update.size_bytes = size(value),
            "Action:" => update.restart |= value.to_lowercase().contains("restart"),
            _ => {}
        }
    }
}

/// `Safari (14.0.2), 67012K [recommended] [restart]`
fn parse_old_details(line: &str, update: &mut Listed) {
    update.restart = line.contains("[restart]");
    let (main, size_part) = match line.rfind("),") {
        Some(at) => (&line[..=at], line[at + 2..].trim()),
        None => (line, ""),
    };
    if let (Some(open), true) = (main.rfind('('), main.ends_with(')')) {
        update.title = main[..open].trim().to_owned();
        let version = main[open + 1..main.len() - 1].trim();
        if !version.is_empty() {
            update.version = Some(version.to_owned());
        }
    } else {
        update.title = main.trim().to_owned();
    }
    update.size_bytes = size_part.split_whitespace().next().and_then(size);
}

/// `158494KiB` or `67012K` in bytes.
fn size(text: &str) -> Option<u64> {
    let text = text.trim();
    let digits = text
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    let number: u64 = digits.parse().ok()?;
    let unit = text[digits.len()..].trim().to_ascii_uppercase();
    let factor = match unit.as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1024,
        "M" | "MB" | "MIB" => 1024 * 1024,
        "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
        _ => return None,
    };
    number.checked_mul(factor)
}

pub(super) fn install(
    _manager: UpdateManager,
    chosen: &[&PendingUpdate],
    everything: bool,
    progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    let program = Path::new(SOFTWAREUPDATE);
    let mut outputs = Vec::new();
    if everything {
        progress.report(format!("Installing {} updates", chosen.len()), None);
        let output = run(
            program,
            &["--install", "--all", "--agree-to-license"],
            &[],
            INSTALL_TIMEOUT,
        )?;
        let items = chosen.iter().map(|update| (*update, 0)).collect::<Vec<_>>();
        outputs.push(output);
        return finish(&items, &outputs);
    }
    let mut items = Vec::new();
    for (index, update) in chosen.iter().enumerate() {
        progress.report(
            format!(
                "Installing {} of {}: {}",
                index + 1,
                chosen.len(),
                update.title
            ),
            percent(index, chosen.len()),
        );
        let output = run(
            program,
            &["--install", "--agree-to-license", &update.id],
            &[],
            INSTALL_TIMEOUT,
        )?;
        items.push((*update, outputs.len()));
        outputs.push(output);
    }
    finish(&items, &outputs)
}

fn percent(done: usize, total: usize) -> Option<u8> {
    (total > 0).then(|| u8::try_from(done * 100 / total).unwrap_or(100))
}

/// Each update's outcome, from what `outputs[index]` said about it and
/// whether it is still pending afterwards.
fn finish(
    items: &[(&PendingUpdate, usize)],
    outputs: &[Output],
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    let combined = outputs
        .iter()
        .map(|output| format!("{}\n{}", output.stdout, output.stderr))
        .collect::<Vec<_>>();
    // What is still pending says best what didn't install.
    let pending = list().ok().map(|updates| {
        updates
            .into_iter()
            .map(|update| update.id)
            .collect::<Vec<_>>()
    });
    let installed = |update: &PendingUpdate, text: &str| match &pending {
        Some(pending) => !pending.contains(&update.id),
        None => reported_done(text, update),
    };
    if items.iter().all(|(update, index)| {
        !installed(update, &combined[*index]) && needs_owner(&combined[*index])
    }) {
        bail!(
            "macOS refused to install without a volume owner's password. On Apple silicon, macOS updates must be installed by a user at the Mac, in System Settings."
        );
    }
    Ok(items
        .iter()
        .map(|(update, index)| {
            let text = &combined[*index];
            let installed = installed(update, text);
            let error = (!installed).then(|| {
                if needs_owner(text) {
                    "macOS needs a volume owner's password to install this; install it at the Mac."
                        .to_owned()
                } else {
                    failure(text, update).unwrap_or_else(|| outputs[*index].complaint())
                }
            });
            UpdateInstallItem {
                id: update.id.clone(),
                title: update.title.clone(),
                kb: None,
                category: update.category,
                outcome: if installed {
                    UpdateItemOutcome::Installed
                } else {
                    UpdateItemOutcome::Failed
                },
                error,
                reboot_required: installed && update.reboot_required,
            }
        })
        .collect())
}

/// Whether the output says the update finished installing.
fn reported_done(text: &str, update: &PendingUpdate) -> bool {
    text.lines().map(str::trim).any(|line| {
        line.strip_prefix("Done with ")
            .or_else(|| line.strip_prefix("Installed "))
            .is_some_and(|name| name == update.id || name == update.title)
    })
}

/// The line that says why the update failed, if any does.
fn failure(text: &str, update: &PendingUpdate) -> Option<String> {
    let failures = text
        .lines()
        .map(str::trim)
        .filter(|line| {
            let lowered = line.to_lowercase();
            lowered.contains("error")
                || lowered.contains("failed")
                || lowered.contains("no such update")
        })
        .collect::<Vec<_>>();
    failures
        .iter()
        .find(|line| line.contains(&update.id) || line.contains(&update.title))
        .or(failures.first())
        .map(|line| (*line).to_owned())
}

/// Whether macOS wanted credentials the Agent doesn't have, as it does for
/// macOS updates on Apple silicon.
fn needs_owner(text: &str) -> bool {
    let lowered = text.to_lowercase();
    [
        "not authorized",
        "password",
        "volume owner",
        "authorization",
        "requires an administrator",
    ]
    .iter()
    .any(|phrase| lowered.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT: &str = "Software Update Tool\n\nFinding available software\nSoftware Update found the following new or updated software:\n* Label: macOS Sonoma 14.4-23E214\n\tTitle: macOS Sonoma 14.4, Version: 14.4, Size: 6997664KiB, Recommended: YES, Action: restart, \n* Label: Safari17.4MontereyAuto-17.4\n\tTitle: Safari, Version: 17.4, Size: 158494KiB, Recommended: YES, \n* Label: macOS Sequoia 15.0-24A335\n\tTitle: macOS Sequoia 15.0, Version: 15.0, Size: 12345KiB, Recommended: YES, Action: restart, \n* Label: XProtectPlistConfigData_10_15-5287\n\tTitle: XProtectPlistConfigData, Version: 5287, Size: 1024KiB, Recommended: YES, Action: none, \n";

    const OLD: &str = "Software Update Tool\n\nFinding available software\nSoftware Update found the following new or updated software:\n   * macOS Catalina 10.15.7 Supplemental Update-\n\tmacOS Catalina 10.15.7 Supplemental Update ( ), 3007464K [recommended] [restart]\n   * Safari14.0.2CatalinaAuto-14.0.2\n\tSafari (14.0.2), 67012K [recommended]\n";

    #[test]
    fn reads_the_current_list_format() {
        let listed = parse_list(CURRENT);
        assert_eq!(listed.len(), 4);
        assert_eq!(
            listed[0],
            Listed {
                label: "macOS Sonoma 14.4-23E214".into(),
                title: "macOS Sonoma 14.4".into(),
                version: Some("14.4".into()),
                size_bytes: Some(6_997_664 * 1024),
                restart: true,
            }
        );
        assert_eq!(listed[1].title, "Safari");
        assert!(!listed[1].restart);
        assert!(!listed[3].restart, "Action: none needs no restart");
        let updates = listed
            .into_iter()
            .map(|listed| listed.into_update(Some(14)))
            .collect::<Vec<_>>();
        assert_eq!(updates[0].id, "macOS Sonoma 14.4-23E214");
        assert_eq!(updates[0].category, UpdateCategory::Security);
        assert!(updates[0].reboot_required);
        assert_eq!(updates[1].category, UpdateCategory::Other);
        assert_eq!(updates[2].category, UpdateCategory::Upgrade);
        assert_eq!(updates[3].category, UpdateCategory::Definition);
    }

    #[test]
    fn reads_the_old_list_format() {
        let listed = parse_list(OLD);
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed[0].label,
            "macOS Catalina 10.15.7 Supplemental Update-"
        );
        assert_eq!(
            listed[0].title,
            "macOS Catalina 10.15.7 Supplemental Update"
        );
        assert_eq!(listed[0].version, None);
        assert_eq!(listed[0].size_bytes, Some(3_007_464 * 1024));
        assert!(listed[0].restart);
        assert_eq!(listed[1].title, "Safari");
        assert_eq!(listed[1].version.as_deref(), Some("14.0.2"));
        assert!(!listed[1].restart);
    }

    #[test]
    fn fields_may_be_missing_or_reordered() {
        let listed = parse_list(
            "* Label: Thing-1\n\tVersion: 2, Title: Thing, with a comma, Action: restart\n* Label: Bare\n",
        );
        assert_eq!(listed[0].title, "Thing, with a comma");
        assert_eq!(listed[0].version.as_deref(), Some("2"));
        assert!(listed[0].restart);
        assert_eq!(listed[1].title, "Bare", "a missing title is the label");
    }

    #[test]
    fn nothing_pending_and_errors_are_told_apart() {
        let none = Output {
            code: Some(0),
            stdout: "Software Update Tool\n\nFinding available software\n".into(),
            stderr: "No new software available.\n".into(),
        };
        assert_eq!(list_error(&none), None);
        assert!(parse_list(&none.stdout).is_empty());
        let offline = Output {
            code: Some(0),
            stdout: String::new(),
            stderr: "Can't connect to the Apple Software Update server, because you are not connected to the Internet.\n".into(),
        };
        assert!(
            list_error(&offline)
                .unwrap()
                .contains("internet connection")
        );
        let failed = Output {
            code: Some(1),
            stdout: String::new(),
            stderr: "Something broke\n".into(),
        };
        assert_eq!(
            list_error(&failed).unwrap(),
            "softwareupdate failed: Something broke"
        );
    }

    #[test]
    fn macos_versions_classify_as_upgrades_only_across_majors() {
        assert_eq!(
            categorize("macOS Tahoe 26.1", Some("26.1"), Some(26)),
            UpdateCategory::Security
        );
        assert_eq!(
            categorize("macOS Tahoe 26", None, Some(15)),
            UpdateCategory::Upgrade
        );
        assert_eq!(
            categorize("macOS Background Security Improvement", None, Some(26)),
            UpdateCategory::Security
        );
        assert_eq!(
            categorize("Command Line Tools for Xcode", Some("16.0"), Some(15)),
            UpdateCategory::Other
        );
        assert_eq!(
            categorize("MRTConfigData", None, None),
            UpdateCategory::Definition
        );
        assert_eq!(size("12M"), Some(12 * 1024 * 1024));
        assert_eq!(size("abc"), None);
    }

    #[test]
    fn install_output_says_what_finished_and_why_not() {
        let update = PendingUpdate {
            id: "Safari17.4MontereyAuto-17.4".into(),
            title: "Safari".into(),
            kb: None,
            version: None,
            current_version: None,
            category: UpdateCategory::Other,
            severity: None,
            size_bytes: None,
            reboot_required: false,
            released_at_unix_ms: None,
            source: None,
        };
        let done = "Software Update Tool\n\nFinding available software\n\nDownloading Safari\nDownloaded Safari\nInstalling Safari\nDone with Safari\nDone.\n";
        assert!(reported_done(done, &update));
        assert!(!reported_done("Downloading Safari\n", &update));
        let failed = "Finding available software\nError downloading Safari17.4MontereyAuto-17.4: The network connection was lost.\n";
        assert!(!reported_done(failed, &update));
        assert_eq!(
            failure(failed, &update).as_deref(),
            Some("Error downloading Safari17.4MontereyAuto-17.4: The network connection was lost.")
        );
        assert!(needs_owner("Password:\nNot authorized to install macOS"));
        assert!(!needs_owner(done));
    }

    /// Scans this Mac for real. Run with `--ignored`; it only lists updates.
    #[test]
    #[ignore = "asks Apple's update server"]
    fn real_scan_is_plausible() {
        let scan = scan();
        println!("{}", serde_json::to_string_pretty(&scan).unwrap());
        assert_eq!(scan.manager, Some(UpdateManager::Softwareupdate));
        assert_eq!(scan.error, None);
        assert!(scan.updates.iter().all(|update| !update.id.is_empty()));
    }
}
