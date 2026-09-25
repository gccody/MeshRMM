//! A log file that rotates itself by size. Several processes may append to the
//! same log (the Agent service, its worker and its update helper; viewers and
//! the viewer's update helper), so each writer checks before every write
//! whether the file it holds is full. The first to find it full renames it
//! aside; the others see a new, smaller file at the path and reopen it.
//!
//! Renames keep a rotated file's permissions, and a new log inherits its
//! folder's, as the first log did.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A log is rotated once it reaches this size.
pub const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated logs kept beside the current one: `agent.1.log` (newest) to `agent.3.log`.
pub const KEEP_ROTATED: usize = 3;
/// How long a writer waits before retrying a rotation that failed, for example
/// because another program holds the log open without sharing it for deletion.
const RETRY_ROTATION_AFTER: Duration = Duration::from_secs(60);

pub struct RotatingFile {
    path: PathBuf,
    file: File,
    limit: u64,
    keep: usize,
    retry_rotation_at: Option<Instant>,
}

impl RotatingFile {
    /// Opens `path` for appending, rotating it first if it is already full.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        Self::with_limits(path.into(), ROTATE_BYTES, KEEP_ROTATED)
    }

    fn with_limits(path: PathBuf, limit: u64, keep: usize) -> io::Result<Self> {
        let mut retry_rotation_at = None;
        if length(&path) >= limit && rotate(&path, keep).is_err() {
            retry_rotation_at = Some(Instant::now() + RETRY_ROTATION_AFTER);
        }
        Ok(Self {
            file: append(&path)?,
            path,
            limit,
            keep,
            retry_rotation_at,
        })
    }

    /// Moves to a new log when the one held is full, rotating it unless
    /// another writer already has. Failures keep the current file, and a write
    /// after [`RETRY_ROTATION_AFTER`] tries again.
    fn rotate_if_full(&mut self) {
        if self
            .retry_rotation_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
            || self
                .file
                .metadata()
                .is_ok_and(|metadata| metadata.len() < self.limit)
        {
            return;
        }
        self.retry_rotation_at = None;
        if length(&self.path) >= self.limit && rotate(&self.path, self.keep).is_err() {
            self.retry_rotation_at = Some(Instant::now() + RETRY_ROTATION_AFTER);
            return;
        }
        if let Ok(file) = append(&self.path) {
            self.file = file;
        }
    }
}

impl Write for RotatingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.rotate_if_full();
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// The size of the file at `path`, or 0 if there is none.
fn length(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

/// `agent.log` → `agent.<number>.log`.
pub fn rotated_path(path: &Path, number: usize) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let name = match path.extension() {
        Some(extension) => format!("{stem}.{number}.{}", extension.to_string_lossy()),
        None => format!("{stem}.{number}"),
    };
    path.with_file_name(name)
}

