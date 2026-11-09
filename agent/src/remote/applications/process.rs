//! Running a package manager or installer with a time limit, and reading
//! what it wrote. Whatever it starts is stopped with it at the limit: a
//! process group on macOS and Linux, a job object on Windows, which holds
//! the program from before its first instruction. Output a lingering child
//! holds open is read for a moment after the program exits; what was read
//! by then is kept, and the readers stop.
//!
//! A scan or run sets a deadline for its thread with [`by`]: every program
//! it runs is stopped by then at the latest, whatever its own limit.
use std::cell::Cell;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// How long output readers may outlast the program.
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);
/// How often a reader with nothing to read checks whether to stop.
const READ_POLL: Duration = Duration::from_millis(50);
/// The most of each stream kept, from its end.
const KEPT_BYTES: usize = 1024 * 1024;

thread_local! {
    /// When every program this thread runs must have finished, if ever.
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Runs `work` with every program it runs stopped by `deadline` at the
/// latest, or by an earlier deadline this thread already has.
pub(crate) fn by<T>(deadline: Instant, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            DEADLINE.set(self.0);
        }
    }
    let outer = DEADLINE.get();
    let _restore = Restore(outer);
    DEADLINE.set(Some(outer.map_or(deadline, |outer| outer.min(deadline))));
    work()
}

/// What a program wrote, and how it exited.
#[derive(Debug, Default)]
pub(crate) struct Ran {
    /// The exit code; on Windows an `HRESULT` such as winget's, as the
    /// unsigned number it is. `None` when a signal ended it.
    pub(crate) code: Option<i64>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Ran {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Both streams, standard output first.
    pub(crate) fn output(&self) -> String {
        match (self.stdout.trim().is_empty(), self.stderr.trim().is_empty()) {
            (_, true) => self.stdout.clone(),
            (true, false) => self.stderr.clone(),
            (false, false) => format!("{}\n{}", self.stdout.trim_end(), self.stderr),
        }
    }

    /// The last lines it wrote, error output first, for a message.
    pub(crate) fn complaint(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let cleaned = meshrmm_protocol::clean_output(text);
        let lines = cleaned
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let tail = lines[lines.len().saturating_sub(3)..].join(" ");
        if tail.is_empty() {
            match self.code {
                Some(code) => format!("it exited with {}", describe_code(code)),
                None => "it was stopped by a signal".to_owned(),
            }
        } else {
            tail
        }
    }
}

/// An exit code as people look it up: Windows' `HRESULT`s in hexadecimal.
pub(crate) fn describe_code(code: i64) -> String {
    if code > 0xFFFF {
        format!("code 0x{code:08X}")
    } else {
        format!("code {code}")
    }
}

/// What a reader has read so far, from the stream's end.
#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    ended: bool,
}

/// A thread reading one of a program's streams. It keeps what it reads as
/// it goes, so output survives a reader that is given up on, and it checks
/// whether to stop whenever the stream is quiet, so it ends even while a
/// lingering child holds the stream open.
struct Reader {
    kept: Arc<Mutex<Kept>>,
    stop: Arc<AtomicBool>,
}

impl Reader {
    fn start<S: pipe::Pipe>(mut stream: S) -> Self {
        let kept = Arc::new(Mutex::new(Kept::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (writer, stopping) = (Arc::clone(&kept), Arc::clone(&stop));
        let spawned = std::thread::Builder::new()
            .name("meshrmm-application-output".into())
            .spawn(move || {
                let mut chunk = [0_u8; 16 * 1024];
                while !stopping.load(Ordering::Relaxed) {
                    let wanted = match pipe::ready(&stream, READ_POLL) {
                        Ok(Some(wanted)) => wanted.min(chunk.len()),
                        Ok(None) => continue,
                        Err(()) => break,
                    };
                    match stream.read(&mut chunk[..wanted]) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut kept = lock(&writer);
                            kept.bytes.extend_from_slice(&chunk[..read]);
                            if kept.bytes.len() > 2 * KEPT_BYTES {
                                let excess = kept.bytes.len() - KEPT_BYTES;
                                kept.bytes.drain(..excess);
                            }
                        }
                    }
                }
                lock(&writer).ended = true;
            });
        if spawned.is_err() {
            lock(&kept).ended = true;
        }
        Self { kept, stop }
    }

