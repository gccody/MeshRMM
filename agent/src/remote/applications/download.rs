//! Downloading a custom installer for a run, into a folder only the Agent's
//! account can use, checking its size and SHA-256 on the way. The server
//! hands out a run's installers only while the run is pending.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use meshrmm_protocol::CustomInstaller;
use sha2::{Digest, Sha256};

use crate::remote::config::{Config, ExecutionMode};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A downloaded installer, removed with its folder when dropped.
pub(crate) struct Staged {
    folder: PathBuf,
    path: PathBuf,
}

impl Staged {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

/// The folder runs download into.
fn root(mode: ExecutionMode) -> anyhow::Result<PathBuf> {
    if mode == ExecutionMode::Console {
        let folder = std::env::temp_dir().join("meshrmm-applications");
        std::fs::create_dir_all(&folder)?;
        return Ok(folder);
    }
    let folder = crate::installer::config_directory()?.join("applications");
    #[cfg(windows)]
    crate::private_directory::secure(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)?;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(folder)
}

/// Run and file IDs name folders and URL segments, so they must be plain.
fn plain(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Downloads the installer of the run's item `index`, keeping its file
/// name, which says what kind of installer it is.
pub(crate) fn fetch(
    config: &Config,
    mode: ExecutionMode,
    run_id: &str,
    index: usize,
    installer: &CustomInstaller,
) -> anyhow::Result<Staged> {
    anyhow::ensure!(
        plain(run_id) && plain(&installer.file_id),
        "the run has an invalid ID"
    );
    anyhow::ensure!(
        meshrmm_protocol::valid_file_name(&installer.file_name),
        "the installer has an invalid file name"
    );
    let folder = root(mode)?.join(format!("{run_id}-{index}"));
    let _ = std::fs::remove_dir_all(&folder);
    std::fs::create_dir(&folder)
        .with_context(|| format!("could not create {}", folder.display()))?;
    let staged = Staged {
        path: folder.join(&installer.file_name),
        folder,
    };
    let url = meshrmm_signaling_client::endpoint_url(
        &config.server,
        &[
            "v1",
            "agents",
            &config.device_id,
            "application-runs",
            run_id,
            "files",
            &installer.file_id,
        ],
        &[],
        false,
    )?;
    let http = ureq::Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .http_status_as_error(true)
        .tls_config(crate::enrollment::https_tls_config())
        .build()
        .new_agent();
    let response = http
        .get(url.as_str())
        .header("Authorization", &format!("Bearer {}", config.agent_token))
        .call()
        .context("the server did not provide the installer")?;
    let mut body = response
        .into_body()
        .into_with_config()
        .limit(installer.size_bytes.saturating_add(1))
        .reader();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged.path)
        .with_context(|| format!("could not create {}", staged.path.display()))?;
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    let mut chunk = vec![0; 64 * 1024];
    loop {
        let read = body
            .read(&mut chunk)
            .context("the download was interrupted")?;
        if read == 0 {
            break;
        }
        size += read as u64;
        anyhow::ensure!(
            size <= installer.size_bytes,
            "the server sent more than the file's size"
        );
        digest.update(&chunk[..read]);
        file.write_all(&chunk[..read])
            .with_context(|| format!("could not write {}", staged.path.display()))?;
    }
    file.flush()?;
    drop(file);
    anyhow::ensure!(size == installer.size_bytes, "the download was incomplete");
    let actual: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    anyhow::ensure!(
        actual == installer.sha256,
        "the download did not match the file's SHA-256"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged.path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_in_paths_are_plain() {
        assert!(plain("0b5e8f5c-58a4-4a5e-9d7e-3c0c4d1c2b3a"));
        assert!(!plain("../x"));
        assert!(!plain(""));
    }
}
