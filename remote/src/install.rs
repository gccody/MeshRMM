//! Installs the Windows viewer for the current user. A viewer started from
//! anywhere else, such as the Downloads folder, copies itself to
//! `%LOCALAPPDATA%\Programs\MeshRMM Remote` and hands its launch to that copy,
//! so links never start a downloaded file. The installed viewer registers the
//! `meshrmm:` link handler and its Apps & features entry each time it starts;
//! `--uninstall` removes all of it.
//!
//! Registry values are written only when they differ, and the link is passed
//! after `--`, so text in a link cannot become a command-line option.
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR};

const PRODUCT: &str = "MeshRMM Remote";
const EXECUTABLE: &str = "meshrmm-remote.exe";
/// Configuration a viewer reads from beside its executable; see `config.rs`.
const CONFIG_FILE: &str = "remote.json";
const CLASS_KEY: &str = r"Software\Classes\meshrmm";
const COMMAND_KEY: &str = r"Software\Classes\meshrmm\shell\open\command";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\MeshRMM Remote";

pub fn is_uninstall_invocation() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--uninstall")
}

/// The installed viewer's folder.
fn install_directory() -> anyhow::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .context("Windows did not provide LOCALAPPDATA to install the viewer in")?;
    Ok(PathBuf::from(local).join("Programs").join(PRODUCT))
}

/// Installs this viewer unless it is the installed one. Returns whether it
/// did, in which case the installed copy has taken over this launch (or,
/// without a link, the user has been told the viewer is ready) and this
/// process should exit.
pub fn install_if_needed() -> anyhow::Result<bool> {
    let current = std::env::current_exe().context("could not locate the viewer executable")?;
    let target = install_directory()?.join(EXECUTABLE);
    if is_same_file(&current, &target) {
        return Ok(false);
    }
    install(&current, &target)?;
    let arguments = std::env::args_os().skip(1).collect::<Vec<OsString>>();
    if arguments.is_empty() {
        crate::platform::show_notice(
            PRODUCT,
            "MeshRMM Remote is installed. Choose Connect on a device in the MeshRMM website to \
             start a session.\n\nYou can delete the file you downloaded.",
        );
    } else {
        Command::new(&target)
            .args(arguments)
            .spawn()
            .with_context(|| {
                format!("could not start the installed viewer {}", target.display())
            })?;
    }
    Ok(true)
}

fn is_same_file(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Copies `current` over the installed viewer at `target`, then registers it.
fn install(current: &Path, target: &Path) -> anyhow::Result<()> {
    install_files(current, target)?;
    register(target)?;
    tracing::info!(target = %target.display(), "installed the viewer");
    Ok(())
}

/// Copies `current`, and the configuration beside it if any, to `target`'s
/// folder. A running installed viewer keeps running: Windows lets its
/// executable be renamed aside, and the leftover is removed by the next
/// install or update.
fn install_files(current: &Path, target: &Path) -> anyhow::Result<()> {
    let directory = target
        .parent()
        .context("the viewer install path has no parent directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;
    let staged = directory.join(format!("{EXECUTABLE}.install-{}", std::process::id()));
    if let Err(error) = std::fs::copy(current, &staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("could not copy the viewer to {}", directory.display()));
    }
    let mut previous = previous_path(target);
    remove_if_present(&previous);
    if previous.exists() {
        // A viewer from an earlier install is still running from it.
        previous = target.with_extension(format!("exe.previous-{}", std::process::id()));
    }
    let replaced = target.exists();
    if replaced && let Err(error) = std::fs::rename(target, &previous) {
        let _ = std::fs::remove_file(&staged);
        return Err(error).with_context(|| {
            format!(
                "could not move aside the installed viewer {}",
                target.display()
            )
        });
    }
    if let Err(error) = std::fs::rename(&staged, target) {
        let _ = std::fs::remove_file(&staged);
        if replaced {
            std::fs::rename(&previous, target).context("could not restore the installed viewer")?;
        }
        return Err(error)
            .with_context(|| format!("could not install the viewer as {}", target.display()));
    }
    remove_if_present(&previous);
    if let Some(source) = current.parent().map(|parent| parent.join(CONFIG_FILE))
        && source.is_file()
    {
        std::fs::copy(&source, directory.join(CONFIG_FILE))
            .with_context(|| format!("could not copy {}", source.display()))?;
    }
    Ok(())
}

/// Where an install or update moves the replaced executable; the updater
/// uses the same name.
fn previous_path(target: &Path) -> PathBuf {
    target.with_extension("exe.previous")
}

fn remove_if_present(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "could not remove file"),
    }
}

