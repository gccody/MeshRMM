//! Reading what the system's tools and files say about processes and
//! services, kept apart from running them so every platform's tests read
//! every platform's output.
use std::collections::HashMap;

use meshrmm_protocol::{ServiceInfo, ServiceStartup, ServiceState};

/// The fields of `/proc/<pid>/stat` the tools use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcStat {
    pub(crate) pid: u32,
    /// The command, which the kernel cuts to 15 characters.
    pub(crate) comm: String,
    /// `Z` for a process that has exited and waits for its parent.
    pub(crate) state: char,
    pub(crate) ppid: u32,
    /// User and system time, in clock ticks.
    pub(crate) cpu_ticks: u64,
    /// When it started, in clock ticks after boot.
    pub(crate) start_ticks: u64,
    /// Resident memory, in pages.
    pub(crate) rss_pages: u64,
}

/// `/proc/<pid>/stat`: the command is in parentheses and may hold spaces
/// and parentheses of its own, so the fields after it are counted from the
/// last `)`.
pub(crate) fn proc_stat(text: &str) -> Option<ProcStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text[..open].trim().parse().ok()?;
    let comm = text.get(open + 1..close)?.to_owned();
    // Field 3, the state, is the first after the command.
    let fields = text[close + 1..].split_whitespace().collect::<Vec<_>>();
    let field = |number: usize| -> Option<u64> { fields.get(number - 3)?.parse().ok() };
    Some(ProcStat {
        pid,
        comm,
        state: fields.first()?.chars().next()?,
        ppid: u32::try_from(field(4)?).ok()?,
        cpu_ticks: field(14)?.saturating_add(field(15)?),
        start_ticks: field(22)?,
        rss_pages: field(24)?,
    })
}

/// The effective user ID in `/proc/<pid>/status`.
pub(crate) fn proc_status_uid(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// `/proc/<pid>/cmdline`: arguments ended by NULs. A kernel thread has
/// none.
pub(crate) fn proc_cmdline(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let arguments = text
        .trim_end_matches('\0')
        .split('\0')
        .collect::<Vec<_>>()
        .join(" ");
    (!arguments.trim().is_empty()).then_some(arguments)
}

/// When the computer started, from `/proc/stat`'s `btime`, in seconds since
/// the Unix epoch.
pub(crate) fn boot_time(proc_stat: &str) -> Option<i64> {
    proc_stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()
}

/// `MemTotal` in `/proc/meminfo`, in bytes.
pub(crate) fn memory_total(meminfo: &str) -> Option<u64> {
    let kib: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// A command line from `sysctl(KERN_PROCARGS2)`: the argument count, the
/// program's path, NUL padding, then the arguments, each ended by a NUL.
pub(crate) fn procargs2(bytes: &[u8]) -> Option<String> {
    let count = usize::try_from(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let rest = &bytes[4..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let mut position = path_end;
    while rest.get(position) == Some(&0) {
        position += 1;
    }
    let arguments = rest[position..]
        .split(|byte| *byte == 0)
        .take(count)
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>();
    let line = arguments.join(" ");
    (!line.trim().is_empty()).then_some(line)
}

/// The unit names `systemctl list-units --type=service --all --plain
/// --no-legend` prints, first on each line; a unit that failed or wasn't
/// found may be marked with `*` or `●` before it.
pub(crate) fn listed_units(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let first = words.next()?;
            let name = if first == "*" || first == "●" {
                words.next()?
            } else {
                first.trim_start_matches(['*', '●'])
            };
            name.ends_with(".service").then(|| name.to_owned())
        })
        .collect()
}

/// The unit files `systemctl list-unit-files --type=service --no-legend`
/// prints, without templates (`name@.service`), which aren't services
/// until they are given an instance.
pub(crate) fn unit_files(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.ends_with(".service") && !name.ends_with("@.service"))
        .map(str::to_owned)
        .collect()
}

/// The units `systemctl show --property=… -- <units>` describes, one block
/// of `Name=value` lines each, between empty lines.
pub(crate) fn systemd_blocks(text: &str) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut block = HashMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            block.insert(name.to_owned(), value.to_owned());
        }
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    blocks
}

