//! The computer's maker, model, serial number, firmware, processor and
//! memory.
use anyhow::{Context, ensure};
use meshrmm_protocol::{Hardware, Processor};
use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY};
use windows::Win32::System::SystemInformation::{
    GROUP_AFFINITY, GetLogicalProcessorInformationEx, GetPhysicallyInstalledSystemMemory,
    GlobalMemoryStatusEx, MEMORYSTATUSEX, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

use super::super::Problems;
use super::registry::Key;
use super::smbios::{self, meaningful};

const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
const PROCESSOR_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";

pub(super) fn collect(problems: &mut Problems) -> Hardware {
    let firmware = problems
        .take("firmware (SMBIOS)", smbios::read())
        .unwrap_or_default();
    let mut hardware = Hardware {
        manufacturer: firmware
            .manufacturer
            .clone()
            .or_else(|| firmware.board_manufacturer.clone()),
        // The motherboard names a self-built computer.
        model: firmware
            .product
            .clone()
            .or_else(|| firmware.board_product.clone()),
        serial_number: firmware.serial_number.clone(),
        firmware_version: firmware.version(),
        ..Default::default()
    };
    // Windows keeps a copy of the main SMBIOS strings, for firmware whose
    // table could not be read.
    if let Ok(bios) = Key::open(HKEY_LOCAL_MACHINE, BIOS_KEY, KEY_WOW64_64KEY) {
        let value = |name: &str| bios.string(name).as_deref().and_then(meaningful);
        hardware.manufacturer = hardware
            .manufacturer
            .or_else(|| value("SystemManufacturer"));
        hardware.model = hardware.model.or_else(|| value("SystemProductName"));
        hardware.manufacturer = hardware
            .manufacturer
            .or_else(|| value("BaseBoardManufacturer"));
        hardware.model = hardware.model.or_else(|| value("BaseBoardProduct"));
        hardware.firmware_version = hardware.firmware_version.or_else(|| {
            let fallback = smbios::Firmware {
                bios_vendor: value("BIOSVendor"),
                bios_version: value("BIOSVersion"),
                bios_date: value("BIOSReleaseDate"),
                ..Default::default()
            };
            fallback.version()
        });
    }
    hardware.processor = problems.take("processor", processor());
    hardware.memory_bytes = problems.take("memory", memory()).unwrap_or(0);
    hardware.disks = problems
        .take("disks", super::disks::list())
        .unwrap_or_default();
    hardware
}

fn processor() -> anyhow::Result<Processor> {
    let name = Key::open(HKEY_LOCAL_MACHINE, PROCESSOR_KEY, KEY_WOW64_64KEY)
        .ok()
        .and_then(|key| key.string("ProcessorNameString"))
        .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let (cores, threads) = cores_and_threads()?;
    Ok(Processor {
        name,
        cores,
        threads,
    })
}

/// Physical cores, and the logical processors they run, in every
/// processor group.
fn cores_and_threads() -> anyhow::Result<(u32, u32)> {
    let mut length = 0u32;
    // SAFETY: no buffer asks for the length.
    match unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut length) } {
        Err(error) if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {}
        result => {
            result.context("GetLogicalProcessorInformationEx")?;
        }
    }
    // u64s keep the records aligned.
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    // SAFETY: the buffer holds `length` bytes.
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(buffer.as_mut_ptr().cast()),
            &mut length,
        )
    }
    .context("GetLogicalProcessorInformationEx")?;
    let bytes = buffer.as_ptr().cast::<u8>();
    let (mut cores, mut threads) = (0u32, 0u32);
    let mut offset = 0usize;
    // Each record starts with its relationship and size, and is only as
    // long as its size says, which can be shorter than the whole union.
    while offset + 8 <= length as usize {
        // SAFETY: a record starts at `offset`, 8-byte aligned; only its
        // header is read until its size is checked.
        let record = unsafe { bytes.add(offset) }.cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>();
        let (relationship, size) = unsafe { ((*record).Relationship, (*record).Size as usize) };
        if size == 0 || offset + size > length as usize {
            break;
        }
        if relationship == RelationProcessorCore {
            cores += 1;
            // SAFETY: a core record holds a processor relationship, whose
            // `GroupCount` group masks it is long enough to hold.
            let processor = unsafe { std::ptr::addr_of!((*record).Anonymous.Processor) };
            let count = usize::from(unsafe { (*processor).GroupCount }.max(1));
            let first =
                unsafe { std::ptr::addr_of!((*processor).GroupMask) }.cast::<GROUP_AFFINITY>();
            for index in 0..count {
                // SAFETY: as above.
                let mask = unsafe { (*first.add(index)).Mask };
                threads += mask.count_ones();
            }
        }
        offset += size;
    }
    ensure!(cores > 0, "Windows lists no processor cores");
    Ok((cores, threads.max(cores)))
}

/// Installed memory, as the firmware reports it, or what Windows can use
/// where it doesn't.
fn memory() -> anyhow::Result<u64> {
    let mut kibibytes = 0u64;
    // SAFETY: the out pointer is valid.
    if unsafe { GetPhysicallyInstalledSystemMemory(&mut kibibytes) }.is_ok() && kibibytes > 0 {
        return Ok(kibibytes * 1024);
    }
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is writable and sized.
    unsafe { GlobalMemoryStatusEx(&mut status) }.context("GlobalMemoryStatusEx")?;
    Ok(status.ullTotalPhys)
}
