//! The remote tools on Linux: processes from `/proc`, services from
//! systemd's units.
use std::collections::{BTreeSet, HashMap};
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

use meshrmm_protocol::{
    ProcessInfo, ServiceAction, ServiceInfo, ServiceStartup, ToolError, ToolErrorCode,
    valid_unit_name,
};

use super::parse;
use super::unix::{self, Signals};
use super::{Deadline, Reading, Snapshot, failed, own_service};
use crate::remote::monitors::{command, is_agent_service, is_protected_process};
use crate::remote::user_database::UserEntry;

/// How long a listing's `systemctl` may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// The units one `systemctl show` describes.
const SHOW_CHUNK: usize = 400;
const PROPERTIES: &str =
    "--property=Id,Description,LoadState,ActiveState,SubState,UnitFileState,MainPID,User";

struct Clock {
    /// Clock ticks per second.
    ticks: u64,
    page: u64,
    /// When the computer started, in milliseconds since the Unix epoch.
    boot_ms: Option<i64>,
}

impl Clock {
    fn read() -> Self {
        // SAFETY: sysconf only reads configuration.
        let (ticks, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK).max(1) as u64,
                libc::sysconf(libc::_SC_PAGESIZE).max(1) as u64,
            )
        };
        let boot_ms = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| parse::boot_time(&text))
            .map(|seconds| seconds * 1000);
        Self {
            ticks,
            page,
            boot_ms,
        }
    }

    fn started_at(&self, stat: &parse::ProcStat) -> Option<i64> {
        let after_boot = i64::try_from(stat.start_ticks.saturating_mul(1000) / self.ticks).ok()?;
        Some(self.boot_ms? + after_boot)
    }
}

fn stat(pid: u32) -> Option<parse::ProcStat> {
    parse::proc_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

pub(super) fn read_processes() -> Result<Snapshot, ToolError> {
    let clock = Clock::read();
    let mut users: HashMap<u32, Option<String>> = HashMap::new();
    let entries = std::fs::read_dir("/proc")
        .map_err(|error| failed(format!("Could not read /proc: {error}")))?;
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // Gone since the folder was read.
        let Some(stat) = stat(pid) else { continue };
        let uid = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .and_then(|text| parse::proc_status_uid(&text));
        let user = uid.and_then(|uid| {
            users
                .entry(uid)
                .or_insert_with(|| {
                    Some(UserEntry::of(uid).map_or_else(|| uid.to_string(), |entry| entry.name))
                })
                .clone()
        });
        let path = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|path| {
                let path = path.to_string_lossy();
                path.strip_suffix(" (deleted)").unwrap_or(&path).to_owned()
            });
        let command_line = std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .and_then(|bytes| parse::proc_cmdline(&bytes));
        let info = ProcessInfo {
            pid,
            parent_pid: (stat.ppid != 0).then_some(stat.ppid),
            name: stat.comm.clone(),
            path,
            user,
            cpu_percent: 0.0,
            memory_bytes: stat.rss_pages.saturating_mul(clock.page),
            started_at: clock.started_at(&stat),
            command_line,
            command_line_truncated: false,
            session: None,
        };
        processes.push(Reading {
            info,
            cpu_ms: stat.cpu_ticks.saturating_mul(1000) / clock.ticks,
        });
    }
    // SAFETY: sysconf only reads configuration.
    let logical_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as u32;
    Ok(Snapshot {
        processes,
        logical_cpus,
        memory_total_bytes: std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| parse::memory_total(&text)),
    })
}

/// The first process, the kernel's threads, the Agent, and what the system
/// can't do without.
pub(super) fn is_protected(info: &ProcessInfo) -> bool {
    info.pid <= 2
        || info.parent_pid == Some(2)
        || is_protected_process(&info.name)
        || std::env::current_exe()
            .ok()
            .zip(info.path.as_deref())
            .is_some_and(|(agent, path)| agent.as_os_str() == path)
}

struct Proc;

impl Proc {
    /// Whether `pid` is still the process listed, and hasn't exited.
    fn same(info: &ProcessInfo) -> bool {
        let clock = Clock::read();
        stat(info.pid)
            .is_some_and(|stat| stat.state != 'Z' && clock.started_at(&stat) == info.started_at)
    }
}

