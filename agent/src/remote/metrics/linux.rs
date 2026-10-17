//! Resource usage from `/proc`, `/sys/class/net` and `statvfs`.
use std::collections::HashSet;
use std::path::Path;

use anyhow::Context;
use meshrmm_protocol::VolumeUsage;

use super::{CpuTime, Interface, Memory};

/// The processor's time in each state, in clock ticks, since Linux started.
pub struct CpuTicks {
    busy: u64,
    idle: u64,
}

impl CpuTicks {
    pub(super) fn since(&self, previous: &Self) -> CpuTime {
        CpuTime {
            busy: self.busy.saturating_sub(previous.busy),
            idle: self.idle.saturating_sub(previous.idle),
        }
    }
}

pub(super) fn cpu_ticks() -> anyhow::Result<CpuTicks> {
    parse_cpu_ticks(&std::fs::read_to_string("/proc/stat").context("/proc/stat")?)
}

/// The first line of `/proc/stat`: user, nice, system, idle, iowait, irq,
/// softirq and steal time. Guest time is already counted in user time.
fn parse_cpu_ticks(stat: &str) -> anyhow::Result<CpuTicks> {
    let fields = stat
        .lines()
        .find_map(|line| line.strip_prefix("cpu "))
        .context("/proc/stat has no cpu line")?
        .split_whitespace()
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .context("/proc/stat has an invalid cpu line")?;
    anyhow::ensure!(fields.len() >= 4, "/proc/stat has a short cpu line");
    let field = |index: usize| fields.get(index).copied().unwrap_or(0);
    Ok(CpuTicks {
        busy: field(0) + field(1) + field(2) + field(5) + field(6) + field(7),
        idle: field(3) + field(4),
    })
}

pub(super) fn memory() -> anyhow::Result<Memory> {
    parse_memory(&std::fs::read_to_string("/proc/meminfo").context("/proc/meminfo")?)
}

/// Used memory is what programs could not have without the kernel giving
/// up something: the total less `MemAvailable`, which counts reclaimable
/// caches as available.
fn parse_memory(meminfo: &str) -> anyhow::Result<Memory> {
    let kibibytes = |name: &str| {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|value| value.split_whitespace().next()?.parse::<u64>().ok())
            .with_context(|| format!("/proc/meminfo has no {name}"))
    };
    let total = kibibytes("MemTotal")? * 1024;
    let available = kibibytes("MemAvailable")? * 1024;
    Ok(Memory {
        used: total.saturating_sub(available),
        total,
    })
}

/// The hardware network interfaces: those with a device behind them.
/// Loopback, bridges, tunnels and container links carry traffic that also
/// crosses a hardware interface, so they would count it twice.
pub(super) fn interfaces() -> anyhow::Result<Vec<Interface>> {
    let table = std::fs::read_to_string("/proc/net/dev").context("/proc/net/dev")?;
    Ok(parse_interfaces(&table)
        .filter_map(|(name, received, sent)| {
            let class = Path::new("/sys/class/net").join(name);
            if !class.join("device").exists() {
                return None;
            }
            let id = std::fs::read_to_string(class.join("ifindex"))
                .ok()?
                .trim()
                .parse()
                .ok()?;
            Some(Interface { id, received, sent })
        })
        .collect())
}

/// Each interface's name and its received and sent byte counters.
fn parse_interfaces(table: &str) -> impl Iterator<Item = (&str, u64, u64)> {
    table.lines().skip(2).filter_map(|line| {
        let (name, counters) = line.split_once(':')?;
        let counters = counters
            .split_whitespace()
            .map(|value| value.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        Some((name.trim(), *counters.first()?, *counters.get(8)?))
    })
}

pub(super) fn uptime_seconds() -> anyhow::Result<u64> {
    let uptime = std::fs::read_to_string("/proc/uptime").context("/proc/uptime")?;
    let seconds: f64 = uptime
        .split_whitespace()
        .next()
        .context("/proc/uptime is empty")?
        .parse()
        .context("/proc/uptime is invalid")?;
    Ok(seconds as u64)
}

/// File systems on local block devices, each device once: Btrfs subvolumes
/// and bind mounts show the same device under several mount points.
pub(super) fn volumes() -> anyhow::Result<Vec<VolumeUsage>> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").context("/proc/self/mounts")?;
    let mut devices = HashSet::new();
    let mut volumes = Vec::new();
    for (device, mount_point, file_system) in parse_mounts(&mounts) {
        if !local_disk(device, file_system) || !devices.insert(device.to_owned()) {
            continue;
        }
        let Ok((total_bytes, free_bytes)) = usage(&mount_point) else {
            continue;
        };
        if total_bytes == 0 {
            continue;
        }
        volumes.push(VolumeUsage {
            name: mount_point.to_owned(),
            total_bytes,
            free_bytes,
        });
    }
    Ok(volumes)
}

