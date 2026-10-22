//! Whether the computer must restart to finish installing updates.
//!
//! - Debian and Ubuntu: a package that needs one creates
//!   `/run/reboot-required`.
//! - Fedora and Red Hat: `dnf needs-restarting -r` (or yum-utils'
//!   `needs-restarting -r`) exits with 1 when core packages changed since
//!   the computer started. Without it, a kernel installed after the running
//!   one says so.
//! - Arch: a kernel upgrade removes the running kernel's modules folder.
use std::path::Path;
use std::time::Duration;

use meshrmm_protocol::UpdateManager;

use super::super::process::{find, run};

const TIMEOUT: Duration = Duration::from_secs(2 * 60);

pub(super) fn pending(manager: UpdateManager) -> bool {
    match manager {
        UpdateManager::Apt => {
            Path::new("/run/reboot-required").exists()
                || Path::new("/var/run/reboot-required").exists()
        }
        UpdateManager::Dnf | UpdateManager::Yum => {
            needs_restarting(manager).unwrap_or_else(newer_kernel_installed)
        }
        UpdateManager::Pacman => running_kernel()
            .is_some_and(|kernel| !Path::new("/usr/lib/modules").join(kernel).exists()),
        UpdateManager::WindowsUpdate | UpdateManager::Softwareupdate => false,
    }
}

/// The release of the running kernel, such as `6.8.0-147-generic`.
fn running_kernel() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|release| release.trim().to_owned())
        .filter(|release| !release.is_empty())
}

/// What `needs-restarting -r` says, or `None` when it isn't installed or
/// gives no answer. The repositories aren't loaded: it needs only what is
/// installed.
fn needs_restarting(manager: UpdateManager) -> Option<bool> {
    let output = if manager == UpdateManager::Dnf
        && let Some(dnf) = find(&["dnf5", "dnf"])
    {
        run(
            &dnf,
            &["-q", "--disablerepo=*", "needs-restarting", "-r"],
            &[],
            TIMEOUT,
        )
        .ok()
    } else {
        None
    };
    let output = match output {
        Some(output) if matches!(output.code, Some(0 | 1)) => output,
        _ => run(&find(&["needs-restarting"])?, &["-r"], &[], TIMEOUT).ok()?,
    };
    match output.code {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

/// Whether the newest installed kernel isn't the one running.
fn newer_kernel_installed() -> bool {
    let (Some(running), Some(rpm)) = (running_kernel(), find(&["rpm"])) else {
        return false;
    };
    let Ok(output) = run(
        &rpm,
        &[
            "-q",
            "kernel-core",
            "kernel",
            "--qf",
            "%{INSTALLTIME} %{VERSION}-%{RELEASE}.%{ARCH}\\n",
        ],
        &[],
        TIMEOUT,
    ) else {
        return false;
    };
    newest_kernel(&output.stdout).is_some_and(|newest| newest != running)
}

/// The kernel installed last, from `INSTALLTIME release` lines.
fn newest_kernel(text: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| {
            let (time, release) = line.trim().split_once(' ')?;
            Some((time.parse::<i64>().ok()?, release.to_owned()))
        })
        .max()
        .map(|(_, release)| release)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_kernel_is_the_last_installed() {
        assert_eq!(
            newest_kernel(
                "1792158860 5.14.0-687.57.1.el9_8.x86_64\n1791176404 5.14.0-687.53.1.el9_8.x86_64\npackage kernel is not installed\n"
            )
            .as_deref(),
            Some("5.14.0-687.57.1.el9_8.x86_64")
        );
        assert_eq!(
            newest_kernel("package kernel-core is not installed\n"),
            None
        );
    }
}
