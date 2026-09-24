//! Process handling shared by the Agent and viewer update helpers on Windows.
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};
use windows::core::PWSTR;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// A process an update helper waits for, and terminates if it does not exit in time.
pub struct PreviousProcess(HANDLE);

impl PreviousProcess {
    /// Opens the process only while it runs `image` and was created before `created_before`, a
    /// FILETIME value such as [`current_process_created`], so a recycled process ID, including
    /// one reused by a newer instance, is never waited on or terminated. `None` means the process
    /// has exited.
    pub fn open(process_id: u32, image: &Path, created_before: u64) -> Option<Self> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                process_id,
            )
        }
        .ok()?;
        let process = Self(handle);
        let mut name = vec![0_u16; 32_768];
        let mut length = name.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process.0,
                PROCESS_NAME_WIN32,
                PWSTR(name.as_mut_ptr()),
                &mut length,
            )
        }
        .ok()?;
        let name = OsString::from_wide(&name[..length as usize]);
        let same_image = name
            .to_string_lossy()
            .eq_ignore_ascii_case(&image.as_os_str().to_string_lossy());
        let created = creation_time(process.0).ok()?;
        (same_image && created < created_before).then_some(process)
    }

    pub fn has_exited(&self) -> bool {
        self.wait(Duration::ZERO)
    }

    /// Returns whether the process exited within `timeout`.
    pub fn wait(&self, timeout: Duration) -> bool {
        let milliseconds = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        (unsafe { WaitForSingleObject(self.0, milliseconds) }) == WAIT_OBJECT_0
    }

    pub fn terminate(&self) -> windows::core::Result<()> {
        unsafe { TerminateProcess(self.0, 1) }
    }
}

impl Drop for PreviousProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// When this process was created. A process that started this one was created before it.
pub fn current_process_created() -> windows::core::Result<u64> {
    creation_time(unsafe { GetCurrentProcess() })
}

/// The current time, comparable with [`current_process_created`]. A process that is running
/// now was created before it.
pub fn now() -> u64 {
    from_file_time(unsafe { GetSystemTimeAsFileTime() })
}

fn creation_time(process: HANDLE) -> windows::core::Result<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }?;
    Ok(from_file_time(created))
}

fn from_file_time(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// Builds a detached `cmd.exe` that waits about two seconds for `helper` to exit, then deletes it
/// and its now-empty directory. `working_directory` must be outside `helper_directory`.
pub fn helper_cleanup_command(
    helper: &Path,
    helper_directory: &Path,
    working_directory: &Path,
) -> Command {
    let cleanup = format!(
        "ping.exe 127.0.0.1 -n 3 >NUL & del /f /q \"{}\" & rmdir /q \"{}\"",
        helper.display(),
        helper_directory.display()
    );
    let mut command = Command::new("cmd.exe");
    command
        .args(["/D", "/S", "/C"])
        // `arg` would escape the inner quotes as \", which cmd does not understand. With /S, cmd
        // removes only the outer pair of quotes.
        .raw_arg(format!("\"{cleanup}\""))
        .current_dir(working_directory)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_cleanup_deletes_the_helper_and_its_directory() {
        let parent =
            std::env::temp_dir().join(format!("meshrmm cleanup {}-{}", std::process::id(), now()));
        let helper_directory = parent.join("helper dir");
        std::fs::create_dir_all(&helper_directory).unwrap();
        let helper = helper_directory.join("update-helper.exe");
        std::fs::write(&helper, b"MZ helper").unwrap();

        let mut command = helper_cleanup_command(&helper, &helper_directory, &parent);
        // cmd receives the quoted paths unescaped inside one outer pair of quotes.
        let cleanup = format!(
            "\"ping.exe 127.0.0.1 -n 3 >NUL & del /f /q \"{}\" & rmdir /q \"{}\"\"",
            helper.display(),
            helper_directory.display()
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["/D", "/S", "/C", cleanup.as_str()].map(std::ffi::OsStr::new)
        );
        let status = command.spawn().unwrap().wait().unwrap();
        assert!(status.success());
        assert!(!helper_directory.exists());
        std::fs::remove_dir(&parent).unwrap();
    }

    #[test]
    fn only_processes_created_before_the_cutoff_open() {
        let current = std::process::id();
        let image = std::env::current_exe().unwrap();
        assert!(PreviousProcess::open(current, &image, now()).is_some());
        assert!(
            PreviousProcess::open(current, &image, current_process_created().unwrap()).is_none()
        );
        let other = image.with_file_name("meshrmm-remote.exe");
        assert!(PreviousProcess::open(current, &other, now()).is_none());
    }
}