/// Registers `executable` as the `meshrmm:` link handler and in Apps &
/// features, writing only values that changed.
pub fn register(executable: &Path) -> anyhow::Result<()> {
    let command = format!("\"{}\" -- \"%1\"", executable.display());
    let mut changed = false;
    changed |= set_string(CLASS_KEY, None, "URL:MeshRMM Remote Protocol")?;
    changed |= set_string(CLASS_KEY, Some("URL Protocol"), "")?;
    changed |= set_string(COMMAND_KEY, None, &command)?;
    if changed {
        tracing::info!(command, "registered the meshrmm: link handler");
    }
    let directory = executable
        .parent()
        .context("the viewer executable has no parent directory")?;
    let quoted = format!("\"{}\"", executable.display());
    let mut changed = false;
    changed |= set_string(UNINSTALL_KEY, Some("DisplayName"), PRODUCT)?;
    changed |= set_string(
        UNINSTALL_KEY,
        Some("DisplayVersion"),
        meshrmm_self_update::CURRENT_VERSION,
    )?;
    changed |= set_string(UNINSTALL_KEY, Some("Publisher"), "MeshRMM")?;
    changed |= set_string(UNINSTALL_KEY, Some("DisplayIcon"), &quoted)?;
    changed |= set_string(
        UNINSTALL_KEY,
        Some("InstallLocation"),
        &directory.display().to_string(),
    )?;
    changed |= set_string(
        UNINSTALL_KEY,
        Some("UninstallString"),
        &format!("{quoted} --uninstall"),
    )?;
    changed |= set_dword(UNINSTALL_KEY, "NoModify", 1)?;
    changed |= set_dword(UNINSTALL_KEY, "NoRepair", 1)?;
    if changed {
        tracing::info!("registered the viewer in Apps & features");
    }
    Ok(())
}

/// Removes the link handler, the Apps & features entry and the installed
/// files. The user's settings, identity and logs stay. A running executable
/// can't delete itself, so when this is the installed viewer it returns the
/// command that removes it and its folder, to start just before exiting.
pub fn uninstall() -> anyhow::Result<Option<Command>> {
    let directory = install_directory()?;
    for key in [CLASS_KEY, UNINSTALL_KEY] {
        delete_tree(key)?;
    }
    let current = std::env::current_exe().context("could not locate the viewer executable")?;
    let running_installed = is_same_file(&current, &directory.join(EXECUTABLE));
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", directory.display()));
        }
    };
    for entry in entries {
        let path = entry?.path();
        if running_installed && is_same_file(&path, &current) {
            continue;
        }
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .with_context(|| {
            format!(
                "could not remove {}; close MeshRMM Remote and try again",
                path.display()
            )
        })?;
    }
    if running_installed {
        let parent = directory
            .parent()
            .context("the viewer install folder has no parent directory")?;
        return Ok(Some(meshrmm_self_update::windows::helper_cleanup_command(
            &current, &directory, parent,
        )));
    }
    std::fs::remove_dir(&directory)
        .with_context(|| format!("could not remove {}", directory.display()))?;
    Ok(None)
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn open_key(subkey: &str) -> anyhow::Result<Key> {
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
        bail!("could not open HKCU\\{subkey}: {status:?}");
    }
    Ok(Key(key))
}

/// Sets a string value under HKEY_CURRENT_USER. Returns whether it changed.
fn set_string(subkey: &str, name: Option<&str>, value: &str) -> anyhow::Result<bool> {
    let wanted: Vec<u8> = value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    set_value(subkey, name, REG_SZ, &wanted)
}

/// Sets a DWORD value under HKEY_CURRENT_USER. Returns whether it changed.
fn set_dword(subkey: &str, name: &str, value: u32) -> anyhow::Result<bool> {
    set_value(subkey, Some(name), REG_DWORD, &value.to_le_bytes())
}

