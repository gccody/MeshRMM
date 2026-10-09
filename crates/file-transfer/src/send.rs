use anyhow::{Context, ensure};
use meshrmm_protocol::{FILE_CHUNK_BYTES, FileDestination, FileMessage};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use crate::{check_limit, valid_path};

/// Keeps up to `limit` transfer messages unacknowledged. The receiver
/// acknowledges each message in order or answers with an error.
pub(super) struct AckWindow<'a> {
    acknowledgements: &'a mpsc::Receiver<bool>,
    limit: usize,
    pub(super) unacknowledged: usize,
}
impl<'a> AckWindow<'a> {
    pub(super) fn new(acknowledgements: &'a mpsc::Receiver<bool>, limit: usize) -> Self {
        Self {
            acknowledgements,
            limit,
            unacknowledged: 0,
        }
    }
    /// A receiver rejects a transfer when it begins or learns its totals, so
    /// nothing follows those until they are accepted. A transfer is complete
    /// once its finish is acknowledged.
    pub(super) fn settles(&self, message: &FileMessage) -> bool {
        matches!(
            message,
            FileMessage::Begin { .. } | FileMessage::Totals { .. } | FileMessage::Finish { .. }
        )
    }
    /// Records one sent message, then waits until another may be sent.
    pub(super) fn sent(&mut self, settle: bool) -> anyhow::Result<()> {
        self.unacknowledged += 1;
        let limit = if settle { 1 } else { self.limit };
        while self.unacknowledged >= limit {
            ensure!(
                self.acknowledgements
                    .recv_timeout(Duration::from_secs(120))
                    .context("transfer acknowledgement timed out")?,
                "peer rejected transfer"
            );
            self.unacknowledged -= 1;
        }
        Ok(())
    }
}

pub(super) fn send_paths(
    id: u64,
    paths: Vec<PathBuf>,
    destination: FileDestination,
    mut send: impl FnMut(FileMessage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    fn visit(
        path: PathBuf,
        relative: String,
        entries: &mut Vec<(PathBuf, String, Option<u64>)>,
        depth: usize,
    ) -> anyhow::Result<()> {
        ensure!(
            depth < 128 && entries.len() < 100_000,
            "folder tree is too large"
        );
        valid_path(&relative)?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "symbolic links are not transferred"
        );
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "only regular files and folders can be transferred"
        );
        entries.push((
            path.clone(),
            relative.clone(),
            metadata.is_file().then_some(metadata.len()),
        ));
        if metadata.is_dir() {
            for child in fs::read_dir(path)? {
                let child = child?;
                let name = child
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("file name is not Unicode"))?;
                visit(
                    child.path(),
                    format!("{relative}/{name}"),
                    entries,
                    depth + 1,
                )?;
            }
        }
        Ok(())
    }
    for path in paths {
        let name = path
            .file_name()
            .context("cannot transfer a filesystem root")?
            .to_str()
            .context("file name is not Unicode")?
            .to_owned();
        visit(path, name, &mut entries, 0)?;
    }
    let bytes = entries.iter().try_fold(0u64, |sum, (_, _, size)| {
        sum.checked_add(size.unwrap_or(0))
            .context("transfer size overflow")
    })?;
    check_limit(&destination, bytes)?;
    send(FileMessage::Begin { id, destination })?;
    send(FileMessage::Totals {
        id,
        bytes,
        entries: entries.len() as u64,
    })?;
    for (path, relative, size) in entries {
        send(FileMessage::Entry {
            id,
            path: relative,
            size,
        })?;
        if let Some(size) = size {
            send_file(id, &path, size, &mut send)?;
        }
    }
    send(FileMessage::Finish { id })
}

/// Sends exactly `size` bytes of `path` and its checksum. A file that grew
/// since it was listed is cut at `size`; one that shrank fails the transfer.
pub(super) fn send_file(
    id: u64,
    path: &Path,
    size: u64,
    send: &mut impl FnMut(FileMessage) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut file = File::open(path)?.take(size);
    let mut hash = Sha256::new();
    let mut buffer = vec![0; FILE_CHUNK_BYTES];
    let mut sent = 0;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        sent += count as u64;
        hash.update(&buffer[..count]);
        send(FileMessage::Chunk {
            id,
            data: buffer[..count].to_vec(),
        })?;
    }
    ensure!(
        sent == size,
        "{} changed while it was being sent",
        path.display()
    );
    send(FileMessage::EndEntry {
        id,
        sha256: hash.finalize().to_vec(),
    })
}
