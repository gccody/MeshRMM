//! The file browser message loop and its keyboard shortcuts.
use super::*;

/// Modifier state for the message loop, which sees key messages before
/// IsDialogMessage consumes them.
struct Keyboard<'a> {
    state: &'a RefCell<State>,
    hwnd: HWND,
    location: HWND,
    search: HWND,
    list: HWND,
    status: HWND,
    control_down: bool,
    shift_down: bool,
    alt_down: bool,
}

pub(super) fn message_loop(state: &RefCell<State>) {
    let mut keyboard = {
        let current = state.borrow();
        Keyboard {
            state,
            hwnd: current.hwnd,
            location: current.location,
            search: current.search,
            list: current.list,
            status: current.status,
            control_down: false,
            shift_down: false,
            alt_down: false,
        }
    };
    unsafe {
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            keyboard.track_modifiers(&message);
            let handled = matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN)
                && GetAncestor(message.hwnd, GA_ROOT) == keyboard.hwnd
                && keyboard.shortcut(&message);
            if !handled && !IsDialogMessageW(GetAncestor(message.hwnd, GA_ROOT), &message).as_bool()
            {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
}

impl Keyboard<'_> {
    fn track_modifiers(&mut self, message: &MSG) {
        if matches!(
            message.message,
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
        ) {
            let down = matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN);
            match message.wParam.0 {
                0x11 | 0xa2 | 0xa3 => {
                    self.control_down = down;
                    self.state.borrow_mut().control_down = down;
                }
                0x10 | 0xa0 | 0xa1 => self.shift_down = down,
                0x12 | 0xa4 | 0xa5 => self.alt_down = down,
                _ => {}
            }
        }
    }

    /// Returns whether the key press was consumed.
    fn shortcut(&self, message: &MSG) -> bool {
        let key = message.wParam.0;
        let control_down = self.control_down;
        let alt_down = self.alt_down;
        unsafe {
            let editing = message.hwnd == self.location
                || message.hwnd == self.search
                || SendMessageW(self.list, LVM_GETEDITCONTROL, None, None).0
                    == message.hwnd.0 as isize;
            let command = self.command(message, editing);
            if key == 0x41 && control_down && editing {
                SendMessageW(message.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            } else if (control_down && key == 0x4c) || (alt_down && key == 0x44) || key == 0x75 {
                self.state.borrow_mut().address_edit = true;
                self.state.borrow().layout();
                let _ = SetFocus(Some(self.location));
                SendMessageW(self.location, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            } else if (control_down && matches!(key, 0x46 | 0x45)) || key == 0x72 {
                let _ = SetFocus(Some(self.search));
                SendMessageW(self.search, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            } else if let Some(command) = command {
                if let Err(e) = self.state.borrow_mut().command(command) {
                    set_text(self.status, &format!("{e:#}"));
                }
            } else {
                return false;
            }
            true
        }
    }

    fn command(&self, message: &MSG, editing: bool) -> Option<usize> {
        let control_down = self.control_down;
        let alt_down = self.alt_down;
        match message.wParam.0 {
            0x5a if control_down && !editing => Some(UNDO),
            0x41 if control_down && !editing => Some(SELECT_ALL),
            0x43 if control_down && !editing => Some(COPY),
            0x58 if control_down && !editing => Some(CUT),
            0x56 if control_down && !editing => Some(PASTE),
            0x4e if control_down && self.shift_down && !editing => Some(NEW_FOLDER),
            0x71 if !editing => Some(RENAME),
            0x74 => Some(REFRESH),
            0x2e if !editing => Some(DELETE),
            0x25 if alt_down => Some(BACK),
            0x27 if alt_down => Some(FORWARD),
            0x26 if alt_down => Some(UP),
            0x08 if !editing => Some(UP),
            13 if alt_down => Some(PROPERTIES),
            13 if message.hwnd == self.location => Some(GO),
            13 if message.hwnd == self.search => Some(SEARCH_GO),
            13 if message.hwnd == self.list => Some(OPEN),
            27 if message.hwnd == self.search => {
                set_text(self.search, "");
                Some(SEARCH_GO)
            }
            27 if !editing => Some(CANCEL),
            _ => None,
        }
    }
}
