//! The remote tools on a Mac: processes from libproc, services from
//! launchd's system domain, as the monitors watch them.
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use meshrmm_protocol::{
    ProcessInfo, ServiceAction, ServiceInfo, ServiceStartup, ServiceState, ToolError,
    ToolErrorCode, valid_launchd_label,
};

use super::parse;
use super::unix::{self, Signals};
use super::{Deadline, Reading, Snapshot, failed, own_service};
use crate::remote::monitors::{command, is_agent_service, is_protected_process};
use crate::remote::user_database::UserEntry;

const LIST_TIMEOUT: Duration = Duration::from_secs(20);
/// Where the system domain's jobs are defined, a job's file named for its
/// label.
const DAEMON_FOLDERS: [&str; 2] = ["/Library/LaunchDaemons", "/System/Library/LaunchDaemons"];
/// The ways a job may be set to start: enabled, or disabled.
const CHOICES: [ServiceStartup; 2] = [ServiceStartup::Automatic, ServiceStartup::Disabled];
/// `SZOMB` in `pbi_status`: exited, waiting for its parent.
const ZOMBIE: u32 = 5;

fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is sized for the flavour asked for.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: the call filled it.
    (written == size).then(|| unsafe { info.assume_init() })
}

fn task_info(pid: u32) -> Option<libc::proc_taskinfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: as for `bsd_info`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: the call filled it.
    (written == size).then(|| unsafe { info.assume_init() })
}

fn started_at(info: &libc::proc_bsdinfo) -> Option<i64> {
    let seconds = i64::try_from(info.pbi_start_tvsec).ok()?;
    let micros = i64::try_from(info.pbi_start_tvusec).ok()?;
    Some(seconds * 1000 + micros / 1000)
}