    /// Waits up to `drain` for the stream to end, then stops the reader and
    /// returns what it read.
    fn finish(self, drain: Duration) -> Vec<u8> {
        let until = Instant::now() + drain;
        while !lock(&self.kept).ended && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.stop.store(true, Ordering::Relaxed);
        let mut bytes = std::mem::take(&mut lock(&self.kept).bytes);
        if bytes.len() > KEPT_BYTES {
            bytes.drain(..bytes.len() - KEPT_BYTES);
        }
        bytes
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Nothing panics while holding these locks, so a poisoned one is whole.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Waiting for a stream to have something to read, so a reader can stop.
#[cfg(unix)]
mod pipe {
    use std::os::fd::AsRawFd;
    use std::time::Duration;

    pub(super) trait Pipe: std::io::Read + AsRawFd + Send + 'static {}
    impl<T: std::io::Read + AsRawFd + Send + 'static> Pipe for T {}

    /// `Some` with how much to read when there is something to read or the
    /// stream ended, `None` when nothing came within `wait`.
    pub(super) fn ready(stream: &impl AsRawFd, wait: Duration) -> Result<Option<usize>, ()> {
        let mut poll = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: one valid pollfd for an open descriptor.
        match unsafe { libc::poll(&mut poll, 1, wait) } {
            0 => Ok(None),
            ready if ready > 0 => Ok(Some(usize::MAX)),
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                Ok(None)
            }
            _ => Err(()),
        }
    }
}

/// Waiting for a stream to have something to read, so a reader can stop.
/// Anonymous pipes can't be waited on, so this peeks.
#[cfg(windows)]
mod pipe {
    use std::os::windows::io::AsRawHandle;
    use std::time::Duration;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    pub(super) trait Pipe: std::io::Read + AsRawHandle + Send + 'static {}
    impl<T: std::io::Read + AsRawHandle + Send + 'static> Pipe for T {}