fn set_value(
    subkey: &str,
    name: Option<&str>,
    kind: REG_VALUE_TYPE,
    wanted: &[u8],
) -> anyhow::Result<bool> {
    let key = open_key(subkey)?;
    let name = name.map(HSTRING::from);
    let name = name
        .as_ref()
        .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr()));
    if read_value(&key, name, kind).as_deref() == Some(wanted) {
        return Ok(false);
    }
    let status = unsafe { RegSetValueExW(key.0, name, None, kind, Some(wanted)) };
    if status != ERROR_SUCCESS {
        bail!("could not write HKCU\\{subkey}: {status:?}");
    }
    Ok(true)
}

/// The raw bytes of a value of type `kind`, including a string's terminator.
fn read_value(key: &Key, name: PCWSTR, kind: REG_VALUE_TYPE) -> Option<Vec<u8>> {
    let mut found = REG_VALUE_TYPE::default();
    let mut length = 0_u32;
    let status =
        unsafe { RegQueryValueExW(key.0, name, None, Some(&mut found), None, Some(&mut length)) };
    if status != ERROR_SUCCESS || found != kind {
        return None;
    }
    let mut data = vec![0_u8; length as usize];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            name,
            None,
            Some(&mut found),
            Some(data.as_mut_ptr()),
            Some(&mut length),
        )
    };
    (status == ERROR_SUCCESS && found == kind).then(|| {
        data.truncate(length as usize);
        data
    })
}

/// Deletes a key under HKEY_CURRENT_USER with everything below it; a missing
/// key is already deleted.
fn delete_tree(subkey: &str) -> anyhow::Result<()> {
    let status: WIN32_ERROR = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(subkey)) };
    if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
        bail!("could not delete HKCU\\{subkey}: {status:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(name: &str) -> String {
        format!(r"Software\MeshRMM-Test-{name}-{}", std::process::id())
    }

    #[test]
    fn values_are_written_only_when_they_change() {
        let subkey = test_key("values");
        let result = std::panic::catch_unwind(|| {
            assert!(set_string(&subkey, None, "first").unwrap());
            assert!(!set_string(&subkey, None, "first").unwrap());
            assert!(set_string(&subkey, None, "second").unwrap());
            assert!(set_string(&subkey, Some("URL Protocol"), "").unwrap());
            assert!(!set_string(&subkey, Some("URL Protocol"), "").unwrap());
            assert!(set_dword(&subkey, "NoModify", 1).unwrap());
            assert!(!set_dword(&subkey, "NoModify", 1).unwrap());
            // The same bytes as another type are a change.
            assert!(set_string(&subkey, Some("NoModify"), "\u{1}").unwrap());
        });
        delete_tree(&subkey).unwrap();
        result.unwrap();
    }

    #[test]
    fn deleting_a_missing_key_succeeds() {
        let subkey = test_key("missing");
        delete_tree(&subkey).unwrap();
        set_string(&subkey, None, "value").unwrap();
        delete_tree(&subkey).unwrap();
        delete_tree(&subkey).unwrap();
    }

    #[test]
    fn install_replaces_the_target_and_copies_its_configuration() {
        let root = std::env::temp_dir().join(format!("meshrmm install {}", std::process::id()));
        let source = root.join("Downloads");
        let directory = root.join("Programs").join(PRODUCT);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let current = source.join("meshrmm-remote-windows-x64.exe");
        let target = directory.join(EXECUTABLE);
        std::fs::write(&current, b"MZ new").unwrap();
        std::fs::write(source.join(CONFIG_FILE), b"{}").unwrap();
        std::fs::write(&target, b"MZ old").unwrap();
        std::fs::write(previous_path(&target), b"MZ older").unwrap();

        // Registering touches the user's real link handler, so only the file
        // steps run here; `register` is covered by the values test.
        let result = install_files(&current, &target);
        let installed = std::fs::read(&target);
        let config = std::fs::read(directory.join(CONFIG_FILE));
        let leftovers = std::fs::read_dir(&directory).unwrap().count();
        std::fs::remove_dir_all(&root).unwrap();
        result.unwrap();
        assert_eq!(installed.unwrap(), b"MZ new");
        assert_eq!(config.unwrap(), b"{}");
        assert_eq!(leftovers, 2);
    }
}
