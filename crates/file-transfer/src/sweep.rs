use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use crate::{CACHE_BATCH_AGE, STALE_PARTIAL_AGE, TRANSFER_FOLDER, native};

/// Removes what earlier sessions left behind: interrupted transfers in both
/// transfer folders and clipboard/drop copies no app can still be reading.
pub(super) fn sweep_transfer_folders() {
    if let Ok(documents) = native::documents() {
        sweep_partials(&documents.join(TRANSFER_FOLDER), SystemTime::now());
    }
    if let Ok(cache) = native::cache() {
        let keep = native::clipboard_files().unwrap_or_default();
        sweep_cache(&cache.join(TRANSFER_FOLDER), &keep);
    }
}

/// `keep` lists the files on the clipboard, whose batches stay.
pub(super) fn sweep_cache(base: &Path, keep: &[PathBuf]) {
    sweep_partials(base, SystemTime::now());
    sweep_batches(base, keep, SystemTime::now());
}

/// Deletes `.partial-*` folders nothing has written to for [`STALE_PARTIAL_AGE`].
/// Another viewer may be receiving into the same folder, so recent ones stay.
pub(super) fn sweep_partials(base: &Path, now: SystemTime) {
    sweep(base, ".partial-", STALE_PARTIAL_AGE, &[], now);
}

/// Deletes finished clipboard/drop batches older than [`CACHE_BATCH_AGE`]
/// unless they hold a file that is still on the clipboard.
pub(super) fn sweep_batches(base: &Path, keep: &[PathBuf], now: SystemTime) {
    sweep(base, "transfer-", CACHE_BATCH_AGE, keep, now);
}

fn sweep(base: &Path, prefix: &str, age: Duration, keep: &[PathBuf], now: SystemTime) {
    let Ok(children) = fs::read_dir(base) else {
        return;
    };
    for child in children.flatten() {
        let path = child.path();
        let named = child
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix));
        let is_dir = child.file_type().is_ok_and(|kind| kind.is_dir());
        if !named || !is_dir || keep.iter().any(|kept| kept.starts_with(&path)) {
            continue;
        }
        let stale = latest_write(&path, 0)
            .is_ok_and(|written| now.duration_since(written).is_ok_and(|idle| idle >= age));
        if stale {
            match fs::remove_dir_all(&path) {
                Ok(()) => tracing::info!(path = %path.display(), "removed old transferred files"),
                Err(error) => {
                    tracing::debug!(%error, path = %path.display(), "could not remove old transfer")
                }
            }
        }
    }
}

/// The most recent modification time in a folder tree, without following links.
fn latest_write(path: &Path, depth: usize) -> io::Result<SystemTime> {
    let metadata = fs::symlink_metadata(path)?;
    let mut latest = metadata.modified()?;
    if metadata.is_dir() && depth < 128 {
        for child in fs::read_dir(path)? {
            latest = latest.max(latest_write(&child?.path(), depth + 1)?);
        }
    }
    Ok(latest)
}

/// Deletes the cache batch that holds `paths` once they were copied elsewhere.
pub(super) fn remove_batch(paths: &[PathBuf]) {
    let Some(batch) = paths.first().and_then(|path| path.parent()) else {
        return;
    };
    if batch
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("transfer-"))
        && let Err(error) = fs::remove_dir_all(batch)
    {
        tracing::debug!(%error, "could not remove a copied transfer batch");
    }
}
