//! Physical disks, from the storage stack's device descriptors. A disk is
//! opened without read or write access, which these queries don't need.
use meshrmm_protocol::{DiskMedia, PhysicalDisk};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_PROPERTY_ID, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty,
};
use windows::core::PCWSTR;

use crate::win32::{OwnedHandle, wide};

/// Disk numbers can have gaps where a disk was removed, so the search stops
/// only after this many missing in a row.
const MAX_MISSING_IN_A_ROW: u32 = 8;
const MAX_DISK_NUMBER: u32 = 64;

pub(super) fn list() -> anyhow::Result<Vec<PhysicalDisk>> {
    let mut disks = Vec::new();
    let mut missing = 0;
    for number in 0..MAX_DISK_NUMBER {
        let name = format!("PhysicalDrive{number}");
        let path = wide(format!(r"\\.\{name}"));
        // SAFETY: the path is NUL-terminated.
        let handle = match unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        } {
            Ok(handle) => OwnedHandle(handle),
            Err(error)
                if error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                    || error.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
            {
                missing += 1;
                if missing >= MAX_MISSING_IN_A_ROW {
                    break;
                }
                continue;
            }
            Err(error) => {
                tracing::debug!(disk = name, %error, "could not open a disk");
                continue;
            }
        };
        missing = 0;
        let Some(size_bytes) = size(&handle).filter(|size| *size > 0) else {
            // Such as a card reader without a card.
            continue;
        };
        let (model, interface) = descriptor(&handle).unwrap_or_default();
        disks.push(PhysicalDisk {
            name,
            model,
            size_bytes,
            media: seek_penalty(&handle).map(|penalty| {
                if penalty {
                    DiskMedia::Hdd
                } else {
                    DiskMedia::Ssd
                }
            }),
            interface: interface.map(str::to_owned),
        });
    }
    Ok(disks)
}

/// Sends a storage property query and returns the bytes it answered.
fn query(handle: &OwnedHandle, property: STORAGE_PROPERTY_ID, capacity: usize) -> Option<Vec<u8>> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: property,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    // u64s keep the descriptor aligned.
    let mut buffer = vec![0u64; capacity.div_ceil(8)];
    let mut returned = 0u32;
    // SAFETY: the query and buffer are valid for the sizes passed.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&raw const query).cast()),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * 8) as u32,
            Some(&mut returned),
            None,
        )
    }
    .ok()?;
    let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
    Some(bytes[..(returned as usize).min(bytes.len())].to_vec())
}

/// The disk's model and the bus it is attached by.
fn descriptor(handle: &OwnedHandle) -> Option<(Option<String>, Option<&'static str>)> {
    let bytes = query(handle, StorageDeviceProperty, 1024)?;
    if bytes.len() < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return None;
    }
    // SAFETY: the bytes hold a whole descriptor; it is read unaligned.
    let descriptor =
        unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>()) };
    let text = |offset: u32| ascii_at(&bytes, offset as usize);
    let model = model(
        text(descriptor.VendorIdOffset),
        text(descriptor.ProductIdOffset),
    );
    Some((model, bus_name(descriptor.BusType.0)))
}

/// The NUL-terminated ASCII string at `offset`, trimmed; offset 0 means none.
fn ascii_at(bytes: &[u8], offset: usize) -> Option<String> {
    if offset == 0 {
        return None;
    }
    let rest = bytes.get(offset..)?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(rest.len());
    // Fixed-width ATA strings pad with runs of spaces.
    let text = String::from_utf8_lossy(&rest[..end])
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// The vendor and product IDs as one model name. ATA and NVMe disks often
/// put the whole name in the product ID, or repeat the vendor there.
fn model(vendor: Option<String>, product: Option<String>) -> Option<String> {
    match (vendor, product) {
        (Some(vendor), Some(product))
            if !product.to_lowercase().starts_with(&vendor.to_lowercase())
                && vendor.to_lowercase() != "nvme" =>
        {
            Some(format!("{vendor} {product}"))
        }
        (_, Some(product)) => Some(product),
        (vendor, None) => vendor,
    }
}

/// `STORAGE_BUS_TYPE` as a name.
fn bus_name(bus: i32) -> Option<&'static str> {
    Some(match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "IEEE 1394",
        5 => "SSA",
        6 => "Fibre Channel",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 | 15 => "Virtual",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => return None,
    })
}

/// Whether the disk has a seek penalty, which only spinning disks have.
fn seek_penalty(handle: &OwnedHandle) -> Option<bool> {
    let bytes = query(handle, StorageDeviceSeekPenaltyProperty, 64)?;
    if bytes.len() < std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
        return None;
    }
    // SAFETY: the bytes hold a whole descriptor.
    let descriptor = unsafe {
        std::ptr::read_unaligned(bytes.as_ptr().cast::<DEVICE_SEEK_PENALTY_DESCRIPTOR>())
    };
    Some(descriptor.IncursSeekPenalty)
}

fn size(handle: &OwnedHandle) -> Option<u64> {
    let mut geometry = DISK_GEOMETRY_EX::default();
    let mut returned = 0u32;
    // SAFETY: `geometry` is writable for its size.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            None,
            0,
            Some((&raw mut geometry).cast()),
            std::mem::size_of::<DISK_GEOMETRY_EX>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .ok()?;
    u64::try_from(geometry.DiskSize).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buses_are_named() {
        assert_eq!(bus_name(17), Some("NVMe"));
        assert_eq!(bus_name(11), Some("SATA"));
        assert_eq!(bus_name(7), Some("USB"));
        assert_eq!(bus_name(15), Some("Virtual"));
        assert_eq!(bus_name(16), Some("Storage Spaces"));
        assert_eq!(bus_name(0), None);
        assert_eq!(bus_name(99), None);
    }

    #[test]
    fn models_join_vendor_and_product_once() {
        let model = |vendor: Option<&str>, product: Option<&str>| {
            super::model(vendor.map(Into::into), product.map(Into::into))
        };
        assert_eq!(
            model(Some("Msft"), Some("Virtual Disk")).as_deref(),
            Some("Msft Virtual Disk")
        );
        assert_eq!(
            model(None, Some("Samsung SSD 980 PRO 1TB")).as_deref(),
            Some("Samsung SSD 980 PRO 1TB")
        );
        assert_eq!(
            model(Some("WDC"), Some("WDC WD40EFRX-68N32N0")).as_deref(),
            Some("WDC WD40EFRX-68N32N0")
        );
        assert_eq!(
            model(Some("NVMe"), Some("WD_BLACK SN850X 2000GB")).as_deref(),
            Some("WD_BLACK SN850X 2000GB")
        );
        assert_eq!(model(Some("Kingston"), None).as_deref(), Some("Kingston"));
        assert_eq!(model(None, None), None);
    }

    #[test]
    fn descriptor_strings_are_read_by_offset() {
        let bytes = b"\0\0\0\0ACME  \0Disk     1\0";
        assert_eq!(ascii_at(bytes, 4).as_deref(), Some("ACME"));
        assert_eq!(ascii_at(bytes, 11).as_deref(), Some("Disk 1"));
        assert_eq!(ascii_at(bytes, 0), None);
        assert_eq!(ascii_at(bytes, 99), None);
    }
}
