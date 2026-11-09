//! Running the system's tools for monitors: without a shell, in the C
//! locale so their output reads the same everywhere, with a time limit, and
//! keeping at most a few megabytes of what they write.
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

pub(super) struct Finished {
    /// The exit code; `None` when a signal ended it.
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
    /// It wrote more output than is kept.
    pub(super) truncated: bool,
}

impl Finished {
    /// Its error output, or else its output, for a message.
    pub(super) fn complaint(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        meshrmm_protocol::truncate_text(line.trim(), 300).to_owned()
    }
}

/// What a pipe wrote, up to the cap, and whether there was more.
fn drain(mut pipe: impl Read + Send + 'static) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut dropped = false;
        let mut chunk = [0; 64 * 1024];
        while let Ok(read) = pipe.read(&mut chunk) {
            if read == 0 {
                break;
            }
            let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
            dropped |= read > room;
            kept.extend_from_slice(&chunk[..read.min(room)]);
        }
        let _ = sender.send((kept, dropped));
    });
    receiver
}

/// Runs `program` with `arguments`, stopping it after `timeout`.
pub(super) fn run(
    program: &str,
    arguments: &[&str],
    timeout: Duration,
) -> anyhow::Result<Finished> {
    let mut child = Command::new(program)
        .args(arguments)
        .env("LC_ALL", "C")
        .env("PATH", PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| anyhow::anyhow!("could not run {program}: {error}"))?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "{program} didn't finish within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let collect = |receiver: Option<mpsc::Receiver<(Vec<u8>, bool)>>| {
        receiver
            .and_then(|receiver| receiver.recv_timeout(Duration::from_secs(2)).ok())
            .map(|(bytes, dropped)| (String::from_utf8_lossy(&bytes).into_owned(), dropped))
            .unwrap_or_default()
    };
    let (stdout, truncated) = collect(stdout);
    Ok(Finished {
        code: status.code(),
        stdout,
        stderr: collect(stderr).0,
        truncated,
    })
}

/// How long ended processes get to exit before they are killed.
const END_GRACE: Duration = Duration::from_secs(5);

fn alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Asks the processes in `pids` to end, killing those still running after
/// [`END_GRACE`]. The first process and the Agent itself are never ended.
/// Returns how many ended.
pub(super) fn end(pids: &[u32]) -> anyhow::Result<usize> {
    let own = std::process::id();
    let targets = pids
        .iter()
        .copied()
        .filter(|pid| *pid > 1 && *pid != own)
        .filter_map(|pid| libc::pid_t::try_from(pid).ok())
        .collect::<Vec<_>>();
    let mut signalled = Vec::new();
    let mut failure = None;
    for pid in targets {
        // SAFETY: kill only sends a signal.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            signalled.push(pid);
        } else {
            failure = Some(std::io::Error::last_os_error());
        }
    }
    let deadline = Instant::now() + END_GRACE;
    while signalled.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    for pid in &signalled {
        if alive(*pid) {
            // SAFETY: as above.
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
    }
    if signalled.is_empty()
        && let Some(error) = failure
    {
        anyhow::bail!("could not end it: {error}");
    }
    Ok(signalled.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_report_output_and_stop_at_the_limit() {
        let finished = run(
            "/bin/sh",
            &["-c", "echo out; echo err >&2; exit 3"],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(finished.code, Some(3));
        assert_eq!(finished.stdout, "out\n");
        assert_eq!(finished.complaint(), "err");
        let error = run("/bin/sh", &["-c", "sleep 5"], Duration::from_millis(200))
            .err()
            .unwrap();
        assert!(error.to_string().contains("didn't finish"));
    }
}
