//! Updates the Linux viewer as it starts. Linux lets a running program's
//! file be replaced, so the viewer puts the new one in its own place and
//! starts it, with no helper in between.

use std::ffi::OsString;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_self_update::{CLIENT_LINUX_ARM64, CLIENT_LINUX_X64, CURRENT_VERSION, UpdateManifest};

use super::download;
use crate::config::Config;
use crate::launch_status::{self, LaunchStatus};

const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_UPDATE_BYTES: usize = 256 * 1024 * 1024;

/// Whether the viewer can replace the folder's files: not one the system's
/// packages installed, which they update.
fn writable(directory: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(directory.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // Safety: the path is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Where an update moves the viewer it replaces, until the new one runs.
fn previous_path(executable: &Path) -> PathBuf {
    let mut name = executable.file_name().unwrap_or_default().to_owned();
    name.push(".previous");
    executable.with_file_name(name)
}

/// Removes what an update that was interrupted left beside the viewer.
pub fn remove_stale_files() {
    let Ok(current) = std::env::current_exe() else {
        return;
    };
    let previous = previous_path(&current);
    // While an update starts this viewer, the one it replaces is still
    // running from that file and may have to be put back.
    if std::env::var_os("MESHRMM_UPDATE_READY_FILE").is_none() && previous.is_file() {
        match std::fs::remove_file(&previous) {
            Ok(()) => tracing::info!(path = %previous.display(), "removed a replaced viewer"),
            Err(error) => {
                tracing::warn!(path = %previous.display(), %error, "could not remove a replaced viewer");
            }
        }
    }
}

/// Installs a newer viewer and hands this launch to it. Returns whether it
/// did, in which case this process should exit.
pub async fn check_and_schedule(config: &Config) -> anyhow::Result<bool> {
    if !config.auto_update {
        tracing::info!("automatic viewer updates disabled by local configuration");
        return Ok(false);
    }
    if std::env::var_os("MESHRMM_UPDATE_READY_FILE").is_some() {
        return Ok(false);
    }
    let current = std::env::current_exe().context("could not locate the client executable")?;
    let directory = current
        .parent()
        .context("client executable has no parent directory")?;
    if !writable(directory) {
        tracing::info!(
            directory = %directory.display(),
            "the viewer's folder is not the user's to change; leaving updates to who installed it"
        );
        return Ok(false);
    }
    launch_status::report(LaunchStatus::CheckingForUpdate);
    let http = crate::http::client_builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to create client update HTTP client")?;
    let manifest_bytes = download(
        &http,
        &config.update_manifest_url,
        MAX_MANIFEST_BYTES,
        |_| {},
    )
    .await
    .context("failed to download the client update manifest")?;
    let manifest = UpdateManifest::parse(&manifest_bytes)?;
    let target = if cfg!(target_arch = "aarch64") {
        CLIENT_LINUX_ARM64
    } else {
        CLIENT_LINUX_X64
    };
    let Some(release) = manifest.newer_release(target, CURRENT_VERSION)? else {
        return Ok(false);
    };

    tracing::info!(
        current_version = CURRENT_VERSION,
        release_version = %release.version,
        "downloading client update for this launch"
    );
    let executable = download(&http, &release.url, MAX_UPDATE_BYTES, |progress| {
        launch_status::report(progress.status(&release.version))
    })
    .await
    .context("failed to download the client update")?;
    release.verify(&executable)?;
    if !executable.starts_with(b"\x7fELF") {
        bail!("downloaded client update is not a Linux executable");
    }
    launch_status::report(LaunchStatus::InstallingUpdate {
        version: release.version.clone(),
    });

    let bootstrap = serde_json::to_string(
        config
            .bootstrap
            .as_ref()
            .context("missing launch session")?,
    )?;
    let previous = replace(&current, &executable)?;
    let mut command = Command::new(&current);
    command
        .args(std::env::args_os().skip(1).collect::<Vec<OsString>>())
        .env("MESHRMM_SESSION_BOOTSTRAP", bootstrap);
    if let Err(error) = super::launch_verified(command) {
        std::fs::rename(&previous, &current)
            .context("the client update failed and the previous viewer could not be restored")?;
        return Err(error).context("the updated viewer could not be launched");
    }
    let _ = std::fs::remove_file(previous);
    Ok(true)
}

/// Puts `update` in the place of the viewer at `current`, which moves
/// aside. Returns where it moved to.
fn replace(current: &Path, update: &[u8]) -> anyhow::Result<PathBuf> {
    let mut staged = current.file_name().unwrap_or_default().to_owned();
    staged.push(format!(".update-{}", std::process::id()));
    let staged = current.with_file_name(staged);
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&staged)?;
        std::io::Write::write_all(&mut file, update)?;
        // The mode asked for is less whatever the user's mask takes away.
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("failed to write client update {}", staged.display()));
    }
    let previous = previous_path(current);
    if let Err(error) = std::fs::rename(current, &previous) {
        let _ = std::fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("failed to move aside the viewer {}", current.display()));
    }
    if let Err(error) = std::fs::rename(&staged, current) {
        let _ = std::fs::remove_file(&staged);
        std::fs::rename(&previous, current).context("could not restore the previous viewer")?;
        return Err(error).context("failed to install the client update");
    }
    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_takes_the_viewers_place_and_the_viewer_moves_aside() {
        let directory = std::env::temp_dir().join(format!("meshrmm-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let current = directory.join("meshrmm-remote");
        std::fs::write(&current, b"old").unwrap();

        let previous = replace(&current, b"new").unwrap();
        assert_eq!(previous, directory.join("meshrmm-remote.previous"));
        assert_eq!(std::fs::read(&previous).unwrap(), b"old");
        assert_eq!(std::fs::read(&current).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&current).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // Nothing else is left beside them.
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        assert!(writable(&directory));
        assert!(!writable(&directory.join("missing")));
        let _ = std::fs::remove_dir_all(directory);
    }
}
