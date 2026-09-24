//! Sends the shortcuts Windows keeps for itself (the Windows keys, Alt+Tab,
//! Alt+Esc and Ctrl+Esc) to the device while the viewer window has keyboard
//! focus. The low-level hook exists only while it does: the window installs it
//! when it gains focus and removes it when it loses focus or closes.
use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_ALTDOWN,
    LLKHF_EXTENDED, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP,
    WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::shortcuts::SystemShortcuts;

/// Posted to the viewer window for a key taken from Windows. WPARAM holds the
/// scan code (bits 0–15), whether it is extended (bit 16) and pressed (bit 17).
pub(super) const WM_SYSTEM_SHORTCUT_KEY: u32 = WM_APP + 0x40;

thread_local! {
    static HOOK: Cell<Option<HHOOK>> = const { Cell::new(None) };
    static TARGET: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    static SHORTCUTS: RefCell<SystemShortcuts> = RefCell::new(SystemShortcuts::default());
}

/// Installs the hook for `window`. Windows calls it on this thread, the
/// window's, which pumps messages; the hook only posts a message, so it never
/// delays other applications' keyboard input.
pub(super) fn install(window: HWND) {
    TARGET.set(window);
    if HOOK.get().is_some() {
        return;
    }
    let hook = unsafe { GetModuleHandleW(None) }.and_then(|module| unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), Some(HINSTANCE(module.0)), 0)
    });
    match hook {
        Ok(hook) => HOOK.set(Some(hook)),
        Err(error) => tracing::warn!(%error, "could not capture Windows shortcuts"),
    }
}

pub(super) fn remove() {
    if let Some(hook) = HOOK.take()
        && let Err(error) = unsafe { UnhookWindowsHookEx(hook) }
    {
        tracing::warn!(%error, "could not remove the Windows shortcut hook");
    }
    TARGET.set(HWND(std::ptr::null_mut()));
    SHORTCUTS.with_borrow_mut(SystemShortcuts::clear);
}

/// Unpacks a [`WM_SYSTEM_SHORTCUT_KEY`] WPARAM into the scan code, whether it
/// is extended, and whether the key was pressed.
pub(super) fn unpack(wparam: WPARAM) -> (u16, bool, bool) {
    (
        (wparam.0 & 0xffff) as u16,
        wparam.0 & (1 << 16) != 0,
        wparam.0 & (1 << 17) != 0,
    )
}

fn pack(scan_code: u32, extended: bool, pressed: bool) -> WPARAM {
    WPARAM((scan_code & 0xffff) as usize | usize::from(extended) << 16 | usize::from(pressed) << 17)
}

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let target = TARGET.get();
    if code == HC_ACTION as i32
        && !target.is_invalid()
        && unsafe { GetForegroundWindow() } == target
    {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        let alt = event.flags.0 & LLKHF_ALTDOWN.0 != 0;
        let control = unsafe { GetAsyncKeyState(i32::from(VK_CONTROL.0)) } < 0;
        if SHORTCUTS.with_borrow_mut(|keys| keys.take(event.vkCode, pressed, alt, control)) {
            let extended = event.flags.0 & LLKHF_EXTENDED.0 != 0;
            let message = pack(event.scanCode, extended, pressed);
            if unsafe { PostMessageW(Some(target), WM_SYSTEM_SHORTCUT_KEY, message, LPARAM(0)) }
                .is_ok()
            {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_keys_round_trip() {
        for (scan_code, extended, pressed) in [
            (0x5b, true, true),
            (0x0f, false, false),
            (0x01, false, true),
        ] {
            assert_eq!(
                unpack(pack(scan_code, extended, pressed)),
                (scan_code as u16, extended, pressed)
            );
        }
    }
}