/// Shifts `path.1` … `path.{keep - 1}` up by one, dropping `path.{keep}`, and
/// moves `path` to `path.1`. Windows can rename a log other processes hold
/// open because the standard library shares files for deletion, but not one a
/// program opened without that sharing. The log is moved aside before anything
/// else, so when it cannot be, the rotated logs stay as they were.
fn rotate(path: &Path, keep: usize) -> io::Result<()> {
    if keep == 0 {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    // A name no other writer uses, so writers rotating at once cannot replace
    // each other's full log.
    static ROTATION: AtomicU64 = AtomicU64::new(0);
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(
        ".{}-{}.rotating",
        std::process::id(),
        ROTATION.fetch_add(1, Ordering::Relaxed)
    ));
    let aside = PathBuf::from(aside);
    match fs::rename(path, &aside) {
        Ok(()) => {}
        // Another writer rotated it first.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let _ = fs::remove_file(rotated_path(path, keep));
    for number in (1..keep).rev() {
        let _ = fs::rename(rotated_path(path, number), rotated_path(path, number + 1));
    }
    fs::rename(&aside, rotated_path(path, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "meshrmm-log-file-{name}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn read(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.0.join(name)).ok()
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn names_rotated_logs_before_the_extension() {
        let path = Path::new("logs").join("agent.log");
        assert_eq!(
            rotated_path(&path, 2),
            Path::new("logs").join("agent.2.log")
        );
        assert_eq!(
            rotated_path(Path::new("remote"), 1),
            PathBuf::from("remote.1")
        );
    }

    #[test]
    fn rotates_at_the_limit_and_keeps_only_the_newest_logs() {
        let sandbox = Sandbox::new("rotate");
        let path = sandbox.0.join("agent.log");
        let mut log = RotatingFile::with_limits(path.clone(), 10, 2).unwrap();
        for record in ["first-rec\n", "second-rec\n", "third-rec\n", "fourth-rec\n"] {
            log.write_all(record.as_bytes()).unwrap();
        }
        // Each record fills the log, so the next one starts a new file.
        assert_eq!(sandbox.read("agent.log").unwrap(), "fourth-rec\n");
        assert_eq!(sandbox.read("agent.1.log").unwrap(), "third-rec\n");
        assert_eq!(sandbox.read("agent.2.log").unwrap(), "second-rec\n");
        assert_eq!(sandbox.read("agent.3.log"), None);
    }

    #[test]
    fn opening_a_full_log_rotates_it_first() {
        let sandbox = Sandbox::new("open");
        let path = sandbox.0.join("remote.log");
        fs::write(&path, "x".repeat(64)).unwrap();
        let mut log = RotatingFile::with_limits(path.clone(), 32, 3).unwrap();
        log.write_all(b"new\n").unwrap();
        assert_eq!(sandbox.read("remote.log").unwrap(), "new\n");
        assert_eq!(sandbox.read("remote.1.log").unwrap().len(), 64);
    }

    #[cfg(windows)]
    #[test]
    fn a_log_held_open_without_delete_sharing_keeps_its_history() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;

        let sandbox = Sandbox::new("locked");
        let path = sandbox.0.join("agent.log");
        fs::write(rotated_path(&path, 1), "one").unwrap();
        fs::write(rotated_path(&path, 2), "two").unwrap();
        let mut log = RotatingFile::with_limits(path.clone(), 8, 2).unwrap();
        log.write_all(b"full log\n").unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_WRITE)
            .open(&path)
            .unwrap();
        log.write_all(b"more\n").unwrap();
        log.write_all(b"again\n").unwrap();
        assert_eq!(sandbox.read("agent.1.log").unwrap(), "one");
        assert_eq!(sandbox.read("agent.2.log").unwrap(), "two");
        assert_eq!(
            sandbox.read("agent.log").unwrap(),
            "full log\nmore\nagain\n"
        );

        // Once the program lets go, the next retry rotates normally.
        drop(reader);
        log.retry_rotation_at = None;
        log.write_all(b"new\n").unwrap();
        assert_eq!(sandbox.read("agent.log").unwrap(), "new\n");
        assert_eq!(
            sandbox.read("agent.1.log").unwrap(),
            "full log\nmore\nagain\n"
        );
        assert_eq!(sandbox.read("agent.2.log").unwrap(), "one");
    }

    #[test]
    fn writers_sharing_a_log_follow_each_others_rotation() {
        let sandbox = Sandbox::new("shared");
        let path = sandbox.0.join("agent.log");
        let mut service = RotatingFile::with_limits(path.clone(), 16, 3).unwrap();
        let mut worker = RotatingFile::with_limits(path.clone(), 16, 3).unwrap();
        service.write_all(b"service one\n").unwrap();
        worker.write_all(b"worker one\n").unwrap();
        // The worker finds the shared log full and rotates it once.
        worker.write_all(b"worker two\n").unwrap();
        // The service's file is full too, but the log at the path is not:
        // it reopens that log instead of rotating it again.
        service.write_all(b"service two\n").unwrap();
        assert_eq!(
            sandbox.read("agent.1.log").unwrap(),
            "service one\nworker one\n"
        );
        assert_eq!(
            sandbox.read("agent.log").unwrap(),
            "worker two\nservice two\n"
        );
        assert_eq!(sandbox.read("agent.2.log"), None);
    }
}
