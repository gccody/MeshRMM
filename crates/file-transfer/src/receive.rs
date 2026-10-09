use anyhow::{Context, bail, ensure};
use meshrmm_protocol::{FILE_CHUNK_BYTES, FileDestination, FileMessage};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::{
    FREE_SPACE_RESERVE_BYTES, TRANSFER_FOLDER, check_limit, mebibytes, native, valid_path,
};

/// Tags a received file or folder the way browsers tag downloads (the
/// quarantine attribute on macOS, Mark of the Web on Windows), so opening a
/// received app or installer gets the same checks. Both the viewer and the
/// agent tag what they receive. A volume without extended attributes or
/// alternate streams keeps the file untagged.
fn mark_received(path: &Path) {
    if let Err(error) = native::mark_received(path) {
        tracing::warn!(%error, path = %path.display(), "could not mark a received file as downloaded");
    }
}

/// Moves `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`]
/// instead of replacing an existing file or folder.
pub(super) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    match native::rename_no_replace(from, to) {
        // Some network volumes lack an atomic no-replace rename.
        Err(error) if error.kind() == io::ErrorKind::Unsupported => {
            if to.try_exists()? {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            fs::rename(from, to)
        }
        result => result,
    }
}

/// Moves `from` into `folder` as `name`, or under a name from `fallback`
/// while that name is taken, without ever replacing an existing file.
pub(super) fn move_unique(
    from: &Path,
    folder: &Path,
    name: &str,
    fallback: impl Fn() -> String,
) -> anyhow::Result<PathBuf> {
    let mut target = folder.join(name);
    for _ in 0..8 {
        match rename_no_replace(from, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                target = folder.join(fallback());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not save {name:?}"));
            }
        }
    }
    bail!("could not find a free name for {name:?}")
}

pub(super) struct Incoming {
    pub(super) id: u64,
    pub(super) destination: FileDestination,
    pub(super) stage: PathBuf,
    pub(super) base: PathBuf,
    roots: Vec<String>,
    file: Option<(File, u64, Sha256)>,
    pub(super) entries: usize,
    pub(super) received_bytes: u64,
    /// Sum of the file sizes announced so far; never above `total_bytes`.
    declared_bytes: u64,
    pub(super) total_bytes: u64,
    pub(super) total_entries: u64,
    pub(super) current_name: String,
}
impl Incoming {
    pub(super) fn new(
        id: u64,
        destination: FileDestination,
        documents: PathBuf,
    ) -> anyhow::Result<Self> {
        let base = documents.join(TRANSFER_FOLDER);
        fs::create_dir_all(&base)?;
        ensure!(
            !fs::symlink_metadata(&base)?.file_type().is_symlink(),
            "transfer directory cannot be a link"
        );
        let stage = base.join(format!(".partial-{}-{}", id, crate::id()));
        fs::create_dir(&stage)?;
        Ok(Self {
            id,
            destination,
            stage,
            base,
            roots: Vec::new(),
            file: None,
            entries: 0,
            received_bytes: 0,
            declared_bytes: 0,
            total_bytes: 0,
            total_entries: 0,
            current_name: String::new(),
        })
    }
    pub(super) fn accept(&mut self, message: FileMessage) -> anyhow::Result<()> {
        match message {
            FileMessage::Totals { bytes, entries, .. } => {
                ensure!(
                    self.total_entries == 0 && entries > 0 && entries <= 100_000,
                    "invalid transfer totals"
                );
                check_limit(&self.destination, bytes)?;
                let free = native::available_space(&self.stage)
                    .context("could not check free disk space")?;
                ensure!(
                    free >= bytes.saturating_add(FREE_SPACE_RESERVE_BYTES),
                    "Not enough disk space: the files need {} and {} is free",
                    mebibytes(bytes),
                    mebibytes(free)
                );
                self.total_bytes = bytes;
                self.total_entries = entries;
            }
            FileMessage::Entry { path, size, .. } => {
                ensure!(self.total_entries > 0, "transfer totals are missing");
                self.current_name = path.clone();
                ensure!(self.file.is_none(), "previous file is unfinished");
                ensure!(
                    (self.entries as u64) < self.total_entries,
                    "more entries than announced"
                );
                self.entries += 1;
                let relative = valid_path(&path)?;
                if let Some(size) = size {
                    self.declared_bytes = self
                        .declared_bytes
                        .checked_add(size)
                        .filter(|declared| *declared <= self.total_bytes)
                        .context("files are larger than announced")?;
                }
                let root = path
                    .split_once('/')
                    .map_or(&*path, |(root, _)| root)
                    .to_owned();
                if !self.roots.contains(&root) {
                    self.roots.push(root);
                }
                let target = self.stage.join(relative);
                if let Some(parent) = target.parent() {
                    ensure!(parent.is_dir(), "parent directory missing");
                }
                if let Some(size) = size {
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)?;
                    mark_received(&target);
                    self.file = Some((file, size, Sha256::new()));
                } else {
                    fs::create_dir(&target)?;
                    mark_received(&target);
                }
            }
            FileMessage::Chunk { data, .. } => {
                ensure!(
                    !data.is_empty() && data.len() <= FILE_CHUNK_BYTES,
                    "invalid chunk size"
                );
                let (file, remaining, hash) = self.file.as_mut().context("no open file")?;
                ensure!(
                    data.len() as u64 <= *remaining,
                    "file exceeds declared size"
                );
                file.write_all(&data)?;
                hash.update(&data);
                *remaining -= data.len() as u64;
                self.received_bytes = self
                    .received_bytes
                    .checked_add(data.len() as u64)
                    .context("transfer size overflow")?;
            }
            FileMessage::EndEntry { sha256, .. } => {
                let (file, remaining, hash) = self.file.take().context("no open file")?;
                ensure!(
                    remaining == 0 && hash.finalize().as_slice() == sha256,
                    "file checksum or size mismatch"
                );
                file.sync_all()?;
            }
            _ => bail!("unexpected transfer packet"),
        }
        Ok(())
    }
    pub(super) fn finish(&mut self) -> anyhow::Result<Vec<PathBuf>> {
        ensure!(
            self.file.is_none()
                && !self.roots.is_empty()
                && self.entries as u64 == self.total_entries
                && self.received_bytes == self.total_bytes,
            "incomplete transfer"
        );
        if self.destination != FileDestination::Documents {
            let batch_name = || format!("transfer-{}", crate::id());
            let batch = move_unique(&self.stage, &self.base, &batch_name(), batch_name)?;
            return Ok(self.roots.iter().map(|root| batch.join(root)).collect());
        }
        self.roots
            .iter()
            .map(|root| {
                move_unique(&self.stage.join(root), &self.base, root, || {
                    format!("{}-{root}", crate::id())
                })
            })
            .collect()
    }
}
impl Drop for Incoming {
    fn drop(&mut self) {
        self.file = None;
        let _ = fs::remove_dir_all(&self.stage);
    }
}
