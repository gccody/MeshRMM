//! What each kind of monitor checks, and the self-healing actions.
use std::time::Duration;

use meshrmm_protocol::{
    MAX_MONITOR_OUTPUT_BYTES, MonitorCheck, ProcessAlert, RemediationAction, RunAs, ScriptLanguage,
    ScriptRunRequest, ScriptRunStatus,
};

use super::parse::{self, EventFilter, LogEvent, LogRead, ProcessSample, ServiceStatus};
use super::{Context, Memory, Outcome, network, platform};

/// Events listed in a failing result's output.
const MAX_LISTED_EVENTS: usize = 20;
/// How long CPU use is measured over.
const CPU_SAMPLE: Duration = Duration::from_secs(1);

pub(super) fn run(check: &MonitorCheck, context: &Context, memory: &mut Memory) -> Outcome {
    match check {
        MonitorCheck::Service { name, automatic } => service(name, *automatic),
        MonitorCheck::Process {
            name,
            alert_when,
            percent,
            megabytes,
            minutes,
        } => process(
            name,
            *alert_when,
            Threshold {
                percent: *percent,
                megabytes: *megabytes,
                minutes: minutes.unwrap_or(0),
            },
            context,
            memory,
        ),
        MonitorCheck::EventLog {
            log,
            source,
            event_ids,
            levels,
            message,
        } => {
            let filter = EventFilter {
                source: source.clone(),
                event_ids: event_ids.clone(),
                levels: levels.clone(),
                message: message.clone(),
            };
            events(log, &filter, context, memory)
        }
        MonitorCheck::Ping { host } => network::ping(host, context.timeout),
        MonitorCheck::TcpPort { host, port } => network::tcp(host, *port, context.timeout),
        MonitorCheck::Http {
            url,
            status,
            keyword,
            verify_tls,
        } => network::http(url, *status, keyword, *verify_tls, context.timeout),
        MonitorCheck::Certificate { host, port, days } => {
            network::certificate(host, *port, *days, context.timeout, context.now_ms)
        }
        MonitorCheck::ScriptCheck {
            run_as,
            language,
            body,
            ..
        } => script(*language, body, *run_as, context),
    }
}

fn service(name: &str, automatic: bool) -> Outcome {
    match platform::service(name) {
        Ok(status) => judge_service(name, automatic, &status),
        Err(error) => Outcome::failing(format!("Could not read “{name}”: {error:#}")),
    }
}

