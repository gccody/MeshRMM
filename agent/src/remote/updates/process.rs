//! Running an update tool with a time limit, and reading what it wrote.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

/// What a tool wrote, and how it exited.
#[derive(Debug)]
pub(crate) struct Output {
    /// `None` when a signal ended it.
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Output {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The last lines of its error output, or of its output, for a message.
    pub(crate) fn complaint(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        let tail = lines[lines.len().saturating_sub(4)..].join(" ");
        if tail.is_empty() {
            match self.code {
                Some(code) => format!("it exited with code {code}"),
                None => "it was stopped by a signal".to_owned(),
            }
        } else {
            tail
        }
    }
}

/// The first of `names` in the usual system folders.
pub(crate) fn find(names: &[&str]) -> Option<PathBuf> {
    const FOLDERS: [&str; 6] = [
        "/usr/sbin",
        "/usr/bin",
        "/sbin",
        "/bin",
        "/usr/local/sbin",
        "/usr/local/bin",
    ];
    names.iter().find_map(|name| {
        FOLDERS
            .iter()
            .map(|folder| Path::new(folder).join(name))
            .find(|path| path.is_file())
    })
}

/// Runs `program` with `args` and the C locale, so its output can be read,
/// plus `environment`. Stops it after `timeout`. Any exit code is returned;
/// only a tool that couldn't start or ran too long is an error.
pub(crate) fn run(
    program: &Path,
    args: &[&str],
    environment: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("could not run {}", program.display()))?;
    let mut stdout = child.stdout.take().context("no standard output")?;
    let mut stderr = child.stderr.take().context("no standard error")?;
    // Reading on other threads keeps a full pipe from stalling the tool.
    let output = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let errors = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr.read_to_end(&mut text);
        text
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} took longer than {} minutes and was stopped",
                program.display(),
                timeout.as_secs().div_ceil(60)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = output.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();
    Ok(Output {
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_report_their_exit_code_and_output() {
        let shell = Path::new("/bin/sh");
        let output = run(
            shell,
            &["-c", "echo out; echo err >&2; exit 100"],
            &[("EXTRA", "1")],
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.code, Some(100));
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.complaint(), "err");
        assert!(run(shell, &["-c", "sleep 5"], &[], Duration::from_millis(100)).is_err());
        assert_eq!(
            run(
                shell,
                &["-c", "echo $EXTRA"],
                &[("EXTRA", "yes")],
                Duration::from_secs(10)
            )
            .unwrap()
            .stdout,
            "yes\n"
        );
    }
}
