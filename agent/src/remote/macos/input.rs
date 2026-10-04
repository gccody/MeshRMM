//! Remote keyboard and pointer input posted as Quartz events, which needs
//! the Accessibility permission. Every posted event carries [`INPUT_TAG`], so
//! an event tap can tell the viewer's input from the local user's.
use std::collections::HashSet;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_protocol::{CursorShape, Display, DisplayId, PointerButton, RemoteInput};
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CGPoint, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton,
    CGPreflightPostEventAccess, CGScrollEventUnit,
};

use super::keymap::{self, Modifier};

/// Marks events the Agent posts for the viewer.
pub(crate) const INPUT_TAG: i64 = 0x4d52_4d4d;
/// Presses closer together than this, at about the same place, count as one
/// multi-click, as with macOS's default double-click speed.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;
/// The macOS viewer sends 120 wheel units for 60 points of precise scrolling.
const WHEEL_UNITS_PER_POINT: i32 = 2;
/// The left/right-specific modifier bits in the low word of event flags.
const DEVICE_MODIFIER_BITS: u64 = 0xffff;
/// Unicode characters per typed key event, as macOS reads at most 20.
const TYPED_CHARACTERS_PER_EVENT: usize = 20;

struct Click {
    button: PointerButton,
    at: Instant,
    position: CGPoint,
    count: i64,
}

pub(crate) struct InputController {
    source: CFRetained<CGEventSource>,
    iso: bool,
    active_display: Option<Display>,
    displays: Vec<Display>,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
    modifiers: Vec<(u16, Modifier)>,
    caps_lock: bool,
    position: CGPoint,
    last_click: Option<Click>,
    ownership: Option<OwnershipTap>,
    viewer_controls_input: Arc<AtomicBool>,
}

// SAFETY: the event source is only used behind the controller's lock, and
// Quartz event sources may be used from any thread.
unsafe impl Send for InputController {}

impl InputController {
    pub(crate) fn new() -> anyhow::Result<Self> {
        if !CGPreflightPostEventAccess() {
            tracing::warn!(
                "the MeshRMM Agent cannot control this Mac until it is allowed under Accessibility in System Settings"
            );
        }
        // A private source keeps the local keyboard's modifiers out of remote
        // events; each event carries the viewer's own.
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .context("Quartz could not create an event source")?;
        let viewer_controls_input = Arc::new(AtomicBool::new(false));
        let ownership = OwnershipTap::start(Arc::clone(&viewer_controls_input))
            .inspect_err(
                |error| tracing::warn!(%error, "could not follow who controls the pointer"),
            )
            .ok();
        Ok(Self {
            source,
            iso: keymap::keyboard_is_iso(),
            active_display: None,
            displays: Vec::new(),
            pressed_keys: HashSet::new(),
            pressed_buttons: HashSet::new(),
            modifiers: Vec::new(),
            caps_lock: false,
            position: current_pointer(),
            last_click: None,
            ownership,
            viewer_controls_input,
        })
    }

    pub(crate) fn set_active_display(&mut self, display: Display) -> anyhow::Result<()> {
        if self
            .active_display
            .as_ref()
            .is_some_and(|active| active.id != display.id)
        {
            self.release_all()?;
        }
        self.displays = super::display::enumerate()?;
        self.active_display = Some(display);
        Ok(())
    }

    pub(crate) fn active_display(&self) -> Option<Display> {
        self.active_display.clone()
    }

    pub(crate) fn viewer_controls_input(&self) -> bool {
        if self.ownership.is_none() {
            // Without the tap, only a pointer the user moved away shows local use.
            let pointer = current_pointer();
            if (pointer.x - self.position.x).abs() > 1.0
                || (pointer.y - self.position.y).abs() > 1.0
            {
                self.viewer_controls_input.store(false, Ordering::SeqCst);
            }
        }
        self.viewer_controls_input.load(Ordering::SeqCst)
    }

    pub(crate) fn agent_pointer_display(&self) -> Option<DisplayId> {
        if self.viewer_controls_input() {
            return None;
        }
        let pointer = current_pointer();
        super::display::at(&self.displays, pointer.x, pointer.y)
    }

    pub(crate) fn cursor_shape(&self) -> CursorShape {
        CursorShape::Default
    }

