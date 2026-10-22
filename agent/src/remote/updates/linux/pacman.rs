//! pacman, on Arch Linux. Arch supports only upgrading everything at once,
//! so a scan never syncs the real package databases: an `-Sy` without the
//! upgrade would leave the next package installed against newer libraries
//! than the system has. It uses `checkupdates` where pacman-contrib provides
//! it, and otherwise does what that script does: syncs copies of the
//! databases in a temporary folder that shares the local database, and asks
//! what is out of date against those. Arch publishes no security
//! classification, so every update is an ordinary one.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{PendingUpdate, UpdateCategory, UpdateInstallItem};

use super::super::Progress;
use super::super::process::{find, run};
use super::{needs_reboot, outcomes, packages};

const SYNC_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const QUERY_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);
const DATABASE: &str = "/var/lib/pacman";
/// `checkupdates`' exit code when nothing is out of date.
const NO_UPDATES: i32 = 2;

fn pacman() -> anyhow::Result<PathBuf> {
    find(&["pacman"]).context("pacman isn't installed")
}

pub(super) fn scan() -> anyhow::Result<Vec<PendingUpdate>> {
    if let Some(checkupdates) = find(&["checkupdates"]) {
        let output = run(&checkupdates, &[], &[], SYNC_TIMEOUT)?;
        match output.code {
            Some(0) => return Ok(parse_updates(&output.stdout)),
            Some(NO_UPDATES) => return Ok(Vec::new()),
            _ => tracing::warn!(
                error = output.complaint(),
                "checkupdates failed; syncing a copy of the databases instead"
            ),
        }
    }
    let copy = TemporaryDatabase::new()?;
    let path = copy.0.to_string_lossy().into_owned();
    let pacman = pacman()?;
    let sync = run(
        &pacman,
        &["-Sy", "--dbpath", &path, "--logfile", "/dev/null"],
        &[],
        SYNC_TIMEOUT,
    )?;
    if !sync.success() {
        bail!("Could not sync the package databases: {}", sync.complaint());
    }
    let outdated = run(&pacman, &["-Qu", "--dbpath", &path], &[], QUERY_TIMEOUT)?;
    // pacman exits with 1 when nothing is out of date.
    if !outdated.success() && !(outdated.code == Some(1) && outdated.stdout.trim().is_empty()) {
        bail!(
            "Could not list the out of date packages: {}",
            outdated.complaint()
        );
    }
    Ok(parse_updates(&outdated.stdout))
}

pub(super) fn install(
    chosen: &[&PendingUpdate],
    progress: &Progress,
) -> anyhow::Result<Vec<UpdateInstallItem>> {
    if Path::new(DATABASE).join("db.lck").exists() {
        bail!(
            "Another program is using pacman ({DATABASE}/db.lck exists); try again when it finishes"
        );
    }
    progress.report(
        format!("Upgrading the system: {}", packages(chosen.len())),
        None,
    );
    let output = run(
        &pacman()?,
        &["-Syu", "--noconfirm", "--noprogressbar"],
        &[],
        INSTALL_TIMEOUT,
    )?;
    let installed = installed_versions()?;
    Ok(outcomes(chosen, &installed, &output))
}

/// Each installed package's version, by name.
fn installed_versions() -> anyhow::Result<HashMap<String, Vec<String>>> {
    let output = run(&pacman()?, &["-Q"], &[], QUERY_TIMEOUT)?;
    if !output.success() {
        bail!(
            "Could not read the installed packages: {}",
            output.complaint()
        );
    }
    Ok(output
        .stdout
        .lines()
        .filter_map(|line| {
            let (name, version) = line.split_once(' ')?;
            Some((name.to_owned(), vec![version.trim().to_owned()]))
        })
        .collect())
}

/// Updates from lines such as `linux 7.2.10.arch1-1 -> 7.2.11.arch1-1`.
/// Packages pacman is set to ignore say `[ignored]`, and an upgrade leaves
/// them as they are.
fn parse_updates(text: &str) -> Vec<PendingUpdate> {
    text.lines()
        .filter(|line| !line.ends_with("[ignored]"))
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let [name, current, "->", version] = fields.as_slice() else {
                return None;
            };
            Some(PendingUpdate {
                id: (*name).to_owned(),
                title: (*name).to_owned(),
                kb: None,
                version: Some((*version).to_owned()),
                current_version: Some((*current).to_owned()),
                category: UpdateCategory::Other,
                severity: None,
                size_bytes: None,
                reboot_required: needs_reboot(name),
                released_at_unix_ms: None,
                source: None,
            })
        })
        .collect()
}

/// A folder for synced copies of the databases, beside the real local
/// database, removed when dropped. pacman downloads as its own unprivileged
/// user, which must be able to reach into it.
struct TemporaryDatabase(PathBuf);

impl TemporaryDatabase {
    fn new() -> anyhow::Result<Self> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let path = std::env::temp_dir().join(format!("meshrmm-pacman-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)
            .with_context(|| format!("could not create {}", path.display()))?;
        let folder = Self(path);
        std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o755))?;
        symlink(Path::new(DATABASE).join("local"), folder.0.join("local"))
            .context("could not link the local package database")?;
        Ok(folder)
    }
}

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outdated_packages_are_read_and_ignored_ones_skipped() {
        let updates = parse_updates(
            "linux 7.2.10.arch1-1 -> 7.2.11.arch1-1\n\
             librsvg 2:2.63.2-1 -> 2:2.63.3-1\n\
             systemd-libs 262-1 -> 262.1-1\n\
             firefox 140.0-1 -> 141.0-1 [ignored]\n\
             :: warning line\n",
        );
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].id, "linux");
        assert!(updates[0].reboot_required);
        assert_eq!(updates[1].current_version.as_deref(), Some("2:2.63.2-1"));
        assert_eq!(updates[1].version.as_deref(), Some("2:2.63.3-1"));
        assert!(!updates[2].reboot_required);
        assert_eq!(updates[2].category, UpdateCategory::Other);
    }
}
