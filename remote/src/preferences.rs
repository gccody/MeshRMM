//! Viewer-wide preferences for the current OS user, independent of agent/session IDs.
use crate::shortcuts::{ShortcutKey, ViewerShortcut};
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Preferences {
    disconnect_confirmation: bool,
    clipboard_sync: bool,
    clear_clipboard_on_close: bool,
    /// macOS: the Command key sends Ctrl instead of the Windows key.
    command_as_control: bool,
    /// Windows: the Windows key, Alt+Tab, Alt+Esc and Ctrl+Esc go to the device.
    send_windows_shortcuts: bool,
    diagnostics_key: ShortcutKey,
    /// Windows: cycles the viewed display.
    next_display_key: ShortcutKey,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            disconnect_confirmation: true,
            clipboard_sync: true,
            clear_clipboard_on_close: true,
            command_as_control: false,
            send_windows_shortcuts: true,
            diagnostics_key: ShortcutKey::F12,
            next_display_key: ShortcutKey::F8,
        }
    }
}
static PREFERENCES: OnceLock<Mutex<Preferences>> = OnceLock::new();
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn path() -> anyhow::Result<PathBuf> {
    #[cfg(windows)]
    let root = PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is unavailable")?);
    #[cfg(not(windows))]
    let root = PathBuf::from(std::env::var_os("HOME").context("Home directory is unavailable")?)
        .join("Library/Application Support");
    Ok(root.join("MeshRMM/viewer-preferences.json"))
}
fn load(path: &Path) -> Preferences {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error,"invalid viewer preferences; using defaults");
            Preferences::default()
        }),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error,"could not read viewer preferences");
            }
            Preferences::default()
        }
    }
}
fn current() -> &'static Mutex<Preferences> {
    PREFERENCES.get_or_init(|| Mutex::new(path().map(|p| load(&p)).unwrap_or_default()))
}
fn get<T>(field: fn(&Preferences) -> T) -> T {
    field(&current().lock().unwrap_or_else(|e| e.into_inner()))
}
fn update(change: impl FnOnce(&mut Preferences)) -> anyhow::Result<()> {
    let mut current = current().lock().unwrap_or_else(|e| e.into_inner());
    let mut next = current.clone();
    change(&mut next);
    save(&path()?, &next)?;
    *current = next;
    Ok(())
}
fn toggle(field: fn(&mut Preferences) -> &mut bool) -> anyhow::Result<()> {
    update(|p| {
        let value = field(p);
        *value = !*value;
    })
}
pub fn disconnect_confirmation() -> bool {
    get(|p| p.disconnect_confirmation)
}
pub fn toggle_disconnect_confirmation() -> anyhow::Result<()> {
    toggle(|p| &mut p.disconnect_confirmation)
}
/// Automatic text/rich-text/image and file clipboard exchange with the Agent.
pub fn clipboard_sync() -> bool {
    get(|p| p.clipboard_sync)
}
pub fn toggle_clipboard_sync() -> anyhow::Result<()> {
    toggle(|p| &mut p.clipboard_sync)
}
/// Empty the viewed Windows session's clipboard when the remote session ends.
pub fn clear_clipboard_on_close() -> bool {
    get(|p| p.clear_clipboard_on_close)
}
pub fn toggle_clear_clipboard_on_close() -> anyhow::Result<()> {
    toggle(|p| &mut p.clear_clipboard_on_close)
}
#[cfg(target_os = "macos")]
pub fn command_as_control() -> bool {
    get(|p| p.command_as_control)
}
#[cfg(target_os = "macos")]
pub fn toggle_command_as_control() -> anyhow::Result<()> {
    toggle(|p| &mut p.command_as_control)
}
#[cfg(windows)]
pub fn send_windows_shortcuts() -> bool {
    get(|p| p.send_windows_shortcuts)
}
#[cfg(windows)]
pub fn toggle_send_windows_shortcuts() -> anyhow::Result<()> {
    toggle(|p| &mut p.send_windows_shortcuts)
}
pub fn shortcut_key(shortcut: ViewerShortcut) -> ShortcutKey {
    let (diagnostics, next_display) = get(|p| (p.diagnostics_key, p.next_display_key));
    match shortcut {
        ViewerShortcut::Diagnostics => diagnostics,
        ViewerShortcut::NextDisplay => next_display,
    }
}
pub fn set_shortcut_key(shortcut: ViewerShortcut, key: ShortcutKey) -> anyhow::Result<()> {
    update(|p| assign_shortcut(p, shortcut, key))
}
/// A key serves one shortcut: giving it to one turns the other off.
fn assign_shortcut(preferences: &mut Preferences, shortcut: ViewerShortcut, key: ShortcutKey) {
    let (assigned, other) = match shortcut {
        ViewerShortcut::Diagnostics => (
            &mut preferences.diagnostics_key,
            &mut preferences.next_display_key,
        ),
        ViewerShortcut::NextDisplay => (
            &mut preferences.next_display_key,
            &mut preferences.diagnostics_key,
        ),
    };
    *assigned = key;
    if *other == key {
        *other = ShortcutKey::Off;
    }
}
fn save(path: &Path, preferences: &Preferences) -> anyhow::Result<()> {
    let directory = path.parent().context("preferences path has no parent")?;
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(
        ".viewer-preferences-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(preferences)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.context("Could not save viewer preferences")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_default_on_and_persist_across_new_loads() {
        let dir = std::env::temp_dir().join(format!(
            "meshrmm-preferences-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = dir.join("preferences.json");
        assert!(load(&path).disconnect_confirmation);
        assert!(load(&path).clipboard_sync);
        save(
            &path,
            &Preferences {
                disconnect_confirmation: false,
                clipboard_sync: false,
                clear_clipboard_on_close: false,
                command_as_control: true,
                ..Preferences::default()
            },
        )
        .unwrap();
        assert!(!load(&path).disconnect_confirmation);
        assert!(!load(&path).clipboard_sync);
        assert!(!load(&path).clear_clipboard_on_close);
        assert!(load(&path).command_as_control);
        save(&path, &Preferences::default()).unwrap();
        assert!(load(&path).disconnect_confirmation);
        assert!(load(&path).clipboard_sync);
        assert!(load(&path).clear_clipboard_on_close);
        // Files written before these preferences existed keep their defaults.
        std::fs::write(&path, r#"{"disconnect_confirmation":false}"#).unwrap();
        assert!(!load(&path).disconnect_confirmation);
        assert!(load(&path).clipboard_sync);
        assert!(load(&path).clear_clipboard_on_close);
        assert!(!load(&path).command_as_control);
        assert!(load(&path).send_windows_shortcuts);
        assert_eq!(load(&path).diagnostics_key, ShortcutKey::F12);
        assert_eq!(load(&path).next_display_key, ShortcutKey::F8);
        // The session close action used to be saved here; it is now per session.
        std::fs::write(
            &path,
            r#"{"disconnect_confirmation":false,"session_close_action":"lock"}"#,
        )
        .unwrap();
        assert!(!load(&path).disconnect_confirmation);
        std::fs::write(&path, "broken json").unwrap();
        assert!(load(&path).disconnect_confirmation);
        assert!(load(&path).clipboard_sync);
        assert!(load(&path).clear_clipboard_on_close);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn shortcut_keys_persist_and_never_share_a_key() {
        let mut preferences = Preferences::default();
        assign_shortcut(
            &mut preferences,
            ViewerShortcut::Diagnostics,
            ShortcutKey::F11,
        );
        assert_eq!(
            (preferences.diagnostics_key, preferences.next_display_key),
            (ShortcutKey::F11, ShortcutKey::F8)
        );
        assign_shortcut(
            &mut preferences,
            ViewerShortcut::NextDisplay,
            ShortcutKey::F11,
        );
        assert_eq!(
            (preferences.diagnostics_key, preferences.next_display_key),
            (ShortcutKey::Off, ShortcutKey::F11)
        );
        assign_shortcut(
            &mut preferences,
            ViewerShortcut::Diagnostics,
            ShortcutKey::Off,
        );
        assign_shortcut(
            &mut preferences,
            ViewerShortcut::NextDisplay,
            ShortcutKey::Off,
        );
        assert_eq!(
            (preferences.diagnostics_key, preferences.next_display_key),
            (ShortcutKey::Off, ShortcutKey::Off)
        );
        let json = serde_json::to_string(&Preferences {
            diagnostics_key: ShortcutKey::F10,
            send_windows_shortcuts: false,
            ..Preferences::default()
        })
        .unwrap();
        let loaded: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.diagnostics_key, ShortcutKey::F10);
        assert!(!loaded.send_windows_shortcuts);
        // An unknown key makes the file invalid, so the defaults apply.
        let invalid: Result<Preferences, _> = serde_json::from_str(r#"{"diagnostics_key":"F1"}"#);
        assert!(invalid.is_err());
    }
}
