//! Installs the macOS viewer for the current user. A viewer opened from
//! anywhere but an Applications folder, such as the Downloads folder (where
//! macOS runs a downloaded app from a randomized, read-only copy), copies its
//! bundle to `~/Applications`, registers that copy for `meshrmm:` links, moves
//! a downloaded original to the Trash and hands its launch to the installed
//! copy. `~/Applications` needs no administrator rights, so the installed
//! viewer can update itself in place.
use std::ffi::{CStr, c_void};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use objc2::rc::Retained;
use objc2_foundation::{NSFileManager, NSString, NSURL};

const BUNDLE_NAME: &str = "MeshRMM Remote.app";

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    /// `CFURLRef` is toll-free bridged with `NSURL`.
    fn LSRegisterURL(url: *const NSURL, update: u8) -> i32;
}

/// Installs this viewer unless it already runs from an Applications folder,
/// and returns the installed bundle when it did. A viewer that isn't in an
/// app bundle, such as a `cargo run` build, is left where it is.
pub fn install_if_needed() -> anyhow::Result<Option<PathBuf>> {
    let executable = std::env::current_exe().context("could not locate the viewer executable")?;
    let Some(bundle) = app_bundle(&executable) else {
        return Ok(None);
    };
    let home = PathBuf::from(objc2_foundation::NSHomeDirectory().to_string());
    if is_installed_location(&bundle, &home) {
        return Ok(None);
    }
    let applications = home.join("Applications");
    let target = applications.join(BUNDLE_NAME);
    install_bundle(&bundle, &target)?;
    register(&target);
    let original = original_location(&bundle);
    if is_download(&original, &home) {
        trash(&original);
    }
    tracing::info!(target = %target.display(), source = %original.display(), "installed the viewer");
    Ok(Some(target))
}

/// Starts the installed viewer with this launch's link.
pub fn launch_installed(bundle: &Path, link: Option<&str>) -> anyhow::Result<()> {
    let mut command = Command::new(bundle.join("Contents/MacOS/meshrmm-remote"));
    command
        .env_remove("MESHRMM_SESSION_BOOTSTRAP")
        .env_remove("MESHRMM_UPDATE_READY_FILE");
    if let Some(link) = link {
        command.arg(link);
    }
    command
        .spawn()
        .with_context(|| format!("could not start the installed viewer {}", bundle.display()))?;
    Ok(())
}

/// The `.app` bundle `executable` runs from, if any.
fn app_bundle(executable: &Path) -> Option<PathBuf> {
    let bundle = executable.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_owned())
}

/// Whether `bundle` is in `/Applications` or `~/Applications`, where Finder,
/// Launchpad and the updater expect it. A translocated copy never is.
fn is_installed_location(bundle: &Path, home: &Path) -> bool {
    !is_translocated(bundle)
        && (bundle.starts_with("/Applications") || bundle.starts_with(home.join("Applications")))
}

fn is_translocated(bundle: &Path) -> bool {
    bundle
        .components()
        .any(|component| component.as_os_str() == "AppTranslocation")
}

/// Only a copy in Downloads is the user's download to clean up; a build
/// opened from a project folder stays.
fn is_download(original: &Path, home: &Path) -> bool {
    !is_translocated(original) && original.starts_with(home.join("Downloads"))
}