    /// `Some` with how much is waiting, `None` when nothing came within
    /// `wait`; an error once the writer closed it and it's empty.
    pub(super) fn ready(stream: &impl AsRawHandle, wait: Duration) -> Result<Option<usize>, ()> {
        let mut waiting = 0_u32;
        // SAFETY: the handle is the open read end of a pipe.
        unsafe {
            PeekNamedPipe(
                HANDLE(stream.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut waiting),
                None,
            )
        }
        .map_err(|_| ())?;
        if waiting == 0 {
            std::thread::sleep(wait);
            return Ok(None);
        }
        Ok(Some(waiting as usize))
    }
}

/// Text a program wrote: UTF-8, else (on Windows) the OEM code page some
/// installers use, read as Latin-1 rather than lost.
fn text(bytes: Vec<u8>) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) if cfg!(windows) => bytes.iter().map(|byte| char::from(*byte)).collect(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Runs `command` with no input, stopping it and what it started after
/// `timeout`, or at the thread's deadline (see [`by`]) if that's sooner.
/// Any exit code is returned; only a program that couldn't start or ran
/// too long is an error.
pub(crate) fn run(mut command: Command, timeout: Duration) -> anyhow::Result<Ran> {
    let program = command.get_program().to_string_lossy().into_owned();
    let deadline = DEADLINE.get();
    let limit = deadline.map_or(timeout, |deadline| {
        timeout.min(deadline.saturating_duration_since(Instant::now()))
    });
    if limit.is_zero() {
        bail!("there was no time left to run {program}");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // It starts in the job object, before it can start anything.
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {program}"))?;
    #[cfg(windows)]
    let tree = match windows_tree::Tree::start(&child) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error.context(format!("could not start {program}")));
        }
    };
    let stdout = Reader::start(child.stdout.take().context("no standard output")?);
    let stderr = Reader::start(child.stderr.take().context("no standard error")?);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > limit {
            #[cfg(unix)]
            {
                // SAFETY: kill has no memory effects; the group is the child's own.
                let group = i32::try_from(child.id()).unwrap_or(0);
                if group > 0 {
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                }
            }
            #[cfg(windows)]
            tree.stop();
            let _ = child.kill();
            let _ = child.wait();
            let _ = (stdout.finish(Duration::ZERO), stderr.finish(Duration::ZERO));
            if limit < timeout {
                bail!("{program} was stopped when the time allowed for this ran out");
            }
            bail!(
                "{program} took longer than {} minutes and was stopped",
                timeout.as_secs().div_ceil(60)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let code = status.code().map(|code| {
        if cfg!(windows) {
            // Windows exit codes are unsigned; HRESULTs read as negative.
            i64::from(code as u32)
        } else {
            i64::from(code)
        }
    });
    Ok(Ran {
        code,
        stdout: text(stdout.finish(OUTPUT_DRAIN)),
        stderr: text(stderr.finish(OUTPUT_DRAIN)),
    })
}

#[cfg(windows)]
mod windows_tree {
    use std::os::windows::io::AsRawHandle;

    use anyhow::Context;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    use windows::core::PCWSTR;

    use crate::win32::OwnedHandle;

    /// A job object holding a program and everything it starts, so a time
    /// limit stops installers a package manager ran too. Windows Installer
    /// does its work in the `msiserver` service, outside it.
    pub(super) struct Tree(OwnedHandle);

    impl Tree {
        /// Puts the suspended `child` in a new job, then lets it run.
        pub(super) fn start(child: &std::process::Child) -> anyhow::Result<Self> {
            // SAFETY: an unnamed job with default security.
            let job = OwnedHandle(
                unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                    .context("could not create a job object")?,
            );
            let process = HANDLE(child.as_raw_handle());
            // SAFETY: both handles are open; the child's outlives this call.
            unsafe { AssignProcessToJobObject(job.0, process) }
                .context("could not put the program in a job object")?;
            resume(child.id())?;
            Ok(Self(job))
        }

        pub(super) fn stop(&self) {
            // SAFETY: the job handle is open.
            let _ = unsafe { TerminateJobObject(self.0.0, 1) };
        }
    }

    /// Resumes the threads of the suspended process `pid`: its first one,
    /// as it was created.
    fn resume(pid: u32) -> anyhow::Result<()> {
        // SAFETY: a snapshot of the system's threads, closed on drop.
        let snapshot = OwnedHandle(
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }
                .context("could not list threads")?,
        );
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut resumed = 0;
        // SAFETY: the snapshot is open and entry's size is set.
        let mut more = unsafe { Thread32First(snapshot.0, &mut entry) }.is_ok();
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: the thread ID is from the snapshot; the handle is
                // closed on drop.
                let thread =
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                        .map(OwnedHandle)
                        .context("could not open the program's thread")?;
                // SAFETY: the thread handle is open.
                if unsafe { ResumeThread(thread.0) } == u32::MAX {
                    anyhow::bail!("could not resume the program");
                }
                resumed += 1;
            }
            // SAFETY: as above.
            more = unsafe { Thread32Next(snapshot.0, &mut entry) }.is_ok();
        }
        anyhow::ensure!(resumed > 0, "could not find the program's thread");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn programs_report_their_exit_code_and_output_and_stop_at_the_limit() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo out; echo err >&2; exit 100"]);
        let ran = run(command, Duration::from_secs(10)).unwrap();
        assert_eq!(ran.code, Some(100));
        assert_eq!(ran.stdout, "out\n");
        assert_eq!(ran.complaint(), "err");
        assert_eq!(ran.output(), "out\nerr\n");
        let mut slow = Command::new("/bin/sh");
        slow.args(["-c", "sleep 30 & sleep 30"]);
        let started = Instant::now();
        assert!(run(slow, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn output_survives_a_child_that_keeps_the_stream_open() {
        // The program exits; the child it left holds standard output.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo before; sleep 30 & echo after"]);
        let started = Instant::now();
        let ran = run(command, Duration::from_secs(20)).unwrap();
        assert_eq!(ran.code, Some(0));
        assert_eq!(ran.stdout, "before\nafter\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(windows)]
    #[test]
    fn programs_report_their_exit_code_and_output_and_stop_at_the_limit() {
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/c", "echo out& echo err 1>&2& exit /b 100"]);
        let ran = run(command, Duration::from_secs(10)).unwrap();
        assert_eq!(ran.code, Some(100));
        assert_eq!(ran.stdout.trim(), "out");
        assert_eq!(ran.complaint(), "err");
        // What it starts at once is in the job too.
        let mut slow = Command::new("cmd.exe");
        slow.args(["/d", "/c", "start /b powershell.exe -NoProfile -Command Start-Sleep 30 & powershell.exe -NoProfile -Command Start-Sleep 30"]);
        let started = Instant::now();
        assert!(run(slow, Duration::from_millis(1500)).is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(windows)]
    #[test]
    fn output_survives_a_child_that_keeps_the_stream_open() {
        // The program exits; the child it left holds standard output.
        let mut command = Command::new("cmd.exe");
        command.args([
            "/d",
            "/c",
            "echo before& start /b powershell.exe -NoProfile -Command Start-Sleep 30& echo after",
        ]);
        let started = Instant::now();
        let ran = run(command, Duration::from_secs(20)).unwrap();
        assert_eq!(ran.code, Some(0));
        assert_eq!(
            ran.stdout.split_whitespace().collect::<Vec<_>>(),
            ["before", "after"]
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn deadlines_stop_programs_before_their_own_limit() {
        let slow = || {
            if cfg!(windows) {
                let mut command = Command::new("powershell.exe");
                command.args(["-NoProfile", "-Command", "Start-Sleep 30"]);
                command
            } else {
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "sleep 30"]);
                command
            }
        };
        let started = Instant::now();
        let error = by(Instant::now() + Duration::from_millis(1500), || {
            // A later deadline inside doesn't extend it.
            by(Instant::now() + Duration::from_secs(600), || {
                run(slow(), Duration::from_secs(600))
            })
        })
        .unwrap_err();
        assert!(error.to_string().contains("time allowed"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(by(Instant::now(), || run(slow(), Duration::from_secs(600))).is_err());
        assert_eq!(DEADLINE.get(), None, "the deadline ends with its work");
    }

    #[test]
    fn codes_read_as_people_look_them_up() {
        assert_eq!(describe_code(1603), "code 1603");
        assert_eq!(describe_code(0x8A15_0014), "code 0x8A150014");
    }
}
