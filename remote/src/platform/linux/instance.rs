//! One Linux viewer per device. A dashboard link for a device that already
//! has a viewer asks that viewer to end its session and waits for it to exit.
//! Otherwise the new handoff is redeemed while the old lease is still active,
//! and the server refuses it for up to 15 minutes.
//!
//! The viewer for a device holds a lock on a file named for it, which the
//! kernel releases however the viewer ends, and listens on a socket beside
//! it; a connection to the socket is the request to end.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;

/// How often a viewer waiting to take over repeats its request.
const RESIGNAL_INTERVAL: Duration = Duration::from_millis(250);

/// Holds this device's viewer lock until dropped or the process exits.
pub struct InstanceGuard {
    _lock: File,
    socket: PathBuf,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // The listener's thread ends with the process; a later viewer
        // replaces the socket.
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Where the user's viewers keep their locks: the desktop session's own
/// folder, which only the user can read.
fn directory() -> anyhow::Result<PathBuf> {
    let directory = match std::env::var_os("XDG_RUNTIME_DIR").filter(|path| !path.is_empty()) {
        Some(runtime) => PathBuf::from(runtime).join("meshrmm-remote"),
        // Safety: geteuid has no preconditions.
        None => std::env::temp_dir().join(format!("meshrmm-remote-{}", unsafe { libc::geteuid() })),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    Ok(directory)
}

/// A file name for `device_id`, which comes from a link.
fn file_stem(device_id: &str) -> String {
    let name: String = device_id
        .chars()
        .take(64)
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!("viewer-{name}")
}

/// Takes the lock on `file` if no one holds it.
fn try_lock(file: &File) -> std::io::Result<bool> {
    // Safety: the descriptor is open for as long as `file` is borrowed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Becomes the viewer for `device_id`. An existing viewer for the device is
/// asked to end its session, and this waits up to `timeout` for it to exit.
/// `on_replaced` runs, once, when a later viewer asks this one to end.
pub fn claim(
    device_id: &str,
    timeout: Duration,
    on_replaced: impl FnOnce() + Send + 'static,
) -> anyhow::Result<InstanceGuard> {
    claim_in(&directory()?, device_id, timeout, on_replaced)
}

fn claim_in(
    directory: &Path,
    device_id: &str,
    timeout: Duration,
    on_replaced: impl FnOnce() + Send + 'static,
) -> anyhow::Result<InstanceGuard> {
    let stem = file_stem(device_id);
    let socket = directory.join(format!("{stem}.sock"));
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(directory.join(format!("{stem}.lock")))
        .context("could not open the viewer instance lock")?;
    if !try_lock(&lock).context("could not take the viewer instance lock")? {
        tracing::info!(
            device_id,
            "asking the open viewer for this device to end its session"
        );
        crate::launch_status::report(crate::launch_status::LaunchStatus::ClosingPreviousViewer);
        let deadline = Instant::now() + timeout;
        loop {
            // Asked again each time: a viewer that has only just taken the
            // lock is not listening yet.
            let _ = UnixStream::connect(&socket);
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(RESIGNAL_INTERVAL),
            );
            if try_lock(&lock).context("could not take the viewer instance lock")? {
                break;
            }
            if Instant::now() >= deadline {
                return Err(crate::errors::UserFacing(format!(
                    "The remote viewer that is already open for this device did not close within {} seconds. Close it and try again.",
                    timeout.as_secs()
                ))
                .into());
            }
        }
    }
    // The socket of the viewer that had the lock before is no one's now.
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).context("could not listen for a replacing viewer")?;
    std::thread::Builder::new()
        .name("viewer-replacement".into())
        .spawn(move || {
            if listener.accept().is_ok() {
                on_replaced();
            }
        })
        .context("could not start the viewer replacement listener")?;
    Ok(InstanceGuard {
        _lock: lock,
        socket,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn scratch(test: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("meshrmm-instance-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn a_device_id_becomes_a_plain_file_name() {
        assert_eq!(
            file_stem("0190a3c2-7d5e-7c1a-b2f4-0123456789ab"),
            "viewer-0190a3c2-7d5e-7c1a-b2f4-0123456789ab"
        );
        assert_eq!(file_stem("../../etc/passwd"), "viewer-______etc_passwd");
        assert_eq!(file_stem(&"a".repeat(200)).len(), "viewer-".len() + 64);
    }

    #[test]
    fn a_second_viewer_asks_the_first_to_end_and_takes_over_once_it_has() {
        let directory = scratch("replace");
        let replaced = Arc::new(AtomicBool::new(false));
        let first = claim_in(&directory, "device", Duration::from_secs(5), {
            let replaced = Arc::clone(&replaced);
            move || replaced.store(true, Ordering::SeqCst)
        })
        .unwrap();
        let second = std::thread::spawn({
            let directory = directory.clone();
            move || claim_in(&directory, "device", Duration::from_secs(10), || {})
        });
        let asked = Instant::now();
        while !replaced.load(Ordering::SeqCst) {
            assert!(asked.elapsed() < Duration::from_secs(5), "never asked");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!second.is_finished());
        drop(first);
        let second = second.join().unwrap().unwrap();
        // Another device's viewer is not in the way.
        let other = claim_in(&directory, "other", Duration::from_secs(1), || {}).unwrap();
        drop((second, other));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn a_viewer_that_stays_open_is_reported_after_the_timeout() {
        let directory = scratch("timeout");
        let _first = claim_in(&directory, "device", Duration::from_secs(5), || {}).unwrap();
        let error = claim_in(&directory, "device", Duration::from_millis(300), || {})
            .err()
            .expect("the first viewer still holds the lock");
        assert!(
            error.to_string().contains("did not close within"),
            "{error:#}"
        );
        let _ = std::fs::remove_dir_all(directory);
    }
}
