//! Where the macOS Agent keeps its state. The installed Agent runs as root and
//! keeps it in `/Library/Application Support/MeshRMM/Agent`, which only root
//! can change; a console Agent run by a user for development keeps it in that
//! user's own Application Support folder.
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;

const SYSTEM_CONFIG_DIRECTORY: &str = "/Library/Application Support/MeshRMM/Agent";

/// Replaces `path` atomically with a file only its owner can read.
pub(crate) fn replace_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.new",
        path.extension().and_then(OsStr::to_str).unwrap_or("tmp")
    ));
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed to replace {}", path.display()))
}

pub(crate) fn config_directory() -> anyhow::Result<PathBuf> {
    // SAFETY: geteuid has no preconditions.
    let directory = if unsafe { libc::geteuid() } == 0 {
        PathBuf::from(SYSTEM_CONFIG_DIRECTORY)
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Path::new(&home).join("Library/Application Support/MeshRMM/Agent")
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    Ok(directory)
}

pub(crate) fn identity_directory() -> anyhow::Result<PathBuf> {
    Ok(config_directory()?.join("identity"))
}
