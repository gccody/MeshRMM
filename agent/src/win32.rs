//! Small Win32 helpers shared across the Agent.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;

use anyhow::{Context, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::{
    DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, UpdateProcThreadAttribute,
};

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

/// Creates a non-inheritable pipe and returns its (read, write) ends.
pub(crate) fn create_pipe() -> anyhow::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, None, 0) }.context("failed to create a pipe")?;
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

/// A PROC_THREAD_ATTRIBUTE_HANDLE_LIST that limits what a child inherits to
/// the listed handles. The handle array must outlive the process launch.
pub(crate) struct HandleListAttribute<'a> {
    // u64 storage keeps the opaque attribute list pointer-aligned.
    buffer: Vec<u64>,
    _handles: std::marker::PhantomData<&'a [HANDLE]>,
}

impl<'a> HandleListAttribute<'a> {
    pub(crate) fn new(handles: &'a [HANDLE]) -> anyhow::Result<Self> {
        let mut size = 0;
        // The sizing call reports ERROR_INSUFFICIENT_BUFFER by design.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        anyhow::ensure!(size > 0, "Windows reported no process attribute list size");
        let mut buffer = vec![0u64; size.div_ceil(std::mem::size_of::<u64>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
            .context("failed to create a process attribute list")?;
        let attribute = Self {
            buffer,
            _handles: std::marker::PhantomData,
        };
        unsafe {
            UpdateProcThreadAttribute(
                attribute.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(handles),
                None,
                None,
            )
        }
        .context("failed to limit the handles a child process inherits")?;
        Ok(attribute)
    }

    pub(crate) fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_ptr().cast_mut().cast())
    }
}

impl Drop for HandleListAttribute<'_> {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
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
