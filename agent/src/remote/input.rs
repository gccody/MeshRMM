use std::collections::HashSet;

use anyhow::{Context, bail};
use pulsermm_protocol::{Display, PointerButton, RemoteInput};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub struct WindowsInputController {
    active_display: Option<Display>,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
}

impl WindowsInputController {
    pub fn new() -> Self {
        Self {
            active_display: None,
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
        }
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

impl Drop for WindowsInputController {
    fn drop(&mut self) {
        if let Err(error) = self.release_all() {
            tracing::warn!(error = %error, "failed to release remote input state");
        }
    }
}

fn move_pointer(display: &Display, x: u16, y: u16) -> anyhow::Result<()> {
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
    send_mouse_at(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        normalized_x,
        normalized_y,
        0,
    )
}

fn send_mouse_button(button: PointerButton, pressed: bool) -> anyhow::Result<()> {
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
    send_mouse(flag, data)
}

fn send_mouse(flags: MOUSE_EVENT_FLAGS, data: u32) -> anyhow::Result<()> {
    send_mouse_at(flags, 0, 0, data)
}

fn send_mouse_at(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: u32) -> anyhow::Result<()> {
    let input = INPUT {
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
    };
    send(&[input])
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
