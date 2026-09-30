//! Real mouse and keyboard input for Session 0, sent with `SendInput` once the
//! background desktop is Session 0's input desktop. Windows then does what it
//! does for a local user: activation, double-clicks, menu loops, scrollbar and
//! caption tracking, wheel routing, and the cursor position apps read.
use crate::remote::input::{key_input, mouse_button_input, mouse_input, send, text_inputs};
use meshrmm_protocol::PointerButton;
use std::collections::HashSet;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Keys and buttons it pressed, so `release` can lift them.
#[derive(Default)]
pub(super) struct Injector {
    keys: HashSet<(u16, bool)>,
    buttons: HashSet<PointerButton>,
}

impl Injector {
    pub(super) fn move_to(&self, point: POINT) -> anyhow::Result<()> {
        inject(&[move_input(point)])
    }

    pub(super) fn button(&mut self, button: PointerButton, pressed: bool) -> anyhow::Result<()> {
        inject(&[mouse_button_input(button, pressed)])?;
        if pressed {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        Ok(())
    }

    pub(super) fn wheel(&self, horizontal: i16, vertical: i16) -> anyhow::Result<()> {
        let inputs: Vec<_> = [
            (MOUSEEVENTF_WHEEL, vertical),
            (MOUSEEVENTF_HWHEEL, horizontal),
        ]
        .into_iter()
        .filter(|(_, delta)| *delta != 0)
        .map(|(flag, delta)| mouse_input(flag, 0, 0, i32::from(delta) as u32))
        .collect();
        if inputs.is_empty() {
            return Ok(());
        }
        inject(&inputs)
    }

    pub(super) fn key(
        &mut self,
        scan_code: u16,
        extended: bool,
        pressed: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            scan_code != 0,
            "remote keyboard event had an empty scan code"
        );
        inject(&[key_input(scan_code, extended, pressed)])?;
        if pressed {
            self.keys.insert((scan_code, extended));
        } else {
            self.keys.remove(&(scan_code, extended));
        }
        Ok(())
    }

    pub(super) fn text(&mut self, text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            text.len() <= meshrmm_protocol::MAX_CLIPBOARD_TEXT_BYTES && !text.contains('\0'),
            "invalid text input"
        );
        // Held modifiers would turn typed characters into shortcuts.
        self.release()?;
        for batch in text_inputs(text).chunks(128) {
            inject(batch)?;
        }
        Ok(())
    }

    pub(super) fn release(&mut self) -> anyhow::Result<()> {
        let mut inputs: Vec<_> = self
            .keys
            .iter()
            .map(|&(scan_code, extended)| key_input(scan_code, extended, false))
            .collect();
        inputs.extend(
            self.buttons
                .iter()
                .map(|&button| mouse_button_input(button, false)),
        );
        self.keys.clear();
        self.buttons.clear();
        if inputs.is_empty() {
            return Ok(());
        }
        inject(&inputs)
    }
}

/// Session 0 can switch its input desktop away, and input to it is then denied,
/// so switch back and retry once.
fn inject(inputs: &[INPUT]) -> anyhow::Result<()> {
    send(inputs).or_else(|error| {
        tracing::debug!(%error, "Session 0 refused input; reclaiming its input desktop");
        super::screen::reclaim()?;
        send(inputs)
    })
}

/// An absolute move to a pixel on Session 0's screen. Windows maps a normalized
/// coordinate `n` to pixel `n * size / 65536`, so round up to land on `point`.
fn move_input(point: POINT) -> INPUT {
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let normalize = |value: i32, size: i32| {
        let size = i64::from(size.max(1));
        ((i64::from(value).clamp(0, size - 1) * 65_536 + size - 1) / size).min(65_535) as i32
    };
    mouse_input(
        MOUSEEVENTF_MOVE | MOUSEEVENTF_MOVE_NOCOALESCE | MOUSEEVENTF_ABSOLUTE,
        normalize(point.x, width),
        normalize(point.y, height),
        0,
    )
}
