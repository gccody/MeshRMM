//! Downloads an Agent update straight into the file it is staged as, on every platform.
//!
//! A build is about 30 MB, so it is never held whole: each piece the server sends passes through
//! one fixed buffer of [`BUFFER_BYTES`], is hashed, and is written, and the download stops as soon
//! as it is larger than its limit. The file is created new, so nothing else can already have it
//! open, where only the Agent, running as SYSTEM or root, can write: the private update directory
//! on Windows and macOS, the root-owned install directory on Linux. So the bytes hashed are the
//! bytes on disk, and the platform updater gets the path only from [`Verified::keep`], after the
//! SHA-256 the release key signed has been checked over all of them.
//!
//! A download that is cut short, is too large, fills the disk or does not match is removed. One
//! cut short by a crash or a power cut has the name of a staged update, so the sweep of what
//! earlier updates left removes it when the Agent next starts (and, on macOS and Linux, before
//! the next download). Nothing is resumed: the next check downloads the build again.
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use meshrmm_self_update::{Release, ReleaseDigest};

/// What a download holds in memory at once, besides the HTTP client's own buffers.
pub(crate) const BUFFER_BYTES: usize = 64 * 1024;

/// Downloads `release` from `body` into a new file at `path`, which must not exist, failing once
/// more than `maximum` bytes arrive. `complete` runs once the whole body has arrived, before it is
/// checked; then the file is flushed to disk and checked against the release's SHA-256. On any
/// error the file is removed.
pub(crate) fn download(
    body: impl Read,
    path: &Path,
    maximum: u64,
    release: &Release,
    complete: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<Verified> {
    let mut download = Download::create(path, release)?;
    download.receive(body, maximum)?;
    complete()?;
    download.verify()
}

/// A downloaded update that matches its release, removed when dropped unless it is kept.
pub(crate) struct Verified {
    // Before `removal`, so the file is closed before it is removed.
    file: File,
    removal: Removal,
}

impl Verified {
    /// The file, through the handle it was written with.
    #[cfg(any(windows, target_os = "linux"))]
    pub(crate) fn file(&self) -> &File {
        &self.file
    }

    /// Closes the file and keeps it, for the platform updater; returns its path.
    pub(crate) fn keep(self) -> PathBuf {
        let Self { file, removal } = self;
        drop(file);
        removal.disarm()
    }
}

/// A download in progress, removed when dropped.
struct Download {
    // Before `removal`, so the file is closed before it is removed.
    file: File,
    digest: ReleaseDigest,
    removal: Removal,
}

impl Download {
    fn create(path: &Path, release: &Release) -> anyhow::Result<Self> {
        let file = create_new(path)
            .with_context(|| format!("failed to create the Agent update {}", path.display()))?;
        Ok(Self {
            file,
            digest: release.digest(),
            removal: Removal(Some(path.to_owned())),
        })
    }

    fn receive(&mut self, body: impl Read, maximum: u64) -> anyhow::Result<()> {
        let path = self.removal.path();
        let received =
            copy(body, &mut self.file, &mut self.digest, maximum).with_context(|| {
                format!("failed to download the Agent update to {}", path.display())
            })?;
        tracing::info!(bytes = received, path = %path.display(), "downloaded the Agent update");
        Ok(())
    }

    fn verify(self) -> anyhow::Result<Verified> {
        let Self {
            file,
            digest,
            removal,
        } = self;
        let path = removal.path().to_owned();
        let checked = file
            .sync_all()
            .map_err(|error| disk_error(error, "flush"))
            .with_context(|| format!("failed to write the Agent update {}", path.display()))
            .and_then(|()| digest.verify());
        if let Err(error) = checked {
            drop(file);
            drop(removal);
            return Err(error);
        }
        Ok(Verified { file, removal })
    }

    /// Leaves the file as a crash or a power cut would.
    #[cfg(test)]
    fn crash(self) {
        let Self { file, removal, .. } = self;
        drop(file);
        removal.disarm();
    }
}

/// Copies `body` to `file` through one buffer of [`BUFFER_BYTES`], adding each piece to `digest`,
/// and returns how many bytes it copied. It writes no more than `maximum` bytes: a body larger than
/// that fails as soon as the piece that crosses the limit arrives.
fn copy(
    mut body: impl Read,
    file: &mut impl Write,
    digest: &mut ReleaseDigest,
    maximum: u64,
) -> anyhow::Result<u64> {
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut received = 0_u64;
    loop {
        let read = match body.read(&mut buffer) {
            Ok(0) => return Ok(received),
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("the download was cut short after {received} bytes"));
            }
        };
        if read as u64 > maximum - received {
            bail!("the download is larger than its {maximum}-byte limit");
        }
        let piece = &buffer[..read];
        digest.update(piece);
        file.write_all(piece)
            .map_err(|error| disk_error(error, "write"))
            .with_context(|| format!("could not write the download after {received} bytes"))?;
        received += read as u64;
    }
}

