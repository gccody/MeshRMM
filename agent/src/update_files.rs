//! The files an Agent update stages on macOS and Linux, the swap that installs them, and the
//! lock that tells whether an installer may still need them.
//!
//! The coordinator takes the [`InstallLock`] before it stages a release and holds it until it
//! exits; the installer it starts queues for the lock at once and holds it to the end, through
//! the wait for the new build to connect and any rollback. So whoever finds the lock free knows
//! that no installer can still need the staged files or the previous build, and removes what
//! earlier updates left: one cut short when the Agent restarted while it waited for running
//! work, the computer restarted, or the installer was killed.
//!
//! The swap is one atomic exchange of the staged build with the installed one, which leaves the
//! previous build where the staged one was, and the rollback is the same exchange again. The
//! installed path holds a complete build at every moment, so an installer that dies at any point
//! leaves an Agent that starts, and only leftovers to remove.
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context;

/// In the private update directory: the file the [`InstallLock`] locks.
const INSTALL_LOCK_FILE: &str = "install.lock";
/// How often the installer checks whether the coordinator has exited.
const COORDINATOR_POLL: Duration = Duration::from_millis(250);

/// Held by the coordinator from staging a release until it exits, and by that release's
/// installer from its start to its end. The operating system releases it when its holder exits
/// or dies.
pub(crate) struct InstallLock {
    _file: File,
}

impl InstallLock {
    fn open(updates: &Path) -> anyhow::Result<File> {
        let path = updates.join(INSTALL_LOCK_FILE);
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))
    }

    /// Takes the lock in the update directory `updates`, or returns `None` while another
    /// process, or another open of it in this one, holds it.
    pub(crate) fn try_take(updates: &Path) -> anyhow::Result<Option<Self>> {
        let file = Self::open(updates)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).context("could not lock the Agent update directory")
            }
        }
    }

    /// Starts waiting for the lock on a thread of its own. The installer queues before the
    /// coordinator exits, so the operating system hands it the lock the coordinator releases
    /// before a coordinator the service manager restarts meanwhile can take it.
    pub(crate) fn queue(updates: &Path) -> anyhow::Result<QueuedLock> {
        let file = Self::open(updates)?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(file.lock().map(|()| Self { _file: file }));
        });
        Ok(QueuedLock(receiver))
    }

    /// Keeps the lock until this process exits.
    pub(crate) fn hold_until_exit(self) {
        std::mem::forget(self);
    }
}

/// A wait for the [`InstallLock`] started by [`InstallLock::queue`].
pub(crate) struct QueuedLock(mpsc::Receiver<std::io::Result<InstallLock>>);

impl QueuedLock {
    /// The lock, once it is free, or an error if it is not free within `timeout`.
    pub(crate) fn wait(&self, timeout: Duration) -> anyhow::Result<InstallLock> {
        match self.0.recv_timeout(timeout) {
            Ok(lock) => lock.context("could not lock the Agent update directory"),
            Err(_) => anyhow::bail!("another Agent update is still being installed"),
        }
    }
}

/// Removes the entries of `directory` whose names start with `prefix`, files and folders alike,
/// and returns how many it removed. A symbolic link is removed, not followed.
pub(crate) fn remove_leftovers(directory: &Path, prefix: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(prefix))
        {
            continue;
        }
        let path = entry.path();
        let result = match entry.file_type() {
            Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        match result {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not remove what an earlier Agent update left"
            ),
        }
    }
    removed
}