pub(super) fn judge_service(name: &str, automatic: bool, status: &ServiceStatus) -> Outcome {
    if !status.found {
        return Outcome::failing(format!("“{name}” isn't installed."));
    }
    if !status.running {
        return Outcome::failing(format!("“{name}” isn't running ({}).", status.state));
    }
    if automatic && status.automatic == Some(false) {
        return Outcome::failing(format!(
            "“{name}” is running but isn't set to start automatically."
        ));
    }
    Outcome::passing(format!("“{name}” is running."))
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Threshold {
    pub(super) percent: Option<u32>,
    pub(super) megabytes: Option<u64>,
    pub(super) minutes: u32,
}

fn process(
    name: &str,
    alert_when: ProcessAlert,
    threshold: Threshold,
    context: &Context,
    memory: &mut Memory,
) -> Outcome {
    let first = match platform::processes(name) {
        Ok(processes) => processes,
        Err(error) => return Outcome::failing(format!("Could not list processes: {error:#}")),
    };
    let cpu_percent = if alert_when == ProcessAlert::CpuAbove && !first.is_empty() {
        let started = std::time::Instant::now();
        std::thread::sleep(CPU_SAMPLE.min(context.timeout / 2));
        let second = match platform::processes(name) {
            Ok(processes) => processes,
            Err(error) => {
                return Outcome::failing(format!("Could not list processes: {error:#}"));
            }
        };
        let cores = std::thread::available_parallelism().map_or(1, usize::from);
        Some(cpu_percent(&first, &second, started.elapsed(), cores))
    } else {
        None
    };
    judge_process(
        name,
        alert_when,
        threshold,
        &first,
        cpu_percent,
        context.now_ms,
        memory,
    )
}

/// The CPU the processes in `second` used since `first`, as a percentage of
/// every core's time.
pub(super) fn cpu_percent(
    first: &[ProcessSample],
    second: &[ProcessSample],
    elapsed: Duration,
    cores: usize,
) -> f64 {
    let used = second
        .iter()
        .map(|process| {
            let before = first
                .iter()
                .find(|earlier| earlier.pid == process.pid)
                .map_or(process.cpu_ms, |earlier| earlier.cpu_ms);
            process.cpu_ms.saturating_sub(before)
        })
        .fold(0, u64::saturating_add) as f64;
    let available = elapsed.as_secs_f64() * 1000.0 * cores.max(1) as f64;
    if available <= 0.0 {
        return 0.0;
    }
    (used / available * 100.0).min(100.0)
}

/// "1 minute" or "N minutes".
fn minutes(count: i64) -> String {
    if count == 1 {
        "1 minute".to_owned()
    } else {
        format!("{count} minutes")
    }
}

pub(super) fn judge_process(
    name: &str,
    alert_when: ProcessAlert,
    threshold: Threshold,
    processes: &[ProcessSample],
    cpu_percent: Option<f64>,
    now_ms: i64,
    memory: &mut Memory,
) -> Outcome {
    let count = processes.len();
    let running = match count {
        0 => format!("“{name}” isn't running."),
        1 => format!("“{name}” is running."),
        _ => format!("{count} “{name}” processes are running."),
    };
    let over = |load: f64, limit: f64, what: String, memory: &mut Memory| {
        if load <= limit {
            memory.over_since_ms = None;
            return Outcome::passing(what).value(load);
        }
        let since = *memory.over_since_ms.get_or_insert(now_ms);
        let lasted = now_ms.saturating_sub(since) / 60_000;
        let sustained = lasted >= i64::from(threshold.minutes);
        Outcome::when(
            !sustained,
            if sustained {
                format!("{what}, for {} or more.", minutes(lasted))
            } else {
                format!(
                    "{what}, for less than {} so far.",
                    minutes(i64::from(threshold.minutes))
                )
            },
        )
        .value(load)
    };
    match alert_when {
        ProcessAlert::NotRunning => Outcome::when(count > 0, running).value(count as f64),
        ProcessAlert::Running => Outcome::when(count == 0, running).value(count as f64),
        ProcessAlert::CpuAbove => {
            if count == 0 {
                memory.over_since_ms = None;
                return Outcome::passing(running).value(0.0);
            }
            let load = cpu_percent.unwrap_or(0.0);
            let limit = f64::from(threshold.percent.unwrap_or(100));
            over(
                (load * 10.0).round() / 10.0,
                limit,
                format!("“{name}” uses {load:.1}% CPU (the limit is {limit}%)"),
                memory,
            )
        }
        ProcessAlert::MemoryAbove => {
            if count == 0 {
                memory.over_since_ms = None;
                return Outcome::passing(running).value(0.0);
            }
            let megabytes = processes
                .iter()
                .map(|process| process.memory_bytes)
                .fold(0, u64::saturating_add) as f64
                / (1024.0 * 1024.0);
            let megabytes = megabytes.round();
            let limit = threshold.megabytes.unwrap_or(u64::MAX) as f64;
            over(
                megabytes,
                limit,
                format!("“{name}” uses {megabytes} MB of memory (the limit is {limit} MB)"),
                memory,
            )
        }
    }
}

fn events(log: &str, filter: &EventFilter, context: &Context, memory: &mut Memory) -> Outcome {
    // Since the last check, or one interval back on the first.
    let since_ms = memory
        .checked_at_ms
        .filter(|checked| *checked < context.now_ms)
        .unwrap_or(context.now_ms - context.interval.as_millis() as i64);
    match platform::events(
        log,
        filter,
        since_ms,
        context.now_ms,
        context.timeout,
        &mut memory.log_record,
    ) {
        Ok(read) => judge_events(&read, filter, log, context.timeout),
        Err(error) => Outcome::failing(format!("Could not read the log: {error:#}")),
    }
}

/// Fails when any event the read found since the last check matches the
/// filter. A read that stopped at its cap or at the check's time limit
/// (`timeout`) says so, since older entries went unread.
pub(super) fn judge_events(
    read: &LogRead,
    filter: &EventFilter,
    log: &str,
    timeout: Duration,
) -> Outcome {
    let unread = if read.out_of_time {
        let search = if filter.message.is_empty() {
            "read"
        } else {
            "search by message text"
        };
        Some(format!(
            "the “{log}” log is too busy to {search} within the {}-second time limit, so older entries since the last check weren't looked at; a source or event IDs would narrow the search",
            timeout.as_secs()
        ))
    } else if read.truncated {
        Some(
            "the log had more entries than one check reads, so older ones weren't looked at"
                .to_owned(),
        )
    } else {
        None
    };
    let mut matching = read
        .events
        .iter()
        .filter(|event| filter.matches(event))
        .collect::<Vec<_>>();
    matching.sort_by_key(|event| std::cmp::Reverse(event.at_ms));
    let count = matching.len();
    let Some(latest) = matching.first() else {
        let detail = match &unread {
            Some(unread) => {
                format!("No matching events in the newest entries since the last check; {unread}.")
            }
            None => "No matching events since the last check.".to_owned(),
        };
        return Outcome::passing(detail).value(0.0);
    };
    let describe = |event: &LogEvent| {
        let mut line = parse::format_time(event.at_ms);
        if let Some(level) = event.level {
            line.push_str(&format!(" {}", level.label()));
        }
        if let Some(source) = filter.source_of(event) {
            line.push_str(&format!(" {source}"));
        }
        if let Some(id) = event.event_id {
            line.push_str(&format!(" {id}"));
        }
        let message = event.message.lines().next().unwrap_or_default().trim();
        if !message.is_empty() {
            line.push_str(&format!(": {message}"));
        }
        line
    };
    let mut output = String::new();
    for event in matching.iter().take(MAX_LISTED_EVENTS) {
        output.push_str(&describe(event));
        output.push('\n');
        if output.len() > MAX_MONITOR_OUTPUT_BYTES {
            break;
        }
    }
    if count > MAX_LISTED_EVENTS {
        output.push_str(&format!("… and {} more\n", count - MAX_LISTED_EVENTS));
    }
    let events = if count == 1 {
        "1 matching event".to_owned()
    } else {
        format!("{count} matching events")
    };
    let detail = if let Some(unread) = &unread {
        output.push_str(&format!("({unread})\n"));
        format!(
            "At least {events} since the last check ({unread}); the latest: {}",
            describe(latest)
        )
    } else if count == 1 {
        format!("{events} since the last check: {}", describe(latest))
    } else {
        format!(
            "{events} since the last check; the latest: {}",
            describe(latest)
        )
    };
    Outcome::failing(detail).value(count as f64).output(output)
}

fn script(
    language: Option<ScriptLanguage>,
    body: &str,
    run_as: RunAs,
    context: &Context,
) -> Outcome {
    let Some(language) = language else {
        return Outcome::failing("The server didn't send the script.");
    };
    let run = ScriptRunRequest {
        run_id: format!("monitor-{}", uuid::Uuid::new_v4().simple()),
        language,
        body: body.to_owned(),
        run_as,
        timeout_seconds: context.timeout.as_secs().max(1) as u32,
    };
    let report = crate::remote::toolbox::run_check(context.mode, &run);
    let mut output = report.stdout;
    if !report.stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&report.stderr);
    }
    let outcome = match (report.status, report.exit_code) {
        (ScriptRunStatus::Completed, Some(0)) => Outcome::passing(format!(
            "The script exited with code 0 as {}.",
            report.ran_as
        )),
        (ScriptRunStatus::Completed, Some(code)) => Outcome::failing(format!(
            "The script exited with code {code} as {}.",
            report.ran_as
        )),
        _ => Outcome::failing(
            report
                .error
                .unwrap_or_else(|| "The script didn't finish.".to_owned()),
        ),
    };
    let value = report.exit_code.map(f64::from);
    let outcome = outcome.output(output);
    match value {
        Some(value) => outcome.value(value),
        None => outcome,
    }
}