/// How a unit's file state starts it, and whether it may be changed.
pub(crate) fn systemd_startup(file_state: &str) -> (Option<ServiceStartup>, bool) {
    match file_state {
        "enabled" | "enabled-runtime" => (Some(ServiceStartup::Automatic), true),
        "disabled" => (Some(ServiceStartup::Manual), true),
        "masked" | "masked-runtime" => (Some(ServiceStartup::Disabled), true),
        "" => (None, false),
        // Static, indirect, generated, transient, alias, linked: others
        // start it, or no unit file of its own says how.
        _ => (Some(ServiceStartup::Static), false),
    }
}

/// The ways a unit may be set to start: with the computer (`enable`), when
/// asked (`disable`), or never (`mask`).
pub(crate) const SYSTEMD_CHOICES: [ServiceStartup; 3] = [
    ServiceStartup::Automatic,
    ServiceStartup::Manual,
    ServiceStartup::Disabled,
];

/// A unit from its `systemctl show` block, unless it doesn't exist.
pub(crate) fn systemd_service(
    block: &HashMap<String, String>,
    protected: bool,
) -> Option<ServiceInfo> {
    let value = |name: &str| {
        block
            .get(name)
            .map(String::as_str)
            .unwrap_or_default()
            .trim()
    };
    let name = value("Id");
    if name.is_empty() || matches!(value("LoadState"), "not-found" | "") {
        return None;
    }
    let active = value("ActiveState");
    let sub = value("SubState");
    let state = match active {
        "active" | "reloading" | "refreshing" => ServiceState::Running,
        "inactive" => ServiceState::Stopped,
        "activating" => ServiceState::Starting,
        "deactivating" => ServiceState::Stopping,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Other,
    };
    let file_state = value("UnitFileState");
    let (startup, changeable) = systemd_startup(file_state);
    let text = |name: &str| Some(value(name).to_owned()).filter(|text| !text.is_empty());
    let user = text("User").unwrap_or_else(|| "root".to_owned());
    Some(ServiceInfo {
        name: name.to_owned(),
        display_name: name.to_owned(),
        state,
        state_detail: Some(if sub.is_empty() {
            active.to_owned()
        } else {
            format!("{active} ({sub})")
        }),
        startup,
        startup_detail: text("UnitFileState"),
        startup_choices: if changeable && !protected {
            SYSTEMD_CHOICES.to_vec()
        } else {
            Vec::new()
        },
        account: Some(user),
        description: text("Description"),
        pid: value("MainPID").parse().ok().filter(|pid| *pid != 0),
        protected,
    })
}

/// A job from `launchctl list`: its process ID while it runs, its last
/// exit status (negative for the signal that ended it), and its label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchdJob {
    pub(crate) pid: Option<u32>,
    pub(crate) status: Option<i32>,
    pub(crate) label: String,
}

/// `launchctl list`'s `PID Status Label` lines, under its header.
pub(crate) fn launchctl_list(text: &str) -> Vec<LaunchdJob> {
    text.lines()
        .filter_map(|line| {
            let mut columns = line.split('\t');
            let pid = columns.next()?.trim();
            let status = columns.next()?.trim();
            let label = columns.next()?.trim();
            if pid == "PID" || label.is_empty() {
                return None;
            }
            Some(LaunchdJob {
                pid: pid.parse().ok(),
                status: status.parse().ok(),
                label: label.to_owned(),
            })
        })
        .collect()
}

