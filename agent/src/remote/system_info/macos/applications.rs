//! Installed applications: the bundles in `/Applications` and each user's
//! `~/Applications`, named as Finder shows them, with the version from their
//! Info.plist and the publisher from their code signature. Apple's apps in
//! `/System/Applications` are part of macOS and are left out.
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use meshrmm_protocol::{Application, ApplicationSource};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType, CFURL};

use super::super::Problems;
use super::{array, created_ms, plist_string, read_plist};

/// How many folders deep bundles are looked for, as in
/// `/Applications/Vendor/Suite/Tool.app`.
const MAX_DEPTH: usize = 3;
/// `kSecCSSigningInformation`: read the certificates, without validating.
const SIGNING_INFORMATION: u32 = 1 << 1;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecStaticCodeCreateWithPath(path: &CFURL, flags: u32, code: *mut *const c_void) -> i32;
    fn SecCodeCopySigningInformation(
        code: *const c_void,
        flags: u32,
        information: *mut *const CFDictionary,
    ) -> i32;
    fn SecCertificateCopySubjectSummary(certificate: *const c_void) -> Option<NonNull<CFString>>;
    static kSecCodeInfoCertificates: &'static CFString;
}

pub(super) fn collect(problems: &mut Problems) -> Vec<Application> {
    let mut bundles = Vec::new();
    if let Err(error) = find_bundles(Path::new("/Applications"), 0, &mut bundles) {
        problems.note("applications (/Applications)", &error);
    }
    if let Ok(homes) = std::fs::read_dir("/Users") {
        for home in homes.flatten() {
            let folder = home.path().join("Applications");
            // Other users' homes are closed to a test run that isn't root.
            let _ = find_bundles(&folder, 0, &mut bundles);
        }
    }
    bundles
        .iter()
        .filter_map(|bundle| application(bundle))
        .collect()
}

/// The `.app` bundles in `folder` and the plain folders under it.
fn find_bundles(folder: &Path, depth: usize, bundles: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(folder)?.flatten() {
        // Hidden folders hold backups and caches, not installed apps.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        // Symbolic links are followed only to bundles, never to folders.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if is_bundle(&path) {
            if kind.is_dir() || path.is_dir() {
                bundles.push(path);
            }
        } else if kind.is_dir() && depth + 1 < MAX_DEPTH {
            let _ = find_bundles(&path, depth + 1, bundles);
        }
    }
    Ok(())
}

fn is_bundle(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "app")
}

fn application(bundle: &Path) -> Option<Application> {
    let name = bundle.file_stem()?.to_string_lossy().into_owned();
    let info = read_plist(&bundle.join("Contents/Info.plist")).ok();
    let version = info.as_ref().and_then(|info| {
        plist_string(info, "CFBundleShortVersionString")
            .or_else(|| plist_string(info, "CFBundleVersion"))
    });
    Some(Application {
        name,
        version,
        publisher: signer(bundle).as_deref().and_then(publisher),
        architecture: None,
        installed_at_unix_ms: created_ms(bundle),
        source: ApplicationSource::MacosApp,
    })
}

