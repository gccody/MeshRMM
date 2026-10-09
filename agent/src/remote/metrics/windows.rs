//! Resource usage from the Win32 system information and IP helper APIs.
use anyhow::Context;
use meshrmm_protocol::VolumeUsage;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_ROW2};
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW,
};
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::core::PCWSTR;

use super::{CpuTime, Interface, Memory};

/// `DRIVE_FIXED` from WinBase.h, which lives in a `windows` feature the
/// Agent does not otherwise need.
const DRIVE_FIXED: u32 = 3;

/// Calls `visit` with each hardware network adapter's row. Loopback, tunnel
/// and virtual switch interfaces are left out, since their traffic also
/// crosses a hardware adapter and would be counted twice.
pub(crate) fn for_each_hardware_interface(
    mut visit: impl FnMut(&MIB_IF_ROW2),
) -> windows::core::Result<()> {
    let mut table = std::ptr::null_mut();
    unsafe { GetIfTable2(&mut table) }.ok()?;
    if table.is_null() {
        return Ok(());
    }
    // The table holds `NumEntries` rows until it is freed below.
    let rows = unsafe {
        std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize)
    };
    for row in rows {
        // The first bit is HardwareInterface.
        if row.InterfaceAndOperStatusFlags._bitfield & 1 != 0 {
            visit(row);
        }
    }
    unsafe { FreeMibTable(table.cast()) };
    Ok(())
}

fn time(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

/// The processor time of every core together since Windows started, in
/// 100-nanosecond units.
pub struct CpuTicks {
    /// Kernel and user time; kernel time includes idle time.
    total: u64,
    idle: u64,
}

impl CpuTicks {
    pub(super) fn since(&self, previous: &Self) -> CpuTime {
        let total = self.total.saturating_sub(previous.total);
        let idle = self.idle.saturating_sub(previous.idle).min(total);
        CpuTime {
            busy: total - idle,
            idle,
        }
    }
}

pub(super) fn cpu_ticks() -> anyhow::Result<CpuTicks> {
    let (mut idle, mut kernel, mut user) = Default::default();
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }
        .context("GetSystemTimes")?;
    Ok(CpuTicks {
        total: time(kernel).saturating_add(time(user)),
        idle: time(idle),
    })
}

/// Physical memory in use: everything not available to programs, which
/// leaves out the standby cache as Task Manager does.
pub(super) fn memory() -> anyhow::Result<Memory> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }.context("GlobalMemoryStatusEx")?;
    Ok(Memory {
        used: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
        total: status.ullTotalPhys,
    })
}

pub(super) fn interfaces() -> anyhow::Result<Vec<Interface>> {
    let mut interfaces = Vec::new();
    for_each_hardware_interface(|row| {
        interfaces.push(Interface {
            id: row.InterfaceIndex,
            received: row.InOctets,
            sent: row.OutOctets,
        });
    })
    .context("GetIfTable2")?;
    Ok(interfaces)
}

pub(super) fn uptime_seconds() -> anyhow::Result<u64> {
    Ok(unsafe { GetTickCount64() } / 1000)
}

/// The fixed drives with a letter, named like `C:`. Removable, network and
/// optical drives are left out.
pub(super) fn volumes() -> anyhow::Result<Vec<VolumeUsage>> {
    let length = unsafe { GetLogicalDriveStringsW(None) };
    anyhow::ensure!(
        length > 0,
        "GetLogicalDriveStringsW failed: {}",
        std::io::Error::last_os_error()
    );
    // Room for drives added between the two calls.
    let mut buffer = vec![0u16; length as usize + 64];
    let written = unsafe { GetLogicalDriveStringsW(Some(&mut buffer)) } as usize;
    anyhow::ensure!(
        written > 0 && written < buffer.len(),
        "GetLogicalDriveStringsW failed: {}",
        std::io::Error::last_os_error()
    );
    let mut volumes = Vec::new();
    for root in buffer[..written].split(|&unit| unit == 0) {
        if root.is_empty() {
            continue;
        }
        let path: Vec<u16> = root.iter().copied().chain(Some(0)).collect();
        let path = PCWSTR(path.as_ptr());
        if unsafe { GetDriveTypeW(path) } != DRIVE_FIXED {
            continue;
        }
        let name = drive_name(root);
        let (mut free, mut total) = (0u64, 0u64);
        if let Err(error) =
            unsafe { GetDiskFreeSpaceExW(path, Some(&mut free), Some(&mut total), None) }
        {
            // Such as a BitLocker drive that is still locked.
            tracing::debug!(volume = %name, %error, "could not read a volume's capacity");
            continue;
        }
        volumes.push(VolumeUsage {
            name,
            total_bytes: total,
            free_bytes: free,
        });
    }
    Ok(volumes)
}

/// `C:\` as `C:`.
fn drive_name(root: &[u16]) -> String {
    let name = String::from_utf16_lossy(root);
    name.strip_suffix('\\').unwrap_or(&name).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_time_is_part_of_kernel_time() {
        let previous = CpuTicks {
            total: 1_000,
            idle: 400,
        };
        let next = CpuTicks {
            total: 2_000,
            idle: 1_150,
        };
        assert_eq!(
            next.since(&previous),
            CpuTime {
                busy: 250,
                idle: 750
            }
        );
        // Counters that went back measure nothing.
        assert_eq!(previous.since(&next), CpuTime::default());
    }

    #[test]
    fn drives_are_named_by_letter() {
        let root: Vec<u16> = "C:\\".encode_utf16().collect();
        assert_eq!(drive_name(&root), "C:");
    }
}
