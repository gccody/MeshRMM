use super::*;
use crate::platform::macos::keyboard::RemoteKey;

impl RemoteView {
    pub(super) fn send_pointer(&self, event: &NSEvent) {
        // While annotating, the mouse draws rather than moves the device's pointer.
        if self.annotating() {
            return;
        }
        let Some((x, y)) = self.pointer_position(event) else {
            return;
        };
        self.send(SessionMessage::Input(RemoteInput::PointerMove {
            display_id: self.ivars().active_display.borrow().id,
            x,
            y,
        }));
    }

    pub(super) fn pointer_position(&self, event: &NSEvent) -> Option<(u16, u16)> {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let mut bounds = self.bounds();
        bounds.size.height = (bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
        normalized_video_position(
            point,
            bounds,
            self.ivars().video_width.get(),
            self.ivars().video_height.get(),
        )
    }

    pub(super) fn send_button(&self, event: &NSEvent, button: PointerButton, pressed: bool) {
        let position = self.pointer_position(event);
        if pressed && position.is_some() {
            // Cmd-click and Ctrl-click use the held modifier.
            self.sync_modifiers(event.modifierFlags());
            self.engage_command();
        }
        let display_id = self.ivars().active_display.borrow().id;
        let input = self
            .ivars()
            .held
            .borrow_mut()
            .button(display_id, position, button, pressed);
        if let Some(input) = input {
            self.send(SessionMessage::Input(input));
        }
    }

    pub(super) fn sync_modifiers(&self, flags: NSEventModifierFlags) {
        let keys = self.ivars().keyboard.borrow_mut().sync(flags.0 as u64);
        self.send_keys(keys);
    }

    /// Sends any held Command key before an action that uses it.
    pub(super) fn engage_command(&self) {
        let keys = self.ivars().keyboard.borrow_mut().engage_command();
        self.send_keys(keys);
    }

    pub(super) fn send_key(&self, key_code: u16, pressed: bool) {
        let iso = matches!(key_code, 10 | 50) && keyboard::keyboard_is_iso();
        let Some((scan_code, extended)) = keyboard::scan_code(key_code, iso) else {
            return;
        };
        self.send_scan_code(scan_code, extended, pressed);
    }

    pub(super) fn send_keys(&self, keys: Vec<RemoteKey>) {
        for key in keys {
            self.send_scan_code(key.scan_code, key.extended, key.pressed);
        }
    }

    pub(super) fn send_scan_code(&self, scan_code: u16, extended: bool, pressed: bool) {
        let display_id = self.ivars().active_display.borrow().id;
        let input = self
            .ivars()
            .held
            .borrow_mut()
            .key(display_id, scan_code, extended, pressed);
        self.send(SessionMessage::Input(input));
    }

    pub(super) fn select_adjacent(&self, next: bool) {
        let all_displays = self.ivars().displays.borrow();
        let active = self.ivars().active_display.borrow();
        let displays = active.session_displays(&all_displays);
        if displays.len() < 2 {
            return;
        }
        let current = displays
            .iter()
            .position(|display| display.id == self.ivars().active_display.borrow().id)
            .unwrap_or(0);
        let selected = if next {
            (current + 1) % displays.len()
        } else {
            (current + displays.len() - 1) % displays.len()
        };
        self.send(SessionMessage::SelectDisplay {
            display_id: displays[selected].id,
        });
    }

    pub(in crate::platform::macos) fn release_input(&self) {
        self.ivars().keyboard.borrow_mut().reset();
        let display_id = self.ivars().active_display.borrow().id;
        let released = self.ivars().held.borrow_mut().release_all(display_id);
        for input in released {
            self.send(SessionMessage::Input(input));
        }
    }

    pub(in crate::platform::macos) fn disable_input(&self) {
        self.ivars().annotator.borrow_mut().finish();
        self.release_input();
        self.ivars().control.set_input_enabled(false);
    }
}
