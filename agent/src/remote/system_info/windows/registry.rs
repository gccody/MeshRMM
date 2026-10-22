//! Read-only registry keys, closed when dropped.
use windows::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, KEY_READ, REG_SAM_FLAGS, RRF_NOEXPAND, RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ,
    RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::win32::wide;

/// The longest string value read, in UTF-16 units. Uninstall entries and
/// version keys are far shorter.
const MAX_STRING_UNITS: u32 = 4096;

pub(super) struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened by `open` and is closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

impl Key {
    /// Opens `path` under `root` for reading, in the registry view `view`
    /// adds (`KEY_WOW64_64KEY`, `KEY_WOW64_32KEY`, or none).
    pub(super) fn open(root: HKEY, path: &str, view: REG_SAM_FLAGS) -> anyhow::Result<Self> {
        let path = wide(path);
        let mut key = HKEY::default();
        // SAFETY: the path is NUL-terminated and `key` is writable.
        unsafe { RegOpenKeyExW(root, PCWSTR(path.as_ptr()), None, KEY_READ | view, &mut key) }
            .ok()?;
        Ok(Self(key))
    }

    /// Opens a subkey of this key.
    pub(super) fn child(&self, name: &str, view: REG_SAM_FLAGS) -> anyhow::Result<Self> {
        Self::open(self.0, name, view)
    }

    /// A `REG_SZ` or `REG_EXPAND_SZ` value, unexpanded, or `None` if it is
    /// missing or of another type.
    pub(super) fn string(&self, name: &str) -> Option<String> {
        let name = wide(name);
        let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
        let mut size = 0u32;
        // SAFETY: a null buffer asks for the size in bytes.
        let result = unsafe {
            RegGetValueW(
                self.0,
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                flags,
                None,
                None,
                Some(&mut size),
            )
        };
        if result != ERROR_SUCCESS && result != ERROR_MORE_DATA {
            return None;
        }
        let units = (size / 2 + 1).min(MAX_STRING_UNITS);
        let mut text = vec![0u16; units as usize];
        let mut size = units * 2;
        // SAFETY: the buffer holds `size` bytes; a value that grew past it
        // fails with ERROR_MORE_DATA and is skipped.
        unsafe {
            RegGetValueW(
                self.0,
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                flags,
                None,
                Some(text.as_mut_ptr().cast()),
                Some(&mut size),
            )
        }
        .ok()
        .ok()?;
        let end = text
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(text.len());
        Some(String::from_utf16_lossy(&text[..end]))
    }

    /// A `REG_DWORD` value.
    pub(super) fn dword(&self, name: &str) -> Option<u32> {
        let name = wide(name);
        let mut value = 0u32;
        let mut size = 4u32;
        // SAFETY: `value` holds the four bytes `size` says.
        unsafe {
            RegGetValueW(
                self.0,
                PCWSTR::null(),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some((&raw mut value).cast()),
                Some(&mut size),
            )
        }
        .ok()
        .ok()?;
        Some(value)
    }

    /// The names of the key's subkeys.
    pub(super) fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        for index in 0..65_536u32 {
            // Key names are at most 255 characters.
            let mut name = [0u16; 256];
            let mut length = name.len() as u32;
            // SAFETY: `name` holds `length` units.
            let result = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut length,
                    None,
                    None,
                    None,
                    None,
                )
            };
            if result == ERROR_NO_MORE_ITEMS {
                break;
            }
            if result != ERROR_SUCCESS {
                // A key removed while listing; the rest still list.
                continue;
            }
            names.push(String::from_utf16_lossy(&name[..length as usize]));
        }
        names
    }
}