    pub(crate) fn apply(&mut self, input: RemoteInput) -> anyhow::Result<()> {
        let display = self
            .active_display
            .clone()
            .context("remote input arrived before a display was selected")?;
        if input.display_id() != display.id {
            bail!(
                "input targeted stale display {} while display {} is active",
                input.display_id().0,
                display.id.0
            );
        }
        if self.ownership.is_none() {
            self.viewer_controls_input.store(true, Ordering::SeqCst);
        }
        match input {
            RemoteInput::TypeText { text, .. } => {
                anyhow::ensure!(
                    text.len() <= meshrmm_protocol::MAX_CLIPBOARD_TEXT_BYTES
                        && !text.contains('\0'),
                    "invalid text input"
                );
                self.release_all()?;
                self.type_text(&text)
            }
            RemoteInput::PointerMove { x, y, .. } => self.move_pointer(&display, x, y),
            RemoteInput::PointerButton {
                button, pressed, ..
            } => self.button(button, pressed),
            RemoteInput::PointerButtonAt {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                self.move_pointer(&display, x, y)?;
                self.button(button, pressed)
            }
            RemoteInput::Wheel {
                horizontal,
                vertical,
                ..
            } => self.wheel(horizontal, vertical),
            RemoteInput::WheelAt {
                x,
                y,
                horizontal,
                vertical,
                ..
            } => {
                self.move_pointer(&display, x, y)?;
                self.wheel(horizontal, vertical)
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
                self.key(scan_code, extended, pressed)
            }
        }
    }

