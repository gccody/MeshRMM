//! Prepares the SYSTEM profile for the applications the workspace starts.
use std::path::Path;

/// Creates SYSTEM's missing Desktop folders. Windows doesn't create them, and
/// every Open and Save dialog otherwise opens with "Desktop is unavailable".
/// 32-bit applications read the profile under `SysWOW64`, which exists only
/// on 64-bit Windows.
pub(super) fn create_desktop_folders() {
    match crate::win32::windows_directory() {
        Ok(windows) => create_desktop_folders_in(&windows),
        Err(error) => tracing::warn!(%error, "could not find the SYSTEM profile's Desktop folder"),
    }
}

fn create_desktop_folders_in(windows: &Path) {
    for system in ["System32", "SysWOW64"] {
        let profile = windows.join(system).join(r"config\systemprofile");
        if !profile.is_dir() {
            continue;
        }
        let desktop = profile.join("Desktop");
        match std::fs::create_dir(&desktop) {
            Ok(()) => {
                tracing::info!(path = %desktop.display(), "created the SYSTEM Desktop folder")
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => tracing::warn!(
                %error,
                path = %desktop.display(),
                "could not create the SYSTEM Desktop folder"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_desktop_folders_only_in_existing_profiles() {
        let windows = std::env::temp_dir().join(format!("meshrmm-profile-{}", std::process::id()));
        let profile = windows.join(r"System32\config\systemprofile");
        std::fs::create_dir_all(&profile).unwrap();

        create_desktop_folders_in(&windows);
        create_desktop_folders_in(&windows);

        assert!(profile.join("Desktop").is_dir());
        assert!(!windows.join("SysWOW64").exists());
        std::fs::remove_dir_all(&windows).unwrap();
    }
}
