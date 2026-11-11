//! Ending processes on macOS and Linux: `SIGTERM` to each, then `SIGKILL`
//! to those still running a few seconds later, as `kill` and then
//! `kill -9` would.
use std::time::{Duration, Instant};

use meshrmm_protocol::{ProcessInfo, ToolError, ToolErrorCode};

use super::{Deadline, out_of_time};

/// How long ended processes get to exit before they are killed.
const EXIT_WAIT: Duration = Duration::from_secs(5);
/// How long a killed process gets to go.
const KILL_WAIT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(50);

/// How a platform signals one process, only while it is the one listed, and
/// tells whether it still runs.
pub(super) trait Signals {
    fn signal(&self, info: &ProcessInfo, signal: libc::c_int) -> Result<(), ToolError>;
    fn alive(&self, info: &ProcessInfo) -> bool;
}

/// The error for a failed `kill` of `info`.
pub(super) fn signal_error(info: &ProcessInfo, error: std::io::Error) -> ToolError {
    match error.raw_os_error() {
        Some(libc::ESRCH) => ToolError::new(
            ToolErrorCode::Changed,
            format!("{} has already exited.", info.name),
        ),
        Some(libc::EPERM) => ToolError::new(
            ToolErrorCode::AccessDenied,
            format!("The system doesn't let the Agent end {}.", info.name),
        ),
        _ => ToolError::new(
            ToolErrorCode::Failed,
            format!("Could not end {}: {error}", info.name),
        ),
    }
}

/// The error for a process that started at another time than listed.
pub(super) fn changed(info: &ProcessInfo) -> ToolError {
    ToolError::new(
        ToolErrorCode::Changed,
        format!("{} has already exited.", info.name),
    )
}

fn wait(signals: &impl Signals, targets: &[&ProcessInfo], indices: &[usize], until: Instant) {
    while indices.iter().any(|index| signals.alive(targets[*index])) && Instant::now() < until {
        std::thread::sleep(POLL);
    }
}

/// Ends each of `targets` in order, answering for each.
pub(super) fn end(
    signals: &impl Signals,
    targets: &[&ProcessInfo],
    deadline: &Deadline,
) -> Vec<Result<(), ToolError>> {
    let mut results = vec![Ok(()); targets.len()];
    let mut signalled = Vec::new();
    for (index, info) in targets.iter().enumerate() {
        if deadline.passed() {
            results[index] = Err(out_of_time(info));
            continue;
        }
        match signals.signal(info, libc::SIGTERM) {
            Ok(()) => signalled.push(index),
            Err(error) => results[index] = Err(error),
        }
    }
    wait(
        signals,
        targets,
        &signalled,
        Instant::now() + EXIT_WAIT.min(deadline.left()),
    );
    let survivors = signalled
        .into_iter()
        .filter(|index| signals.alive(targets[*index]))
        .collect::<Vec<_>>();
    for index in &survivors {
        // One that went in the meantime has ended as asked.
        let _ = signals.signal(targets[*index], libc::SIGKILL);
    }
    wait(signals, targets, &survivors, Instant::now() + KILL_WAIT);
    for index in survivors {
        if signals.alive(targets[index]) {
            results[index] = Err(ToolError::new(
                ToolErrorCode::Failed,
                format!(
                    "{} was still running after it was killed; it may be stuck in the kernel.",
                    targets[index].name
                ),
            ));
        }
    }
    results
}
