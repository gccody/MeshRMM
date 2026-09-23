use std::{
    cell::Cell,
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, bail};
use meshrmm_protocol::{CursorShape, Display, PointerButton, RemoteInput};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetSystemMetrics, IDC_APPSTARTING, IDC_ARROW,
    IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_PERSON, IDC_PIN, IDC_SIZEALL,
    IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT, LoadCursorW,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub struct WindowsInputController {
    _ownership_hooks: Option<super::input_block::InputBlock>,
    viewer_controls_input: Arc<AtomicBool>,
    viewer_cursor: Cell<CursorShape>,
    block: Option<super::input_block::InputBlock>,
    blackout: Option<super::blackout::Blackout>,
    manually_blocked: bool,
    active_display: Option<Display>,
    displays: Vec<Display>,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
    system_cursors: Vec<(usize, CursorShape)>,
}

impl WindowsInputController {
    pub fn new() -> Self {
        let viewer_controls_input = Arc::new(AtomicBool::new(false));
        let ownership_hooks =
            super::input_block::InputBlock::track_ownership(Arc::clone(&viewer_controls_input))
                .map_err(|error| tracing::warn!(%error, "Could not track cursor ownership"))
                .ok();
        Self {
            _ownership_hooks: ownership_hooks,
            viewer_controls_input,
            viewer_cursor: Cell::new(CursorShape::Default),
            block: None,
            blackout: None,
            manually_blocked: false,
            active_display: None,
            displays: Vec::new(),
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            system_cursors: system_cursor_handles(),
        }
    }

    pub fn blacked_out(&self) -> bool {
        self.blackout.is_some()
    }
    pub fn set_blackout(&mut self, enabled: bool, text: &str) -> anyhow::Result<()> {
        if enabled && self.blackout.is_none() {
            // Install the input hooks before hiding the monitors. If creating
            // the overlays fails, restore the previous input-blocking state.
            self.update_input_block(true)?;
            match super::blackout::Blackout::show(text) {
                Ok(blackout) => self.blackout = Some(blackout),
                Err(error) => {
                    self.update_input_block(self.manually_blocked)?;
                    return Err(error);
                }
            }
        } else if !enabled {
            self.blackout = None;
            self.update_input_block(self.manually_blocked)?;
        }
        Ok(())
    }

    pub fn blocked(&self) -> bool {
        self.block.is_some()
    }

    pub fn set_blocked(&mut self, blocked: bool) -> anyhow::Result<()> {
        self.update_input_block(blocked || self.blacked_out())?;
        self.manually_blocked = blocked;
        Ok(())
    }

    fn update_input_block(&mut self, blocked: bool) -> anyhow::Result<()> {
        if blocked && self.block.is_none() {
            self.release_all()?;
            self.block = Some(super::input_block::InputBlock::start()?);
        } else if !blocked {
            self.block = None;
        }
        Ok(())
    }

    pub fn viewer_controls_input(&self) -> bool {
        self.viewer_controls_input.load(Ordering::SeqCst)
    }