/// The common name of the certificate that signed the bundle, such as
/// `Developer ID Application: Google LLC (EQHXZ8M8AV)`. The signature is read,
/// not checked, which is quick.
fn signer(bundle: &Path) -> Option<String> {
    let url = CFURL::from_directory_path(bundle)?;
    let mut code = std::ptr::null();
    // SAFETY: the URL is valid and `code` a valid out pointer.
    if unsafe { SecStaticCodeCreateWithPath(&url, 0, &mut code) } != 0 || code.is_null() {
        return None;
    }
    // SAFETY: the code object is a +1 CF reference.
    let code = unsafe { CFRetained::from_raw(NonNull::new(code.cast_mut().cast::<CFType>())?) };
    let mut information = std::ptr::null();
    // SAFETY: the code object is valid and `information` a valid out pointer.
    let status = unsafe {
        SecCodeCopySigningInformation(
            CFRetained::as_ptr(&code).as_ptr().cast(),
            SIGNING_INFORMATION,
            &mut information,
        )
    };
    if status != 0 || information.is_null() {
        return None;
    }
    // SAFETY: the dictionary is a +1 reference keyed by strings.
    let information: CFRetained<CFDictionary<CFString, CFType>> =
        unsafe { CFRetained::from_raw(NonNull::new(information.cast_mut())?.cast()) };
    // SAFETY: the constant is a valid string.
    let certificates = array(information.get(unsafe { kSecCodeInfoCertificates })?)?;
    let leaf = certificates.get(0)?;
    // SAFETY: the certificate is a SecCertificate; the summary is +1.
    let summary =
        unsafe { SecCertificateCopySubjectSummary(CFRetained::as_ptr(&leaf).as_ptr().cast()) }?;
    // SAFETY: the reference is owned.
    Some(unsafe { CFRetained::from_raw(summary) }.to_string())
}

/// The organization a signing certificate's common name names. Apple signs
/// its own apps as `Software Signing`; the Mac App Store re-signs every app
/// with one certificate that names no developer.
fn publisher(common_name: &str) -> Option<String> {
    if matches!(common_name, "Software Signing" | "macOS Software Signing")
        || common_name.starts_with("Apple Code Signing")
    {
        return Some("Apple Inc.".into());
    }
    if common_name.starts_with("Apple Mac OS Application Signing")
        || common_name.starts_with("Apple iPhone OS Application Signing")
    {
        return None;
    }
    let (_, name) = common_name.split_once(": ")?;
    let name = match name.rsplit_once(" (") {
        Some((name, team)) if team.ends_with(')') => name,
        _ => name,
    };
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishers_come_from_the_signing_certificate() {
        assert_eq!(
            publisher("Developer ID Application: Google LLC (EQHXZ8M8AV)").as_deref(),
            Some("Google LLC")
        );
        assert_eq!(
            publisher("Developer ID Application: Docker Inc (9BNSXJN65R)").as_deref(),
            Some("Docker Inc")
        );
        assert_eq!(
            publisher("Developer ID Application: Zoë (Studio) Ltd (ABCDE12345)").as_deref(),
            Some("Zoë (Studio) Ltd")
        );
        assert_eq!(publisher("Software Signing").as_deref(), Some("Apple Inc."));
        assert_eq!(
            publisher("macOS Software Signing").as_deref(),
            Some("Apple Inc.")
        );
        assert_eq!(publisher("Apple Mac OS Application Signing"), None);
        assert_eq!(publisher("adhoc"), None);
    }

    #[test]
    fn bundles_are_found_in_plain_folders_only() {
        let root = std::env::temp_dir().join(format!("meshrmm-apps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for folder in [
            "Top.app/Contents/Helpers/Inner.app",
            "Vendor/Tool.app/Contents",
            "Vendor/Suite/Deep.app",
            "a/b/c/TooDeep.app",
            "Folder",
            ".backup/Old.app",
        ] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        std::fs::write(
            root.join("Vendor/Tool.app/Contents/Info.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>CFBundleName</key><string>Short</string>
<key>CFBundleVersion</key><string>42</string>
<key>CFBundleShortVersionString</key><string>1.2.3</string>
</dict></plist>"#,
        )
        .unwrap();
        let mut bundles = Vec::new();
        find_bundles(&root, 0, &mut bundles).unwrap();
        let mut names: Vec<_> = bundles
            .iter()
            .map(|bundle| bundle.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["Deep", "Tool", "Top"]);
        let tool = bundles
            .iter()
            .find(|bundle| bundle.ends_with("Tool.app"))
            .unwrap();
        let tool = application(tool).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(tool.name, "Tool");
        assert_eq!(tool.version.as_deref(), Some("1.2.3"));
        assert_eq!(tool.publisher, None);
        assert!(tool.installed_at_unix_ms.is_some());
    }
}
