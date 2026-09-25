//! Keys the viewer keeps for itself, and the Windows shortcuts it sends to the
//! device instead of letting the local Windows act on them.
use serde::{Deserialize, Serialize};

/// A function key a viewer shortcut can use, or none. A key no shortcut uses
/// is sent to the device like any other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShortcutKey {
    F8,
    F9,
    F10,
    F11,
    F12,
    Off,
}

impl ShortcutKey {
    pub const ALL: [Self; 6] = [
        Self::F8,
        Self::F9,
        Self::F10,
        Self::F11,
        Self::F12,
        Self::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::F8 => "F8",
            Self::F9 => "F9",
            Self::F10 => "F10",
            Self::F11 => "F11",
            Self::F12 => "F12",
            Self::Off => "Off (send the key to the device)",
        }
    }

    #[cfg(any(windows, test))]
    fn windows_virtual_key(self) -> Option<u16> {
        match self {
            Self::F8 => Some(0x77),
            Self::F9 => Some(0x78),
            Self::F10 => Some(0x79),
            Self::F11 => Some(0x7a),
            Self::F12 => Some(0x7b),
            Self::Off => None,
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn macos_key_code(self) -> Option<u16> {
        match self {
            Self::F8 => Some(100),
            Self::F9 => Some(101),
            Self::F10 => Some(109),
            Self::F11 => Some(103),
            Self::F12 => Some(111),
            Self::Off => None,
        }
    }
}

/// What a viewer shortcut does. Only the Windows viewer has a display key; the
/// macOS viewer switches displays with Control-Option-Arrow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerShortcut {
    Diagnostics,
    #[cfg_attr(not(windows), allow(dead_code))]
    NextDisplay,
}

/// The shortcut a Windows virtual key triggers, if any.
#[cfg(any(windows, test))]
pub fn windows_shortcut(
    virtual_key: u16,
    diagnostics: ShortcutKey,
    next_display: ShortcutKey,
) -> Option<ViewerShortcut> {
    if next_display.windows_virtual_key() == Some(virtual_key) {
        Some(ViewerShortcut::NextDisplay)
    } else if diagnostics.windows_virtual_key() == Some(virtual_key) {
        Some(ViewerShortcut::Diagnostics)
    } else {
        None
    }
}

/// Whether a macOS key code toggles diagnostics.
#[cfg(any(target_os = "macos", test))]
pub fn macos_toggles_diagnostics(key_code: u16, diagnostics: ShortcutKey) -> bool {
    diagnostics.macos_key_code() == Some(key_code)
}

/// The window-title hint, such as "F8 display · F12 diagnostics".
pub fn hint(display: Option<&str>, diagnostics: ShortcutKey) -> String {
    let mut parts = Vec::new();
    if let Some(display) = display {
        parts.push(format!("{display} display"));
    }
    if diagnostics != ShortcutKey::Off {
        parts.push(format!("{} diagnostics", diagnostics.label()));
    }
    parts.join(" · ")
}

#[cfg(any(windows, test))]
const VK_TAB: u32 = 0x09;
#[cfg(any(windows, test))]
const VK_ESCAPE: u32 = 0x1b;
#[cfg(any(windows, test))]
const VK_LWIN: u32 = 0x5b;
#[cfg(any(windows, test))]
const VK_RWIN: u32 = 0x5c;

/// Decides which keys the Windows viewer's low-level keyboard hook takes from
/// Windows while the viewer has focus: the Windows keys, Alt+Tab, Alt+Esc and
/// Ctrl+Esc. Windows never delivers these to a window, so they are forwarded
/// from the hook. A key's release is taken only if its press was, so a key
/// already held when the viewer gained focus is released locally.
/// Ctrl+Alt+Del and Windows+L always stay with Windows.
#[cfg(any(windows, test))]
#[derive(Default)]
pub struct SystemShortcuts {
    held: Vec<u32>,
}

#[cfg(any(windows, test))]
impl SystemShortcuts {
    /// Returns whether the key event goes to the device instead of Windows.
    pub fn take(&mut self, virtual_key: u32, pressed: bool, alt: bool, control: bool) -> bool {
        if !pressed {
            let held = self.held.iter().position(|key| *key == virtual_key);
            return held.map(|index| self.held.swap_remove(index)).is_some();
        }
        let take = matches!(virtual_key, VK_LWIN | VK_RWIN)
            || (virtual_key == VK_TAB && alt)
            || (virtual_key == VK_ESCAPE && (alt || control));
        if take && !self.held.contains(&virtual_key) {
            self.held.push(virtual_key);
        }
        take
    }

    /// Forgets taken keys when the hook is removed; the viewer releases them
    /// on the device when it loses focus.
    pub fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_keys_trigger_only_their_shortcut() {
        use ShortcutKey::*;
        assert_eq!(
            windows_shortcut(0x77, F12, F8),
            Some(ViewerShortcut::NextDisplay)
        );
        assert_eq!(
            windows_shortcut(0x7b, F12, F8),
            Some(ViewerShortcut::Diagnostics)
        );
        assert_eq!(windows_shortcut(0x7a, F12, F8), None);
        // A shortcut that is off sends its old key to the device.
        assert_eq!(windows_shortcut(0x7b, Off, F8), None);
        assert_eq!(windows_shortcut(0x77, F12, Off), None);
        assert_eq!(
            windows_shortcut(0x7a, F11, Off),
            Some(ViewerShortcut::Diagnostics)
        );
        assert!(macos_toggles_diagnostics(111, F12));
        assert!(!macos_toggles_diagnostics(111, F11));
        assert!(macos_toggles_diagnostics(103, F11));
        assert!(!macos_toggles_diagnostics(111, Off));
    }

    #[test]
    fn title_hint_lists_the_keys_in_use() {
        use ShortcutKey::*;
        assert_eq!(hint(Some("F8"), F12), "F8 display · F12 diagnostics");
        assert_eq!(hint(None, F11), "F11 diagnostics");
        assert_eq!(hint(Some("F9"), Off), "F9 display");
        assert_eq!(hint(None, Off), "");
    }

    #[test]
    fn windows_shortcuts_are_taken_with_their_releases() {
        let mut keys = SystemShortcuts::default();
        // Windows key: press and release both go to the device.
        assert!(keys.take(VK_LWIN, true, false, false));
        assert!(keys.take(VK_LWIN, true, false, false), "auto-repeat");
        assert!(keys.take(VK_LWIN, false, false, false));
        // Alt+Tab, released after Alt.
        assert!(keys.take(VK_TAB, true, true, false));
        assert!(keys.take(VK_TAB, false, false, false));
        // Tab alone stays with the window's normal key handling.
        assert!(!keys.take(VK_TAB, true, false, false));
        assert!(!keys.take(VK_TAB, false, false, false));
        assert!(keys.take(VK_ESCAPE, true, false, true), "Ctrl+Esc");
        assert!(keys.take(VK_ESCAPE, false, false, true));
        assert!(keys.take(VK_ESCAPE, true, true, false), "Alt+Esc");
        assert!(!keys.take(0x41, true, true, true));
        // A Windows key held before focus is released locally.
        keys.clear();
        assert!(!keys.take(VK_RWIN, false, false, false));
    }
}
