use std::collections::HashSet;

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
    active_display: Option<Display>,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
    system_cursors: Vec<(usize, CursorShape)>,
}

impl WindowsInputController {
    pub fn new() -> Self {
        Self {
            active_display: None,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            system_cursors: system_cursor_handles(),
        }
    }

    pub fn cursor_shape(&self) -> CursorShape {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err() || info.flags.0 & CURSOR_SHOWING.0 == 0 {
            return CursorShape::Default;
        }
        let handle = info.hCursor.0 as usize;
        self.system_cursors
            .iter()
            .find_map(|(candidate, shape)| (*candidate == handle).then_some(*shape))
            .unwrap_or_default()
    }

    pub fn set_active_display(&mut self, display: Display) -> anyhow::Result<()> {
        self.release_all()?;
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
        for (scan_code, extended) in self.pressed_keys.drain().collect::<Vec<_>>() {
            send_key(scan_code, extended, false)?;
        }
        for button in self.pressed_buttons.drain().collect::<Vec<_>>() {
            send_mouse_button(button, false)?;
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
                dwExtraInfo: 0,
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
                dwExtraInfo: 0,
            },
        },
    };
    send(&[input])
}

fn send(inputs: &[INPUT]) -> anyhow::Result<()> {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(windows::core::Error::from_thread()).context("Windows rejected remote input");
    }
    Ok(())
}
