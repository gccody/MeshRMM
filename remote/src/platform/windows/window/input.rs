//! Keyboard, mouse and file-drop input over the video, forwarded to the device.

use super::*;

impl WindowContext {
    pub(super) fn annotating(&self) -> bool {
        self.annotator.borrow().enabled()
    }

    pub(super) fn toggle_annotating(&self) {
        // Keys and buttons held on the device stay there otherwise.
        self.release_input();
        let message = self.annotator.borrow_mut().toggle();
        if let Some(message) = message {
            self.send(message);
        }
        unsafe { apply_cursor(self.video_cursor()) };
    }

    /// The mouse while annotating: the left button draws and the right
    /// button erases. Nothing reaches the device's pointer.
    fn annotate_mouse(&self, window: HWND, message: u32, lparam: LPARAM) {
        let display_id = self.active_display_id();
        let annotation = match message {
            WM_LBUTTONDOWN => {
                let annotation = self
                    .annotator
                    .borrow_mut()
                    .start(display_id, self.pointer_position(window, lparam));
                if annotation.is_some() {
                    let _ = unsafe { SetCapture(window) };
                }
                annotation
            }
            WM_MOUSEMOVE => {
                // Like a drag on the device, a stroke that leaves the video
                // follows its edge.
                let position = self.video_rect(window).map(|video| {
                    video_layout::normalize_clamped(
                        video,
                        signed_low_word(lparam.0),
                        signed_high_word(lparam.0),
                    )
                });
                self.annotator.borrow_mut().extend(display_id, position)
            }
            WM_LBUTTONUP => {
                self.annotator.borrow_mut().finish();
                let _ = unsafe { ReleaseCapture() };
                None
            }
            WM_RBUTTONDOWN if self.pointer_position(window, lparam).is_some() => {
                self.annotator.borrow_mut().clear()
            }
            _ => None,
        };
        if let Some(annotation) = annotation {
            self.send(annotation);
        }
    }

    pub(super) fn move_pointer(&self, window: HWND, lparam: LPARAM) {
        if self.annotating() {
            self.annotate_mouse(window, WM_MOUSEMOVE, lparam);
            return;
        }
        let dragging = self.held.borrow().buttons_held();
        let position = if !dragging {
            self.pointer_position(window, lparam)
        } else {
            // A drag that started on the video keeps mouse capture; pin the
            // remote pointer to the nearest edge instead of dropping motion.
            self.video_rect(window).map(|video| {
                video_layout::normalize_clamped(
                    video,
                    signed_low_word(lparam.0),
                    signed_high_word(lparam.0),
                )
            })
        };
        if let Some((x, y)) = position {
            self.send(SessionMessage::Input(RemoteInput::PointerMove {
                display_id: self.active_display_id(),
                x,
                y,
            }));
        }
    }

    pub(super) fn mouse_button(&self, window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) {
        if self.annotating() {
            self.annotate_mouse(window, message, lparam);
            return;
        }
        let button = match message {
            WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Left,
            WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Right,
            WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
            _ if ((wparam.0 >> 16) as u16) == XBUTTON1 => PointerButton::Back,
            _ => PointerButton::Forward,
        };
        let pressed = matches!(
            message,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        );
        let position = self.pointer_position(window, lparam);
        let input =
            self.held
                .borrow_mut()
                .button(self.active_display_id(), position, button, pressed);
        let Some(input) = input else {
            return;
        };
        self.send(SessionMessage::Input(input));
        if pressed {
            let _ = unsafe { SetCapture(window) };
        } else if !self.held.borrow().buttons_held() {
            let _ = unsafe { ReleaseCapture() };
        }
    }

    pub(super) fn wheel(&self, window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) {
        let mut point = windows::Win32::Foundation::POINT {
            x: signed_low_word(lparam.0),
            y: signed_high_word(lparam.0),
        };
        if unsafe { ScreenToClient(window, &mut point) }.as_bool()
            && let Some((x, y)) = self.normalized_client_position(window, point.x, point.y)
        {
            let delta = ((wparam.0 >> 16) as u16) as i16;
            self.send(SessionMessage::Input(RemoteInput::WheelAt {
                display_id: self.active_display_id(),
                x,
                y,
                horizontal: if message == WM_MOUSEHWHEEL { delta } else { 0 },
                vertical: if message == WM_MOUSEWHEEL { delta } else { 0 },
            }));
        }
    }

    /// Handles WM_KEYDOWN, WM_KEYUP and their WM_SYS variants: the viewer's
    /// shortcut keys, pasting files with Ctrl+V, and keys for the device.
    pub(super) fn key_message(&self, message: u32, wparam: WPARAM, lparam: LPARAM) {
        let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if let Some(shortcut) = crate::shortcuts::windows_shortcut(
            wparam.0 as u16,
            self.control.shortcut_key(ViewerShortcut::Diagnostics),
            self.control.shortcut_key(ViewerShortcut::NextDisplay),
        ) {
            if pressed && lparam.0 & (1 << 30) == 0 {
                match shortcut {
                    ViewerShortcut::NextDisplay => self.select_next_display(),
                    ViewerShortcut::Diagnostics => self.toggle_debug(),
                }
            }
            return;
        }
        if pressed
            && wparam.0 == 0x56
            && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) } < 0
            && self.control.files().paste_files(self.active_display_id())
        {
            self.release_input();
            return;
        }
        let scan_code = ((lparam.0 >> 16) & 0xff) as u16;
        let extended = lparam.0 & (1 << 24) != 0;
        self.send_key(scan_code, extended, pressed);
    }

    pub(super) fn send_key(&self, scan_code: u16, extended: bool, pressed: bool) {
        if scan_code == 0 {
            return;
        }
        let input =
            self.held
                .borrow_mut()
                .key(self.active_display_id(), scan_code, extended, pressed);
        self.send(SessionMessage::Input(input));
    }

    /// Releases every key and button held on the device.
    pub(super) fn release_input(&self) {
        let released = self.held.borrow_mut().release_all(self.active_display_id());
        for input in released {
            self.send(SessionMessage::Input(input));
        }
    }

    /// Sends files dropped on the window: to the point they were dropped on,
    /// or to Documents if that is not on the video.
    pub(super) unsafe fn drop_files(&self, window: HWND, wparam: WPARAM) {
        let drop = windows::Win32::UI::Shell::HDROP(wparam.0 as *mut _);
        let paths = unsafe { meshrmm_file_transfer::windows::paths_from_drop(drop) };
        let mut point = windows::Win32::Foundation::POINT::default();
        unsafe {
            let _ = windows::Win32::UI::Shell::DragQueryPoint(drop, &mut point);
            windows::Win32::UI::Shell::DragFinish(drop);
        }
        let destination = self
            .normalized_client_position(window, point.x, point.y)
            .map(|(x, y)| meshrmm_protocol::FileDestination::Drop {
                display_id: self.active_display_id(),
                x,
                y,
            })
            .unwrap_or(meshrmm_protocol::FileDestination::Documents);
        self.release_input();
        self.control.files().send(paths, destination);
    }
}