/// Exchanges what `first` and `second` name, files or folders, in one step: either both have
/// moved or, on an error, neither. Both must exist, on one file system.
pub(crate) fn exchange(first: &Path, second: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let first = std::ffi::CString::new(first.as_os_str().as_bytes())?;
    let second = std::ffi::CString::new(second.as_os_str().as_bytes())?;
    #[cfg(target_os = "linux")]
    // SAFETY: both are NUL-terminated paths that live until the call returns.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            first.as_ptr(),
            libc::AT_FDCWD,
            second.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: as above.
    let result = unsafe { libc::renamex_np(first.as_ptr(), second.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Removes a macOS staging folder once its installer is done with it, moving its installer's
/// `apply.log` to `failed_log` first when the update failed: the log of the last failed attempt
/// is kept for diagnosis, and the download and the builds it held are not.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn discard_staging(staging: &Path, failed_log: Option<&Path>) {
    if let Some(failed_log) = failed_log {
        // The installer still writes its last lines to the moved log.
        if let Err(error) = std::fs::rename(staging.join("apply.log"), failed_log) {
            tracing::warn!(%error, "could not keep the failed Agent update's log");
        }
    }
    if let Err(error) = std::fs::remove_dir_all(staging) {
        tracing::warn!(%error, path = %staging.display(), "could not remove the Agent update's staging folder");
    }
}

/// Whether the process `pid` exists.
pub(crate) fn process_exists(pid: u32) -> bool {
    // Never 0 or negative, which would name a process group.
    libc::pid_t::try_from(pid).is_ok_and(|pid| {
        // SAFETY: signal 0 only checks that the process exists.
        pid > 0 && unsafe { libc::kill(pid, 0) } == 0
    })
}

/// Waits up to `timeout` for the coordinator `pid` to exit, while `running` says it still does.
/// One that is still running then is ended with `kill`, which asks the service manager to signal
/// the process it runs, and only while `service_process`, that process, is still `pid`: by then
/// the number may belong to another program. `now` reads the clock and `pause` waits.
pub(crate) fn wait_for_coordinator(
    pid: u32,
    timeout: Duration,
    mut now: impl FnMut() -> Instant,
    mut pause: impl FnMut(Duration),
    mut running: impl FnMut() -> bool,
    service_process: impl FnOnce() -> Option<u32>,
    kill: impl FnOnce(),
) {
    let deadline = now() + timeout;
    while running() {
        if now() >= deadline {
            if service_process() == Some(pid) {
                tracing::warn!(
                    pid,
                    "the Agent did not stop for its update in time; ending it"
                );
                kill();
            } else {
                tracing::warn!(
                    pid,
                    "the Agent's coordinator is no longer the service's process; not signalling it"
                );
            }
            return;
        }
        pause(COORDINATOR_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary folder, removed when dropped.
    struct Folder(std::path::PathBuf);

    impl Folder {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "meshrmm-update-files-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
            path
        }

        fn read(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.0.join(name)).ok()
        }

        fn names(&self) -> Vec<String> {
            let mut names = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_installer_holding_the_lock_keeps_others_from_it_until_it_exits() {
        let updates = Folder::new();
        let installer = InstallLock::try_take(&updates.0).unwrap().unwrap();
        assert!(InstallLock::try_take(&updates.0).unwrap().is_none());
        drop(installer);
        assert!(InstallLock::try_take(&updates.0).unwrap().is_some());
    }

    #[test]
    fn a_queued_installer_gets_the_lock_the_coordinator_releases() {
        let updates = Folder::new();
        let coordinator = InstallLock::try_take(&updates.0).unwrap().unwrap();
        let queued = InstallLock::queue(&updates.0).unwrap();
        assert!(queued.wait(Duration::from_millis(200)).is_err());
        drop(coordinator);
        let installer = queued.wait(Duration::from_secs(10)).unwrap();
        // And holds it: a coordinator started meanwhile finds it taken.
        assert!(InstallLock::try_take(&updates.0).unwrap().is_none());
        drop(installer);
        assert!(InstallLock::try_take(&updates.0).unwrap().is_some());
    }

    #[test]
    fn removes_what_updates_left_and_nothing_else() {
        // The Linux install directory.
        let install = Folder::new();
        install.write("meshrmm-agent", "installed");
        install.write("meshrmm-agent-1.2.3", "staged");
        install.write("meshrmm-agent-1.2.4.new", "cut short");
        install.write("meshrmm-agent.new", "a repair's");
        assert_eq!(remove_leftovers(&install.0, "meshrmm-agent-"), 2);
        assert_eq!(install.names(), ["meshrmm-agent", "meshrmm-agent.new"]);
        // The macOS update directory, with staging folders.
        let updates = Folder::new();
        updates.write(
            "agent-1.2.3-41/MeshRMM Agent.app/Contents/Info.plist",
            "bundle",
        );
        updates.write("agent-1.2.3-41/agent.zip", "download");
        updates.write("agent-1.2.3-41/apply.log", "log");
        updates.write("agent-1.2.4-57/agent.zip", "download");
        for kept in [
            "update-attempts.json",
            "next-check.json",
            "update-pending.json",
            "connected.json",
            "failed-update.log",
            "install.lock",
        ] {
            updates.write(kept, "{}");
        }
        assert_eq!(remove_leftovers(&updates.0, "agent-"), 2);
        assert_eq!(
            updates.names(),
            [
                "connected.json",
                "failed-update.log",
                "install.lock",
                "next-check.json",
                "update-attempts.json",
                "update-pending.json",
            ]
        );
    }

    #[test]
    fn removes_a_link_without_following_it() {
        let elsewhere = Folder::new();
        elsewhere.write("precious", "keep");
        let updates = Folder::new();
        std::os::unix::fs::symlink(&elsewhere.0, updates.0.join("agent-link")).unwrap();
        assert_eq!(remove_leftovers(&updates.0, "agent-"), 1);
        assert_eq!(elsewhere.read("precious").as_deref(), Some("keep"));
    }

    #[test]
    fn the_installed_executable_is_never_missing_during_an_update() {
        let install = Folder::new();
        let installed = install.write("meshrmm-agent", "previous build");
        let staged = install.write("meshrmm-agent-1.2.3", "new build");
        // The install: one step, after which an installer that dies leaves the new build
        // installed and the previous one as a leftover.
        exchange(&staged, &installed).unwrap();
        assert_eq!(install.read("meshrmm-agent").as_deref(), Some("new build"));
        assert_eq!(
            install.read("meshrmm-agent-1.2.3").as_deref(),
            Some("previous build")
        );
        // The rollback: the same step again.
        exchange(&staged, &installed).unwrap();
        assert_eq!(
            install.read("meshrmm-agent").as_deref(),
            Some("previous build")
        );
        // An installer that died after either step leaves a leftover the next sweep removes.
        remove_leftovers(&install.0, "meshrmm-agent-");
        assert_eq!(install.names(), ["meshrmm-agent"]);
        assert_eq!(
            install.read("meshrmm-agent").as_deref(),
            Some("previous build")
        );
    }

    #[test]
    fn the_installed_bundle_is_never_missing_during_an_update() {
        let support = Folder::new();
        let installed = support.write("MeshRMM Agent.app/Contents/Info.plist", "previous build");
        let installed = installed.parent().unwrap().parent().unwrap();
        let staged = support.write(
            "Agent/updates/agent-1.2.3-41/MeshRMM Agent.app/Contents/Info.plist",
            "new build",
        );
        let staged = staged.parent().unwrap().parent().unwrap();
        let bundle = "MeshRMM Agent.app/Contents/Info.plist";
        exchange(staged, installed).unwrap();
        assert_eq!(support.read(bundle).as_deref(), Some("new build"));
        let staging = support.0.join("Agent/updates/agent-1.2.3-41");
        assert_eq!(
            std::fs::read_to_string(staging.join(bundle)).unwrap(),
            "previous build"
        );
        exchange(staged, installed).unwrap();
        assert_eq!(support.read(bundle).as_deref(), Some("previous build"));
    }

    #[test]
    fn a_swap_that_cannot_happen_changes_nothing() {
        let install = Folder::new();
        let staged = install.write("meshrmm-agent-1.2.3", "new build");
        assert!(exchange(&staged, &install.0.join("meshrmm-agent")).is_err());
        assert_eq!(install.names(), ["meshrmm-agent-1.2.3"]);
    }

    #[test]
    fn keeps_only_the_log_of_a_failed_attempt() {
        let updates = Folder::new();
        updates.write(
            "agent-1.2.3-41/MeshRMM Agent.app/Contents/Info.plist",
            "failed build",
        );
        updates.write("agent-1.2.3-41/agent.zip", "download");
        updates.write("agent-1.2.3-41/apply.log", "what went wrong");
        updates.write("failed-update.log", "an older failure");
        let failed_log = updates.0.join("failed-update.log");
        discard_staging(&updates.0.join("agent-1.2.3-41"), Some(&failed_log));
        assert_eq!(updates.names(), ["failed-update.log"]);
        assert_eq!(
            updates.read("failed-update.log").as_deref(),
            Some("what went wrong")
        );
        // A committed update keeps nothing of its own.
        updates.write("agent-1.2.4-57/apply.log", "Agent 1.2.4 installed");
        discard_staging(&updates.0.join("agent-1.2.4-57"), None);
        assert_eq!(updates.names(), ["failed-update.log"]);
        assert_eq!(
            updates.read("failed-update.log").as_deref(),
            Some("what went wrong")
        );
    }

    /// Runs [`wait_for_coordinator`] on a clock that only its pauses move, for a coordinator
    /// that runs for `runs_for`, while the service manager runs `service_process`; returns
    /// whether it was killed and how long the wait took.
    fn wait_on_fake_clock(runs_for: Duration, service_process: Option<u32>) -> (bool, Duration) {
        let start = Instant::now();
        let now = std::cell::Cell::new(start);
        let mut killed = false;
        wait_for_coordinator(
            41,
            Duration::from_secs(60),
            || now.get(),
            |pause| now.set(now.get() + pause),
            || now.get() - start < runs_for,
            || service_process,
            || killed = true,
        );
        (killed, now.get() - start)
    }

    #[test]
    fn waits_for_a_coordinator_that_stops() {
        let (killed, waited) = wait_on_fake_clock(Duration::from_secs(5), Some(41));
        assert!(!killed);
        assert_eq!(waited, Duration::from_secs(5));
    }

    #[test]
    fn ends_a_stuck_coordinator_through_the_service_manager() {
        let (killed, waited) = wait_on_fake_clock(Duration::MAX, Some(41));
        assert!(killed);
        assert_eq!(waited, Duration::from_secs(60));
    }

    #[test]
    fn never_signals_a_process_id_the_coordinator_no_longer_has() {
        // The coordinator exited and its number went to another program, or the service
        // manager runs another process now.
        assert!(!wait_on_fake_clock(Duration::MAX, Some(57)).0);
        assert!(!wait_on_fake_clock(Duration::MAX, None).0);
    }

    #[test]
    fn checks_only_real_process_ids() {
        assert!(process_exists(std::process::id()));
        // Process groups, and every process, are not processes.
        assert!(!process_exists(0));
        assert!(!process_exists(u32::MAX));
    }
}
