//! Running an update tool with a time limit, and reading what it wrote,
//! through the process handling application runs use.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
/// plus `environment`. Stops it and whatever it started after `timeout`, or
/// at the thread's deadline if that's sooner (see
/// [`crate::remote::process`]), and never waits on output that something it
/// left running holds open. Any exit code is returned; only a tool that
/// couldn't start or ran too long is an error.
pub(crate) fn run(
    program: &Path,
    args: &[&str],
    environment: &[(&str, &str)],
    timeout: Duration,
) -> anyhow::Result<Output> {
    let mut command = Command::new(program);
    command.args(args).env("LC_ALL", "C").env("LANG", "C");
    for (name, value) in environment {
        command.env(name, value);
    }
    let ran = crate::remote::process::run(command, timeout)?;
    Ok(Output {
        // Unix exit codes are 0 to 255.
        code: ran.code.and_then(|code| i32::try_from(code).ok()),
        stdout: ran.stdout,
        stderr: ran.stderr,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

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

    /// Whether process `pid` has ended: gone, or a zombie its new parent
    /// hasn't reaped yet.
    fn ended(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z')
            })
    }

    #[test]
    fn a_child_holding_the_output_is_stopped_with_the_tool() {
        // The tool and a child it starts, which holds standard output open,
        // both outlive the limit, as a package script's daemon might.
        let pid_file =
            std::env::temp_dir().join(format!("meshrmm-update-survivor-{}", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let started = Instant::now();
        let error = run(
            Path::new("/bin/sh"),
            &[
                "-c",
                "sleep 60 & echo $! > \"$1\"; sleep 60",
                "sh",
                pid_file.to_str().unwrap(),
            ],
            &[],
            Duration::from_secs(2),
        )
        .unwrap_err();
        assert!(error.to_string().contains("was stopped"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(15));
        let pid = std::fs::read_to_string(&pid_file)
            .expect("the shell wrote its child's process ID")
            .trim()
            .parse::<i32>()
            .unwrap();
        let _ = std::fs::remove_file(&pid_file);
        let waited = Instant::now();
        while !ended(pid) {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "the child is still running"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_child_left_holding_the_output_doesnt_hold_up_the_tool() {
        // The tool exits; the child it left holds standard output.
        let started = Instant::now();
        let output = run(
            Path::new("/bin/sh"),
            &["-c", "echo before; sleep 60 & echo after"],
            &[],
            Duration::from_secs(30),
        )
        .unwrap();
        assert_eq!(output.code, Some(0));
        assert_eq!(output.stdout, "before\nafter\n");
        assert!(started.elapsed() < Duration::from_secs(15));
    }
}