impl Signals for Proc {
    /// Signals through a pidfd where the kernel has them (5.3 and newer),
    /// which holds on to the process checked, so the ID can't pass to
    /// another in between.
    fn signal(&self, info: &ProcessInfo, signal: libc::c_int) -> Result<(), ToolError> {
        let pid = libc::pid_t::try_from(info.pid).map_err(|_| unix::changed(info))?;
        // SAFETY: pidfd_open takes a process ID and no flags.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOSYS) {
                return Err(unix::signal_error(info, error));
            }
            if !Self::same(info) {
                return Err(unix::changed(info));
            }
            // SAFETY: kill only sends a signal.
            return if unsafe { libc::kill(pid, signal) } == 0 {
                Ok(())
            } else {
                Err(unix::signal_error(info, std::io::Error::last_os_error()))
            };
        }
        // SAFETY: the descriptor was just opened and is owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
        if !Self::same(info) {
            return Err(unix::changed(info));
        }
        use std::os::fd::AsRawFd;
        // SAFETY: the descriptor is open; no signal information is passed.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent == 0 {
            Ok(())
        } else {
            Err(unix::signal_error(info, std::io::Error::last_os_error()))
        }
    }

    fn alive(&self, info: &ProcessInfo) -> bool {
        Self::same(info)
    }
}

pub(super) fn end_processes(
    targets: &[&ProcessInfo],
    deadline: &Deadline,
) -> Vec<Result<(), ToolError>> {
    unix::end(&Proc, targets, deadline)
}

fn check_unit(name: &str) -> Result<(), ToolError> {
    if valid_unit_name(name) {
        Ok(())
    } else {
        Err(ToolError::new(
            ToolErrorCode::Invalid,
            format!("“{name}” isn't a systemd unit name."),
        ))
    }
}

/// Runs `systemctl` with `arguments`, failing with what it said.
fn systemctl(arguments: &[&str], timeout: Duration) -> Result<command::Finished, ToolError> {
    let mut all = vec!["--no-pager", "--no-ask-password"];
    all.extend_from_slice(arguments);
    let finished = command::run("systemctl", &all, timeout).map_err(failed)?;
    if finished.code == Some(0) {
        return Ok(finished);
    }
    let complaint = finished.complaint();
    let code = if complaint.contains("not found") || complaint.contains("does not exist") {
        ToolErrorCode::NotFound
    } else if complaint.contains("Access denied") {
        ToolErrorCode::AccessDenied
    } else if complaint.contains("masked") {
        ToolErrorCode::Invalid
    } else {
        ToolErrorCode::Failed
    };
    Err(ToolError::new(
        code,
        if complaint.is_empty() {
            "systemctl failed.".to_owned()
        } else {
            complaint
        },
    ))
}

fn show(names: &[String]) -> Result<Vec<ServiceInfo>, ToolError> {
    let mut services = Vec::new();
    for chunk in names.chunks(SHOW_CHUNK) {
        let mut arguments = vec!["show", PROPERTIES, "--"];
        arguments.extend(chunk.iter().map(String::as_str));
        let finished = systemctl(&arguments, LIST_TIMEOUT)?;
        services.extend(
            parse::systemd_blocks(&finished.stdout)
                .iter()
                .filter_map(|block| {
                    let id = block.get("Id").map(String::as_str).unwrap_or_default();
                    parse::systemd_service(block, is_agent_service(id))
                }),
        );
    }
    Ok(services)
}

pub(super) fn list_services() -> Result<Vec<ServiceInfo>, ToolError> {
    let units = systemctl(
        &[
            "list-units",
            "--type=service",
            "--all",
            "--plain",
            "--no-legend",
        ],
        LIST_TIMEOUT,
    )?;
    let files = systemctl(
        &["list-unit-files", "--type=service", "--no-legend"],
        LIST_TIMEOUT,
    )?;
    let names = parse::listed_units(&units.stdout)
        .into_iter()
        .chain(parse::unit_files(&files.stdout))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    show(&names)
}

fn describe(name: &str) -> Result<ServiceInfo, ToolError> {
    show(&[name.to_owned()])?.pop().ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::NotFound,
            format!("{name} isn't a unit on this computer."),
        )
    })
}

/// How long a change may take: what is left of the deadline, and at least
/// a second.
fn left(deadline: &Deadline) -> Duration {
    deadline.left().max(Duration::from_secs(1))
}

/// `error` from a change, or the time running out if that stopped it.
fn stopped_by(error: ToolError, deadline: &Deadline, name: &str) -> ToolError {
    if deadline.passed() {
        ToolError::new(
            ToolErrorCode::TimedOut,
            format!(
                "{name} hadn't finished when the time ran out; refresh in a moment to see whether it did."
            ),
        )
    } else {
        error
    }
}

pub(super) fn control_service(
    name: &str,
    action: ServiceAction,
    deadline: &Deadline,
) -> Result<ServiceInfo, ToolError> {
    check_unit(name)?;
    let current = describe(name)?;
    if action != ServiceAction::Start && current.protected {
        return Err(own_service());
    }
    systemctl(&[action.as_str(), "--", &current.name], left(deadline))
        .map_err(|error| stopped_by(error, deadline, &current.name))?;
    describe(&current.name)
}

