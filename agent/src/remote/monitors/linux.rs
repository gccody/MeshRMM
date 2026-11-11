//! Monitors on Linux: systemd for services, /proc for processes, the
//! journal for events, and `ping`.
use std::time::Duration;

use super::command::{self, Finished};
use super::parse::{self, EventFilter, LogPlace, LogRead, LogWindow, ProcessSample, ServiceStatus};

const TOOL_TIMEOUT: Duration = Duration::from_secs(20);
const RESTART_TIMEOUT: Duration = Duration::from_secs(120);
/// The most journal entries one check reads, newest first, after the
/// source and priority filters.
const MAX_JOURNAL_ENTRIES: usize = 5000;

/// A systemd unit name: letters, digits and `:-_.@\`. The server checks
/// the same before sending the monitor.
fn check_unit(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        meshrmm_protocol::valid_unit_name(name),
        "“{name}” isn't a systemd unit name"
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

pub(super) fn service(name: &str) -> anyhow::Result<ServiceStatus> {
    check_unit(name)?;
    let finished = command::run(
        "systemctl",
        &[
            "show",
            "--property=LoadState,ActiveState,SubState,UnitFileState",
            "--",
            name,
        ],
        TOOL_TIMEOUT,
    )?;
    let finished = succeeded(finished, "systemctl")?;
    Ok(parse::systemd_unit(&finished.stdout))
}

pub(super) fn restart_service(name: &str) -> anyhow::Result<()> {
    check_unit(name)?;
    let finished = command::run("systemctl", &["restart", "--", name], RESTART_TIMEOUT)?;
    succeeded(finished, "systemctl restart").map(drop)
}

pub(super) fn is_agent_service(name: &str) -> bool {
    let agent = crate::installer::SERVICE;
    name == agent || Some(name) == agent.strip_suffix(".service")
}

/// The file name of a link's target, such as `/proc/<pid>/exe`'s.
fn link_name(path: &str) -> Option<String> {
    let target = std::fs::read_link(path).ok()?;
    let name = target.file_name()?.to_string_lossy();
    Some(name.strip_suffix(" (deleted)").unwrap_or(&name).to_owned())
}

/// The processes called `name`: by their command, which the kernel cuts
/// to 15 characters, or their program's file name.
pub(super) fn processes(name: &str) -> anyhow::Result<Vec<ProcessSample>> {
    // SAFETY: sysconf only reads configuration.
    let (ticks, page) = unsafe {
        (
            libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64,
            libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64,
        )
    };
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(pid) = file_name
            .to_str()
            .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((pid, comm, cpu_ticks, rss)) = parse::proc_stat(&stat) else {
            continue;
        };
        let mut names = vec![comm];
        if let Some(program) = link_name(&format!("/proc/{pid}/exe"))
            && !names.contains(&program)
        {
            names.push(program);
        }
        if !names
            .iter()
            .any(|candidate| parse::process_name_matches(candidate, name, false))
        {
            continue;
        }
        found.push(ProcessSample {
            pid,
            names,
            cpu_ms: cpu_ticks.saturating_mul(1000) / ticks,
            memory_bytes: rss.saturating_mul(page),
        });
    }
    Ok(found)
}

pub(super) fn is_protected_process(name: &str) -> bool {
    ["systemd", "init", "meshrmm-agent", "kthreadd"].contains(&name)
}

pub(super) fn end_processes(name: &str) -> anyhow::Result<usize> {
    let pids = processes(name)?
        .into_iter()
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    command::end(&pids)
}

pub(super) fn events(
    _log: &str,
    filter: &EventFilter,
    window: &LogWindow,
    timeout: Duration,
    // The journal is read by time.
    _place: &mut Option<LogPlace>,
) -> anyhow::Result<LogRead> {
    let since_ms = window.since_ms;
    let arguments = journal_arguments(filter, since_ms);
    let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
    let finished = command::run("journalctl", &arguments, timeout)?;
    let finished = succeeded(finished, "journalctl")?;
    let lines = finished.stdout.lines().collect::<Vec<_>>();
    Ok(LogRead {
        // Stopped at the entry cap, or at the output cap before it.
        truncated: lines.len() >= MAX_JOURNAL_ENTRIES || finished.truncated,
        events: lines
            .into_iter()
            .filter_map(parse::journal_event)
            // `--since` counts whole seconds.
            .filter(|event| event.at_ms >= since_ms)
            .collect(),
        ..LogRead::default()
    })
}

