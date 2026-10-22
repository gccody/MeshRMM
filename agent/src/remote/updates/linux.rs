//! Updates on Linux, through the distribution's package manager: APT on
//! Debian and Ubuntu, DNF (or YUM) on Fedora and Red Hat's family, and
//! pacman on Arch Linux. Other package managers, such as zypper, aren't
//! supported, and the scan says so by naming no manager.
//!
//! A scan refreshes the package lists and asks the package manager what an
//! upgrade would install, without changing anything installed. An install
//! upgrades the chosen packages, or everything, and then reads the installed
//! versions to tell which updates went in.
mod apt;
mod pacman;
mod reboot;
mod rpm;

use std::collections::HashMap;

use meshrmm_protocol::{
    PendingUpdate, UpdateInstallItem, UpdateItemOutcome, UpdateManager, UpdateScan,
};

use super::Progress;
use super::process::{Output, find};

pub(super) fn scan() -> UpdateScan {
    let Some(manager) = manager() else {
        return UpdateScan::default();
    };
    let found = match manager {
        UpdateManager::Apt => apt::scan(),
        UpdateManager::Dnf | UpdateManager::Yum => rpm::scan(manager),
        UpdateManager::Pacman => pacman::scan(),
        UpdateManager::WindowsUpdate | UpdateManager::Softwareupdate => {
            unreachable!("not a Linux package manager")
        }
    };
    let reboot_pending = reboot::pending(manager);
    match found {
        Ok(updates) => UpdateScan {
            manager: Some(manager),
            updates,
            reboot_pending,
            ..Default::default()
        },
        Err(error) => UpdateScan {
            manager: Some(manager),
            reboot_pending,
            error: Some(format!("{error:#}")),
            ..Default::default()
        },
    }
}

pub(super) fn install(
    manager: UpdateManager,
    chosen: &[&PendingUpdate],
    everything: bool,
    progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    match manager {
        UpdateManager::Apt => apt::install(chosen, everything, progress),
        UpdateManager::Dnf | UpdateManager::Yum => {
            rpm::install(manager, chosen, everything, progress)
        }
        UpdateManager::Pacman => pacman::install(chosen, progress),
        UpdateManager::WindowsUpdate | UpdateManager::Softwareupdate => {
            anyhow::bail!("{} isn't available on Linux", manager.label())
        }
    }
}

/// The package manager this computer updates with. APT comes first, since
/// some Debian systems carry an rpm or dnf too.
fn manager() -> Option<UpdateManager> {
    if find(&["apt-get"]).is_some() && find(&["dpkg-query"]).is_some() {
        Some(UpdateManager::Apt)
    } else if find(&["dnf5", "dnf"]).is_some() {
        Some(UpdateManager::Dnf)
    } else if find(&["yum"]).is_some() {
        Some(UpdateManager::Yum)
    } else if find(&["pacman"]).is_some() {
        Some(UpdateManager::Pacman)
    } else {
        None
    }
}

/// Whether upgrading the package named `name` usually needs a restart: the
/// kernel, firmware, CPU microcode, the C library and the init system.
fn needs_reboot(name: &str) -> bool {
    let name = name.split(':').next().unwrap_or(name);
    const EXACT: [&str; 12] = [
        "linux",
        "linux-lts",
        "linux-zen",
        "linux-hardened",
        "kernel",
        "glibc",
        "libc6",
        "systemd",
        "dbus",
        "dbus-broker",
        "microcode_ctl",
        "linux-firmware",
    ];
    const PREFIXES: [&str; 6] = [
        "linux-image-",
        "linux-modules-",
        "kernel-core",
        "kernel-modules",
        "linux-firmware",
        "linux-generic",
    ];
    EXACT.contains(&name)
        || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        || name.ends_with("-firmware")
        || name.ends_with("-ucode")
        || name.ends_with("-microcode")
}

/// How each chosen update went, from the versions installed afterwards
/// (`installed`, by update ID) and the package manager's output. An update
/// went in if its new version is installed, or the version it replaced no
/// longer is.
fn outcomes(
    chosen: &[&PendingUpdate],
    installed: &HashMap<String, Vec<String>>,
    output: &Output,
) -> Vec<UpdateInstallItem> {
    chosen
        .iter()
        .map(|update| {
            let versions = installed
                .get(&update.id)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let has = |version: &Option<String>| {
                version
                    .as_ref()
                    .is_some_and(|version| versions.iter().any(|installed| installed == version))
            };
            let went_in = has(&update.version)
                || (!versions.is_empty()
                    && update.current_version.is_some()
                    && !has(&update.current_version));
            UpdateInstallItem {
                id: update.id.clone(),
                title: update.title.clone(),
                kb: None,
                category: update.category,
                outcome: if went_in {
                    UpdateItemOutcome::Installed
                } else {
                    UpdateItemOutcome::Failed
                },
                error: (!went_in).then(|| {
                    if output.success() {
                        "The package manager didn't upgrade it.".to_owned()
                    } else {
                        output.complaint()
                    }
                }),
                reboot_required: went_in && update.reboot_required,
            }
        })
        .collect()
}

