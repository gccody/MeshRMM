//! Registers this viewer as the `meshrmm:` link handler for the current
//! Windows user. Values are written only when they differ, and the link is
//! passed after `--`, so text in a link cannot become a command-line option.
use anyhow::Context;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR};

const CLASS_KEY: &str = r"Software\Classes\meshrmm";
const COMMAND_KEY: &str = r"Software\Classes\meshrmm\shell\open\command";

pub fn register() -> anyhow::Result<()> {
    let executable = std::env::current_exe().context("could not locate the viewer executable")?;
    let command = format!("\"{}\" -- \"%1\"", executable.display());
    let mut changed = false;
    changed |= set_string(CLASS_KEY, None, "URL:MeshRMM Remote Protocol")?;
    changed |= set_string(CLASS_KEY, Some("URL Protocol"), "")?;
    changed |= set_string(COMMAND_KEY, None, &command)?;
    if changed {
        tracing::info!(command, "registered the meshrmm: link handler");
    }
    Ok(())
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// Sets a string value under HKEY_CURRENT_USER. Returns whether it changed.
fn set_string(subkey: &str, name: Option<&str>, value: &str) -> anyhow::Result<bool> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(subkey),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if status != ERROR_SUCCESS {
        anyhow::bail!("could not open HKCU\\{subkey}: {status:?}");
    }
    let key = Key(key);
    let name = name.map(HSTRING::from);
    let name = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
    let wanted: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let wanted_bytes: Vec<u8> = wanted.iter().flat_map(|unit| unit.to_le_bytes()).collect();
    if read_string(&key, name).as_deref() == Some(wanted_bytes.as_slice()) {
        return Ok(false);
    }
    let status = unsafe { RegSetValueExW(key.0, name, None, REG_SZ, Some(&wanted_bytes)) };
    if status != ERROR_SUCCESS {
        anyhow::bail!("could not write HKCU\\{subkey}: {status:?}");
    }
    Ok(true)
}

/// The raw bytes of a REG_SZ value, including its terminator.
fn read_string(key: &Key, name: PCWSTR) -> Option<Vec<u8>> {
    let mut kind = REG_VALUE_TYPE::default();
    let mut length = 0_u32;
    let status =
        unsafe { RegQueryValueExW(key.0, name, None, Some(&mut kind), None, Some(&mut length)) };
    if status != ERROR_SUCCESS || kind != REG_SZ {
        return None;
    }
    let mut data = vec![0_u8; length as usize];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&mut kind),
            Some(data.as_mut_ptr()),
            Some(&mut length),
        )
    };
    (status == ERROR_SUCCESS && kind == REG_SZ).then(|| {
        data.truncate(length as usize);
        data
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::RegDeleteTreeW;

    #[test]
    fn values_are_written_only_when_they_change() {
        let subkey = format!(r"Software\MeshRMM-Test-{}", std::process::id());
        let result = std::panic::catch_unwind(|| {
            assert!(set_string(&subkey, None, "first").unwrap());
            assert!(!set_string(&subkey, None, "first").unwrap());
            assert!(set_string(&subkey, None, "second").unwrap());
            assert!(set_string(&subkey, Some("URL Protocol"), "").unwrap());
            assert!(!set_string(&subkey, Some("URL Protocol"), "").unwrap());
        });
        let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(&subkey)) };
        result.unwrap();
    }
}
