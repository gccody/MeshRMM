//! Viewer-wide preferences for the current OS user, independent of agent/session IDs.
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
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            disconnect_confirmation: true,
            clipboard_sync: true,
            clear_clipboard_on_close: true,
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
            },
        )
        .unwrap();
        assert!(!load(&path).disconnect_confirmation);
        assert!(!load(&path).clipboard_sync);
        assert!(!load(&path).clear_clipboard_on_close);
        save(&path, &Preferences::default()).unwrap();
        assert!(load(&path).disconnect_confirmation);
        assert!(load(&path).clipboard_sync);
        assert!(load(&path).clear_clipboard_on_close);
        // Files written before these preferences existed keep their defaults.
        std::fs::write(&path, r#"{"disconnect_confirmation":false}"#).unwrap();
        assert!(!load(&path).disconnect_confirmation);
        assert!(load(&path).clipboard_sync);
        assert!(load(&path).clear_clipboard_on_close);
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
}
