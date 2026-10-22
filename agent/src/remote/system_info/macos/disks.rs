//! Physical disks: the whole-disk media IOKit lists, with the model, medium
//! and bus of the storage device beneath each.
use meshrmm_protocol::{DiskMedia, PhysicalDisk};

use super::registry::Entry;
use super::{Dictionary, dictionary, lookup, number, text};

/// How far up from a disk's media its storage device can be.
const MAX_PROVIDERS: usize = 12;

/// The storage device's description, as its registry entry has it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Device {
    vendor: Option<String>,
    product: Option<String>,
    medium: Option<String>,
    interconnect: Option<String>,
}

pub(super) fn collect() -> anyhow::Result<Vec<PhysicalDisk>> {
    let mut disks = Vec::new();
    for media in Entry::services("IOMedia")? {
        let whole = media
            .property("Whole")
            .and_then(|value| value.downcast::<objc2_core_foundation::CFBoolean>().ok())
            .is_some_and(|value| value.as_bool());
        if !whole {
            continue;
        }
        let Some(name) = media.property("BSD Name").and_then(|value| text(&value)) else {
            continue;
        };
        let Some(device) = storage_device(&media) else {
            continue;
        };
        if let Some(disk) = physical_disk(
            name,
            media.property("Size").and_then(|v| number(&v)),
            device,
        ) {
            disks.push(disk);
        }
    }
    disks.sort_by_key(|disk| natural_key(&disk.name));
    Ok(disks)
}

/// The storage device the media is on, or `None` for a disk macOS
/// synthesizes on top of other media, such as an APFS container: those
/// reach another media object before any device.
fn storage_device(media: &Entry) -> Option<Device> {
    let mut current = media.parent()?;
    for _ in 0..MAX_PROVIDERS {
        if current.conforms_to("IOMedia") {
            return None;
        }
        if let Some(characteristics) = current
            .property("Device Characteristics")
            .and_then(dictionary)
        {
            let protocol = current
                .property("Protocol Characteristics")
                .and_then(dictionary)
                .or_else(|| {
                    current
                        .parent()?
                        .property("Protocol Characteristics")
                        .and_then(dictionary)
                });
            return Some(describe(&characteristics, protocol.as_deref()));
        }
        current = current.parent()?;
    }
    None
}

fn describe(characteristics: &Dictionary, protocol: Option<&Dictionary>) -> Device {
    let field = |dictionary: &Dictionary, key: &str| lookup(dictionary, key).and_then(|v| text(&v));
    Device {
        vendor: field(characteristics, "Vendor Name"),
        product: field(characteristics, "Product Name"),
        medium: field(characteristics, "Medium Type"),
        interconnect: protocol.and_then(|protocol| field(protocol, "Physical Interconnect")),
    }
}

/// The disk a device describes, or `None` for disk images and other
/// virtual devices.
fn physical_disk(name: String, size: Option<i64>, device: Device) -> Option<PhysicalDisk> {
    if device.interconnect.as_deref() == Some("Virtual Interface") {
        return None;
    }
    let model = match (device.vendor, device.product) {
        (Some(vendor), Some(product)) if !product.starts_with(&vendor) => {
            Some(format!("{vendor} {product}"))
        }
        (_, Some(product)) => Some(product),
        (vendor, None) => vendor,
    };
    Some(PhysicalDisk {
        name,
        model,
        size_bytes: size.and_then(|size| u64::try_from(size).ok()).unwrap_or(0),
        media: match device.medium.as_deref() {
            Some("Solid State") => Some(DiskMedia::Ssd),
            Some("Rotational") => Some(DiskMedia::Hdd),
            _ => None,
        },
        interface: device.interconnect,
    })
}

/// `disk2` before `disk10`.
fn natural_key(name: &str) -> (String, u64) {
    let digits = name.trim_start_matches(|c: char| !c.is_ascii_digit());
    let prefix = &name[..name.len() - digits.len()];
    (prefix.to_owned(), digits.parse().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(vendor: &str, product: &str, medium: &str, interconnect: &str) -> Device {
        let some = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        Device {
            vendor: some(vendor),
            product: some(product),
            medium: some(medium),
            interconnect: some(interconnect),
        }
    }

    #[test]
    fn devices_describe_their_disks() {
        let internal = physical_disk(
            "disk0".into(),
            Some(251_000_193_024),
            device("", "APPLE SSD AP0256Q", "Solid State", "Apple Fabric"),
        )
        .unwrap();
        assert_eq!(internal.model.as_deref(), Some("APPLE SSD AP0256Q"));
        assert_eq!(internal.size_bytes, 251_000_193_024);
        assert_eq!(internal.media, Some(DiskMedia::Ssd));
        assert_eq!(internal.interface.as_deref(), Some("Apple Fabric"));
        let usb = physical_disk(
            "disk6".into(),
            Some(1),
            device("SanDisk", "Extreme 55AE", "Rotational", "USB"),
        )
        .unwrap();
        assert_eq!(usb.model.as_deref(), Some("SanDisk Extreme 55AE"));
        assert_eq!(usb.media, Some(DiskMedia::Hdd));
        let image = device("Apple", "Disk Image", "Solid State", "Virtual Interface");
        assert_eq!(physical_disk("disk9".into(), Some(1), image), None);
    }

    #[test]
    fn disks_sort_by_number() {
        let mut names = ["disk10", "disk2", "disk0"];
        names.sort_by_key(|name| natural_key(name));
        assert_eq!(names, ["disk0", "disk2", "disk10"]);
    }

    #[test]
    fn this_macs_disks_are_physical() {
        let disks = collect().unwrap();
        println!("{disks:#?}");
        assert!(disks.iter().all(|disk| disk.size_bytes > 0));
        let mut names: Vec<_> = disks.iter().map(|disk| disk.name.as_str()).collect();
        names.dedup();
        assert_eq!(names.len(), disks.len());
    }
}