/// Each mount's device, mount point and file system type. Mount points
/// escape spaces and the like as octal.
fn parse_mounts(mounts: &str) -> impl Iterator<Item = (&str, String, &str)> {
    mounts.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let device = fields.next()?;
        let mount_point = unescape(fields.next()?);
        let file_system = fields.next()?;
        Some((device, mount_point, file_system))
    })
}

fn unescape(field: &str) -> String {
    let mut bytes = Vec::with_capacity(field.len());
    let mut rest = field.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'\\'
            && let Some(code) = tail
                .get(..3)
                .and_then(|digits| std::str::from_utf8(digits).ok())
                .and_then(|digits| u8::from_str_radix(digits, 8).ok())
        {
            bytes.push(code);
            rest = &tail[3..];
            continue;
        }
        bytes.push(byte);
        rest = tail;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Whether a mount is a disk worth reporting: a block device, but not a
/// loop device holding a snap or an image, nor a read-only image format.
fn local_disk(device: &str, file_system: &str) -> bool {
    device.starts_with("/dev/")
        && !device.starts_with("/dev/loop")
        && !matches!(file_system, "squashfs" | "iso9660" | "udf" | "erofs")
}

fn usage(mount_point: &str) -> anyhow::Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(Path::new(mount_point).as_os_str().as_bytes())?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `stats` is writable for one statvfs.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context(mount_point.to_owned());
    }
    // SAFETY: statvfs succeeded and initialized the structure.
    let stats = unsafe { stats.assume_init() };
    let block = stats.f_frsize;
    Ok((
        stats.f_blocks.saturating_mul(block),
        stats.f_bavail.saturating_mul(block),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_processor_time() {
        let ticks =
            parse_cpu_ticks("cpu  100 5 50 1000 20 3 2 1 0 0\ncpu0 50 2 25 500 10 1 1 0 0 0\n")
                .unwrap();
        assert_eq!((ticks.busy, ticks.idle), (161, 1020));
        let later = CpuTicks {
            busy: 171,
            idle: 1050,
        };
        assert_eq!(later.since(&ticks), CpuTime { busy: 10, idle: 30 });
        assert!(parse_cpu_ticks("intr 1 2 3").is_err());
    }

    #[test]
    fn used_memory_leaves_out_what_is_available() {
        let memory = parse_memory(
            "MemTotal:        4000000 kB\nMemFree:          100000 kB\nMemAvailable:    3000000 kB\n",
        )
        .unwrap();
        assert_eq!(memory.total, 4_000_000 * 1024);
        assert_eq!(memory.used, 1_000_000 * 1024);
    }

    #[test]
    fn reads_interface_counters() {
        let table = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo:    1000      10    0    0    0     0          0         0     1000      10    0    0    0     0       0          0\n  eth0: 5000      50    0    0    0     0          0         0     7000      70    0    0    0     0       0          0\n";
        assert_eq!(
            parse_interfaces(table).collect::<Vec<_>>(),
            [("lo", 1000, 1000), ("eth0", 5000, 7000)]
        );
    }

    #[test]
    fn mount_points_are_unescaped_and_only_disks_count() {
        let mounts = "/dev/sda2 / ext4 rw 0 0\nproc /proc proc rw 0 0\n/dev/sdb1 /mnt/My\\040Disk vfat rw 0 0\n/dev/loop3 /snap/core/1 squashfs ro 0 0\n";
        let parsed = parse_mounts(mounts).collect::<Vec<_>>();
        assert_eq!(parsed[2].1, "/mnt/My Disk");
        let disks = parsed
            .iter()
            .filter(|(device, _, file_system)| local_disk(device, file_system))
            .map(|(_, mount_point, _)| mount_point.as_str())
            .collect::<Vec<_>>();
        assert_eq!(disks, ["/", "/mnt/My Disk"]);
    }

    #[test]
    fn real_readings_are_plausible() {
        assert!(memory().unwrap().total > 0);
        assert!(uptime_seconds().unwrap() > 0);
        let volumes = volumes().unwrap();
        assert!(
            volumes.iter().any(|volume| volume.name == "/"),
            "{volumes:?}"
        );
        cpu_ticks().unwrap();
        interfaces().unwrap();
    }
}
