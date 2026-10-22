//! The Mac's system information, from `sysctl`, the IOKit registry,
//! SystemConfiguration and the application bundles' Info.plist files.
use std::ffi::{CStr, CString};
use std::path::Path;

use anyhow::{Context, bail, ensure};
use meshrmm_protocol::{Hardware, OperatingSystem, Processor, SystemInfo};
use objc2_core_foundation::{
    CFArray, CFData, CFDictionary, CFNumber, CFPropertyListCreateWithData, CFRetained, CFString,
    CFType,
};

use super::{Problems, unix, unix_ms};

pub(super) use super::unix::local_ipv4_addresses;

mod applications;
mod disks;
mod network;
mod registry;

const SYSTEM_VERSION: &str = "/System/Library/CoreServices/SystemVersion.plist";
/// Setup Assistant creates it when the Mac is set up for the first time.
const SETUP_DONE: &str = "/var/db/.AppleSetupDone";

pub(super) fn collect(problems: &mut Problems) -> SystemInfo {
    SystemInfo {
        os: operating_system(problems),
        hardware: hardware(problems),
        network: network::collect(problems),
        users: unix::utmpx_users(),
        applications: applications::collect(problems),
        ..Default::default()
    }
}

fn operating_system(problems: &mut Problems) -> OperatingSystem {
    let mut os = OperatingSystem {
        id: "macos".into(),
        name: "macOS".into(),
        ..Default::default()
    };
    if let Some(version) = problems.take("operating system version", system_version()) {
        os.name = version.name.unwrap_or(os.name);
        os.version = version.version.unwrap_or_default();
        os.build = version.build;
    }
    os.kernel = problems.take("kernel", kernel());
    os.architecture = architecture();
    os.installed_at_unix_ms = created_ms(Path::new(SETUP_DONE));
    os.boot_time_unix_ms = problems.take("boot time", boot_time());
    os.time_zone = problems.take("time zone", unix::time_zone());
    os.package_managers = package_managers(
        unix::find_tool("brew").is_some(),
        Path::new("/opt/local/bin/port").exists(),
    );
    os
}

/// `ProductName`, `ProductVersion` and `ProductBuildVersion`, as `sw_vers`
/// shows them.
#[derive(Debug, Default, PartialEq, Eq)]
struct SystemVersion {
    name: Option<String>,
    version: Option<String>,
    build: Option<String>,
}

fn system_version() -> anyhow::Result<SystemVersion> {
    let plist = read_plist(Path::new(SYSTEM_VERSION))?;
    Ok(SystemVersion {
        name: plist_string(&plist, "ProductName"),
        version: plist_string(&plist, "ProductVersion"),
        build: plist_string(&plist, "ProductBuildVersion"),
    })
}

