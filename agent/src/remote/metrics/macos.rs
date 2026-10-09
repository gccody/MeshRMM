//! Resource usage from the Mach host statistics and `sysctl`.
use std::ffi::CStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail, ensure};
use meshrmm_protocol::VolumeUsage;

use super::{CpuTime, Interface, Memory};

/// `IFT_ETHER` and `IFT_CELLULAR` in net/if_types.h. Ethernet covers Wi-Fi
/// too. Tunnels such as VPNs, and bridges, carry traffic that also crosses a
/// physical interface, so they would count it twice.
const IFT_ETHER: u8 = 0x06;
const IFT_CELLULAR: u8 = 0xff;

// libc deprecates its Mach bindings in favor of the mach2 crate; these few
// are declared here instead.
unsafe extern "C" {
    /// What the `mach_task_self()` macro reads.
    static mach_task_self_: libc::mach_port_t;
    fn mach_host_self() -> libc::mach_port_t;
    fn host_page_size(host: libc::mach_port_t, size: *mut libc::vm_size_t) -> libc::kern_return_t;
    fn mach_port_deallocate(
        task: libc::mach_port_t,
        name: libc::mach_port_t,
    ) -> libc::kern_return_t;
}

/// A send right to the host port, released when dropped.
struct Host(libc::mach_port_t);

impl Host {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: mach_host_self has no preconditions.
        let port = unsafe { mach_host_self() };
        ensure!(port != 0, "could not get the host port");
        Ok(Self(port))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // SAFETY: the right was returned by mach_host_self and is released once.
        unsafe { mach_port_deallocate(mach_task_self_, self.0) };
    }
}

/// The processor ticks of every core together, since the Mac started.
pub struct CpuTicks([u32; libc::CPU_STATE_MAX as usize]);

impl CpuTicks {
    /// The ticks since `previous`. The kernel's 32-bit counters wrap after
    /// about seven weeks on a ten-core Mac, so they are subtracted modulo 2^32.
    pub(super) fn since(&self, previous: &Self) -> CpuTime {
        let elapsed = |state: libc::c_int| {
            u64::from(self.0[state as usize].wrapping_sub(previous.0[state as usize]))
        };
        CpuTime {
            busy: elapsed(libc::CPU_STATE_USER)
                + elapsed(libc::CPU_STATE_SYSTEM)
                + elapsed(libc::CPU_STATE_NICE),
            idle: elapsed(libc::CPU_STATE_IDLE),
        }
    }
}

pub(super) fn cpu_ticks() -> anyhow::Result<CpuTicks> {
    let host = Host::new()?;
    let mut info = libc::host_cpu_load_info {
        cpu_ticks: [0; libc::CPU_STATE_MAX as usize],
    };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `info` holds `count` integers.
    let result = unsafe {
        libc::host_statistics(
            host.0,
            libc::HOST_CPU_LOAD_INFO,
            (&raw mut info).cast(),
            &mut count,
        )
    };
    ensure!(
        result == libc::KERN_SUCCESS,
        "host_statistics failed with {result}"
    );
    Ok(CpuTicks(info.cpu_ticks))
}

/// Memory used the way Activity Monitor counts it: app memory, wired memory
/// and the compressor's pages. Caches the system can drop are not used.
pub(super) fn memory() -> anyhow::Result<Memory> {
    let total: u64 = sysctl(&mut [libc::CTL_HW, libc::HW_MEMSIZE]).context("hw.memsize")?;
    let host = Host::new()?;
    // SAFETY: the structure is plain integers, for which zero is valid.
    let mut statistics: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `statistics` holds `count` integers; an older kernel fills fewer.
    let result = unsafe {
        libc::host_statistics64(
            host.0,
            libc::HOST_VM_INFO64,
            (&raw mut statistics).cast(),
            &mut count,
        )
    };
    ensure!(
        result == libc::KERN_SUCCESS,
        "host_statistics64 failed with {result}"
    );
    let mut page_size: libc::vm_size_t = 0;
    // SAFETY: the out pointer is valid. The counts are in the kernel's page
    // size, which an x86_64 build running on Apple silicon does not share.
    let result = unsafe { host_page_size(host.0, &mut page_size) };
    ensure!(
        result == libc::KERN_SUCCESS && page_size > 0,
        "host_page_size failed with {result}"
    );
    // The structure is packed, so its fields are copied out before use.
    let (internal, purgeable, wired, compressed) = (
        statistics.internal_page_count,
        statistics.purgeable_count,
        statistics.wire_count,
        statistics.compressor_page_count,
    );
    let pages =
        u64::from(internal.saturating_sub(purgeable)) + u64::from(wired) + u64::from(compressed);
    Ok(Memory {
        used: pages.saturating_mul(page_size as u64).min(total),
        total,
    })
}

