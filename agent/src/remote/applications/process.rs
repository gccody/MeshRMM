//! Running a package manager or installer with a time limit, and reading
//! what it wrote. Whatever it starts is stopped with it at the limit: a
//! process group on macOS and Linux, a job object on Windows. Output a
//! lingering child holds open is read for a moment after the program exits,
//! then given up on.
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// How long output readers may outlast the program.
const OUTPUT_DRAIN: Duration = Duration::from_secs(2);
/// The most of each stream kept, from its end.
const KEPT_BYTES: usize = 1024 * 1024;

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

fn reader(mut stream: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0_u8; 16 * 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    kept.extend_from_slice(&chunk[..read]);
                    if kept.len() > 2 * KEPT_BYTES {
                        kept.drain(..kept.len() - KEPT_BYTES);
                    }
                }
            }
        }
        if kept.len() > KEPT_BYTES {
            kept.drain(..kept.len() - KEPT_BYTES);
        }
        let _ = sender.send(kept);
    });
    receiver
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
/// `timeout`. Any exit code is returned; only a program that couldn't
/// start or ran too long is an error.
pub(crate) fn run(mut command: Command, timeout: Duration) -> anyhow::Result<Ran> {
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
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {program}"))?;
    #[cfg(windows)]
    let tree = windows_tree::Tree::of(&child);
    let stdout = reader(child.stdout.take().context("no standard output")?);
    let stderr = reader(child.stderr.take().context("no standard error")?);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
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
            bail!(
                "{program} took longer than {} minutes and was stopped",
                timeout.as_secs().div_ceil(60)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let drain =
        |receiver: mpsc::Receiver<Vec<u8>>| receiver.recv_timeout(OUTPUT_DRAIN).unwrap_or_default();
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
        stdout: text(drain(stdout)),
        stderr: text(drain(stderr)),
    })
}

#[cfg(windows)]
mod windows_tree {
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows::core::PCWSTR;

    use crate::win32::OwnedHandle;

    /// A job object holding a program and everything it starts, so a time
    /// limit stops installers a package manager ran too.
    pub(super) struct Tree(Option<OwnedHandle>);

    impl Tree {
        pub(super) fn of(child: &std::process::Child) -> Self {
            // SAFETY: an unnamed job with default security.
            let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                .ok()
                .map(OwnedHandle);
            let process = HANDLE(child.as_raw_handle());
            // SAFETY: both handles are open; the child's outlives this call.
            Self(job.filter(|job| unsafe { AssignProcessToJobObject(job.0, process) }.is_ok()))
        }

        pub(super) fn stop(&self) {
            if let Some(job) = &self.0 {
                // SAFETY: the job handle is open.
                let _ = unsafe { TerminateJobObject(job.0, 1) };
            }
        }
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

    #[cfg(windows)]
    #[test]
    fn programs_report_their_exit_code_and_output_and_stop_at_the_limit() {
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/c", "echo out& echo err 1>&2& exit /b 100"]);
        let ran = run(command, Duration::from_secs(10)).unwrap();
        assert_eq!(ran.code, Some(100));
        assert_eq!(ran.stdout.trim(), "out");
        assert_eq!(ran.complaint(), "err");
        let mut slow = Command::new("powershell.exe");
        slow.args(["-NoProfile", "-Command", "Start-Sleep 30"]);
        let started = Instant::now();
        assert!(run(slow, Duration::from_millis(500)).is_err());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn codes_read_as_people_look_them_up() {
        assert_eq!(describe_code(1603), "code 1603");
        assert_eq!(describe_code(0x8A15_0014), "code 0x8A150014");
    }
}