fn text(characters: &[libc::c_char]) -> String {
    let bytes = characters
        .iter()
        .take_while(|character| **character != 0)
        .map(|character| *character as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn path(pid: u32) -> Option<String> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer holds the size given.
    let length = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

fn command_line(pid: u32) -> Option<String> {
    let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer only asks for the size.
    if unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size == 0
    {
        return None;
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer holds `size` bytes.
    if unsafe {
        libc::sysctl(
            name.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buffer.truncate(size);
    parse::procargs2(&buffer)
}

// libc deprecates its Mach bindings in favor of the mach2 crate; this one
// is declared here instead.
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
}

/// Nanoseconds in each unit of `pti_total_user` and `pti_total_system`,
/// which count Mach absolute time.
fn timebase() -> (u64, u64) {
    let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
    // SAFETY: the structure is the one the call fills.
    unsafe { mach_timebase_info(&mut info) };
    (u64::from(info.numer.max(1)), u64::from(info.denom.max(1)))
}

fn memory_total() -> Option<u64> {
    let mut total: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: the output is sized and the name is NUL-terminated.
    let read = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut total).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (read == 0).then_some(total)
}

fn all_pids() -> Vec<u32> {
    // SAFETY: a null buffer only asks for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started meanwhile.
    let mut pids = vec![0 as libc::pid_t; count as usize + 64];
    let size = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: the buffer holds `size` bytes.
    let found = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    pids.truncate(found.max(0) as usize);
    pids.into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .collect()
}

pub(super) fn read_processes() -> Result<Snapshot, ToolError> {
    let (numer, denom) = timebase();
    let mut users: HashMap<u32, Option<String>> = HashMap::new();
    let mut processes = Vec::new();
    for pid in all_pids() {
        let Some(bsd) = bsd_info(pid) else { continue };
        let task = task_info(pid);
        let uid = bsd.pbi_uid;
        let user = users
            .entry(uid)
            .or_insert_with(|| {
                Some(UserEntry::of(uid).map_or_else(|| uid.to_string(), |entry| entry.name))
            })
            .clone();
        let long_name = text(&bsd.pbi_name);
        let path = path(pid);
        let name = path
            .as_deref()
            .and_then(|path| Path::new(path).file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .or_else(|| (!long_name.is_empty()).then_some(long_name))
            .unwrap_or_else(|| text(&bsd.pbi_comm));
        let info = ProcessInfo {
            pid,
            parent_pid: (bsd.pbi_ppid != 0).then_some(bsd.pbi_ppid),
            name,
            path,
            user,
            cpu_percent: 0.0,
            memory_bytes: task.map(|task| task.pti_resident_size).unwrap_or_default(),
            started_at: started_at(&bsd),
            command_line: command_line(pid),
            command_line_truncated: false,
            session: None,
        };
        let cpu_ms = task
            .map(|task| {
                let units = task.pti_total_user.saturating_add(task.pti_total_system);
                (u128::from(units) * u128::from(numer) / u128::from(denom) / 1_000_000) as u64
            })
            .unwrap_or_default();
        processes.push(Reading { info, cpu_ms });
    }
    // SAFETY: sysconf only reads configuration.
    let logical_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as u32;
    Ok(Snapshot {
        processes,
        logical_cpus,
        memory_total_bytes: memory_total(),
    })
}

/// The kernel, launchd, the Agent, and what macOS can't do without.
pub(super) fn is_protected(info: &ProcessInfo) -> bool {
    info.pid <= 1
        || is_protected_process(&info.name)
        || std::env::current_exe()
            .ok()
            .zip(info.path.as_deref())
            .is_some_and(|(agent, path)| agent.as_os_str() == path)
}

struct Proc;

impl Signals for Proc {
    fn signal(&self, info: &ProcessInfo, signal: libc::c_int) -> Result<(), ToolError> {
        if !self.alive(info) {
            return Err(unix::changed(info));
        }
        let pid = libc::pid_t::try_from(info.pid).map_err(|_| unix::changed(info))?;
        // SAFETY: kill only sends a signal.
        if unsafe { libc::kill(pid, signal) } == 0 {
            Ok(())
        } else {
            Err(unix::signal_error(info, std::io::Error::last_os_error()))
        }
    }

    fn alive(&self, info: &ProcessInfo) -> bool {
        bsd_info(info.pid)
            .is_some_and(|bsd| bsd.pbi_status != ZOMBIE && started_at(&bsd) == info.started_at)
    }
}

pub(super) fn end_processes(
    targets: &[&ProcessInfo],
    deadline: &Deadline,
) -> Vec<Result<(), ToolError>> {
    unix::end(&Proc, targets, deadline)
}

fn check_label(label: &str) -> Result<(), ToolError> {
    if valid_launchd_label(label) {
        Ok(())
    } else {
        Err(ToolError::new(
            ToolErrorCode::Invalid,
            format!("“{label}” isn't a launchd job label."),
        ))
    }
}

fn launchctl(arguments: &[&str], timeout: Duration) -> Result<command::Finished, ToolError> {
    let finished = command::run("launchctl", arguments, timeout).map_err(failed)?;
    if finished.code == Some(0) {
        return Ok(finished);
    }
    let complaint = finished.complaint();
    let code = if complaint.contains("Operation not permitted") {
        ToolErrorCode::AccessDenied
    } else if complaint.contains("Could not find") || complaint.contains("No such process") {
        ToolErrorCode::NotFound
    } else {
        ToolErrorCode::Failed
    };
    Err(ToolError::new(
        code,
        if complaint.is_empty() {
            format!("launchctl {} failed.", arguments.first().unwrap_or(&""))
        } else {
            complaint
        },
    ))
}

/// The job file for `label` in the system domain's folders, if there is one.
fn job_file(label: &str) -> Option<String> {
    DAEMON_FOLDERS
        .iter()
        .map(|folder| format!("{folder}/{label}.plist"))
        .find(|path| Path::new(path).is_file())
}

/// The labels of the job files in the system domain's folders.
fn job_files() -> Vec<String> {
    DAEMON_FOLDERS
        .iter()
        .filter_map(|folder| std::fs::read_dir(folder).ok())
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".plist").map(str::to_owned)
        })
        .filter(|label| valid_launchd_label(label))
        .collect()
}

fn describe(
    job: Option<&parse::LaunchdJob>,
    label: &str,
    disabled: bool,
    users: &mut HashMap<u32, Option<String>>,
) -> ServiceInfo {
    let protected = is_agent_service(label);
    let (state, detail) = match job {
        None => (ServiceState::Stopped, "not loaded".to_owned()),
        Some(parse::LaunchdJob { pid: Some(_), .. }) => {
            (ServiceState::Running, "running".to_owned())
        }
        Some(parse::LaunchdJob {
            status: Some(status),
            ..
        }) if *status != 0 => (
            ServiceState::Failed,
            if *status < 0 {
                format!("ended by signal {}", -status)
            } else {
                format!("last exited with {status}")
            },
        ),
        Some(_) => (ServiceState::Stopped, "not running".to_owned()),
    };
    let pid = job.and_then(|job| job.pid);
    let account = pid.and_then(|pid| {
        let uid = bsd_info(pid)?.pbi_uid;
        users
            .entry(uid)
            .or_insert_with(|| UserEntry::of(uid).map(|entry| entry.name))
            .clone()
    });
    ServiceInfo {
        name: label.to_owned(),
        display_name: label.to_owned(),
        state,
        state_detail: Some(detail),
        startup: Some(if disabled {
            ServiceStartup::Disabled
        } else {
            ServiceStartup::Automatic
        }),
        startup_detail: Some(if disabled { "disabled" } else { "enabled" }.to_owned()),
        startup_choices: if protected {
            Vec::new()
        } else {
            CHOICES.to_vec()
        },
        account,
        description: job_file(label),
        pid,
        protected,
    }
}

pub(super) fn list_services() -> Result<Vec<ServiceInfo>, ToolError> {
    // As root, `launchctl list` lists the system domain's jobs.
    let listed = launchctl(&["list"], LIST_TIMEOUT)?;
    let disabled = launchctl(&["print-disabled", "system"], LIST_TIMEOUT)?;
    let disabled = parse::launchd_disabled(&disabled.stdout);
    let mut jobs = parse::launchctl_list(&listed.stdout)
        .into_iter()
        .map(|job| (job.label.clone(), Some(job)))
        .collect::<BTreeMap<_, _>>();
    for label in job_files().into_iter().chain(disabled.keys().cloned()) {
        jobs.entry(label).or_insert(None);
    }
    let mut users = HashMap::new();
    Ok(jobs
        .iter()
        .map(|(label, job)| {
            describe(
                job.as_ref(),
                label,
                disabled.get(label).copied().unwrap_or(false),
                &mut users,
            )
        })
        .collect())
}

fn one(label: &str) -> Result<ServiceInfo, ToolError> {
    list_services()?
        .into_iter()
        .find(|service| service.name == label)
        .ok_or_else(|| {
            ToolError::new(
                ToolErrorCode::NotFound,
                format!("{label} isn't a job in launchd's system domain."),
            )
        })
}

fn left(deadline: &Deadline) -> Duration {
    deadline.left().max(Duration::from_secs(1))
}

/// Loads `label`'s job from its file, if it isn't loaded.
fn load(service: &ServiceInfo, deadline: &Deadline) -> Result<(), ToolError> {
    if service.state_detail.as_deref() != Some("not loaded") {
        return Ok(());
    }
    let file = job_file(&service.name).ok_or_else(|| {
        ToolError::new(
            ToolErrorCode::NotFound,
            format!(
                "{} isn't loaded, and it has no job file to load it from.",
                service.name
            ),
        )
    })?;
    launchctl(&["bootstrap", "system", &file], left(deadline)).map(drop)
}

pub(super) fn control_service(
    label: &str,
    action: ServiceAction,
    deadline: &Deadline,
) -> Result<ServiceInfo, ToolError> {
    check_label(label)?;
    let current = one(label)?;
    if action != ServiceAction::Start && current.protected {
        return Err(own_service());
    }
    if current.startup == Some(ServiceStartup::Disabled) && action != ServiceAction::Stop {
        return Err(ToolError::new(
            ToolErrorCode::Invalid,
            format!("{label} is disabled; enable it first."),
        ));
    }
    let target = format!("system/{label}");
    match action {
        ServiceAction::Start => {
            load(&current, deadline)?;
            launchctl(&["kickstart", &target], left(deadline))?;
        }
        ServiceAction::Restart => {
            load(&current, deadline)?;
            launchctl(&["kickstart", "-k", &target], left(deadline))?;
        }
        // Unloading stops it however it is kept alive; its file stays, so
        // it can be started again.
        ServiceAction::Stop if current.state_detail.as_deref() != Some("not loaded") => {
            launchctl(&["bootout", &target], left(deadline))?;
        }
        ServiceAction::Stop => {}
    }
    one(label)
}

pub(super) fn set_service_startup(
    label: &str,
    startup: ServiceStartup,
    deadline: &Deadline,
) -> Result<ServiceInfo, ToolError> {
    check_label(label)?;
    let current = one(label)?;
    if current.protected {
        return Err(own_service());
    }
    let verb = match startup {
        ServiceStartup::Automatic => "enable",
        ServiceStartup::Disabled => "disable",
        _ => {
            return Err(ToolError::new(
                ToolErrorCode::Invalid,
                "A launchd job is either enabled or disabled.",
            ));
        }
    };
    launchctl(&[verb, &format!("system/{label}")], left(deadline))?;
    one(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_described_from_launchctl() {
        let mut users = HashMap::new();
        let running = parse::LaunchdJob {
            pid: Some(1),
            status: Some(0),
            label: "com.example.web".into(),
        };
        let service = describe(Some(&running), "com.example.web", false, &mut users);
        assert_eq!(service.state, ServiceState::Running);
        assert_eq!(service.startup, Some(ServiceStartup::Automatic));
        assert_eq!(service.startup_choices, CHOICES);
        let crashed = parse::LaunchdJob {
            pid: None,
            status: Some(-9),
            label: "com.example.crash".into(),
        };
        let service = describe(Some(&crashed), "com.example.crash", true, &mut users);
        assert_eq!(service.state, ServiceState::Failed);
        assert_eq!(service.state_detail.as_deref(), Some("ended by signal 9"));
        assert_eq!(service.startup, Some(ServiceStartup::Disabled));
        let unloaded = describe(None, "com.example.off", false, &mut users);
        assert_eq!(unloaded.state_detail.as_deref(), Some("not loaded"));
        assert!(describe(None, crate::installer::DAEMON_LABEL, false, &mut users).protected);
    }
}