/// "1 package" or "3 packages".
fn packages(count: usize) -> String {
    if count == 1 {
        "1 package".to_owned()
    } else {
        format!("{count} packages")
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::UpdateCategory;

    use super::*;

    fn update(id: &str, current: &str, version: &str) -> PendingUpdate {
        PendingUpdate {
            id: id.into(),
            title: id.into(),
            kb: None,
            version: Some(version.into()),
            current_version: Some(current.into()),
            category: UpdateCategory::Other,
            severity: None,
            size_bytes: None,
            reboot_required: id == "kernel-core.x86_64",
            released_at_unix_ms: None,
            source: None,
        }
    }

    #[test]
    fn restarts_follow_kernels_firmware_and_core_libraries() {
        for name in [
            "linux",
            "linux-image-6.8.0-150-generic",
            "linux-generic",
            "kernel-core",
            "kernel-modules-core",
            "libc6",
            "libc6:i386",
            "systemd",
            "amd-ucode",
            "intel-microcode",
            "amd-gpu-firmware",
            "linux-firmware-intel",
        ] {
            assert!(needs_reboot(name), "{name}");
        }
        for name in [
            "openssh-server",
            "systemd-libs",
            "kernel-tools",
            "libglib2.0-0t64",
        ] {
            assert!(!needs_reboot(name), "{name}");
        }
    }

    #[test]
    fn installs_are_judged_by_the_versions_afterwards() {
        let output = Output {
            code: Some(100),
            stdout: String::new(),
            stderr: "E: Unable to fetch some archives\n".into(),
        };
        let kernel = update("kernel-core.x86_64", "7.2.10-200.fc44", "7.2.11-200.fc44");
        let ssh = update("openssh.x86_64", "9.9p1-12", "9.9p1-13");
        let git = update("git", "1:2.53.0-1", "1:2.53.0-2");
        let installed = HashMap::from([
            // Kernels install beside the running one.
            (
                kernel.id.clone(),
                vec!["7.2.10-200.fc44".to_owned(), "7.2.11-200.fc44".to_owned()],
            ),
            // A newer release than the scan saw counts too.
            (ssh.id.clone(), vec!["9.9p1-14".to_owned()]),
            (git.id.clone(), vec!["1:2.53.0-1".to_owned()]),
        ]);
        let items = outcomes(&[&kernel, &ssh, &git], &installed, &output);
        assert_eq!(items[0].outcome, UpdateItemOutcome::Installed);
        assert!(items[0].reboot_required);
        assert_eq!(items[1].outcome, UpdateItemOutcome::Installed);
        assert_eq!(items[2].outcome, UpdateItemOutcome::Failed);
        assert_eq!(
            items[2].error.as_deref(),
            Some("E: Unable to fetch some archives")
        );
        assert_eq!(packages(1), "1 package");
    }

    /// Scans this computer for real and prints the scan. Run as root on a
    /// test machine: it refreshes the package lists.
    #[test]
    #[ignore = "refreshes this computer's package lists"]
    fn real_scan_is_plausible() {
        let scan = scan();
        println!("{}", serde_json::to_string_pretty(&scan).unwrap());
        assert!(scan.manager.is_some(), "{scan:?}");
        assert_eq!(scan.error, None);
        for update in &scan.updates {
            assert!(update.version.is_some(), "{update:?}");
        }
    }

    /// Installs the updates named in `MESHRMM_TEST_INSTALL` (comma
    /// separated, or `all`) for real. Only on a disposable test machine.
    #[test]
    #[ignore = "installs updates on this computer"]
    fn real_install_installs_the_chosen_updates() {
        let wanted = std::env::var("MESHRMM_TEST_INSTALL").expect("MESHRMM_TEST_INSTALL");
        let before = scan();
        let manager = before.manager.expect("a package manager");
        let chosen = before
            .updates
            .iter()
            .filter(|update| wanted == "all" || wanted.split(',').any(|id| id == update.id))
            .collect::<Vec<_>>();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let progress = Progress {
            run_id: "test".into(),
            sender,
        };
        let items = install(
            manager,
            &chosen,
            chosen.len() == before.updates.len(),
            &progress,
        );
        while let Ok(message) = receiver.try_recv() {
            println!("{message:?}");
        }
        let items = items.unwrap();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
        let after = scan();
        println!(
            "pending after: {} (before {}), reboot pending: {}",
            after.updates.len(),
            before.updates.len(),
            after.reboot_pending
        );
        assert!(
            items
                .iter()
                .all(|item| item.outcome == UpdateItemOutcome::Installed),
            "{items:?}"
        );
    }
}
