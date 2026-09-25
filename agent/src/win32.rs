//! Small Win32 helpers shared across the Agent.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;

use anyhow::bail;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

/// A NUL-terminated UTF-16 copy of `value` for Win32 calls. Credentials use
/// their own zeroizing variant.
pub(crate) fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

/// A kernel handle closed when dropped. Handles are process-wide, so it can
/// move between threads.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    pub(crate) fn into_file(self) -> File {
        let handle = std::mem::ManuallyDrop::new(self);
        unsafe { File::from_raw_handle(handle.0.0) }
    }

    /// Gives up ownership without closing the handle.
    pub(crate) fn into_raw(self) -> HANDLE {
        std::mem::ManuallyDrop::new(self).0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// The Windows directory the kernel reports. Unlike the `SystemRoot`
/// variable, a process's environment cannot redirect it.
pub(crate) fn windows_directory() -> anyhow::Result<PathBuf> {
    let mut directory = vec![0_u16; 260];
    let length = unsafe { GetSystemWindowsDirectoryW(Some(&mut directory)) } as usize;
    if length == 0 || length >= directory.len() {
        bail!("Windows did not provide its system directory");
    }
    Ok(PathBuf::from(OsString::from_wide(&directory[..length])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_end_with_nul() {
        assert_eq!(wide("ab"), vec![97, 98, 0]);
        assert_eq!(wide(OsStr::new("é")), vec![0xe9, 0]);
    }

    #[test]
    fn windows_directory_is_absolute() {
        let directory = windows_directory().unwrap();
        assert!(directory.is_absolute(), "{}", directory.display());
        assert!(directory.join("System32").is_dir());
    }
}