/// `journalctl`'s arguments: the time and priority, and the source as
/// journal matches, so the entry cap applies after them. A source matches
/// the unit (with or without `.service`), the syslog identifier or the
/// command, as given or in lower case, since journal matches are exact.
fn journal_arguments(filter: &EventFilter, since_ms: i64) -> Vec<String> {
    let mut arguments = vec![
        "--no-pager".to_owned(),
        "--quiet".to_owned(),
        "--output=json".to_owned(),
        "--reverse".to_owned(),
        format!("--lines={MAX_JOURNAL_ENTRIES}"),
        format!("--since=@{}", since_ms.div_euclid(1000)),
        format!("--priority={}", parse::journal_max_priority(&filter.levels)),
    ];
    let source = filter.source.as_str();
    if source.is_empty() {
        return arguments;
    }
    let mut names = vec![source.to_owned()];
    let lower = source.to_lowercase();
    if lower != source {
        names.push(lower);
    }
    let mut matches = Vec::new();
    for name in &names {
        matches.push(format!("_SYSTEMD_UNIT={name}"));
        if !name.ends_with(".service") {
            matches.push(format!("_SYSTEMD_UNIT={name}.service"));
        }
        matches.push(format!("SYSLOG_IDENTIFIER={name}"));
        matches.push(format!("_COMM={name}"));
    }
    // `+` between matches makes them alternatives.
    for (index, found) in matches.into_iter().enumerate() {
        if index > 0 {
            arguments.push("+".to_owned());
        }
        arguments.push(found);
    }
    arguments
}

pub(super) fn ping(host: &str, timeout: Duration) -> anyhow::Result<Option<f64>> {
    let seconds = timeout.as_secs().max(1).to_string();
    let finished = command::run(
        "ping",
        &["-n", "-c", "1", "-W", &seconds, host],
        timeout + Duration::from_secs(2),
    )?;
    match finished.code {
        Some(0) => Ok(Some(parse::ping_time_ms(&finished.stdout).unwrap_or(0.0))),
        // No reply.
        Some(1) => Ok(None),
        _ => anyhow::bail!("{}", finished.complaint()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_journal_filters_by_source_before_its_cap() {
        let mut filter = EventFilter {
            levels: vec![meshrmm_protocol::EventLevel::Error],
            ..EventFilter::default()
        };
        let plain = journal_arguments(&filter, 1_700_000_000_999);
        assert_eq!(
            plain,
            [
                "--no-pager",
                "--quiet",
                "--output=json",
                "--reverse",
                "--lines=5000",
                "--since=@1700000000",
                "--priority=3",
            ]
        );
        filter.source = "CRON".into();
        let arguments = journal_arguments(&filter, 1_700_000_000_000);
        assert_eq!(
            arguments[plain.len()..],
            [
                "_SYSTEMD_UNIT=CRON",
                "+",
                "_SYSTEMD_UNIT=CRON.service",
                "+",
                "SYSLOG_IDENTIFIER=CRON",
                "+",
                "_COMM=CRON",
                "+",
                "_SYSTEMD_UNIT=cron",
                "+",
                "_SYSTEMD_UNIT=cron.service",
                "+",
                "SYSLOG_IDENTIFIER=cron",
                "+",
                "_COMM=cron",
            ]
        );
        filter.source = "ssh.service".into();
        assert!(
            !journal_arguments(&filter, 0)
                .iter()
                .any(|argument| argument.ends_with(".service.service"))
        );
    }

    #[test]
    fn unit_names_are_checked() {
        assert!(check_unit("cron").is_ok());
        assert!(check_unit("user@1000.service").is_ok());
        assert!(check_unit("dev-disk-by\\x2duuid.device").is_ok());
        assert!(check_unit("-cron").is_err());
        assert!(check_unit("cron; reboot").is_err());
        assert!(check_unit("a b").is_err());
        assert!(is_agent_service("meshrmm-agent"));
        assert!(is_agent_service("meshrmm-agent.service"));
        assert!(!is_agent_service("cron"));
    }

    /// Reads this computer's systemd, journal and ping; run with
    /// `--ignored` on a Linux computer with systemd.
    #[test]
    #[ignore = "needs systemd"]
    fn the_system_tools_answer() {
        let journald = service("systemd-journald").unwrap();
        assert!(journald.found && journald.running, "{journald:?}");
        assert!(!service("no-such-meshrmm-unit").unwrap().found);
        let now = crate::remote::monitors::now_ms();
        let filter = EventFilter::default();
        events(
            "",
            &filter,
            &LogWindow {
                now_ms: now,
                since_ms: now - 24 * 60 * 60 * 1000,
                interval_ms: 24 * 60 * 60 * 1000,
            },
            Duration::from_secs(20),
            &mut None,
        )
        .unwrap();
        assert!(ping("127.0.0.1", Duration::from_secs(2)).unwrap().is_some());
        assert!(ping("no-such-host.invalid", Duration::from_secs(2)).is_err());
    }

    #[test]
    fn this_process_is_found_by_name() {
        let me = std::fs::read_to_string("/proc/self/comm").unwrap();
        let found = processes(me.trim()).unwrap();
        assert!(
            found
                .iter()
                .any(|process| process.pid == std::process::id())
        );
        assert!(processes("no-such-process-name").unwrap().is_empty());
    }
}