/// Copies `source` over `target`, leaving out the download's quarantine:
/// Gatekeeper has already approved this launch, and a quarantined copy would
/// be translocated again. A viewer running from the old copy keeps running.
fn install_bundle(source: &Path, target: &Path) -> anyhow::Result<()> {
    let directory = target
        .parent()
        .context("the viewer install path has no parent directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let staging = directory.join(format!(".meshrmm-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    let staged = staging.join(BUNDLE_NAME);
    let result = ditto(source, &staged).and_then(|()| replace(&staged, target));
    let _ = std::fs::remove_dir_all(&staging);
    result
}

fn ditto(source: &Path, destination: &Path) -> anyhow::Result<()> {
    run(Command::new("/usr/bin/ditto").arg(source).arg(destination))
        .with_context(|| format!("could not copy the viewer to {}", destination.display()))?;
    // ditto keeps the quarantine even with --noqtn.
    run(Command::new("/usr/bin/xattr")
        .args(["-d", "-r", "com.apple.quarantine"])
        .arg(destination))
    .with_context(|| {
        format!(
            "could not clear the quarantine of {}",
            destination.display()
        )
    })
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// Moves `staged` to `target`, putting back the bundle it replaces if that
/// fails.
fn replace(staged: &Path, target: &Path) -> anyhow::Result<()> {
    let previous = target.with_file_name(format!("{BUNDLE_NAME}.previous"));
    if previous.exists() {
        std::fs::remove_dir_all(&previous)
            .with_context(|| format!("could not remove {}", previous.display()))?;
    }
    let replaced = target.exists();
    if replaced {
        std::fs::rename(target, &previous)
            .with_context(|| format!("could not move aside {}", target.display()))?;
    }
    if let Err(error) = std::fs::rename(staged, target) {
        if replaced {
            std::fs::rename(&previous, target).context("could not restore the installed viewer")?;
        }
        return Err(error).with_context(|| format!("could not install {}", target.display()));
    }
    if replaced && let Err(error) = std::fs::remove_dir_all(&previous) {
        tracing::warn!(path = %previous.display(), %error, "could not remove the replaced viewer");
    }
    Ok(())
}

/// Registers the installed bundle with Launch Services, so `meshrmm:` links
/// open it without waiting for macOS to notice the new app.
fn register(bundle: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let status = unsafe { LSRegisterURL(&*url, 1) };
    if status != 0 {
        tracing::warn!(status, bundle = %bundle.display(), "Launch Services did not register the viewer");
    }
}

/// Where a translocated bundle really is. macOS publishes no API for this, so
/// the Security framework's function is looked up at run time; without it the
/// translocated path stands, and the download stays where it is.
fn original_location(bundle: &Path) -> PathBuf {
    if !is_translocated(bundle) {
        return bundle.to_owned();
    }
    type CreateOriginalPath = unsafe extern "C" fn(*const NSURL, *mut *mut c_void) -> *mut NSURL;
    const SECURITY: &CStr = c"/System/Library/Frameworks/Security.framework/Security";
    const SYMBOL: &CStr = c"SecTranslocateCreateOriginalPathForURL";
    let function = unsafe {
        let library = libc::dlopen(SECURITY.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if library.is_null() {
            return bundle.to_owned();
        }
        libc::dlsym(library, SYMBOL.as_ptr())
    };
    if function.is_null() {
        return bundle.to_owned();
    }
    let function: CreateOriginalPath = unsafe { std::mem::transmute(function) };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    // The function follows the Create rule, so the result is owned here.
    let original = unsafe { Retained::from_raw(function(&*url, std::ptr::null_mut())) };
    original
        .and_then(|original| original.path())
        .map_or_else(|| bundle.to_owned(), |path| PathBuf::from(path.to_string()))
}

/// Moves the downloaded bundle to the Trash; it is the user's to empty.
fn trash(bundle: &Path) {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&bundle.to_string_lossy()));
    let manager = NSFileManager::defaultManager();
    match manager.trashItemAtURL_resultingItemURL_error(&url, None) {
        Ok(()) => {
            tracing::info!(bundle = %bundle.display(), "moved the downloaded viewer to the Trash")
        }
        Err(error) => tracing::warn!(
            bundle = %bundle.display(),
            error = %error.localizedDescription(),
            "could not move the downloaded viewer to the Trash"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/tech";

    #[test]
    fn applications_folders_are_installed_locations() {
        let home = Path::new(HOME);
        for installed in [
            "/Applications/MeshRMM Remote.app",
            "/Applications/Utilities/MeshRMM Remote.app",
            "/Users/tech/Applications/MeshRMM Remote.app",
        ] {
            assert!(
                is_installed_location(Path::new(installed), home),
                "{installed}"
            );
        }
        for elsewhere in [
            "/Users/tech/Downloads/MeshRMM Remote.app",
            "/Users/other/Applications/MeshRMM Remote.app",
            "/Users/tech/src/MeshRMM/dist/remote-macos/MeshRMM Remote.app",
            "/private/var/folders/xy/T/AppTranslocation/0A1B/d/MeshRMM Remote.app",
            "/Volumes/MeshRMM/MeshRMM Remote.app",
        ] {
            assert!(
                !is_installed_location(Path::new(elsewhere), home),
                "{elsewhere}"
            );
        }
    }

    #[test]
    fn only_downloads_are_trashed() {
        let home = Path::new(HOME);
        assert!(is_download(
            Path::new("/Users/tech/Downloads/MeshRMM Remote.app"),
            home
        ));
        assert!(is_download(
            Path::new("/Users/tech/Downloads/remote/MeshRMM Remote.app"),
            home
        ));
        for kept in [
            "/Users/tech/Desktop/MeshRMM Remote.app",
            "/Users/tech/src/MeshRMM/dist/remote-macos/MeshRMM Remote.app",
            "/private/var/folders/xy/T/AppTranslocation/0A1B/d/MeshRMM Remote.app",
        ] {
            assert!(!is_download(Path::new(kept), home), "{kept}");
        }
    }

    #[test]
    fn finds_the_bundle_of_an_executable() {
        assert_eq!(
            app_bundle(Path::new(
                "/Users/tech/Downloads/MeshRMM Remote.app/Contents/MacOS/meshrmm-remote"
            )),
            Some(PathBuf::from("/Users/tech/Downloads/MeshRMM Remote.app"))
        );
        assert_eq!(
            app_bundle(Path::new(
                "/Users/tech/src/MeshRMM/target/debug/meshrmm-remote"
            )),
            None
        );
    }

    #[test]
    fn installing_replaces_the_bundle_without_quarantine() {
        let root = std::env::temp_dir().join(format!("meshrmm install {}", std::process::id()));
        let source = root.join("Downloads").join(BUNDLE_NAME);
        let target = root.join("Applications").join(BUNDLE_NAME);
        let executable = source.join("Contents/MacOS/meshrmm-remote");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"new").unwrap();
        let quarantined = Command::new("/usr/bin/xattr")
            .args(["-w", "com.apple.quarantine", "0081;00000000;Test;"])
            .arg(&executable)
            .status()
            .unwrap()
            .success();
        std::fs::create_dir_all(target.join("Contents/MacOS")).unwrap();
        std::fs::write(target.join("Contents/MacOS/old"), b"old").unwrap();

        let result = install_bundle(&source, &target);
        let installed = std::fs::read(target.join("Contents/MacOS/meshrmm-remote"));
        let old_left = target.join("Contents/MacOS/old").exists();
        let quarantine = Command::new("/usr/bin/xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(target.join("Contents/MacOS/meshrmm-remote"))
            .output()
            .unwrap();
        let siblings = std::fs::read_dir(root.join("Applications"))
            .unwrap()
            .count();
        std::fs::remove_dir_all(&root).unwrap();
        result.unwrap();
        assert!(quarantined);
        assert_eq!(installed.unwrap(), b"new");
        assert!(!old_left);
        assert!(!quarantine.status.success(), "the copy kept its quarantine");
        assert_eq!(siblings, 1);
    }
}
