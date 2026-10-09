//! Run-key and Startup-folder entries, and their StartupApproved state.
use anyhow::ensure;
use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::core::{PCWSTR, PWSTR};

use super::time;
use crate::win32::wide;

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

#[derive(Clone, Default)]
pub struct Startup {
    pub name: String,
    pub command: String,
    pub location: String,
    pub enabled: bool,
    pub root: String,
    pub run_key: String,
    pub approval_key: String,
    pub file: Option<(std::path::PathBuf, std::time::SystemTime)>,
}
fn open_key(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> anyhow::Result<Key> {
    let mut key = HKEY::default();
    let path = wide(path);
    unsafe {
        RegOpenKeyExW(root, PCWSTR(path.as_ptr()), None, access, &mut key).ok()?;
    }
    Ok(Key(key))
}
fn approval(root: HKEY, key: &str, name: &str) -> Option<Vec<u8>> {
    let key = wide(key);
    let name = wide(name);
    let mut size = 0;
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size),
        )
        .ok()
        .ok()?;
        if size > 4096 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(bytes.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
        Some(bytes)
    }
}
fn registry_string(root: HKEY, path: &str, name: &str) -> Option<String> {
    let path = wide(path);
    let name = wide(name);
    let mut text = vec![0u16; 32768];
    let mut size = (text.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(path.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
    Some(String::from_utf16_lossy(&text[..end]))
}
/// The machine hive and every loaded user profile hive, as (name, root, subkey prefix).
fn startup_roots() -> anyhow::Result<Vec<(String, HKEY, String)>> {
    let mut roots = vec![("Machine".to_string(), HKEY_LOCAL_MACHINE, String::new())];
    unsafe {
        for index in 0..1024 {
            let mut name = [0u16; 256];
            let mut size = name.len() as u32;
            let result = RegEnumKeyExW(
                HKEY_USERS,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &mut size,
                None,
                None,
                None,
                None,
            );
            if result == ERROR_NO_MORE_ITEMS {
                break;
            }
            result.ok()?;
            let name = String::from_utf16_lossy(&name[..size as usize]);
            if name.starts_with("S-1-5-21-") && !name.ends_with("_Classes") {
                roots.push((name.clone(), HKEY_USERS, format!("{name}\\")));
            }
        }
    }
    Ok(roots)
}
pub(super) fn startups() -> anyhow::Result<Vec<Startup>> {
    let mut rows = Vec::new();
    for (root_name, root, prefix) in startup_roots()? {
        folder_startups(&root_name, root, &prefix, &mut rows);
        run_key_startups(&root_name, root, &prefix, &mut rows)?;
    }
    Ok(rows)
}
fn folder_startups(root_name: &str, root: HKEY, prefix: &str, rows: &mut Vec<Startup>) {
    // Shell Folders stores the user's expanded path, including redirection.
    // Never expand USERPROFILE against this SYSTEM helper's environment.
    let shell =
        format!("{prefix}Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Shell Folders");
    let folder = registry_string(
        root,
        &shell,
        if root_name == "Machine" {
            "Common Startup"
        } else {
            "Startup"
        },
    );
    let Some(folder) = folder else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&folder) else {
        return;
    };
    let approved = format!(
        "{prefix}Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder"
    );
    for entry in entries.take(10000).flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.eq_ignore_ascii_case("desktop.ini") {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let enabled =
            approval(root, &approved, &name).is_none_or(|v| v.first().is_none_or(|s| s & 1 == 0));
        rows.push(Startup {
            name,
            command: entry.path().display().to_string(),
            location: if root_name == "Machine" {
                "All users".into()
            } else {
                root_name.to_owned()
            },
            enabled,
            root: root_name.to_owned(),
            run_key: folder.clone(),
            approval_key: approved.clone(),
            file: Some((entry.path(), modified)),
        });
    }
}
fn run_key_startups(
    root_name: &str,
    root: HKEY,
    prefix: &str,
    rows: &mut Vec<Startup>,
) -> anyhow::Result<()> {
    for (run, approved) in [
        ("Software\\Microsoft\\Windows\\CurrentVersion\\Run", "Run"),
        (
            "Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Run",
            "Run32",
        ),
    ] {
        let path = format!("{prefix}{run}");
        let Ok(key) = open_key(root, &path, KEY_READ) else {
            continue;
        };
        let approved = format!(
            "{prefix}Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\{approved}"
        );
        for index in 0..10000 {
            let mut name = vec![0u16; 16384];
            let mut name_len = name.len() as u32;
            let mut value = vec![0u16; 32768];
            let mut size = (value.len() * 2) as u32;
            let mut kind = 0;
            let result = unsafe {
                RegEnumValueW(
                    key.0,
                    index,
                    Some(PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut kind),
                    Some(value.as_mut_ptr().cast()),
                    Some(&mut size),
                )
            };
            if result == ERROR_NO_MORE_ITEMS {
                break;
            }
            result.ok()?;
            if kind != REG_SZ.0 && kind != REG_EXPAND_SZ.0 {
                continue;
            }
            let name = String::from_utf16_lossy(&name[..name_len as usize]);
            let count = (size as usize / 2).min(value.len());
            let end = value[..count].iter().position(|c| *c == 0).unwrap_or(count);
            let enabled = approval(root, &approved, &name)
                .is_none_or(|v| v.first().is_none_or(|s| s & 1 == 0));
            rows.push(Startup {
                name,
                command: String::from_utf16_lossy(&value[..end]),
                location: if root_name == "Machine" {
                    "All users".into()
                } else {
                    root_name.to_owned()
                },
                enabled,
                root: root_name.to_owned(),
                run_key: path.clone(),
                approval_key: approved.clone(),
                file: None,
            });
        }
    }
    Ok(())
}
pub fn startup_action(row: &Startup) -> anyhow::Result<()> {
    let root = if row.root == "Machine" {
        HKEY_LOCAL_MACHINE
    } else {
        HKEY_USERS
    };
    set_startup(root, row)
}
fn set_startup(root: HKEY, row: &Startup) -> anyhow::Result<()> {
    let name = wide(&row.name);
    if let Some((path, modified)) = &row.file {
        ensure!(
            path.is_file() && path.metadata()?.modified()? == *modified,
            "The startup file changed. Refresh before changing it."
        );
    } else {
        // Re-read the original command before writing only the approval value.
        let key = wide(&row.run_key);
        let mut command = vec![0u16; 32768];
        let mut size = (command.len() * 2) as u32;
        unsafe {
            RegGetValueW(
                root,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                None,
                Some(command.as_mut_ptr().cast()),
                Some(&mut size),
            )
            .ok()?;
        }
        let end = command
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(command.len());
        ensure!(
            String::from_utf16_lossy(&command[..end]) == row.command,
            "The startup entry changed. Refresh before changing it."
        );
    }
    let enabled = approval(root, &row.approval_key, &row.name)
        .is_none_or(|v| v.first().is_none_or(|s| s & 1 == 0));
    ensure!(
        enabled == row.enabled,
        "Startup status changed. Refresh before changing it."
    );
    let mut value = [0u8; 12];
    value[0] = if row.enabled { 3 } else { 2 };
    if row.enabled {
        value[4..].copy_from_slice(&time(unsafe { GetSystemTimeAsFileTime() }).to_le_bytes());
    }
    let key = wide(&row.approval_key);
    unsafe {
        RegSetKeyValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_BINARY.0,
            Some(value.as_ptr().cast()),
            value.len() as u32,
        )
        .ok()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_toggle_preserves_command_and_rejects_stale_state() {
        let path = format!("Software\\MeshRMMTests\\{}", uuid::Uuid::new_v4());
        let key = wide(&path);
        let mut handle = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &mut handle,
                None,
            )
            .ok()
            .unwrap();
        }
        let owner = Key(handle);
        let mut row = Startup {
            name: "Fixture".into(),
            command: "notepad.exe".into(),
            enabled: true,
            run_key: format!("{path}\\Run"),
            approval_key: format!("{path}\\Approved"),
            ..Default::default()
        };
        let run = wide(&row.run_key);
        let name = wide(&row.name);
        let command = wide(&row.command);
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(run.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(command.as_ptr().cast()),
                (command.len() * 2) as u32,
            )
            .ok()
            .unwrap();
        }
        let disabled = set_startup(HKEY_CURRENT_USER, &row);
        let stale = set_startup(HKEY_CURRENT_USER, &row);
        row.enabled = false;
        let enabled = set_startup(HKEY_CURRENT_USER, &row);
        let value = approval(HKEY_CURRENT_USER, &row.approval_key, &row.name);
        row.command = "changed".into();
        let changed = set_startup(HKEY_CURRENT_USER, &row);
        drop(owner);
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()))
                .ok()
                .unwrap();
        }
        disabled.unwrap();
        assert!(stale.is_err());
        enabled.unwrap();
        assert_eq!(value.unwrap()[0], 2);
        assert!(changed.is_err());
    }
}