    pub fn agent_pointer_display(&self) -> Option<meshrmm_protocol::DisplayId> {
        if self.viewer_controls_input() {
            return None;
        }
        let mut point = windows::Win32::Foundation::POINT::default();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut point).ok()?;
        }
        pointer_display_at(&self.displays, point.x, point.y)
    }

    pub fn cursor_shape(&self) -> CursorShape {
        if !self.viewer_controls_input() {
            return self.viewer_cursor.get();
        }
        let shape = self.current_cursor_shape();
        self.viewer_cursor.set(shape);
        shape
    }

    fn current_cursor_shape(&self) -> CursorShape {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // Blackout hides the endpoint's pointer, but the technician still needs
        // the underlying application's cursor shape in the remote viewer.
        if unsafe { GetCursorInfo(&mut info) }.is_err()
            || (!self.blacked_out() && info.flags.0 & CURSOR_SHOWING.0 == 0)
        {
            return CursorShape::Default;
        }
        let handle = info.hCursor.0 as usize;
        self.system_cursors
            .iter()
            .find_map(|(candidate, shape)| (*candidate == handle).then_some(*shape))
            .unwrap_or_default()
    }

    pub fn set_active_display(&mut self, display: Display) -> anyhow::Result<()> {
        // Capture/encoder restarts commonly rediscover the same display. Keep
        // held input intact in that case; input state belongs to the control
        // session, not to the current video capture instance.
        if self
            .active_display
            .as_ref()
            .is_some_and(|active| active.id != display.id)
        {
            self.release_all()?;
        }
        self.displays = super::platform::enumerate_displays()?;
        self.active_display = Some(display);
        Ok(())
    }

    pub fn apply(&mut self, input: RemoteInput) -> anyhow::Result<()> {
        let display = self
            .active_display
            .as_ref()
            .context("remote input arrived before a display was selected")?;
        if input.display_id() != display.id {
            bail!(
                "input targeted stale display {} while display {} is active",
                input.display_id().0,
                display.id.0
            );
        }
        match input {
            RemoteInput::TypeText { text, .. } => {
                anyhow::ensure!(
                    text.len() <= meshrmm_protocol::MAX_CLIPBOARD_TEXT_BYTES
                        && !text.contains('\0'),
                    "invalid text input"
                );
                self.release_all()?;
                let inputs = text_inputs(&text);
                // Send bounded batches locally without network round trips or
                // artificial per-character delays.
                for batch in inputs.chunks(128) {
                    send(batch)?;
                }
                Ok(())
            }

            RemoteInput::PointerMove { x, y, .. } => move_pointer(display, x, y),
            RemoteInput::PointerButton {
                button, pressed, ..
            } => {
                send_mouse_button(button, pressed)?;
                if pressed {
                    self.pressed_buttons.insert(button);
                } else {
                    self.pressed_buttons.remove(&button);
                }
                Ok(())
            }
            RemoteInput::PointerButtonAt {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                send_mouse_button_at(display, x, y, button, pressed)?;
                if pressed {
                    self.pressed_buttons.insert(button);
                } else {
                    self.pressed_buttons.remove(&button);
                }
                Ok(())
            }
            RemoteInput::Wheel {
                horizontal,
                vertical,
                ..
            } => {
                if vertical != 0 {
                    send_mouse(MOUSEEVENTF_WHEEL, i32::from(vertical) as u32)?;
                }
                if horizontal != 0 {
                    send_mouse(MOUSEEVENTF_HWHEEL, i32::from(horizontal) as u32)?;
                }
                Ok(())
            }
            RemoteInput::WheelAt {
                x,
                y,
                horizontal,
                vertical,
                ..
            } => send_wheel_at(display, x, y, horizontal, vertical),
            RemoteInput::Key {
                scan_code,
                extended,
                pressed,
                ..
            } => {
                if scan_code == 0 {
                    bail!("remote keyboard event had an empty scan code");
                }
                send_key(scan_code, extended, pressed)?;
                if pressed {
                    self.pressed_keys.insert((scan_code, extended));
                } else {
                    self.pressed_keys.remove(&(scan_code, extended));
                }
                Ok(())
            }
        }
    }

    pub fn release_all(&mut self) -> anyhow::Result<()> {
        let mut failure = None;
        for (scan_code, extended) in self.pressed_keys.clone() {
            match send_key(scan_code, extended, false) {
                Ok(()) => {
                    self.pressed_keys.remove(&(scan_code, extended));
                }
                Err(error) => {
                    failure = Some(error);
                }
            }
        }
        for button in self.pressed_buttons.clone() {
            match send_mouse_button(button, false) {
                Ok(()) => {
                    self.pressed_buttons.remove(&button);
                }
                Err(error) => {
                    failure = Some(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }
}

fn system_cursor_handles() -> Vec<(usize, CursorShape)> {
    [
        (IDC_ARROW, CursorShape::Default),
        (IDC_IBEAM, CursorShape::Text),
        (IDC_WAIT, CursorShape::Wait),
        (IDC_CROSS, CursorShape::Crosshair),
        (IDC_UPARROW, CursorShape::UpArrow),
        (IDC_SIZENWSE, CursorShape::ResizeNorthWestSouthEast),
        (IDC_SIZENESW, CursorShape::ResizeNorthEastSouthWest),
        (IDC_SIZEWE, CursorShape::ResizeWestEast),
        (IDC_SIZENS, CursorShape::ResizeNorthSouth),
        (IDC_SIZEALL, CursorShape::Move),
        (IDC_NO, CursorShape::NotAllowed),
        (IDC_HAND, CursorShape::Pointer),
        (IDC_APPSTARTING, CursorShape::Progress),
        (IDC_HELP, CursorShape::Help),
        (IDC_PIN, CursorShape::Pin),
        (IDC_PERSON, CursorShape::Person),
    ]
    .into_iter()
    .filter_map(|(resource, shape)| {
        unsafe { LoadCursorW(None, resource) }
            .ok()
            .map(|cursor| (cursor.0 as usize, shape))
    })
    .collect()
}

impl Drop for WindowsInputController {
    fn drop(&mut self) {
        if let Err(error) = self.release_all() {
            tracing::warn!(error = %error, "failed to release remote input state");
        }
    }
}

fn move_pointer(display: &Display, x: u16, y: u16) -> anyhow::Result<()> {
    send(&[pointer_move_input(display, x, y)?])
}

fn pointer_move_input(display: &Display, x: u16, y: u16) -> anyhow::Result<INPUT> {
    let virtual_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let virtual_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let virtual_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let virtual_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if virtual_width <= 1 || virtual_height <= 1 || display.width == 0 || display.height == 0 {
        bail!("Windows virtual desktop dimensions are invalid");
    }
    let desktop_x =
        i64::from(display.x) + i64::from(x) * i64::from(display.width.saturating_sub(1)) / 65_535;
    let desktop_y =
        i64::from(display.y) + i64::from(y) * i64::from(display.height.saturating_sub(1)) / 65_535;
    let normalized_x = ((desktop_x - i64::from(virtual_x)) * 65_535 / i64::from(virtual_width - 1))
        .clamp(0, 65_535) as i32;
    let normalized_y = ((desktop_y - i64::from(virtual_y)) * 65_535 / i64::from(virtual_height - 1))
        .clamp(0, 65_535) as i32;
    Ok(mouse_input(
        MOUSEEVENTF_MOVE
            | MOUSEEVENTF_MOVE_NOCOALESCE
            | MOUSEEVENTF_ABSOLUTE
            | MOUSEEVENTF_VIRTUALDESK,
        normalized_x,
        normalized_y,
        0,
    ))
}

fn send_mouse_button(button: PointerButton, pressed: bool) -> anyhow::Result<()> {
    send(&[mouse_button_input(button, pressed)])
}

fn send_mouse_button_at(
    display: &Display,
    x: u16,
    y: u16,
    button: PointerButton,
    pressed: bool,
) -> anyhow::Result<()> {
    send(&[
        pointer_move_input(display, x, y)?,
        mouse_button_input(button, pressed),
    ])
}

fn mouse_button_input(button: PointerButton, pressed: bool) -> INPUT {
    let (flag, data) = match (button, pressed) {
        (PointerButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (PointerButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (PointerButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (PointerButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (PointerButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (PointerButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (PointerButton::Back, true) => (MOUSEEVENTF_XDOWN, 1),
        (PointerButton::Back, false) => (MOUSEEVENTF_XUP, 1),
        (PointerButton::Forward, true) => (MOUSEEVENTF_XDOWN, 2),
        (PointerButton::Forward, false) => (MOUSEEVENTF_XUP, 2),
    };
    mouse_input(flag, 0, 0, data)
}

fn send_mouse(flags: MOUSE_EVENT_FLAGS, data: u32) -> anyhow::Result<()> {
    send_mouse_at(flags, 0, 0, data)
}

fn send_mouse_at(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> anyhow::Result<()> {
    send(&[mouse_input(flags, dx, dy, data)])
}

fn send_wheel_at(
    display: &Display,
    x: u16,
    y: u16,
    horizontal: i16,
    vertical: i16,
) -> anyhow::Result<()> {
    let mut inputs = vec![pointer_move_input(display, x, y)?];
    if vertical != 0 {
        inputs.push(mouse_input(
            MOUSEEVENTF_WHEEL,
            0,
            0,
            i32::from(vertical) as u32,
        ));
    }
    if horizontal != 0 {
        inputs.push(mouse_input(
            MOUSEEVENTF_HWHEEL,
            0,
            0,
            i32::from(horizontal) as u32,
        ));
    }
    send(&inputs)
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: super::input_block::INPUT_TAG,
            },
        },
    }
}

fn send_key(scan_code: u16, extended: bool, pressed: bool) -> anyhow::Result<()> {
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan_code,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: super::input_block::INPUT_TAG,
            },
        },
    };
    send(&[input])
}

fn text_inputs(text: &str) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    // Treat CRLF as one Enter; do not append an Enter to the clipboard.
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' && chars.peek() == Some(&'\n') {
            chars.next();
        }
        let vk = match ch {
            '\r' | '\n' => VK_RETURN,
            '\t' => VK_TAB,
            _ => VIRTUAL_KEY(0),
        };
        let mut units = [0; 2];
        let codes: &[u16] = if vk.0 != 0 {
            &[0]
        } else {
            ch.encode_utf16(&mut units)
        };
        for &unit in codes {
            for release in [false, true] {
                inputs.push(text_key(vk, unit, release));
            }
        }
    }
    inputs
}

fn text_key(vk: VIRTUAL_KEY, unit: u16, release: bool) -> INPUT {
    let mut flags = if vk.0 == 0 {
        KEYEVENTF_UNICODE
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    if release {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: super::input_block::INPUT_TAG,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> anyhow::Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(windows::core::Error::from_thread()).context("Windows rejected remote input");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_emits_unicode_pairs_and_normalizes_line_endings() {
        let inputs = text_inputs("Aé🔑\r\n\t");
        let keys: Vec<_> = inputs
            .iter()
            .map(|input| unsafe { input.Anonymous.ki })
            .collect();
        assert_eq!(keys.len(), 12);
        assert_eq!(
            keys.iter().step_by(2).map(|k| k.wScan).collect::<Vec<_>>(),
            vec![65, 233, 0xd83d, 0xdd11, 0, 0]
        );
        for pair in keys.chunks_exact(2) {
            assert_eq!(pair[0].wScan, pair[1].wScan);
            assert_eq!(pair[0].wVk, pair[1].wVk);
            assert!(!pair[0].dwFlags.contains(KEYEVENTF_KEYUP));
            assert!(pair[1].dwFlags.contains(KEYEVENTF_KEYUP));
            assert_eq!(pair[0].dwExtraInfo, super::super::input_block::INPUT_TAG);
        }
        assert!(keys[0].dwFlags.contains(KEYEVENTF_UNICODE));
        assert_eq!(keys[8].wVk, VK_RETURN);
        assert_eq!(keys[10].wVk, VK_TAB);
        assert!(text_inputs("").is_empty());
        assert_eq!(text_inputs("\r\n\n\r").len(), 6);
    }

    #[test]
    #[ignore = "requires interactive Windows desktop; injects harmless pointer movement"]
    fn live_cursor_shape_freezes_until_viewer_reclaims_input() {
        use std::{
            thread,
            time::{Duration, Instant},
        };
        fn movement(tag: usize) {
            send(&[INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dwFlags: MOUSEEVENTF_MOVE,
                        dx: 1,
                        dwExtraInfo: tag,
                        ..Default::default()
                    },
                },
            }])
            .unwrap();
        }
        fn wait_for(input: &WindowsInputController, viewer: bool) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while input.viewer_controls_input.load(Ordering::SeqCst) != viewer {
                assert!(
                    Instant::now() < deadline,
                    "input ownership did not change to viewer={viewer}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
        let mut input = WindowsInputController::new();
        assert!(input._ownership_hooks.is_some());
        movement(super::super::input_block::INPUT_TAG);
        wait_for(&input, true);
        assert_eq!(input.cursor_shape(), input.current_cursor_shape());
        movement(0);
        wait_for(&input, false);
        // Use a cached shape different from the live desktop to prove polling
        // retains it, rather than just observing an unchanged desktop cursor.
        let frozen = if input.current_cursor_shape() == CursorShape::Text {
            CursorShape::Pointer
        } else {
            CursorShape::Text
        };
        input.viewer_cursor.set(frozen);
        assert_eq!(input.cursor_shape(), frozen);
        input.release_all().unwrap();
        assert_eq!(input.cursor_shape(), frozen);
        movement(super::super::input_block::INPUT_TAG);
        wait_for(&input, true);
        assert_eq!(input.cursor_shape(), input.current_cursor_shape());
        input.set_blocked(true).unwrap();
        movement(0);
        thread::sleep(Duration::from_millis(50));
        assert!(input.viewer_controls_input.load(Ordering::SeqCst));
        input.set_blocked(false).unwrap();
        movement(0);
        wait_for(&input, false);
    }

    #[test]
    #[ignore = "requires interactive Windows desktop; blacks out monitors and blocks local input"]
    fn blackout_enforces_input_block_and_restores_manual_setting() {
        let mut input = WindowsInputController::new();
        for manually_blocked in [false, true] {
            input.set_blocked(manually_blocked).unwrap();
            input
                .set_blackout(true, "MeshRMM input-block test")
                .unwrap();
            assert!(input.blacked_out());
            assert!(input.blocked());
            input
                .set_blackout(true, "MeshRMM input-block test")
                .unwrap();
            input.set_blackout(false, "").unwrap();
            assert!(!input.blacked_out());
            assert_eq!(input.blocked(), manually_blocked);
        }

        input
            .set_blackout(true, "MeshRMM input-block test")
            .unwrap();
        input.set_blocked(false).unwrap();
        assert!(
            input.blocked(),
            "blackout must prevent unblocking local input"
        );
        input.set_blackout(false, "").unwrap();
        assert!(!input.blocked());
    }
}

fn pointer_display_at(displays: &[Display], x: i32, y: i32) -> Option<meshrmm_protocol::DisplayId> {
    displays
        .iter()
        .find(|d| {
            d.id.0 != meshrmm_remote_screen::ALL_MONITORS_ID
                && i64::from(x) >= i64::from(d.x)
                && i64::from(x) < i64::from(d.x) + i64::from(d.width)
                && i64::from(y) >= i64::from(d.y)
                && i64::from(y) < i64::from(d.y) + i64::from(d.height)
        })
        .map(|d| d.id)
}

#[cfg(test)]
mod pointer_monitor_tests {
    use super::*;
    #[test]
    fn pointer_monitor_excludes_virtual_display_and_handles_edges_and_gaps() {
        let mut displays = vec![
            Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: meshrmm_protocol::DisplayId(1),
                name: "Left".into(),
                x: -100,
                y: -20,
                width: 100,
                height: 100,
                primary: false,
            },
            Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: meshrmm_protocol::DisplayId(2),
                name: "Right".into(),
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                primary: true,
            },
        ];
        displays.insert(
            0,
            Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: meshrmm_protocol::DisplayId(meshrmm_remote_screen::ALL_MONITORS_ID),
                name: "All".into(),
                x: -100,
                y: -20,
                width: 200,
                height: 120,
                primary: false,
            },
        );
        assert_eq!(
            pointer_display_at(&displays, -1, 0),
            Some(meshrmm_protocol::DisplayId(1))
        );
        assert_eq!(
            pointer_display_at(&displays, 0, 0),
            Some(meshrmm_protocol::DisplayId(2))
        );
        assert_eq!(pointer_display_at(&displays, 0, -1), None);
        assert_eq!(pointer_display_at(&displays, 100, 0), None);
    }
}