/// `Darwin 24.6.0`.
fn kernel() -> anyhow::Result<String> {
    let mut name = std::mem::MaybeUninit::<libc::utsname>::zeroed();
    // SAFETY: `name` is a writable utsname.
    ensure!(
        unsafe { libc::uname(name.as_mut_ptr()) } == 0,
        "uname failed: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: uname filled in the structure with NUL-terminated fields.
    let name = unsafe { name.assume_init() };
    let field = |chars: &[libc::c_char]| {
        // SAFETY: each field is NUL-terminated within its array.
        unsafe { CStr::from_ptr(chars.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    Ok(format!("{} {}", field(&name.sysname), field(&name.release)))
}

/// The Mac's processor architecture. An Intel build of the Agent runs under
/// Rosetta on Apple silicon, where the kernel still reports `x86_64` to it.
fn architecture() -> String {
    if sysctl_int("sysctl.proc_translated").unwrap_or(0) == 1 {
        return "arm64".into();
    }
    if cfg!(target_arch = "aarch64") {
        "arm64".into()
    } else {
        "x86_64".into()
    }
}

fn boot_time() -> anyhow::Result<i64> {
    let boot: libc::timeval = sysctl_value("kern.boottime")?;
    ensure!(boot.tv_sec > 0, "kern.boottime is {}", boot.tv_sec);
    Ok(boot.tv_sec * 1000 + i64::from(boot.tv_usec) / 1000)
}

/// The update tools present: macOS's own, and Homebrew and MacPorts.
fn package_managers(homebrew: bool, macports: bool) -> Vec<String> {
    let mut managers = vec!["softwareupdate".to_owned()];
    if homebrew {
        managers.push("homebrew".into());
    }
    if macports {
        managers.push("macports".into());
    }
    managers
}

fn hardware(problems: &mut Problems) -> Hardware {
    let identifier = problems.take("model", sysctl_string("hw.model"));
    let marketing = registry::device_tree_string("IODeviceTree:/product", "product-name");
    let processor = problems
        .take("processor", processor())
        .filter(|processor| !processor.name.is_empty());
    Hardware {
        manufacturer: Some("Apple Inc.".into()),
        model: model_name(marketing.as_deref(), identifier.as_deref()),
        serial_number: problems.take("serial number", registry::serial_number()),
        firmware_version: registry::firmware_version(),
        processor,
        memory_bytes: problems
            .take("memory", sysctl_value::<u64>("hw.memsize"))
            .unwrap_or(0),
        disks: problems.take("disks", disks::collect()).unwrap_or_default(),
    }
}

/// `MacBook Air (M1, 2020) (MacBookAir10,1)`: the name people know the Mac
/// by, which only Apple silicon Macs record, and the model identifier.
fn model_name(marketing: Option<&str>, identifier: Option<&str>) -> Option<String> {
    match (marketing, identifier) {
        (Some(marketing), Some(identifier)) if marketing != identifier => {
            Some(format!("{marketing} ({identifier})"))
        }
        (Some(name), _) | (None, Some(name)) => Some(name.to_owned()),
        (None, None) => None,
    }
}

fn processor() -> anyhow::Result<Processor> {
    Ok(Processor {
        name: sysctl_string("machdep.cpu.brand_string")?,
        cores: sysctl_int("hw.physicalcpu")?.try_into()?,
        threads: sysctl_int("hw.logicalcpu")?.try_into()?,
    })
}

/// A fixed-size `sysctl` value by name.
fn sysctl_value<T: Copy>(name: &str) -> anyhow::Result<T> {
    let bytes = sysctl_bytes(name)?;
    ensure!(
        bytes.len() == std::mem::size_of::<T>(),
        "{name} is {} bytes",
        bytes.len()
    );
    // SAFETY: the kernel filled exactly a T's bytes; T is plain data.
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) })
}

/// A 32- or 64-bit integer `sysctl` value by name.
fn sysctl_int(name: &str) -> anyhow::Result<i64> {
    let bytes = sysctl_bytes(name)?;
    Ok(match bytes.len() {
        4 => i64::from(i32::from_ne_bytes(bytes[..4].try_into()?)),
        8 => i64::from_ne_bytes(bytes[..8].try_into()?),
        length => bail!("{name} is {length} bytes"),
    })
}

fn sysctl_string(name: &str) -> anyhow::Result<String> {
    let bytes = sysctl_bytes(name)?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    Ok(String::from_utf8_lossy(&bytes[..end]).trim().to_owned())
}

fn sysctl_bytes(name: &str) -> anyhow::Result<Vec<u8>> {
    let name_c = CString::new(name)?;
    let mut size = 0;
    // SAFETY: a null buffer asks for the size.
    let result = unsafe {
        libc::sysctlbyname(
            name_c.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context(name.to_owned());
    }
    let mut buffer = vec![0u8; size];
    // SAFETY: the buffer holds `size` bytes.
    let result = unsafe {
        libc::sysctlbyname(
            name_c.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context(name.to_owned());
    }
    buffer.truncate(size);
    Ok(buffer)
}

/// A dictionary keyed by strings, as plists and the registry have them.
type Dictionary = CFDictionary<CFString, CFType>;

/// Reads a binary or XML property list whose root is a dictionary.
fn read_plist(path: &Path) -> anyhow::Result<CFRetained<Dictionary>> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    let data = CFData::from_bytes(&bytes);
    // SAFETY: the data is valid; format and error may be null.
    let plist = unsafe {
        CFPropertyListCreateWithData(
            None,
            Some(&data),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }
    .with_context(|| format!("{} is not a property list", path.display()))?;
    dictionary(plist).with_context(|| format!("{} is not a dictionary", path.display()))
}

fn dictionary(value: CFRetained<CFType>) -> Option<CFRetained<Dictionary>> {
    let value = value.downcast::<CFDictionary>().ok()?;
    // SAFETY: the keys this module reads are looked up as strings and the
    // values checked for their type before use.
    Some(unsafe { CFRetained::cast_unchecked(value) })
}

fn array(value: CFRetained<CFType>) -> Option<CFRetained<CFArray<CFType>>> {
    let value = value.downcast::<CFArray>().ok()?;
    // SAFETY: every array element is a CF object, checked before use.
    Some(unsafe { CFRetained::cast_unchecked(value) })
}

fn lookup(dictionary: &Dictionary, key: &str) -> Option<CFRetained<CFType>> {
    dictionary.get(&CFString::from_str(key))
}

/// A string value, or the text in a data value, as the device tree keeps
/// its strings.
fn text(value: &CFType) -> Option<String> {
    let text = if let Some(string) = value.downcast_ref::<CFString>() {
        string.to_string()
    } else {
        let bytes = value.downcast_ref::<CFData>()?.to_vec();
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        String::from_utf8(bytes[..end].to_vec()).ok()?
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn number(value: &CFType) -> Option<i64> {
    value.downcast_ref::<CFNumber>()?.as_i64()
}

fn plist_string(dictionary: &Dictionary, key: &str) -> Option<String> {
    text(&*lookup(dictionary, key)?)
}

/// When `path` was created, in Unix milliseconds.
fn created_ms(path: &Path) -> Option<i64> {
    unix_ms(std::fs::metadata(path).ok()?.created().ok()?).filter(|ms| *ms > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names_add_the_identifier() {
        assert_eq!(
            model_name(Some("MacBook Air (M1, 2020)"), Some("MacBookAir10,1")).as_deref(),
            Some("MacBook Air (M1, 2020) (MacBookAir10,1)")
        );
        assert_eq!(
            model_name(None, Some("MacBookPro15,1")).as_deref(),
            Some("MacBookPro15,1")
        );
        assert_eq!(model_name(Some("Mac"), None).as_deref(), Some("Mac"));
        assert_eq!(model_name(None, None), None);
    }

    #[test]
    fn package_managers_follow_the_installed_tools() {
        assert_eq!(package_managers(false, false), ["softwareupdate"]);
        assert_eq!(
            package_managers(true, false),
            ["softwareupdate", "homebrew"]
        );
        assert_eq!(
            package_managers(true, true),
            ["softwareupdate", "homebrew", "macports"]
        );
    }

    #[test]
    fn this_macs_version_and_hardware_are_read() {
        let version = system_version().unwrap();
        assert!(
            version.version.is_some() && version.build.is_some(),
            "{version:?}"
        );
        assert!(kernel().unwrap().starts_with("Darwin "));
        assert!(boot_time().unwrap() > 1_000_000_000_000);
        let processor = processor().unwrap();
        assert!(processor.cores > 0 && processor.threads >= processor.cores);
        assert!(sysctl_value::<u64>("hw.memsize").unwrap() > 0);
    }

    #[test]
    fn plists_read_strings_and_data_as_text() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleName</key><string> Example </string>
<key>Blob</key><data>bW9kZWwAAA==</data>
<key>Count</key><integer>7</integer>
</dict></plist>"#;
        let path = std::env::temp_dir().join(format!("meshrmm-plist-{}.plist", std::process::id()));
        std::fs::write(&path, xml).unwrap();
        let plist = read_plist(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            plist_string(&plist, "CFBundleName").as_deref(),
            Some("Example")
        );
        assert_eq!(plist_string(&plist, "Blob").as_deref(), Some("model"));
        assert_eq!(number(&lookup(&plist, "Count").unwrap()), Some(7));
        assert_eq!(plist_string(&plist, "Missing"), None);
    }
}
