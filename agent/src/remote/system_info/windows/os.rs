//! The Windows edition, release, build, architecture, time zone and the
//! update tools present.
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use meshrmm_protocol::OperatingSystem;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, KEY_WOW64_64KEY};
use windows::Win32::System::SystemInformation::{
    GetNativeSystemInfo, GetTickCount64, IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_AMD64,
    IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386, PROCESSOR_ARCHITECTURE_AMD64,
    PROCESSOR_ARCHITECTURE_ARM64, PROCESSOR_ARCHITECTURE_INTEL, SYSTEM_INFO,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
use windows::Win32::System::Time::{DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation};

use super::super::{Problems, unix_ms};
use super::registry::Key;

const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
/// The first build of Windows 11, which still calls itself Windows 10 in
/// `ProductName`.
const WINDOWS_11_BUILD: u32 = 22000;

pub(super) fn collect(problems: &mut Problems) -> OperatingSystem {
    let mut os = OperatingSystem {
        id: "windows".into(),
        name: "Windows".into(),
        architecture: architecture(),
        boot_time_unix_ms: boot_time(),
        package_managers: package_managers(),
        ..Default::default()
    };
    if let Some(key) = problems.take(
        "operating system",
        Key::open(HKEY_LOCAL_MACHINE, CURRENT_VERSION, KEY_WOW64_64KEY),
    ) {
        let build = key
            .string("CurrentBuildNumber")
            .or_else(|| key.string("CurrentBuild"))
            .and_then(|build| build.trim().parse::<u32>().ok());
        if let Some(product) = key.string("ProductName") {
            os.name = product_name(&product, build);
        }
        os.version = key
            .string("DisplayVersion")
            .or_else(|| key.string("ReleaseId"))
            .unwrap_or_default();
        os.build = build.map(|build| match key.dword("UBR") {
            Some(revision) => format!("{build}.{revision}"),
            None => build.to_string(),
        });
        os.kernel = match (
            key.dword("CurrentMajorVersionNumber"),
            key.dword("CurrentMinorVersionNumber"),
            build,
        ) {
            (Some(major), Some(minor), Some(build)) => Some(format!("{major}.{minor}.{build}")),
            _ => key.string("CurrentVersion"),
        };
        os.installed_at_unix_ms = key
            .dword("InstallDate")
            .filter(|seconds| *seconds > 0)
            .map(|seconds| i64::from(seconds) * 1000);
        // A release without a feature-update name, such as an LTSC build.
        if os.version.is_empty() {
            os.version = os.kernel.clone().unwrap_or_default();
        }
    }
    os.time_zone = problems.take("time zone", time_zone());
    os
}

/// `ProductName` with the release Windows 11 doesn't put in it.
fn product_name(product: &str, build: Option<u32>) -> String {
    let product = product.trim();
    if build.is_some_and(|build| build >= WINDOWS_11_BUILD)
        && let Some(edition) = product.strip_prefix("Windows 10")
    {
        return format!("Windows 11{edition}");
    }
    product.to_owned()
}

/// The machine Windows runs on, which an emulated x64 Agent on an ARM64
/// computer would otherwise misreport.
fn architecture() -> String {
    let mut process = IMAGE_FILE_MACHINE::default();
    let mut native = IMAGE_FILE_MACHINE::default();
    // SAFETY: both out pointers are valid.
    if unsafe { IsWow64Process2(GetCurrentProcess(), &mut process, Some(&mut native)) }.is_ok()
        && let Some(name) = machine_name(native)
    {
        return name.into();
    }
    let mut info = SYSTEM_INFO::default();
    // SAFETY: `info` is writable.
    unsafe { GetNativeSystemInfo(&mut info) };
    // SAFETY: the processor architecture is valid in either union variant.
    let architecture = unsafe { info.Anonymous.Anonymous.wProcessorArchitecture };
    match architecture {
        PROCESSOR_ARCHITECTURE_AMD64 => "x86_64",
        PROCESSOR_ARCHITECTURE_ARM64 => "arm64",
        PROCESSOR_ARCHITECTURE_INTEL => "x86",
        _ => "unknown",
    }
    .into()
}

pub(super) fn machine_name(machine: IMAGE_FILE_MACHINE) -> Option<&'static str> {
    match machine {
        IMAGE_FILE_MACHINE_AMD64 => Some("x86_64"),
        IMAGE_FILE_MACHINE_ARM64 => Some("arm64"),
        IMAGE_FILE_MACHINE_I386 => Some("x86"),
        _ => None,
    }
}

fn boot_time() -> Option<i64> {
    // SAFETY: no preconditions.
    let up = Duration::from_millis(unsafe { GetTickCount64() });
    SystemTime::now().checked_sub(up).and_then(unix_ms)
}

/// Windows' own name for the zone, such as `W. Europe Standard Time`.
fn time_zone() -> anyhow::Result<String> {
    let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: `zone` is writable.
    let result = unsafe { GetDynamicTimeZoneInformation(&mut zone) };
    anyhow::ensure!(
        result != u32::MAX,
        "GetDynamicTimeZoneInformation: {}",
        std::io::Error::last_os_error()
    );
    let text = |units: &[u16]| {
        let end = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        String::from_utf16_lossy(&units[..end])
    };
    [text(&zone.TimeZoneKeyName), text(&zone.StandardName)]
        .into_iter()
        .find(|name| !name.trim().is_empty())
        .context("Windows names no time zone")
}

fn package_managers() -> Vec<String> {
    let mut managers = vec!["windows_update".to_owned()];
    let program_files =
        std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
    // The Microsoft Store package that brings winget.
    let winget = std::fs::read_dir(Path::new(&program_files).join("WindowsApps"))
        .map(|entries| {
            entries.flatten().any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("Microsoft.DesktopAppInstaller_")
            })
        })
        .unwrap_or(false);
    if winget {
        managers.push("winget".into());
    }
    let program_data = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    if Path::new(&program_data)
        .join(r"chocolatey\bin\choco.exe")
        .is_file()
    {
        managers.push("chocolatey".into());
    }
    managers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_11_is_named_by_its_build() {
        assert_eq!(
            product_name("Windows 10 Pro", Some(26100)),
            "Windows 11 Pro"
        );
        assert_eq!(
            product_name("Windows 10 Enterprise LTSC 2021", Some(19044)),
            "Windows 10 Enterprise LTSC 2021"
        );
        assert_eq!(
            product_name("Windows Server 2022 Standard", Some(20348)),
            "Windows Server 2022 Standard"
        );
        assert_eq!(
            product_name("Windows Server 2025 Datacenter", Some(26100)),
            "Windows Server 2025 Datacenter"
        );
        assert_eq!(product_name(" Windows 10 Home ", None), "Windows 10 Home");
    }

    #[test]
    fn machines_are_named_like_the_other_platforms() {
        assert_eq!(machine_name(IMAGE_FILE_MACHINE_AMD64), Some("x86_64"));
        assert_eq!(machine_name(IMAGE_FILE_MACHINE_ARM64), Some("arm64"));
        assert_eq!(machine_name(IMAGE_FILE_MACHINE::default()), None);
    }
}
