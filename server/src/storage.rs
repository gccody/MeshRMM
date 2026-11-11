//! Files the server keeps in its data directory: each device's latest screen
//! thumbnail and the toolbox library.
//!
//! A file appears under its final name only once it is complete: it is
//! written to a hidden `.part` file in the same directory, flushed to disk
//! when it must survive a power cut (see [`Durability`]), then renamed into
//! place. Callers name files by IDs the server generated and validated, never
//! by user input.
//!
//! Thumbnails are spread over 256 folders named by the first two hex digits
//! of the device ID, so 100,000 devices make folders of about 400 files
//! rather than one folder of 100,000.
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
const REPORTS_DIR: &str = "reports";
const PARTIAL_SUFFIX: &str = ".part";

#[derive(Debug, Clone)]
pub struct Storage {
    thumbnails: PathBuf,
    toolbox: PathBuf,
    reports: PathBuf,
    /// How many files were flushed to disk, so tests can tell which writes
    /// asked for it.
    #[cfg(test)]
    syncs: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Whether a stored file must survive a power cut. Either way it appears
/// whole or not at all when only the server stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Flushed to disk before it is renamed into place. For files nothing
    /// would put back, like the toolbox library's.
    Durable,
    /// Renamed into place without waiting for the disk. A power cut may lose
    /// the file, or leave the one it replaced or an empty one under its
    /// name. For screen thumbnails, which arrive hundreds of times a second
    /// on a large fleet and which each Agent sends again within the hour.
    Replaceable,
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
            reports: data_dir.join(REPORTS_DIR),
            #[cfg(test)]
            syncs: Default::default(),
        };
        for dir in [&storage.thumbnails, &storage.toolbox, &storage.reports] {
            create_private_dir(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(storage)
    }

    /// Where the device's thumbnail is kept. The first write creates its
    /// folder.
    pub fn thumbnail(&self, device_id: &str) -> PathBuf {
        self.thumbnails
            .join(thumbnail_shard(device_id))
            .join(format!("{device_id}.jpg"))
    }

    pub fn toolbox_file(&self, file_id: &str) -> PathBuf {
        self.toolbox.join(file_id)
    }

    /// Where a generated report's document is kept.
    pub fn report(&self, report_id: &str) -> PathBuf {
        self.reports.join(format!("{report_id}.json"))
    }

    /// Replaces `path` with `contents` in one step.
    pub async fn write(
        &self,
        path: PathBuf,
        contents: Bytes,
        durability: Durability,
    ) -> io::Result<()> {
        let sync = self.must_sync(durability);
        tokio::task::spawn_blocking(move || {
            let partial = partial_path(&path);
            let written = (|| {
                let mut file = create_private(&partial)?;
                io::Write::write_all(&mut file, &contents)?;
                if sync {
                    file.sync_all()?;
                }
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
        durability: Durability,
    ) -> Result<Received, ReceiveError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin,
    {
        let partial = partial_path(path);
        let sync = self.must_sync(durability);
        let received = receive_into(&partial, path, body, max_bytes, expected_sha256, sync).await;
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

    /// The reports kept, by report ID, whatever their age.
    pub fn report_ids(&self) -> io::Result<Vec<String>> {
        old_files(&self.reports, Duration::ZERO, ".json")
    }

    /// Thumbnails last changed before `age` ago, by device ID.
    pub fn old_thumbnails(&self, age: Duration) -> io::Result<Vec<String>> {
        let mut ids = Vec::new();
        for shard in self.thumbnail_shards()? {
            ids.extend(old_files(&shard, age, ".jpg")?);
        }
        Ok(ids)
    }

    /// Deletes partial files older than `age`, left by a server that stopped
    /// mid-write. Returns how many it deleted.
    pub fn sweep_partial(&self, age: Duration) -> io::Result<u64> {
        let cutoff = SystemTime::now()
            .checked_sub(age)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut removed = 0;
        let mut dirs = self.thumbnail_shards()?;
        dirs.push(self.toolbox.clone());
        dirs.push(self.reports.clone());
        for dir in dirs {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(PARTIAL_SUFFIX)
                {
                    continue;
                }
                // A write may rename its partial file into place between
                // listing it and looking at it.
                let stale = modified(&entry)?.is_some_and(|modified| modified < cutoff);
                if stale && fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// The thumbnail folders that exist.
    fn thumbnail_shards(&self) -> io::Result<Vec<PathBuf>> {
        let mut shards = Vec::new();
        for entry in fs::read_dir(&self.thumbnails)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                shards.push(entry.path());
            }
        }
        Ok(shards)
    }

    /// Whether a write with `durability` flushes its file to disk.
    fn must_sync(&self, durability: Durability) -> bool {
        let sync = durability == Durability::Durable;
        #[cfg(test)]
        if sync {
            self.syncs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        sync
    }
}

/// A short token that changes whenever the file is replaced, for an ETag.
/// It is read from the file's metadata alone, so checking it costs one
/// `stat` and no read.
///
/// It covers the modification time to the nanosecond, the size and, on Unix,
/// the inode. Every write renames a new file into place, created while the
/// one it replaces still holds its inode, so two writes in a row differ in
/// inode even within one tick of the file system's clock and at the same
/// size. Only a file at least two writes later could match an older one,
/// and only if the older one's inode number was reused and all of the
/// writes landed within one tick.
pub fn version(metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(metadata);
    #[cfg(not(unix))]
    let inode = 0u64;
    let described = format!("{modified}:{}:{inode}", metadata.len());
    hex(&Sha256::digest(described.as_bytes())[..16])
}

/// The folder of a device's thumbnail: the first two digits of its ID. IDs
/// are random UUIDs in lowercase hex, so devices spread evenly over 256
/// folders with safe names. An ID that doesn't start with two hex digits,
/// which the server never makes, goes to `00`.
fn thumbnail_shard(device_id: &str) -> &str {
    device_id
        .get(..2)
        .filter(|prefix| prefix.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or("00")
}

/// When the entry was last changed, or `None` if it is already gone.
fn modified(entry: &fs::DirEntry) -> io::Result<Option<SystemTime>> {
    match entry.metadata().and_then(|metadata| metadata.modified()) {
        Ok(modified) => Ok(Some(modified)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
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
        // A file removed between listing it and looking at it, as with its
        // device, is not old.
        if name.starts_with('.') || modified(&entry)?.is_none_or(|modified| modified >= cutoff) {
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
    sync: bool,
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
    if sync {
        file.sync_all().await?;
    }
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

/// Creates a file only the server's user can read, and its folder if that is
/// missing, as a thumbnail's is on its first write.
fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    match options.open(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                create_private_dir(parent)?;
            }
            options.open(path)
        }
        opened => opened,
    }
}

/// Creates `dir` and its missing parents, readable by the server's user only.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    // In different thumbnail folders.
    const DEVICE_1: &str = "0a1b2c3d-4e5f-4a6b-8c7d-0123456789ab";
    const DEVICE_2: &str = "f00dbabe-4e5f-4a6b-8c7d-0123456789ab";

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

    fn syncs(storage: &Storage) -> u64 {
        storage.syncs.load(Ordering::Relaxed)
    }

    async fn write_thumbnail(storage: &Storage, device_id: &str, contents: &'static [u8]) {
        storage
            .write(
                storage.thumbnail(device_id),
                Bytes::from_static(contents),
                Durability::Replaceable,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn received_files_are_checked_and_appear_whole() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let path = storage.toolbox_file("file-1");
        let sha256 = hex(&Sha256::digest(b"hello world"));

        let received = storage
            .receive(
                &path,
                chunks(&[b"hello ", b"world"]),
                11,
                Some(&sha256),
                Durability::Durable,
            )
            .await
            .unwrap();
        assert_eq!(received.size_bytes, 11);
        assert_eq!(received.sha256, sha256);
        assert_eq!(fs::read(&path).unwrap(), b"hello world");

        let other = storage.toolbox_file("file-2");
        assert!(matches!(
            storage
                .receive(
                    &other,
                    chunks(&[b"hello ", b"world!"]),
                    11,
                    None,
                    Durability::Durable
                )
                .await,
            Err(ReceiveError::TooLarge(11))
        ));
        assert!(matches!(
            storage
                .receive(
                    &other,
                    chunks(&[b"hello moon"]),
                    11,
                    Some(&sha256),
                    Durability::Durable
                )
                .await,
            Err(ReceiveError::ChecksumMismatch)
        ));
        let interrupted = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"hello")),
            Err(io::Error::other("reset")),
        ]);
        assert!(matches!(
            storage
                .receive(&other, interrupted, 11, None, Durability::Durable)
                .await,
            Err(ReceiveError::Interrupted)
        ));
        assert_eq!(
            files(&dir.path().join(TOOLBOX_DIR)),
            ["file-1"],
            "failures leave nothing behind"
        );
    }

    #[tokio::test]
    async fn only_durable_files_are_flushed_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        write_thumbnail(&storage, DEVICE_1, b"jpeg").await;
        storage
            .receive(
                &storage.toolbox_file("file-1"),
                chunks(&[b"tool"]),
                4,
                None,
                Durability::Replaceable,
            )
            .await
            .unwrap();
        assert_eq!(syncs(&storage), 0);

        storage
            .receive(
                &storage.toolbox_file("file-2"),
                chunks(&[b"tool"]),
                4,
                None,
                Durability::Durable,
            )
            .await
            .unwrap();
        storage
            .write(
                storage.toolbox_file("file-3"),
                Bytes::from_static(b"tool"),
                Durability::Durable,
            )
            .await
            .unwrap();
        assert_eq!(syncs(&storage), 2);
        assert_eq!(
            files(&dir.path().join(TOOLBOX_DIR)),
            ["file-1", "file-2", "file-3"]
        );
        assert_eq!(fs::read(storage.thumbnail(DEVICE_1)).unwrap(), b"jpeg");
    }

    #[tokio::test]
    async fn thumbnails_are_kept_in_folders_by_device_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let thumbnails = dir.path().join(THUMBNAILS_DIR);
        let path = storage.thumbnail(DEVICE_1);
        assert_eq!(path, thumbnails.join("0a").join(format!("{DEVICE_1}.jpg")));

        write_thumbnail(&storage, DEVICE_1, b"one").await;
        write_thumbnail(&storage, DEVICE_1, b"two").await;
        write_thumbnail(&storage, DEVICE_2, b"other").await;
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(files(&thumbnails), ["0a", "f0"]);
        assert_eq!(files(&thumbnails.join("0a")), [format!("{DEVICE_1}.jpg")]);
        assert_eq!(files(&thumbnails.join("f0")), [format!("{DEVICE_2}.jpg")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&thumbnails), 0o700);
            assert_eq!(mode(&thumbnails.join("0a")), 0o700);
            assert_eq!(mode(&path), 0o600);
        }

        storage.remove(&path).await.unwrap();
        storage.remove(&path).await.unwrap();
        assert!(!path.exists());
        assert!(storage.thumbnail(DEVICE_2).exists());
    }

    #[test]
    fn thumbnail_folders_are_two_hex_digits() {
        assert_eq!(thumbnail_shard(DEVICE_1), "0a");
        assert_eq!(thumbnail_shard(DEVICE_2), "f0");
        for unexpected in ["", "a", "../x", "zz-not-an-id", "é-id"] {
            assert_eq!(thumbnail_shard(unexpected), "00", "{unexpected}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn every_write_changes_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let path = storage.thumbnail(DEVICE_1);
        let current = || version(&fs::metadata(&path).unwrap());

        write_thumbnail(&storage, DEVICE_1, b"one").await;
        let first = current();
        assert_eq!(current(), first, "looking does not change it");
        write_thumbnail(&storage, DEVICE_1, b"two").await;
        let second = current();
        assert_ne!(second, first, "a new image of the same size");
        write_thumbnail(&storage, DEVICE_1, b"two").await;
        assert_ne!(current(), second, "the same image written again");
    }

    #[tokio::test]
    async fn old_files_are_listed_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        fs::write(storage.toolbox_file("file-1"), b"whole").unwrap();
        fs::write(partial_path(&storage.toolbox_file("file-2")), b"half").unwrap();
        write_thumbnail(&storage, DEVICE_1, b"jpeg").await;
        write_thumbnail(&storage, DEVICE_2, b"jpeg").await;
        fs::write(partial_path(&storage.thumbnail(DEVICE_2)), b"half").unwrap();
        assert!(
            storage
                .old_toolbox_files(Duration::from_secs(3600))
                .unwrap()
                .is_empty()
        );
        assert!(
            storage
                .old_thumbnails(Duration::from_secs(3600))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            storage.old_toolbox_files(Duration::ZERO).unwrap(),
            ["file-1"]
        );
        let mut thumbnails = storage.old_thumbnails(Duration::ZERO).unwrap();
        thumbnails.sort();
        assert_eq!(thumbnails, [DEVICE_1, DEVICE_2]);
    }

    #[tokio::test]
    async fn only_old_partial_files_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        fs::write(partial_path(&storage.toolbox_file("file-1")), b"half").unwrap();
        fs::write(storage.toolbox_file("file-2"), b"whole").unwrap();
        for device_id in [DEVICE_1, DEVICE_2] {
            write_thumbnail(&storage, device_id, b"jpeg").await;
            fs::write(partial_path(&storage.thumbnail(device_id)), b"half").unwrap();
        }
        assert_eq!(storage.sweep_partial(Duration::from_secs(3600)).unwrap(), 0);
        assert_eq!(storage.sweep_partial(Duration::ZERO).unwrap(), 3);
        assert_eq!(files(&dir.path().join(TOOLBOX_DIR)), ["file-2"]);
        let thumbnails = dir.path().join(THUMBNAILS_DIR);
        assert_eq!(files(&thumbnails.join("0a")), [format!("{DEVICE_1}.jpg")]);
        assert_eq!(files(&thumbnails.join("f0")), [format!("{DEVICE_2}.jpg")]);
    }
}