/// `error`, reported plainly when it is a full disk, which is the error a device that is short of
/// space most often meets.
fn disk_error(error: std::io::Error, what: &str) -> anyhow::Error {
    if matches!(
        error.kind(),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded
    ) {
        tracing::error!(
            %error,
            "the disk is full, so the Agent update could not be downloaded; removed what arrived of it"
        );
        anyhow::Error::new(error)
            .context(format!("the disk is full: could not {what} the download"))
    } else {
        error.into()
    }
}

/// Removes its file when dropped, unless disarmed.
struct Removal(Option<PathBuf>);

impl Removal {
    fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    fn disarm(mut self) -> PathBuf {
        self.0.take().unwrap_or_default()
    }
}

impl Drop for Removal {
    fn drop(&mut self) {
        let Some(path) = self.0.take() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "removed the failed Agent update download")
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove the failed Agent update download; the next start removes it"
            ),
        }
    }
}

/// Creates `path` for reading and writing, failing if anything, a link included, is already there.
/// Only the Agent can then open it: on macOS and Linux it is the service's own, mode 0600, and on
/// Windows it takes the private update directory's DACL and, while this handle is open, no other
/// handle may write it, rename it or delete it.
fn create_new(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0);
    }
    options.open(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::cell::Cell;

    /// A temporary staging folder, removed when dropped.
    pub(crate) struct Staging(pub(crate) PathBuf);

    impl Staging {
        pub(crate) fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "meshrmm-update-download-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub(crate) fn names(&self) -> Vec<String> {
            let mut names = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    impl Drop for Staging {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The `index`th byte of a test build, so a large one need not be held to be served.
    fn byte_at(index: u64) -> u8 {
        (index.wrapping_mul(2_654_435_761) >> 13) as u8
    }

    /// An unsigned release of a test build of `length` bytes, which is all its download checks.
    pub(crate) fn release_of(length: u64) -> Release {
        let mut hasher = Sha256::new();
        let mut index = 0;
        let mut piece = vec![0; BUFFER_BYTES];
        while index < length {
            let count = piece.len().min((length - index) as usize);
            for (offset, byte) in piece[..count].iter_mut().enumerate() {
                *byte = byte_at(index + offset as u64);
            }
            hasher.update(&piece[..count]);
            index += count as u64;
        }
        Release {
            version: "1.2.3".to_owned(),
            url: "https://downloads.example.com/agent".to_owned(),
            sha256: format!("{:x}", hasher.finalize()),
            signature: None,
            developer_id: None,
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    pub(crate) fn release_of_bytes(bytes: &[u8]) -> Release {
        Release {
            sha256: format!("{:x}", Sha256::digest(bytes)),
            ..release_of(0)
        }
    }

    /// Serves a test build of `length` bytes (`u64::MAX` for one that never ends), then fails
    /// with `then` instead of ending if it is set. It refuses a read of more than `largest`
    /// bytes, and checks before serving each piece that `written` holds all but the last
    /// [`BUFFER_BYTES`] it served already, so a downloader that held the body whole would fail.
    struct Body<'a> {
        length: u64,
        served: &'a Cell<u64>,
        then: Option<ErrorKind>,
        largest: usize,
        written: Option<&'a Path>,
    }

    impl<'a> Body<'a> {
        fn new(length: u64, served: &'a Cell<u64>) -> Self {
            Self {
                length,
                served,
                then: None,
                largest: usize::MAX,
                written: None,
            }
        }
    }

    impl Read for Body<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if buffer.len() > self.largest {
                return Err(std::io::Error::other(format!(
                    "asked for {} bytes at once",
                    buffer.len()
                )));
            }
            let served = self.served.get();
            if let Some(written) = self.written {
                let on_disk = std::fs::metadata(written).map_or(0, |metadata| metadata.len());
                if on_disk + (BUFFER_BYTES as u64) < served {
                    return Err(std::io::Error::other(format!(
                        "{served} bytes served but only {on_disk} written"
                    )));
                }
            }
            let left = self.length - served;
            if left == 0 {
                return match self.then {
                    Some(kind) => Err(kind.into()),
                    None => Ok(0),
                };
            }
            let count = buffer
                .len()
                .min(usize::try_from(left).unwrap_or(usize::MAX));
            for (offset, byte) in buffer[..count].iter_mut().enumerate() {
                *byte = byte_at(served + offset as u64);
            }
            self.served.set(served + count as u64);
            Ok(count)
        }
    }

    fn contents_match(path: &Path, length: u64) -> bool {
        let contents = std::fs::read(path).unwrap();
        contents.len() as u64 == length
            && contents
                .iter()
                .enumerate()
                .all(|(index, &byte)| byte == byte_at(index as u64))
    }

    #[test]
    fn a_good_download_ends_as_one_verified_file_only_the_agent_can_write() {
        let staging = Staging::new();
        let path = staging.0.join("agent-1.2.3");
        let served = Cell::new(0);
        let length = 300_000;
        let mut completed = false;
        let verified = download(
            Body::new(length, &served),
            &path,
            length,
            &release_of(length),
            || {
                completed = true;
                Ok(())
            },
        )
        .unwrap();
        assert!(completed);
        assert_eq!(staging.names(), ["agent-1.2.3"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        #[cfg(windows)]
        {
            // No other handle may change it while the Agent holds it.
            let error = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(32), "{error}"); // ERROR_SHARING_VIOLATION
        }
        assert_eq!(verified.keep(), path);
        assert!(contents_match(&path, length));
        assert_eq!(staging.names(), ["agent-1.2.3"]);
    }

    #[test]
    fn a_download_larger_than_its_limit_stops_at_the_limit_and_leaves_no_file() {
        let staging = Staging::new();
        let path = staging.0.join("agent-1.2.3");
        let served = Cell::new(0);
        let maximum = 1_000_000;
        let mut body = Body::new(u64::MAX, &served);
        // Nothing past the limit is written.
        body.written = Some(&path);
        let error = download(body, &path, maximum, &release_of(maximum), || {
            panic!("an oversized download is not complete")
        })
        .err()
        .unwrap();
        assert!(format!("{error:#}").contains("limit"), "{error:#}");
        assert!(
            served.get() <= maximum + BUFFER_BYTES as u64,
            "read {} bytes",
            served.get()
        );
        assert!(staging.names().is_empty(), "{:?}", staging.names());
    }

    #[test]
    fn a_download_that_does_not_match_is_refused_and_leaves_no_file() {
        let staging = Staging::new();
        let path = staging.0.join("agent-1.2.3");
        let served = Cell::new(0);
        let length = 200_000;
        // The server sends the whole of a build, but not the one the release key signed.
        let mut completed = false;
        let error = download(
            Body::new(length, &served),
            &path,
            length,
            &release_of(length - 1),
            || {
                completed = true;
                Ok(())
            },
        )
        .err()
        .unwrap();
        assert!(completed, "a whole download uses an attempt");
        assert!(format!("{error:#}").contains("SHA-256"), "{error:#}");
        assert!(staging.names().is_empty(), "{:?}", staging.names());
    }

    #[test]
    fn a_download_cut_short_leaves_no_file_and_uses_no_attempt() {
        let staging = Staging::new();
        let path = staging.0.join("agent-1.2.3");
        let length = 500_000;
        for then in [Some(ErrorKind::ConnectionReset), Some(ErrorKind::TimedOut)] {
            let served = Cell::new(0);
            let mut body = Body::new(length / 2, &served);
            body.then = then;
            let error = download(body, &path, length, &release_of(length), || {
                panic!("a download cut short is not complete")
            })
            .err()
            .unwrap();
            assert!(format!("{error:#}").contains("cut short"), "{error:#}");
            assert!(staging.names().is_empty(), "{:?}", staging.names());
        }
        // A server that ends the body early sends a build that does not match.
        let served = Cell::new(0);
        assert!(
            download(
                Body::new(length / 2, &served),
                &path,
                length,
                &release_of(length),
                || Ok(()),
            )
            .is_err()
        );
        assert!(staging.names().is_empty(), "{:?}", staging.names());
    }

    #[test]
    fn a_download_is_never_held_whole() {
        let staging = Staging::new();
        let path = staging.0.join("agent-1.2.3");
        let served = Cell::new(0);
        // Far larger than the buffer, and served only as fast as it reaches the disk.
        let length = 8 * 1024 * 1024 + 123;
        let mut body = Body::new(length, &served);
        body.largest = BUFFER_BYTES;
        body.written = Some(&path);
        let verified = download(body, &path, length + 1, &release_of(length), || Ok(())).unwrap();
        let path = verified.keep();
        assert_eq!(served.get(), length);
        assert!(contents_match(&path, length));
    }

    #[test]
    fn a_full_disk_fails_plainly() {
        struct FullDisk(u64);
        impl Write for FullDisk {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                if self.0 == 0 {
                    return Err(ErrorKind::StorageFull.into());
                }
                let count = buffer.len().min(self.0 as usize);
                self.0 -= count as u64;
                Ok(count)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let served = Cell::new(0);
        let release = release_of(1_000_000);
        let error = copy(
            Body::new(1_000_000, &served),
            &mut FullDisk(100_000),
            &mut release.digest(),
            1_000_000,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("the disk is full"),
            "{error:#}"
        );
        // It stopped there rather than reading the rest.
        assert!(served.get() <= 100_000 + BUFFER_BYTES as u64);
    }

    #[test]
    fn refuses_a_file_already_at_the_path() {
        let staging = Staging::new();
        let path = staging.0.join("agent-1.2.3");
        std::fs::write(&path, b"planted").unwrap();
        let served = Cell::new(0);
        assert!(
            download(
                Body::new(10, &served),
                &path,
                10,
                &release_of(10),
                || Ok(())
            )
            .is_err()
        );
        assert_eq!(served.get(), 0);
        // And leaves what it did not create.
        assert_eq!(std::fs::read(&path).unwrap(), b"planted");
    }

    /// Leaves the partial download `name` in `staging`, as a crash or a power cut would.
    pub(crate) fn crash_during_download(staging: &Path, name: &str) -> PathBuf {
        let path = staging.join(name);
        let mut download = Download::create(&path, &release_of(10)).unwrap();
        download.file.write_all(b"partial").unwrap();
        download.crash();
        assert!(path.is_file());
        path
    }
}