/// `launchctl print-disabled system`: each label it lists, and whether it
/// is disabled (`=> disabled`, or `=> true` before macOS 13).
pub(crate) fn launchd_disabled(text: &str) -> HashMap<String, bool> {
    text.lines()
        .filter_map(|line| {
            let (label, value) = line.trim().split_once("=>")?;
            let label = label.trim().strip_prefix('"')?.strip_suffix('"')?;
            Some((
                label.to_owned(),
                matches!(value.trim(), "disabled" | "true"),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_files_are_read() {
        let stat = "1234 (my (odd) prog) S 1 1234 1234 0 -1 4194560 1 0 0 0 250 50 0 0 20 0 1 0 98765 1000 77 18446744073709551615";
        assert_eq!(
            proc_stat(stat),
            Some(ProcStat {
                pid: 1234,
                comm: "my (odd) prog".into(),
                state: 'S',
                ppid: 1,
                cpu_ticks: 300,
                start_ticks: 98765,
                rss_pages: 77,
            })
        );
        assert_eq!(proc_stat("garbage"), None);
        assert_eq!(proc_status_uid("Name:\tx\nUid:\t1000\t0\t0\t0\n"), Some(0));
        assert_eq!(
            proc_cmdline(b"/usr/bin/python3\0-m\0http.server\0"),
            Some("/usr/bin/python3 -m http.server".into())
        );
        assert_eq!(proc_cmdline(b""), None);
        assert_eq!(
            boot_time("cpu 1 2\nbtime 1700000000\n"),
            Some(1_700_000_000)
        );
        assert_eq!(
            memory_total("MemTotal:       16314564 kB\n"),
            Some(16_314_564 * 1024)
        );
    }

    #[test]
    fn macos_command_lines_are_read() {
        let mut bytes = 3i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/sleep\0\0\0\0sleep\x0030\0--x\0HOME=/var/root\0");
        assert_eq!(procargs2(&bytes), Some("sleep 30 --x".into()));
        assert_eq!(procargs2(&[1, 0]), None);
    }

    #[test]
    fn systemd_lists_are_read() {
        let units = "cron.service loaded active running Regular background program processing daemon\n* nope.service not-found inactive dead nope.service\n● bad.service loaded failed failed Bad\n";
        assert_eq!(
            listed_units(units),
            ["cron.service", "nope.service", "bad.service"]
        );
        let files = "cron.service enabled enabled\ngetty@.service enabled enabled\nssh.service disabled enabled\n";
        assert_eq!(unit_files(files), ["cron.service", "ssh.service"]);
    }

    #[test]
    fn systemd_units_are_described() {
        let show = "Id=cron.service\nDescription=Regular background program processing daemon\nLoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nMainPID=812\nUser=\n\nId=nope.service\nLoadState=not-found\nActiveState=inactive\n\nId=web.service\nLoadState=loaded\nActiveState=failed\nSubState=failed\nUnitFileState=masked\nMainPID=0\nUser=www-data\n\nId=systemd-journald.service\nLoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=static\nMainPID=300\n";
        let blocks = systemd_blocks(show);
        assert_eq!(blocks.len(), 4);
        let cron = systemd_service(&blocks[0], false).unwrap();
        assert_eq!(cron.state, ServiceState::Running);
        assert_eq!(cron.state_detail.as_deref(), Some("active (running)"));
        assert_eq!(cron.startup, Some(ServiceStartup::Automatic));
        assert_eq!(cron.startup_choices, SYSTEMD_CHOICES);
        assert_eq!(cron.account.as_deref(), Some("root"));
        assert_eq!(cron.pid, Some(812));
        assert!(systemd_service(&blocks[1], false).is_none());
        let web = systemd_service(&blocks[2], false).unwrap();
        assert_eq!(web.state, ServiceState::Failed);
        assert_eq!(web.startup, Some(ServiceStartup::Disabled));
        assert_eq!(web.account.as_deref(), Some("www-data"));
        assert_eq!(web.pid, None);
        let journald = systemd_service(&blocks[3], false).unwrap();
        assert_eq!(journald.startup, Some(ServiceStartup::Static));
        assert!(journald.startup_choices.is_empty());
        // The Agent's own unit can't be changed.
        assert!(
            systemd_service(&blocks[0], true)
                .unwrap()
                .startup_choices
                .is_empty()
        );
    }

    #[test]
    fn launchd_lists_are_read() {
        let list = "PID\tStatus\tLabel\n-\t0\tcom.apple.backupd\n412\t0\tcom.openssh.sshd\n-\t-9\tcom.example.crashy\n";
        assert_eq!(
            launchctl_list(list),
            [
                LaunchdJob {
                    pid: None,
                    status: Some(0),
                    label: "com.apple.backupd".into()
                },
                LaunchdJob {
                    pid: Some(412),
                    status: Some(0),
                    label: "com.openssh.sshd".into()
                },
                LaunchdJob {
                    pid: None,
                    status: Some(-9),
                    label: "com.example.crashy".into()
                },
            ]
        );
        let disabled = "disabled services = {\n\t\"com.example.backup\" => disabled\n\t\"com.apple.ftpd\" => true\n\t\"com.example.web\" => enabled\n}\n";
        let disabled = launchd_disabled(disabled);
        assert_eq!(disabled.get("com.example.backup"), Some(&true));
        assert_eq!(disabled.get("com.apple.ftpd"), Some(&true));
        assert_eq!(disabled.get("com.example.web"), Some(&false));
        assert_eq!(disabled.len(), 3);
    }
}