    pub(crate) fn release_all(&mut self) -> anyhow::Result<()> {
        let mut failure = None;
        for (scan_code, extended) in self.pressed_keys.clone() {
            if let Err(error) = self.key(scan_code, extended, false) {
                failure = Some(error);
            }
        }
        for button in self.pressed_buttons.clone() {
            if let Err(error) = self.button(button, false) {
                failure = Some(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn flags(&self) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for (_, modifier) in &self.modifiers {
            flags |= modifier_flag(*modifier);
        }
        if self.caps_lock {
            flags |= CGEventFlags::MaskAlphaShift;
        }
        flags
    }

    fn post(&self, event: Option<CFRetained<CGEvent>>) -> anyhow::Result<()> {
        let event = event.context("Quartz could not create an input event")?;
        CGEvent::set_integer_value_field(
            Some(&event),
            CGEventField::EventSourceUserData,
            INPUT_TAG,
        );
        // The event source remembers keys it posted, so the event's own
        // modifiers can be stale; only its other flags, like the keypad and
        // function bits of arrow keys, are kept.
        let modifiers = CGEventFlags::MaskShift
            | CGEventFlags::MaskControl
            | CGEventFlags::MaskAlternate
            | CGEventFlags::MaskCommand
            | CGEventFlags::MaskAlphaShift
            | CGEventFlags::from_bits_retain(DEVICE_MODIFIER_BITS);
        CGEvent::set_flags(
            Some(&event),
            self.flags() | (CGEvent::flags(Some(&event)) - modifiers),
        );
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn move_pointer(&mut self, display: &Display, x: u16, y: u16) -> anyhow::Result<()> {
        self.position = display_point(display, x, y);
        let (kind, button) = if self.pressed_buttons.contains(&PointerButton::Left) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.pressed_buttons.contains(&PointerButton::Right) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if let Some(&button) = self.pressed_buttons.iter().next() {
            (CGEventType::OtherMouseDragged, mouse_button(button))
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        self.post(CGEvent::new_mouse_event(
            Some(&self.source),
            kind,
            self.position,
            button,
        ))
    }

    fn button(&mut self, button: PointerButton, pressed: bool) -> anyhow::Result<()> {
        let kind = match (button, pressed) {
            (PointerButton::Left, true) => CGEventType::LeftMouseDown,
            (PointerButton::Left, false) => CGEventType::LeftMouseUp,
            (PointerButton::Right, true) => CGEventType::RightMouseDown,
            (PointerButton::Right, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        if pressed {
            let count = match &self.last_click {
                Some(click)
                    if click.button == button
                        && click.at.elapsed() <= MULTI_CLICK_INTERVAL
                        && (click.position.x - self.position.x).abs() <= MULTI_CLICK_DISTANCE
                        && (click.position.y - self.position.y).abs() <= MULTI_CLICK_DISTANCE =>
                {
                    click.count + 1
                }
                _ => 1,
            };
            self.last_click = Some(Click {
                button,
                at: Instant::now(),
                position: self.position,
                count,
            });
        }
        let count = self.last_click.as_ref().map_or(1, |click| click.count);
        let event = CGEvent::new_mouse_event(
            Some(&self.source),
            kind,
            self.position,
            mouse_button(button),
        );
        if let Some(event) = &event {
            CGEvent::set_integer_value_field(
                Some(event),
                CGEventField::MouseEventClickState,
                count,
            );
            CGEvent::set_integer_value_field(
                Some(event),
                CGEventField::MouseEventButtonNumber,
                i64::from(mouse_button(button).0),
            );
        }
        self.post(event)?;
        if pressed {
            self.pressed_buttons.insert(button);
        } else {
            self.pressed_buttons.remove(&button);
        }
        Ok(())
    }

    fn wheel(&self, horizontal: i16, vertical: i16) -> anyhow::Result<()> {
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&self.source),
            CGScrollEventUnit::Pixel,
            2,
            i32::from(vertical) / WHEEL_UNITS_PER_POINT,
            i32::from(horizontal) / WHEEL_UNITS_PER_POINT,
            0,
        );
        if let Some(event) = &event {
            CGEvent::set_location(Some(event), self.position);
        }
        self.post(event)
    }

    fn key(&mut self, scan_code: u16, extended: bool, pressed: bool) -> anyhow::Result<()> {
        let Some(key_code) = keymap::key_code(scan_code, extended, self.iso) else {
            tracing::debug!(scan_code, extended, "no Mac key for a remote scan code");
            return Ok(());
        };
        if let Some(modifier) = keymap::modifier(key_code) {
            if modifier == Modifier::CapsLock {
                if pressed {
                    self.caps_lock = !self.caps_lock;
                }
            } else if pressed {
                if !self.modifiers.iter().any(|(code, _)| *code == key_code) {
                    self.modifiers.push((key_code, modifier));
                }
            } else {
                self.modifiers.retain(|(code, _)| *code != key_code);
            }
            let event = CGEvent::new_keyboard_event(Some(&self.source), key_code, pressed);
            if let Some(event) = &event {
                CGEvent::set_type(Some(event), CGEventType::FlagsChanged);
            }
            self.post(event)?;
        } else {
            self.post(CGEvent::new_keyboard_event(
                Some(&self.source),
                key_code,
                pressed,
            ))?;
        }
        if pressed {
            self.pressed_keys.insert((scan_code, extended));
        } else {
            self.pressed_keys.remove(&(scan_code, extended));
        }
        Ok(())
    }

    fn type_text(&self, text: &str) -> anyhow::Result<()> {
        const RETURN: u16 = 36;
        const TAB: u16 = 48;
        let mut pending = Vec::<u16>::new();
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            let key = match character {
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        continue;
                    }
                    Some(RETURN)
                }
                '\n' => Some(RETURN),
                '\t' => Some(TAB),
                _ => None,
            };
            if let Some(key) = key {
                self.type_units(&pending)?;
                pending.clear();
                for pressed in [true, false] {
                    self.post(CGEvent::new_keyboard_event(
                        Some(&self.source),
                        key,
                        pressed,
                    ))?;
                }
                continue;
            }
            let mut units = [0; 2];
            let encoded = character.encode_utf16(&mut units);
            if pending.len() + encoded.len() > TYPED_CHARACTERS_PER_EVENT {
                self.type_units(&pending)?;
                pending.clear();
            }
            pending.extend_from_slice(encoded);
        }
        self.type_units(&pending)
    }

    fn type_units(&self, units: &[u16]) -> anyhow::Result<()> {
        if units.is_empty() {
            return Ok(());
        }
        for pressed in [true, false] {
            let event = CGEvent::new_keyboard_event(Some(&self.source), 0, pressed);
            if let Some(event) = &event {
                // SAFETY: `units` holds `len` UTF-16 code units.
                unsafe {
                    CGEvent::keyboard_set_unicode_string(
                        Some(event),
                        units.len() as _,
                        units.as_ptr(),
                    )
                };
            }
            self.post(event)?;
        }
        Ok(())
    }
}

impl Drop for InputController {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

fn modifier_flag(modifier: Modifier) -> CGEventFlags {
    match modifier {
        Modifier::Shift => CGEventFlags::MaskShift,
        Modifier::Control => CGEventFlags::MaskControl,
        Modifier::Option => CGEventFlags::MaskAlternate,
        Modifier::Command => CGEventFlags::MaskCommand,
        Modifier::CapsLock => CGEventFlags::MaskAlphaShift,
    }
}

fn mouse_button(button: PointerButton) -> CGMouseButton {
    match button {
        PointerButton::Left => CGMouseButton::Left,
        PointerButton::Right => CGMouseButton::Right,
        PointerButton::Middle => CGMouseButton::Center,
        PointerButton::Back => CGMouseButton(3),
        PointerButton::Forward => CGMouseButton(4),
    }
}

/// A normalized position on `display` in global display coordinates.
fn display_point(display: &Display, x: u16, y: u16) -> CGPoint {
    let axis = |position: u16, origin: i32, length: u32| {
        f64::from(origin)
            + f64::from(position) * f64::from(length.saturating_sub(1).max(1)) / 65_535.0
    };
    CGPoint::new(
        axis(x, display.x, display.width),
        axis(y, display.y, display.height),
    )
}

fn current_pointer() -> CGPoint {
    CGEvent::location(CGEvent::new(None).as_deref())
}

/// Follows who last used the keyboard or pointer, from a listen-only event
/// tap, which needs the Input Monitoring permission.
struct OwnershipTap {
    run_loop: SendRunLoop,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct SendRunLoop(CFRetained<CFRunLoop>);
// SAFETY: CFRunLoopStop may be called from any thread.
unsafe impl Send for SendRunLoop {}

impl OwnershipTap {
    fn start(viewer_controls_input: Arc<AtomicBool>) -> anyhow::Result<Self> {
        let (started, result) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("meshrmm-input-ownership".into())
            .spawn(move || {
                let state = Arc::into_raw(viewer_controls_input);
                let mask = [
                    CGEventType::MouseMoved,
                    CGEventType::LeftMouseDown,
                    CGEventType::LeftMouseUp,
                    CGEventType::LeftMouseDragged,
                    CGEventType::RightMouseDown,
                    CGEventType::RightMouseUp,
                    CGEventType::RightMouseDragged,
                    CGEventType::OtherMouseDown,
                    CGEventType::OtherMouseUp,
                    CGEventType::OtherMouseDragged,
                    CGEventType::ScrollWheel,
                    CGEventType::KeyDown,
                    CGEventType::KeyUp,
                    CGEventType::FlagsChanged,
                ]
                .into_iter()
                .fold(0_u64, |mask, kind| mask | (1 << kind.0));
                // SAFETY: the callback matches CGEventTapCallBack and `state`
                // outlives the tap, which is disabled before it is released.
                let tap = unsafe {
                    CGEvent::tap_create(
                        CGEventTapLocation::HIDEventTap,
                        CGEventTapPlacement::TailAppendEventTap,
                        CGEventTapOptions::ListenOnly,
                        mask,
                        Some(track_ownership),
                        state.cast_mut().cast(),
                    )
                };
                let Some(tap) = tap else {
                    // SAFETY: the tap never took the reference.
                    drop(unsafe { Arc::from_raw(state) });
                    let _ = started.send(Err(anyhow::anyhow!(
                        "macOS refused an input event tap; allow the MeshRMM Agent under Input Monitoring in System Settings"
                    )));
                    return;
                };
                let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0);
                let run_loop = CFRunLoop::current().expect("every thread has a run loop");
                // SAFETY: kCFRunLoopCommonModes is a valid static mode.
                run_loop.add_source(source.as_deref(), unsafe { kCFRunLoopCommonModes });
                let _ = started.send(Ok(SendRunLoop(run_loop)));
                CFRunLoop::run();
                CGEvent::tap_enable(&tap, false);
                drop(tap);
                // SAFETY: the disabled tap no longer calls back.
                drop(unsafe { Arc::from_raw(state) });
            })
            .context("could not start the input ownership thread")?;
        let run_loop = result
            .recv()
            .context("the input ownership thread stopped")??;
        Ok(Self {
            run_loop,
            thread: Some(thread),
        })
    }
}

impl Drop for OwnershipTap {
    fn drop(&mut self) {
        self.run_loop.0.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

unsafe extern "C-unwind" fn track_ownership(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    state: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: the tap was created with a live `AtomicBool` reference.
    let viewer_controls_input = unsafe { &*state.cast::<AtomicBool>() };
    if kind != CGEventType::TapDisabledByTimeout && kind != CGEventType::TapDisabledByUserInput {
        // SAFETY: the event is valid for the duration of the callback.
        let tag = CGEvent::integer_value_field(
            Some(unsafe { event.as_ref() }),
            CGEventField::EventSourceUserData,
        );
        viewer_controls_input.store(tag == INPUT_TAG, Ordering::SeqCst);
    }
    event.as_ptr()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_positions_span_the_display() {
        let display = Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(1),
            name: String::new(),
            x: -1440,
            y: 0,
            width: 1440,
            height: 900,
            primary: false,
        };
        let start = display_point(&display, 0, 0);
        let end = display_point(&display, 65_535, 65_535);
        assert_eq!((start.x, start.y), (-1440.0, 0.0));
        assert_eq!((end.x, end.y), (-1.0, 899.0));
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;

    /// A text window that reports its frame and contents to a file.
    struct TypeTarget {
        child: std::process::Child,
        report: std::path::PathBuf,
    }

    impl TypeTarget {
        fn start() -> Self {
            let directory =
                std::env::temp_dir().join(format!("meshrmm-input-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let source = directory.join("type_target.swift");
            std::fs::write(&source, include_str!("testdata/type_target.swift")).unwrap();
            let binary = directory.join("type-target");
            let status = std::process::Command::new("/usr/bin/swiftc")
                .arg("-o")
                .arg(&binary)
                .arg(&source)
                .status()
                .unwrap();
            assert!(status.success());
            let report = directory.join("report.txt");
            let child = std::process::Command::new(&binary)
                .arg(&report)
                .spawn()
                .unwrap();
            let target = Self { child, report };
            let deadline = Instant::now() + Duration::from_secs(10);
            while !target.read().0.4 {
                assert!(
                    Instant::now() < deadline,
                    "the text window never became active"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
            target
        }

        /// The window frame (left, top, width, height), whether it is active,
        /// and its text.
        fn read(&self) -> ((f64, f64, f64, f64, bool), String) {
            let Ok(report) = std::fs::read_to_string(&self.report) else {
                return ((0.0, 0.0, 0.0, 0.0, false), String::new());
            };
            let (header, text) = report.split_once('\n').unwrap_or((&report, ""));
            let fields = header.split(' ').collect::<Vec<_>>();
            let number = |index: usize| fields[index].parse::<f64>().unwrap();
            (
                (
                    number(0),
                    number(1),
                    number(2),
                    number(3),
                    fields[4] == "true",
                ),
                text.to_owned(),
            )
        }
    }

    impl Drop for TypeTarget {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn key(
        controller: &mut InputController,
        display: DisplayId,
        scan_code: u16,
        extended: bool,
        pressed: bool,
    ) {
        controller
            .apply(RemoteInput::Key {
                display_id: display,
                scan_code,
                extended,
                pressed,
            })
            .unwrap();
    }

    fn tap(controller: &mut InputController, display: DisplayId, scan_code: u16, extended: bool) {
        key(controller, display, scan_code, extended, true);
        key(controller, display, scan_code, extended, false);
    }

    /// Clicks into a text window on the main display, types with scan codes,
    /// Shift, Command shortcuts and Unicode text, and reads the text back.
    /// Needs the Accessibility permission and briefly takes focus.
    #[test]
    #[ignore = "posts real input to a test window; needs the Accessibility permission"]
    fn clicks_types_and_uses_shortcuts_in_a_text_window() {
        let target = TypeTarget::start();
        let ((left, top, width, height, _), _) = target.read();
        let displays = super::super::display::enumerate().unwrap();
        let display = super::super::display::choose(&displays, None).unwrap();
        let mut controller = InputController::new().unwrap();
        controller.set_active_display(display.clone()).unwrap();
        let normalize = |value: f64, origin: i32, length: u32| {
            ((value - f64::from(origin)) * 65_535.0 / f64::from(length - 1)) as u16
        };
        let (x, y) = (
            normalize(left + width / 2.0, display.x, display.width),
            normalize(top + height / 2.0, display.y, display.height),
        );
        for pressed in [true, false] {
            controller
                .apply(RemoteInput::PointerButtonAt {
                    display_id: display.id,
                    x,
                    y,
                    button: PointerButton::Left,
                    pressed,
                })
                .unwrap();
        }
        // "Hi" with Shift held for the H, then typed Unicode text.
        key(&mut controller, display.id, 0x2a, false, true);
        tap(&mut controller, display.id, 0x23, false);
        key(&mut controller, display.id, 0x2a, false, false);
        tap(&mut controller, display.id, 0x17, false);
        controller
            .apply(RemoteInput::TypeText {
                display_id: display.id,
                text: " päss🔑\nnext".into(),
            })
            .unwrap();
        // Command (the Windows key) + A selects everything, so the Up arrow
        // goes to the first line, where End and "!" finish it.
        key(&mut controller, display.id, 0x5b, true, true);
        tap(&mut controller, display.id, 0x1e, false);
        key(&mut controller, display.id, 0x5b, true, false);
        tap(&mut controller, display.id, 0x48, true);
        key(&mut controller, display.id, 0x5b, true, true);
        tap(&mut controller, display.id, 0x4d, true);
        key(&mut controller, display.id, 0x5b, true, false);
        controller
            .apply(RemoteInput::TypeText {
                display_id: display.id,
                text: "!".into(),
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(target.read().1, "Hi päss🔑!\nnext");
        assert!(controller.viewer_controls_input());
    }
}