/// The 64-bit byte counters of each Ethernet, Wi-Fi and cellular interface.
pub(super) fn interfaces() -> anyhow::Result<Vec<Interface>> {
    let mut mib = [libc::CTL_NET, libc::PF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let buffer = sysctl_bytes(&mut mib).context("NET_RT_IFLIST2")?;
    let mut interfaces = Vec::new();
    let mut offset = 0;
    // Every routing message starts with its length, version and type.
    while let Some(&[low, high, _version, kind]) = buffer.get(offset..offset + 4) {
        let length = usize::from(u16::from_ne_bytes([low, high]));
        if length < 4 || offset + length > buffer.len() {
            break;
        }
        if i32::from(kind) == libc::RTM_IFINFO2 && length >= std::mem::size_of::<libc::if_msghdr2>()
        {
            // SAFETY: the message is an if_msghdr2 and is long enough for
            // one. Messages are not aligned for it, hence the unaligned read.
            let info = unsafe {
                std::ptr::read_unaligned(buffer[offset..].as_ptr().cast::<libc::if_msghdr2>())
            };
            let (flags, kind) = (info.ifm_flags, info.ifm_data.ifi_type);
            if flags & libc::IFF_LOOPBACK == 0 && matches!(kind, IFT_ETHER | IFT_CELLULAR) {
                interfaces.push(Interface {
                    id: u32::from(info.ifm_index),
                    received: info.ifm_data.ifi_ibytes,
                    sent: info.ifm_data.ifi_obytes,
                });
            }
        }
        offset += length;
    }
    Ok(interfaces)
}

/// Seconds since the Mac started, counting time asleep, as `uptime` does.
pub(super) fn uptime_seconds() -> anyhow::Result<u64> {
    let boot: libc::timeval =
        sysctl(&mut [libc::CTL_KERN, libc::KERN_BOOTTIME]).context("kern.boottime")?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("the clock is before 1970")?
        .as_secs();
    Ok(now.saturating_sub(u64::try_from(boot.tv_sec).unwrap_or(0)))
}

/// Local volumes a person would see in Finder, named by mount point: the
/// startup disk and writable disks under `/Volumes`. APFS system volumes
/// share the startup disk's space and are hidden from browsing, and
/// read-only ones are mostly disk images, so neither is listed.
pub(super) fn volumes() -> anyhow::Result<Vec<VolumeUsage>> {
    // MNT_NOWAIT lists mounts without asking each file system for fresh
    // numbers, which a network volume can take long to answer.
    // SAFETY: a null buffer asks for the count.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    ensure!(
        count >= 0,
        "getfsstat failed: {}",
        std::io::Error::last_os_error()
    );
    // Room for volumes mounted between the two calls.
    let capacity = count as usize + 8;
    // SAFETY: the structure is plain integers and characters, for which zero is valid.
    let mut mounts: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; capacity];
    // SAFETY: the buffer holds `capacity` structures.
    let count = unsafe {
        libc::getfsstat(
            mounts.as_mut_ptr(),
            (capacity * std::mem::size_of::<libc::statfs>()) as libc::c_int,
            libc::MNT_NOWAIT,
        )
    };
    ensure!(
        count >= 0,
        "getfsstat failed: {}",
        std::io::Error::last_os_error()
    );
    mounts.truncate(count as usize);
    let mut volumes = Vec::new();
    for mount in &mounts {
        // SAFETY: the kernel NUL-terminates the mount point.
        let path = unsafe { CStr::from_ptr(mount.f_mntonname.as_ptr()) };
        if !listed(mount.f_flags, path.to_bytes()) {
            continue;
        }
        // Fresh numbers for the volumes that are listed.
        // SAFETY: the structure is plain integers and characters.
        let mut current: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: the path is NUL-terminated and `current` is a statfs.
        if unsafe { libc::statfs(path.as_ptr(), &mut current) } != 0 {
            tracing::debug!(path = ?path, error = %std::io::Error::last_os_error(), "could not read a volume's capacity");
            continue;
        }
        let block = u64::from(current.f_bsize);
        volumes.push(VolumeUsage {
            name: path.to_string_lossy().into_owned(),
            total_bytes: current.f_blocks.saturating_mul(block),
            free_bytes: current.f_bavail.saturating_mul(block),
        });
    }
    Ok(volumes)
}

/// Whether a volume mounted with `flags` at `path` is reported.
fn listed(flags: u32, path: &[u8]) -> bool {
    let has = |flag: libc::c_int| flags & flag as u32 != 0;
    has(libc::MNT_LOCAL) && !has(libc::MNT_DONTBROWSE) && (!has(libc::MNT_RDONLY) || path == b"/")
}

/// A fixed-size `sysctl` value.
fn sysctl<T: Copy>(mib: &mut [libc::c_int]) -> anyhow::Result<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>();
    // SAFETY: `value` holds `size` bytes.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            value.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    ensure!(
        size == std::mem::size_of::<T>(),
        "sysctl returned {size} bytes"
    );
    // SAFETY: the kernel filled every byte of `value`.
    Ok(unsafe { value.assume_init() })
}

/// A variable-size `sysctl` value.
fn sysctl_bytes(mib: &mut [libc::c_int]) -> anyhow::Result<Vec<u8>> {
    // The value can grow between asking for its size and reading it.
    for _ in 0..4 {
        let mut size = 0;
        // SAFETY: a null buffer asks for the size.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        size += size / 8 + 512;
        let mut buffer = vec![0u8; size];
        // SAFETY: the buffer holds `size` bytes.
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result == 0 {
            buffer.truncate(size);
            return Ok(buffer);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error.into());
        }
    }
    bail!("the value kept growing")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_ticks_survive_counter_wraparound() {
        let previous = CpuTicks([u32::MAX - 9, 5, u32::MAX, 0]);
        let next = CpuTicks([20, 15, 29, 5]);
        assert_eq!(next.since(&previous), CpuTime { busy: 45, idle: 30 });
    }

    #[test]
    fn browsable_writable_local_volumes_are_listed() {
        let local = libc::MNT_LOCAL as u32;
        let read_only = local | libc::MNT_RDONLY as u32;
        assert!(listed(read_only, b"/"));
        assert!(listed(local, b"/Volumes/Backup"));
        assert!(!listed(read_only, b"/Volumes/Installer"));
        assert!(!listed(
            local | libc::MNT_DONTBROWSE as u32,
            b"/System/Volumes/Data"
        ));
        assert!(!listed(0, b"/Volumes/Share"));
    }
}
