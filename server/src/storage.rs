//! Files the server keeps in its data directory: each device's latest screen
//! thumbnail and the toolbox library.
//!
//! A file appears under its final name only once it is complete: it is
//! written to a hidden `.part` file in the same directory, flushed to disk,
//! then renamed into place. Callers name files by IDs the server generated
//! and validated, never by user input.
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::Context;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::secrets::{hex, new_token};

const THUMBNAILS_DIR: &str = "thumbnails";
const TOOLBOX_DIR: &str = "toolbox";
const PARTIAL_SUFFIX: &str = ".part";

#[derive(Debug, Clone)]
pub struct Storage {
    thumbnails: PathBuf,
    toolbox: PathBuf,
}

/// Why [`Storage::receive`] stored nothing.
#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    #[error("the file is larger than {0} bytes")]
    TooLarge(u64),
    #[error("the file does not match its SHA-256")]
    ChecksumMismatch,
    #[error("the upload was interrupted")]
    Interrupted,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A file [`Storage::receive`] stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub size_bytes: u64,
    pub sha256: String,
}

impl Storage {
    /// Creates the storage directories in `data_dir`, readable by the
    /// server's user only, if they do not exist.
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let storage = Self {
            thumbnails: data_dir.join(THUMBNAILS_DIR),
            toolbox: data_dir.join(TOOLBOX_DIR),
        };
        for dir in [&storage.thumbnails, &storage.toolbox] {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(storage)
    }

    pub fn thumbnail(&self, device_id: &str) -> PathBuf {
        self.thumbnails.join(format!("{device_id}.jpg"))
    }

    pub fn toolbox_file(&self, file_id: &str) -> PathBuf {
        self.toolbox.join(file_id)
    }

    /// Replaces `path` with `contents` in one step.
    pub async fn write(&self, path: PathBuf, contents: Bytes) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            let partial = partial_path(&path);
            let written = (|| {
                let mut file = create_private(&partial)?;
                io::Write::write_all(&mut file, &contents)?;
                file.sync_all()?;
                fs::rename(&partial, &path)
            })();
            if written.is_err() {
                let _ = fs::remove_file(&partial);
            }
            written
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Stores `body` at `path` if it is at most `max_bytes` long and, when
    /// `expected_sha256` is given, has that SHA-256 (lowercase hex). Nothing
    /// is left behind when it fails.
    pub async fn receive<S, E>(
        &self,
        path: &Path,
        body: S,
        max_bytes: u64,
        expected_sha256: Option<&str>,
    ) -> Result<Received, ReceiveError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
    {
        let partial = partial_path(path);
        let received = receive_into(&partial, path, body, max_bytes, expected_sha256).await;
        if received.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
        }
        received
    }

    /// Removes a file, if it exists.
    pub async fn remove(&self, path: &Path) -> io::Result<()> {
        match tokio::fs::remove_file(path).await {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Library files last changed before `age` ago, by file ID.
    pub fn old_toolbox_files(&self, age: Duration) -> io::Result<Vec<String>> {
        old_files(&self.toolbox, age, "")
    }

    /// Thumbnails last changed before `age` ago, by device ID.
    pub fn old_thumbnails(&self, age: Duration) -> io::Result<Vec<String>> {
        old_files(&self.thumbnails, age, ".jpg")
    }

    /// Deletes partial files older than `age`, left by a server that stopped
    /// mid-write. Returns how many it deleted.
    pub fn sweep_partial(&self, age: Duration) -> io::Result<u64> {
        let cutoff = SystemTime::now()
            .checked_sub(age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed = 0;
        for dir in [&self.thumbnails, &self.toolbox] {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let stale = entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(PARTIAL_SUFFIX)
                    && entry.metadata()?.modified()? < cutoff;
                if stale && fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

/// The names, less `suffix`, of the complete files in `dir` last changed
/// before `age` ago.
fn old_files(dir: &Path, age: Duration, suffix: &str) -> io::Result<Vec<String>> {
    let cutoff = SystemTime::now()
        .checked_sub(age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || entry.metadata()?.modified()? >= cutoff {
            continue;
        }
        if let Some(id) = name.strip_suffix(suffix) {
            names.push(id.to_owned());
        }
    }
    Ok(names)
}

async fn receive_into<S, E>(
    partial: &Path,
    path: &Path,
    mut body: S,
    max_bytes: u64,
    expected_sha256: Option<&str>,
) -> Result<Received, ReceiveError>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let mut file = tokio::fs::File::from_std(create_private(partial)?);
    let mut digest = Sha256::new();
    let mut size_bytes = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| ReceiveError::Interrupted)?;
        size_bytes = size_bytes.saturating_add(chunk.len() as u64);
        if size_bytes > max_bytes {
            return Err(ReceiveError::TooLarge(max_bytes));
        }
        digest.update(&chunk);
        file.write_all(&chunk).await?;
    }
    let sha256 = hex(&digest.finalize());
    if expected_sha256.is_some_and(|expected| expected != sha256) {
        return Err(ReceiveError::ChecksumMismatch);
    }
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(partial, path).await?;
    Ok(Received { size_bytes, sha256 })
}

/// A hidden name next to `path`, unique to one write.
fn partial_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}{PARTIAL_SUFFIX}", &new_token()[..16]))
}

fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(parts: &[&'static [u8]]) -> impl Stream<Item = Result<Bytes, io::Error>> + Unpin {
        futures_util::stream::iter(
            parts
                .iter()
                .map(|part| Ok(Bytes::from_static(part)))
                .collect::<Vec<_>>(),
        )
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[tokio::test]
    async fn received_files_are_checked_and_appear_whole() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let path = storage.toolbox_file("file-1");
        let sha256 = hex(&Sha256::digest(b"hello world"));

        let received = storage
            .receive(&path, chunks(&[b"hello ", b"world"]), 11, Some(&sha256))
            .await
            .unwrap();
        assert_eq!(received.size_bytes, 11);
        assert_eq!(received.sha256, sha256);
        assert_eq!(fs::read(&path).unwrap(), b"hello world");

        let other = storage.toolbox_file("file-2");
        assert!(matches!(
            storage
                .receive(&other, chunks(&[b"hello ", b"world!"]), 11, None)
                .await,
            Err(ReceiveError::TooLarge(11))
        ));
        assert!(matches!(
            storage
                .receive(&other, chunks(&[b"hello moon"]), 11, Some(&sha256))
                .await,
            Err(ReceiveError::ChecksumMismatch)
        ));
        let interrupted = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"hello")),
            Err(io::Error::other("reset")),
        ]);
        assert!(matches!(
            storage.receive(&other, interrupted, 11, None).await,
            Err(ReceiveError::Interrupted)
        ));
        assert_eq!(
            files(&dir.path().join(TOOLBOX_DIR)),
            ["file-1"],
            "failures leave nothing behind"
        );
    }

    #[tokio::test]
    async fn writes_replace_and_removals_ignore_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let path = storage.thumbnail("device-1");
        storage
            .write(path.clone(), Bytes::from_static(b"one"))
            .await
            .unwrap();
        storage
            .write(path.clone(), Bytes::from_static(b"two"))
            .await
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(files(&dir.path().join(THUMBNAILS_DIR)), ["device-1.jpg"]);
        storage.remove(&path).await.unwrap();
        storage.remove(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn old_files_are_listed_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        fs::write(storage.toolbox_file("file-1"), b"whole").unwrap();
        fs::write(partial_path(&storage.toolbox_file("file-2")), b"half").unwrap();
        fs::write(storage.thumbnail("device-1"), b"jpeg").unwrap();
        assert!(
            storage
                .old_toolbox_files(Duration::from_secs(3600))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            storage.old_toolbox_files(Duration::ZERO).unwrap(),
            ["file-1"]
        );
        assert_eq!(
            storage.old_thumbnails(Duration::ZERO).unwrap(),
            ["device-1"]
        );
    }

    #[test]
    fn only_old_partial_files_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let partial = partial_path(&storage.toolbox_file("file-1"));
        fs::write(&partial, b"half").unwrap();
        fs::write(storage.toolbox_file("file-2"), b"whole").unwrap();
        assert_eq!(storage.sweep_partial(Duration::from_secs(3600)).unwrap(), 0);
        assert_eq!(storage.sweep_partial(Duration::ZERO).unwrap(), 1);
        assert_eq!(files(&dir.path().join(TOOLBOX_DIR)), ["file-2"]);
    }
}
