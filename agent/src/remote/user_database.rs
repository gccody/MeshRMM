//! Accounts from the user database (`getpwuid_r`), for acting as a user on
//! macOS and Linux.
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::ffi::{CStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// An account from the user database.
#[derive(Debug, Clone)]
pub(crate) struct UserEntry {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) name: String,
    pub(crate) home: PathBuf,
    pub(crate) shell: PathBuf,
}

impl UserEntry {
    pub(crate) fn of(uid: u32) -> Option<Self> {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::zeroed();
        let mut buffer = vec![0 as libc::c_char; 16 * 1024];
        let mut found = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the sizes given.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &mut found,
            )
        };
        if status != 0 || found.is_null() {
            return None;
        }
        // SAFETY: getpwuid_r filled in the entry, whose strings live in `buffer`.
        let entry = unsafe { entry.assume_init() };
        let text = |field: *const libc::c_char| {
            if field.is_null() {
                OsString::new()
            } else {
                // SAFETY: the entry's strings are NUL-terminated.
                OsString::from(std::ffi::OsStr::from_bytes(
                    unsafe { CStr::from_ptr(field) }.to_bytes(),
                ))
            }
        };
        Some(Self {
            uid,
            gid: entry.pw_gid,
            name: text(entry.pw_name).to_string_lossy().into_owned(),
            home: PathBuf::from(text(entry.pw_dir)),
            shell: PathBuf::from(text(entry.pw_shell)),
        })
    }

    /// The account's supplementary groups, its primary group included.
    #[cfg(target_os = "linux")]
    pub(crate) fn groups(&self) -> Vec<libc::gid_t> {
        let mut groups = vec![0 as libc::gid_t; 256];
        let mut count = groups.len() as libc::c_int;
        if let Ok(name) = CString::new(self.name.clone()) {
            // SAFETY: the list holds `count` entries.
            if unsafe {
                libc::getgrouplist(name.as_ptr(), self.gid, groups.as_mut_ptr(), &mut count)
            } < 0
            {
                count = 0;
            }
        } else {
            count = 0;
        }
        groups.truncate(count.max(0) as usize);
        if groups.is_empty() {
            groups.push(self.gid);
        }
        groups
    }

    /// The home folder, or `/` when it's gone.
    pub(crate) fn directory(&self) -> PathBuf {
        if self.home.is_dir() {
            self.home.clone()
        } else {
            PathBuf::from("/")
        }
    }
}