pub(super) fn set_service_startup(
    name: &str,
    startup: ServiceStartup,
    deadline: &Deadline,
) -> Result<ServiceInfo, ToolError> {
    check_unit(name)?;
    let current = describe(name)?;
    if current.protected {
        return Err(own_service());
    }
    if !current.startup_choices.contains(&startup) {
        return Err(ToolError::new(
            ToolErrorCode::Invalid,
            format!(
                "{} is {}: other units start it, so how it starts can't be changed.",
                current.name,
                current.startup_detail.as_deref().unwrap_or("static")
            ),
        ));
    }
    let unit = current.name.as_str();
    let run = |verb: &str| {
        systemctl(&[verb, "--", unit], left(deadline))
            .map_err(|error| stopped_by(error, deadline, unit))
    };
    if startup != ServiceStartup::Disabled && current.startup == Some(ServiceStartup::Disabled) {
        run("unmask")?;
    }
    match startup {
        ServiceStartup::Automatic => run("enable")?,
        ServiceStartup::Manual => run("disable")?,
        ServiceStartup::Disabled => run("mask")?,
        ServiceStartup::AutomaticDelayed | ServiceStartup::Static => {
            unreachable!("not among a unit's choices")
        }
    };
    describe(unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::ServiceState;

    #[test]
    fn kernel_threads_and_the_first_process_are_protected() {
        let snapshot = read_processes().unwrap();
        let init = snapshot
            .processes
            .iter()
            .find(|reading| reading.info.pid == 1)
            .expect("the first process is listed");
        assert!(is_protected(&init.info));
        let me = snapshot
            .processes
            .iter()
            .find(|reading| reading.info.pid == std::process::id())
            .unwrap();
        assert!(!is_protected(&me.info) || me.info.path.is_some());
    }

    /// Starts, stops, restarts and changes a throwaway unit this test makes
    /// and removes; run with `--ignored` as root on a computer with
    /// systemd.
    #[test]
    #[ignore = "needs root and systemd"]
    fn a_throwaway_unit_is_controlled() {
        const UNIT: &str = "meshrmm-tools-test.service";
        let path = format!("/run/systemd/system/{UNIT}");
        std::fs::write(
            &path,
            "[Unit]\nDescription=MeshRMM tools test\n[Service]\nExecStart=/bin/sleep 600\n[Install]\nWantedBy=multi-user.target\n",
        )
        .unwrap();
        struct Remove(String);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = command::run("systemctl", &["stop", UNIT], Duration::from_secs(20));
                let _ = command::run("systemctl", &["unmask", UNIT], Duration::from_secs(20));
                let _ = std::fs::remove_file(&self.0);
                let _ = command::run("systemctl", &["daemon-reload"], Duration::from_secs(20));
            }
        }
        let _remove = Remove(path);
        systemctl(&["daemon-reload"], LIST_TIMEOUT).unwrap();
        let deadline = Deadline::distant();
        let listed = list_services().unwrap();
        let unit = listed
            .iter()
            .find(|service| service.name == UNIT)
            .expect("listed");
        assert_eq!(unit.state, ServiceState::Stopped);
        assert_eq!(unit.description.as_deref(), Some("MeshRMM tools test"));
        let started = control_service(UNIT, ServiceAction::Start, &deadline).unwrap();
        assert_eq!(started.state, ServiceState::Running);
        let pid = started.pid.unwrap();
        let restarted = control_service(UNIT, ServiceAction::Restart, &deadline).unwrap();
        assert_eq!(restarted.state, ServiceState::Running);
        assert_ne!(restarted.pid, Some(pid));
        let stopped = control_service(UNIT, ServiceAction::Stop, &deadline).unwrap();
        assert_eq!(stopped.state, ServiceState::Stopped);
        let masked = set_service_startup(UNIT, ServiceStartup::Disabled, &deadline).unwrap();
        assert_eq!(masked.startup, Some(ServiceStartup::Disabled));
        let error = control_service(UNIT, ServiceAction::Start, &deadline).unwrap_err();
        assert_eq!(error.code, ToolErrorCode::Invalid, "{error:?}");
        let manual = set_service_startup(UNIT, ServiceStartup::Manual, &deadline).unwrap();
        assert_eq!(manual.startup, Some(ServiceStartup::Manual));
        let missing = control_service(
            "meshrmm-no-such-unit.service",
            ServiceAction::Start,
            &deadline,
        )
        .unwrap_err();
        assert_eq!(missing.code, ToolErrorCode::NotFound, "{missing:?}");
    }
}