/// How long a restarted service must keep running for the restart to count.
const RESTART_SETTLE: Duration = Duration::from_secs(3);

/// Whether a restart held: the service's manager still says it runs
/// [`RESTART_SETTLE`] after it.
fn restart_held(name: &str, status: &ServiceStatus) -> anyhow::Result<String> {
    anyhow::ensure!(
        status.found,
        "Restarted “{name}”, but then it was gone ({}).",
        status.state
    );
    anyhow::ensure!(
        status.running,
        "Restarted “{name}”, but it stopped again within {} seconds ({}).",
        RESTART_SETTLE.as_secs(),
        status.state
    );
    Ok(format!("Restarted “{name}”."))
}

/// Runs a self-healing action, returning what it did.
pub(super) fn act(action: &RemediationAction) -> anyhow::Result<String> {
    match action {
        RemediationAction::RestartService { name } => {
            anyhow::ensure!(
                !platform::is_agent_service(name),
                "The Agent doesn't restart its own service."
            );
            platform::restart_service(name)?;
            // A restart can return before a service that crashes at once has
            // died, and the check that follows the attempt would then call
            // it fixed: systemd's simple units count as started once their
            // program is, `launchctl kickstart` returns at once, and a
            // Windows service can stop right after reporting it runs.
            std::thread::sleep(RESTART_SETTLE);
            restart_held(name, &platform::service(name)?)
        }
        RemediationAction::EndProcess { name } => {
            anyhow::ensure!(
                !platform::is_protected_process(name),
                "The Agent doesn't end “{name}”: the device needs it."
            );
            let ended = platform::end_processes(name)?;
            Ok(match ended {
                0 => format!("“{name}” wasn't running."),
                1 => format!("Ended “{name}”."),
                _ => format!("Ended {ended} “{name}” processes."),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::EventLevel;

    use super::*;

    fn sample(pid: u32, cpu_ms: u64, memory_bytes: u64) -> ProcessSample {
        ProcessSample {
            pid,
            names: vec!["app".into()],
            cpu_ms,
            memory_bytes,
        }
    }

    #[test]
    fn a_restart_counts_only_if_the_service_still_runs() {
        // A systemd simple unit whose program exits at once, as
        // `systemctl show` reports it a few seconds after the restart.
        let crashed = parse::systemd_unit(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=enabled\n",
        );
        let error = restart_held("web", &crashed).unwrap_err().to_string();
        assert_eq!(
            error,
            "Restarted “web”, but it stopped again within 3 seconds (failed (failed))."
        );
        let looping = parse::systemd_unit(
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\n",
        );
        assert!(
            restart_held("web", &looping).is_err(),
            "restarting again isn't running"
        );
        let up = parse::systemd_unit(
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n",
        );
        assert_eq!(restart_held("web", &up).unwrap(), "Restarted “web”.");

        // A launchd job that launchd will spawn again after a crash.
        let print = "system/com.example.web = {\n\tstate = spawn scheduled\n}\n";
        assert!(restart_held("com.example.web", &parse::launchd_job(Some(print))).is_err());
        let print = "system/com.example.web = {\n\tstate = running\n}\n";
        assert!(restart_held("com.example.web", &parse::launchd_job(Some(print))).is_ok());

        // A Windows service the Service Control Manager reports stopped.
        let stopped = ServiceStatus {
            found: true,
            running: false,
            automatic: Some(true),
            state: "stopped".into(),
        };
        assert!(
            restart_held("Spooler", &stopped)
                .unwrap_err()
                .to_string()
                .ends_with("(stopped).")
        );
        let gone = ServiceStatus {
            found: false,
            running: false,
            automatic: None,
            state: "not installed".into(),
        };
        assert_eq!(
            restart_held("Spooler", &gone).unwrap_err().to_string(),
            "Restarted “Spooler”, but then it was gone (not installed)."
        );
    }

    #[test]
    fn services_fail_when_missing_stopped_or_manual() {
        let status = |found, running, automatic| ServiceStatus {
            found,
            running,
            automatic,
            state: "inactive (dead)".into(),
        };
        assert!(!judge_service("cron", false, &status(false, false, None)).passing);
        let stopped = judge_service("cron", false, &status(true, false, Some(true)));
        assert_eq!(stopped.detail, "“cron” isn't running (inactive (dead)).");
        assert!(judge_service("cron", false, &status(true, true, Some(false))).passing);
        assert!(!judge_service("cron", true, &status(true, true, Some(false))).passing);
        assert!(judge_service("cron", true, &status(true, true, None)).passing);
    }

    #[test]
    fn processes_are_judged_by_presence_and_sustained_load() {
        let mut memory = Memory::default();
        let none = Threshold {
            percent: None,
            megabytes: None,
            minutes: 0,
        };
        let running = [sample(1, 0, 0)];
        assert!(
            !judge_process(
                "app",
                ProcessAlert::NotRunning,
                none,
                &[],
                None,
                0,
                &mut memory
            )
            .passing
        );
        assert!(
            judge_process(
                "app",
                ProcessAlert::NotRunning,
                none,
                &running,
                None,
                0,
                &mut memory
            )
            .passing
        );
        assert!(
            !judge_process(
                "app",
                ProcessAlert::Running,
                none,
                &running,
                None,
                0,
                &mut memory
            )
            .passing
        );

        let cpu = Threshold {
            percent: Some(50),
            megabytes: None,
            minutes: 5,
        };
        let busy = |now, memory: &mut Memory| {
            judge_process(
                "app",
                ProcessAlert::CpuAbove,
                cpu,
                &running,
                Some(80.0),
                now,
                memory,
            )
        };
        assert!(busy(0, &mut memory).passing, "not for long enough yet");
        assert!(busy(4 * 60_000, &mut memory).passing);
        let sustained = busy(5 * 60_000, &mut memory);
        assert!(!sustained.passing);
        assert_eq!(sustained.value, Some(80.0));
        assert!(
            judge_process(
                "app",
                ProcessAlert::CpuAbove,
                cpu,
                &running,
                Some(10.0),
                6 * 60_000,
                &mut memory
            )
            .passing
        );
        assert_eq!(memory.over_since_ms, None, "dropping below starts over");

        let big = Threshold {
            percent: None,
            megabytes: Some(100),
            minutes: 0,
        };
        let heavy = [sample(1, 0, 80 << 20), sample(2, 0, 40 << 20)];
        let outcome = judge_process(
            "app",
            ProcessAlert::MemoryAbove,
            big,
            &heavy,
            None,
            0,
            &mut memory,
        );
        assert!(!outcome.passing, "processes of one name add up");
        assert_eq!(outcome.value, Some(120.0));

        let one_minute = Threshold {
            percent: Some(50),
            minutes: 1,
            ..cpu
        };
        let mut memory = Memory::default();
        let mut busy_for = |now| {
            judge_process(
                "app",
                ProcessAlert::CpuAbove,
                one_minute,
                &running,
                Some(80.0),
                now,
                &mut memory,
            )
            .detail
        };
        assert!(busy_for(0).ends_with("for less than 1 minute so far."));
        assert!(busy_for(60_000).ends_with("for 1 minute or more."));
        assert!(busy_for(120_000).ends_with("for 2 minutes or more."));
    }

    #[test]
    fn cpu_is_shared_across_cores() {
        let first = [sample(1, 1000, 0), sample(2, 0, 0)];
        let second = [sample(1, 1500, 0), sample(2, 500, 0), sample(3, 1000, 0)];
        // 0.5 s + 0.5 s over 1 s of 4 cores; a process that started in
        // between has no use measured yet.
        assert_eq!(
            cpu_percent(&first, &second, Duration::from_secs(1), 4),
            25.0
        );
        assert_eq!(cpu_percent(&first, &first, Duration::ZERO, 4), 0.0);
    }

    #[test]
    fn events_fail_the_check_and_are_listed() {
        let events = include_str!("testdata/journal.json")
            .lines()
            .filter_map(parse::journal_event)
            .collect::<Vec<_>>();
        let mut read = LogRead {
            events,
            ..LogRead::default()
        };
        let filter = EventFilter {
            levels: vec![EventLevel::Error, EventLevel::Warning],
            message: "disk".into(),
            ..EventFilter::default()
        };
        let judge = |read: &LogRead| judge_events(read, &filter, "System", Duration::from_secs(30));
        let outcome = judge(&read);
        assert!(!outcome.passing);
        assert_eq!(outcome.value, Some(1.0));
        assert!(
            outcome
                .detail
                .starts_with("1 matching event since the last check: "),
            "{}",
            outcome.detail
        );
        assert!(
            outcome
                .output
                .contains("Warning meshrmm-fixture: Disk \"D:\" failed & retried"),
            "{}",
            outcome.output
        );
        let nothing = LogRead::default();
        let calm = judge(&nothing);
        assert!(calm.passing);
        assert_eq!(calm.detail, "No matching events since the last check.");

        // A read that stopped at its cap says so.
        read.truncated = true;
        let capped = judge(&read);
        assert!(!capped.passing);
        assert!(
            capped
                .detail
                .starts_with("At least 1 matching event since the last check (the log had more entries than one check reads"),
            "{}",
            capped.detail
        );
        assert!(capped.output.contains("older ones weren't looked at"));
        let capped_calm = judge(&LogRead {
            truncated: true,
            ..LogRead::default()
        });
        assert!(capped_calm.passing);
        assert!(
            capped_calm.detail.contains("older ones weren't looked at"),
            "{}",
            capped_calm.detail
        );
    }

    #[test]
    fn a_read_out_of_time_keeps_what_it_found_and_says_why() {
        let events = include_str!("testdata/journal.json")
            .lines()
            .filter_map(parse::journal_event)
            .collect::<Vec<_>>();
        let filter = EventFilter {
            message: "disk".into(),
            ..EventFilter::default()
        };
        let busy = LogRead {
            events,
            truncated: false,
            out_of_time: true,
        };
        const BUSY: &str = "the “Security” log is too busy to search by message text within the 30-second time limit, so older entries since the last check weren't looked at; a source or event IDs would narrow the search";
        let found = judge_events(&busy, &filter, "Security", Duration::from_secs(30));
        assert!(!found.passing, "what it found raises the alert");
        assert_eq!(found.value, Some(1.0));
        assert!(
            found.detail.starts_with(&format!(
                "At least 1 matching event since the last check ({BUSY}); the latest: "
            )),
            "{}",
            found.detail
        );
        assert!(found.detail.chars().count() <= meshrmm_protocol::MAX_MONITOR_DETAIL_CHARS);
        assert!(found.output.contains("Disk \"D:\" failed & retried"));
        assert!(found.output.ends_with(&format!("({BUSY})\n")));

        let none = judge_events(
            &LogRead {
                out_of_time: true,
                ..LogRead::default()
            },
            &filter,
            "Security",
            Duration::from_secs(30),
        );
        assert!(none.passing);
        assert_eq!(
            none.detail,
            format!("No matching events in the newest entries since the last check; {BUSY}.")
        );
        // Without a message to search for, it says it couldn't read them.
        let plain = judge_events(
            &LogRead {
                out_of_time: true,
                ..LogRead::default()
            },
            &EventFilter::default(),
            "Security",
            Duration::from_secs(30),
        );
        assert!(
            plain
                .detail
                .contains("the “Security” log is too busy to read within the 30-second"),
            "{}",
            plain.detail
        );
    }
}
