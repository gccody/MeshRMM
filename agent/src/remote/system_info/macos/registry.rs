//! Reading the IOKit registry: services by class, their properties and
//! providers, and device tree nodes.
use std::ffi::{CString, c_char, c_void};
use std::ptr::NonNull;

use anyhow::{Context, ensure};
use objc2_core_foundation::{CFRetained, CFString, CFType};

use super::text;

/// `MACH_PORT_NULL`, which IOKit takes as its default main port.
const MAIN_PORT: libc::mach_port_t = 0;
const SERVICE_PLANE: &[u8] = b"IOService\0";

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(
        main_port: libc::mach_port_t,
        matching: *mut c_void,
        existing: *mut u32,
    ) -> libc::kern_return_t;
    fn IOServiceGetMatchingService(main_port: libc::mach_port_t, matching: *mut c_void) -> u32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> libc::kern_return_t;
    fn IOObjectConformsTo(object: u32, class_name: *const c_char) -> u32;
    fn IORegistryEntryFromPath(main_port: libc::mach_port_t, path: *const c_char) -> u32;
    fn IORegistryEntryGetParentEntry(
        entry: u32,
        plane: *const c_char,
        parent: *mut u32,
    ) -> libc::kern_return_t;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: &CFString,
        allocator: *const c_void,
        options: u32,
    ) -> Option<NonNull<CFType>>;
}

/// A registry entry or iterator, released when dropped.
pub(super) struct Entry(u32);

impl Drop for Entry {
    fn drop(&mut self) {
        // SAFETY: the object is owned and released once.
        unsafe { IOObjectRelease(self.0) };
    }
}

impl Entry {
    fn new(object: u32) -> Option<Self> {
        (object != 0).then_some(Self(object))
    }

    /// The entry at a registry path, such as `IODeviceTree:/chosen`.
    pub(super) fn at_path(path: &str) -> Option<Self> {
        let path = CString::new(path).ok()?;
        // SAFETY: the path is NUL-terminated.
        Self::new(unsafe { IORegistryEntryFromPath(MAIN_PORT, path.as_ptr()) })
    }

    /// The first service of `class`.
    pub(super) fn first_service(class: &str) -> Option<Self> {
        let class = CString::new(class).ok()?;
        // SAFETY: the name is NUL-terminated; the returned dictionary is
        // consumed by the lookup.
        let object = unsafe {
            let matching = IOServiceMatching(class.as_ptr());
            if matching.is_null() {
                return None;
            }
            IOServiceGetMatchingService(MAIN_PORT, matching)
        };
        Self::new(object)
    }

    /// Every service of `class`.
    pub(super) fn services(class: &str) -> anyhow::Result<Vec<Self>> {
        let name = CString::new(class)?;
        let mut iterator = 0;
        // SAFETY: as in `first_service`; `iterator` is a valid out pointer.
        let result = unsafe {
            let matching = IOServiceMatching(name.as_ptr());
            ensure!(!matching.is_null(), "IOServiceMatching({class}) failed");
            IOServiceGetMatchingServices(MAIN_PORT, matching, &mut iterator)
        };
        ensure!(
            result == libc::KERN_SUCCESS,
            "IOServiceGetMatchingServices({class}) failed with {result:#x}"
        );
        let iterator = Self::new(iterator).context("no service iterator")?;
        let mut services = Vec::new();
        // SAFETY: the iterator is valid; each entry it returns is owned.
        while let Some(entry) = Self::new(unsafe { IOIteratorNext(iterator.0) }) {
            services.push(entry);
        }
        Ok(services)
    }

    pub(super) fn property(&self, key: &str) -> Option<CFRetained<CFType>> {
        // SAFETY: the entry and key are valid; the property is a +1 reference.
        let value = unsafe {
            IORegistryEntryCreateCFProperty(self.0, &CFString::from_str(key), std::ptr::null(), 0)
        }?;
        // SAFETY: the function returns a +1 reference.
        Some(unsafe { CFRetained::from_raw(value) })
    }

    /// The entry's provider in the service plane.
    pub(super) fn parent(&self) -> Option<Self> {
        let mut parent = 0;
        // SAFETY: the plane is NUL-terminated and `parent` a valid out pointer.
        let result = unsafe {
            IORegistryEntryGetParentEntry(self.0, SERVICE_PLANE.as_ptr().cast(), &mut parent)
        };
        (result == libc::KERN_SUCCESS)
            .then(|| Self::new(parent))
            .flatten()
    }

    pub(super) fn conforms_to(&self, class: &str) -> bool {
        let Ok(class) = CString::new(class) else {
            return false;
        };
        // SAFETY: the entry is valid and the name NUL-terminated.
        unsafe { IOObjectConformsTo(self.0, class.as_ptr()) != 0 }
    }
}

/// A device tree property as text, such as `IODeviceTree:/product`'s
/// `product-name`.
pub(super) fn device_tree_string(path: &str, key: &str) -> Option<String> {
    text(&*Entry::at_path(path)?.property(key)?)
}

pub(super) fn serial_number() -> anyhow::Result<String> {
    let platform = Entry::first_service("IOPlatformExpertDevice").context("no platform expert")?;
    platform
        .property("IOPlatformSerialNumber")
        .and_then(|value| text(&value))
        .context("the platform expert has no serial number")
}

/// The system firmware version, as System Information shows it: Apple
/// silicon's iBoot version, or an Intel Mac's boot ROM version.
pub(super) fn firmware_version() -> Option<String> {
    device_tree_string("IODeviceTree:/chosen", "system-firmware-version")
        .map(|version| firmware_number(&version))
        .or_else(|| device_tree_string("IODeviceTree:/rom", "version"))
}

/// `mBoot-20457.1.29` or `iBoot-11881.1.1` as `20457.1.29`.
fn firmware_number(version: &str) -> String {
    match version.split_once('-') {
        Some((loader, number)) if loader.ends_with("Boot") && !number.is_empty() => {
            number.to_owned()
        }
        _ => version.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_versions_drop_the_loader_name() {
        assert_eq!(firmware_number("mBoot-20457.1.29"), "20457.1.29");
        assert_eq!(firmware_number("iBoot-11881.1.1"), "11881.1.1");
        assert_eq!(firmware_number("1554.140.20.0.0"), "1554.140.20.0.0");
    }
}
