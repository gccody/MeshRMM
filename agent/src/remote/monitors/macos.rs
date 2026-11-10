//! Monitors on a Mac: launchd's system domain for services, `ps` for
//! processes, the unified log for events, and `ping`.
use std::net::IpAddr;
use std::time::Duration;

use super::command::{self, Finished};
use super::parse::{self, EventFilter, LogRead, ProcessSample, ServiceStatus};

const TOOL_TIMEOUT: Duration = Duration::from_secs(20);
const RESTART_TIMEOUT: Duration = Duration::from_secs(120);

/// A launchd label: letters, digits and `.-_`. The server checks the same
/// before sending the monitor.
fn check_label(label: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        meshrmm_protocol::valid_launchd_label(label),
        "“{label}” isn't a launchd job label"
    );
    Ok(())
}

fn succeeded(finished: Finished, what: &str) -> anyhow::Result<Finished> {
    if finished.code == Some(0) {
        return Ok(finished);
    }
    let complaint = finished.complaint();
    if complaint.is_empty() {
        anyhow::bail!("{what} failed");
    }
    anyhow::bail!("{what} failed: {complaint}")
}

/// A job in launchd's system domain. One that is loaded and not disabled
/// starts by itself.
pub(super) fn service(label: &str) -> anyhow::Result<ServiceStatus> {
    check_label(label)?;
    let target = format!("system/{label}");
    let print = command::run("launchctl", &["print", &target], TOOL_TIMEOUT)?;
    let mut status = parse::launchd_job((print.code == Some(0)).then_some(print.stdout.as_str()));
    if status.found {
        let disabled = command::run("launchctl", &["print-disabled", "system"], TOOL_TIMEOUT)?;
        let disabled = succeeded(disabled, "launchctl print-disabled")?;
        status.automatic = Some(!parse::launchd_disabled(&disabled.stdout, label));
    }
    Ok(status)
}

pub(super) fn restart_service(label: &str) -> anyhow::Result<()> {
    check_label(label)?;
    let target = format!("system/{label}");
    let finished = command::run("launchctl", &["kickstart", "-k", &target], RESTART_TIMEOUT)?;
    succeeded(finished, "launchctl kickstart").map(drop)
}

pub(super) fn is_agent_service(label: &str) -> bool {
    label.starts_with(crate::installer::DAEMON_LABEL)
}

pub(super) fn processes(name: &str) -> anyhow::Result<Vec<ProcessSample>> {
    let finished = command::run("ps", &["-axo", "pid=,rss=,time=,comm="], TOOL_TIMEOUT)?;
    let finished = succeeded(finished, "ps")?;
    Ok(parse::ps_processes(&finished.stdout)
        .into_iter()
        .filter(|process| {
            process
                .names
                .iter()
                .any(|candidate| parse::process_name_matches(candidate, name, false))
        })
        .collect())
}

pub(super) fn is_protected_process(name: &str) -> bool {
    [
        "launchd",
        "kernel_task",
        "WindowServer",
        "loginwindow",
        "meshrmm-agent",
    ]
    .contains(&name)
}

pub(super) fn end_processes(name: &str) -> anyhow::Result<usize> {
    let pids = processes(name)?
        .into_iter()
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    command::end(&pids)
}

/// `text` as a quoted predicate string.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The `log show --predicate` for the filter. The event IDs don't apply to
/// the unified log.
pub(super) fn predicate(filter: &EventFilter) -> String {
    let mut parts = vec![format!(
        "({})",
        parse::unified_log_predicate(&filter.levels)
    )];
    if !filter.source.is_empty() {
        let source = quoted(&filter.source);
        parts.push(format!(
            "(subsystem ==[c] {source} OR process ==[c] {source} OR sender ==[c] {source})"
        ));
    }
    if !filter.message.is_empty() {
        parts.push(format!(
            "eventMessage CONTAINS[c] {}",
            quoted(&filter.message)
        ));
    }
    parts.join(" AND ")
}

pub(super) fn events(
    _log: &str,
    filter: &EventFilter,
    since_ms: i64,
    _now_ms: i64,
    timeout: Duration,
    // The unified log is read by time.
    _log_record: &mut Option<u64>,
) -> anyhow::Result<LogRead> {
    let start = format!(
        "{}+0000",
        parse::format_time(since_ms).trim_end_matches(" UTC")
    );
    let predicate = predicate(filter);
    let finished = command::run(
        "log",
        &[
            "show",
            "--style",
            "ndjson",
            "--info",
            "--start",
            &start,
            "--predicate",
            &predicate,
        ],
        timeout,
    )?;
    let finished = succeeded(finished, "log show")?;
    Ok(LogRead {
        events: finished
            .stdout
            .lines()
            .filter_map(parse::unified_log_event)
            // `--start` counts whole seconds.
            .filter(|event| event.at_ms >= since_ms)
            .collect(),
        // Only the output cap stops it.
        truncated: finished.truncated,
        out_of_time: false,
    })
}

pub(super) fn ping(host: &str, timeout: Duration) -> anyhow::Result<Option<f64>> {
    let seconds = timeout.as_secs().max(1).to_string();
    // macOS pings IPv6 with its own tool, which has no overall time limit.
    let ipv6 = host
        .split('%')
        .next()
        .and_then(|address| address.parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_ipv6());
    let run = if ipv6 {
        command::run("ping6", &["-n", "-c", "1", host], timeout)
    } else {
        command::run(
            "ping",
            &["-n", "-c", "1", "-t", &seconds, host],
            timeout + Duration::from_secs(2),
        )
    };
    let finished = match run {
        Ok(finished) => finished,
        Err(_) if ipv6 => return Ok(None),
        Err(error) => return Err(error),
    };
    match finished.code {
        Some(0) => Ok(Some(parse::ping_time_ms(&finished.stdout).unwrap_or(0.0))),
        // No reply.
        Some(2) => Ok(None),
        _ => anyhow::bail!("{}", finished.complaint()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_predicates_are_safe() {
        assert!(check_label("com.example.backup").is_ok());
        assert!(check_label("system/com.example").is_err());
        assert!(check_label("-x").is_err());
        let filter = EventFilter {
            source: "com.apple.storage".into(),
            message: "say \"hi\" \\ now".into(),
            levels: vec![meshrmm_protocol::EventLevel::Error],
            ..EventFilter::default()
        };
        assert_eq!(
            predicate(&filter),
            r#"(messageType == error) AND (subsystem ==[c] "com.apple.storage" OR process ==[c] "com.apple.storage" OR sender ==[c] "com.apple.storage") AND eventMessage CONTAINS[c] "say \"hi\" \\ now""#
        );
        assert!(is_agent_service("com.meshrmm.agent"));
        assert!(!is_agent_service("com.example"));
    }
}
