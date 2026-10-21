//! The viewer's keyboard and pointer, injected through uinput: a virtual
//! keyboard and a virtual absolute pointer that X11 and every Wayland
//! compositor read like real devices, so input reaches the desktop, the lock
//! screen and the login screen alike. The display server applies its own
//! keyboard layout and maps the pointer onto the whole desktop.
//!
//! Only root can open `/dev/uinput`, so the coordinator opens it for each
//! session helper it starts and passes the descriptors on; the helper
//! creates the devices the first time the viewer sends input.
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};

use anyhow::{Context, bail};
use meshrmm_protocol::{Display, PointerButton, RemoteInput};

use super::display;
use super::keymap;

/// The descriptors' environment variable: two numbers, keyboard first.
pub(crate) const DESCRIPTORS_VARIABLE: &str = "MESHRMM_UINPUT_FDS";
/// The pointer's axes run from zero to this across the whole desktop.
const AXIS_MAX: i32 = 65_535;
/// The gap between typed keys.
const TYPING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);
/// A wheel notch in the viewer's units, as Windows counts them.
const WHEEL_NOTCH: i32 = 120;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const KEY_LEFTSHIFT: u16 = 42;
const BUS_VIRTUAL: u16 = 0x06;

/// `_IO('U', n)` and `_IOW('U', n, int)` from linux/uinput.h.
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;

/// `struct uinput_user_dev`, the device description the kernel reads.
#[repr(C)]
struct DeviceDescription {
    name: [u8; 80],
    bus_type: u16,
    vendor: u16,
    product: u16,
    version: u16,
    force_feedback_effects: u32,
    abs_max: [i32; 64],
    abs_min: [i32; 64],
    abs_fuzz: [i32; 64],
    abs_flat: [i32; 64],
}

/// `struct input_event` on 64-bit Linux.
#[repr(C)]
struct Event {
    seconds: i64,
    microseconds: i64,
    kind: u16,
    code: u16,
    value: i32,
}

/// One uinput device, destroyed when dropped.
struct Device {
    file: File,
}

impl Device {
    fn create(
        file: File,
        name: &str,
        configure: impl FnOnce(&File) -> anyhow::Result<()>,
        axes: bool,
    ) -> anyhow::Result<Self> {
        configure(&file)?;
        // SAFETY: the description is plain data, for which zero is valid.
        let mut description: DeviceDescription = unsafe { std::mem::zeroed() };
        let length = name.len().min(description.name.len() - 1);
        description.name[..length].copy_from_slice(&name.as_bytes()[..length]);
        description.bus_type = BUS_VIRTUAL;
        description.vendor = 0x4d52;
        description.product = if axes { 2 } else { 1 };
        description.version = 1;
        if axes {
            description.abs_max[usize::from(ABS_X)] = AXIS_MAX;
            description.abs_max[usize::from(ABS_Y)] = AXIS_MAX;
        }
        // SAFETY: the description is `repr(C)` and fully initialized.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                (&raw const description).cast::<u8>(),
                std::mem::size_of::<DeviceDescription>(),
            )
        };
        (&file)
            .write_all(bytes)
            .context("could not describe the virtual input device")?;
        ioctl(&file, UI_DEV_CREATE, 0).context("could not create the virtual input device")?;
        Ok(Self { file })
    }

    fn send(&self, events: &[(u16, u16, i32)]) -> anyhow::Result<()> {
        let mut bytes = Vec::with_capacity((events.len() + 1) * std::mem::size_of::<Event>());
        for &(kind, code, value) in events.iter().chain([&(EV_SYN, SYN_REPORT, 0)]) {
            let event = Event {
                seconds: 0,
                microseconds: 0,
                kind,
                code,
                value,
            };
            // SAFETY: the event is `repr(C)` plain data.
            bytes.extend_from_slice(unsafe {
                std::slice::from_raw_parts(
                    (&raw const event).cast::<u8>(),
                    std::mem::size_of::<Event>(),
                )
            });
        }
        (&self.file)
            .write_all(&bytes)
            .context("could not send input to the virtual device")
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = ioctl(&self.file, UI_DEV_DESTROY, 0);
    }
}

fn ioctl(file: &File, request: libc::c_ulong, argument: libc::c_int) -> std::io::Result<()> {
    // SAFETY: every uinput request used here takes an int, or nothing.
    if unsafe { libc::ioctl(file.as_raw_fd(), request, argument) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The two uinput descriptors, before the devices exist.
pub(crate) struct UinputFiles {
    keyboard: File,
    pointer: File,
}

impl UinputFiles {
    /// The descriptors the coordinator passed, or `/dev/uinput` opened here,
    /// which works only for root or where the device's permissions allow it.
    pub(crate) fn take() -> anyhow::Result<Self> {
        if let Ok(value) = std::env::var(DESCRIPTORS_VARIABLE) {
            // SAFETY: removing the variable races only with other threads
            // reading the environment, and the helper reads it here first.
            unsafe { std::env::remove_var(DESCRIPTORS_VARIABLE) };
            let mut descriptors = value.split(',').map(str::parse::<i32>);
            if let (Some(Ok(keyboard)), Some(Ok(pointer))) =
                (descriptors.next(), descriptors.next())
            {
                // SAFETY: the coordinator opened these descriptors for this
                // process, which owns them from here on.
                return Ok(unsafe {
                    Self {
                        keyboard: File::from_raw_fd(keyboard),
                        pointer: File::from_raw_fd(pointer),
                    }
                });
            }
            bail!("{DESCRIPTORS_VARIABLE} is invalid");
        }
        Ok(Self {
            keyboard: open_uinput()?,
            pointer: open_uinput()?,
        })
    }
}

/// Opens uinput for writing, as the coordinator does for each helper.
pub(crate) fn open_uinput() -> anyhow::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/uinput")
        .context("could not open /dev/uinput for input injection")
}

struct Devices {
    keyboard: Device,
    pointer: Device,
}

impl Devices {
    fn create(files: UinputFiles) -> anyhow::Result<Self> {
        let keyboard = Device::create(
            files.keyboard,
            "MeshRMM virtual keyboard",
            |file| {
                ioctl(file, UI_SET_EVBIT, EV_KEY.into())?;
                for code in keymap::all_key_codes() {
                    ioctl(file, UI_SET_KEYBIT, code.into())?;
                }
                Ok(())
            },
            false,
        )?;
        let pointer = Device::create(
            files.pointer,
            "MeshRMM virtual pointer",
            |file| {
                ioctl(file, UI_SET_EVBIT, EV_KEY.into())?;
                for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                    ioctl(file, UI_SET_KEYBIT, button.into())?;
                }
                ioctl(file, UI_SET_EVBIT, EV_ABS.into())?;
                ioctl(file, UI_SET_ABSBIT, ABS_X.into())?;
                ioctl(file, UI_SET_ABSBIT, ABS_Y.into())?;
                ioctl(file, UI_SET_EVBIT, EV_REL.into())?;
                for axis in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                    ioctl(file, UI_SET_RELBIT, axis.into())?;
                }
                Ok(())
            },
            true,
        )?;
        Ok(Self { keyboard, pointer })
    }
}

/// The session's input, and the display the viewer sees.
pub(crate) struct InputController {
    files: Option<UinputFiles>,
    devices: Option<Devices>,
    active_display: Option<Display>,
    displays: Vec<Display>,
    pressed_keys: BTreeSet<u16>,
    pressed_buttons: BTreeSet<u16>,
    /// Hi-res wheel units not yet sent as whole notches.
    wheel: (i32, i32),
    blocked: bool,
}

impl InputController {
    pub(crate) fn new() -> Self {
        let files = UinputFiles::take()
            .inspect_err(|error| tracing::warn!(error = ?error, "remote input is unavailable"))
            .ok();
        Self {
            files,
            devices: None,
            active_display: None,
            displays: Vec::new(),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            wheel: (0, 0),
            blocked: false,
        }
    }

    pub(crate) fn set_active_display(
        &mut self,
        display: Display,
        displays: Vec<Display>,
    ) -> anyhow::Result<()> {
        if self
            .active_display
            .as_ref()
            .is_some_and(|active| active.id != display.id)
        {
            self.release_all()?;
        }
        self.active_display = Some(display);
        self.displays = displays;
        Ok(())
    }

    /// The desktop's displays, as the viewer last saw them.
    pub(crate) fn displays(&self) -> &[Display] {
        &self.displays
    }

    fn devices(&mut self) -> anyhow::Result<&Devices> {
        if self.devices.is_none() {
            let files = self
                .files
                .take()
                .context("remote input is unavailable: the Agent could not open /dev/uinput")?;
            self.devices = Some(Devices::create(files)?);
        }
        Ok(self
            .devices
            .as_ref()
            .expect("the devices were just created"))
    }

    /// The pointer's axis values for a point on the active display.
    fn position(&self, x: u16, y: u16) -> anyhow::Result<(i32, i32)> {
        let display = self
            .active_display
            .as_ref()
            .context("remote input arrived before a display was selected")?;
        let (left, top, width, height) =
            display::bounds(&self.displays).context("the desktop has no displays")?;
        let scale = |offset: i32, size: u32, value: u16, origin: i32, extent: u32| {
            let point = f64::from(offset - origin)
                + f64::from(value) / f64::from(u16::MAX) * f64::from(size.saturating_sub(1));
            ((point + 0.5) * f64::from(AXIS_MAX + 1) / f64::from(extent))
                .clamp(0.0, f64::from(AXIS_MAX)) as i32
        };
        Ok((
            scale(display.x, display.width, x, left, width),
            scale(display.y, display.height, y, top, height),
        ))
    }

    pub(crate) fn apply(&mut self, input: RemoteInput) -> anyhow::Result<()> {
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
            RemoteInput::PointerMove { x, y, .. } => self.move_pointer(x, y),
            RemoteInput::PointerButton {
                button, pressed, ..
            } => self.button(button_code(button), pressed),
            RemoteInput::PointerButtonAt {
                x,
                y,
                button,
                pressed,
                ..
            } => {
                let (x, y) = self.position(x, y)?;
                let code = button_code(button);
                self.track_button(code, pressed);
                self.devices()?.pointer.send(&[
                    (EV_ABS, ABS_X, x),
                    (EV_ABS, ABS_Y, y),
                    (EV_KEY, code, pressed.into()),
                ])
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
                self.move_pointer(x, y)?;
                self.wheel(horizontal, vertical)
            }
            RemoteInput::Key {
                scan_code,
                extended,
                pressed,
                ..
            } => {
                let Some(code) = keymap::key_code(scan_code, extended) else {
                    tracing::debug!(scan_code, extended, "dropped a key Linux has no code for");
                    return Ok(());
                };
                self.key(code, pressed)
            }
            RemoteInput::TypeText { text, .. } => self.type_text(&text),
        }
    }

    fn move_pointer(&mut self, x: u16, y: u16) -> anyhow::Result<()> {
        let (x, y) = self.position(x, y)?;
        self.devices()?
            .pointer
            .send(&[(EV_ABS, ABS_X, x), (EV_ABS, ABS_Y, y)])
    }

    fn track_button(&mut self, code: u16, pressed: bool) {
        if pressed {
            self.pressed_buttons.insert(code);
        } else {
            self.pressed_buttons.remove(&code);
        }
    }

    fn button(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        self.track_button(code, pressed);
        self.devices()?
            .pointer
            .send(&[(EV_KEY, code, pressed.into())])
    }

    /// Sends the wheel in hi-res units, and whole notches for programs that
    /// read only those.
    fn wheel(&mut self, horizontal: i16, vertical: i16) -> anyhow::Result<()> {
        let (horizontal, vertical) = (i32::from(horizontal), i32::from(vertical));
        self.wheel.0 += horizontal;
        self.wheel.1 += vertical;
        let notches = (self.wheel.0 / WHEEL_NOTCH, self.wheel.1 / WHEEL_NOTCH);
        self.wheel.0 -= notches.0 * WHEEL_NOTCH;
        self.wheel.1 -= notches.1 * WHEEL_NOTCH;
        let mut events = Vec::with_capacity(4);
        if vertical != 0 {
            events.push((EV_REL, REL_WHEEL_HI_RES, vertical));
        }
        if notches.1 != 0 {
            events.push((EV_REL, REL_WHEEL, notches.1));
        }
        if horizontal != 0 {
            events.push((EV_REL, REL_HWHEEL_HI_RES, horizontal));
        }
        if notches.0 != 0 {
            events.push((EV_REL, REL_HWHEEL, notches.0));
        }
        if events.is_empty() {
            return Ok(());
        }
        self.devices()?.pointer.send(&events)
    }

    fn key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
        if pressed {
            self.pressed_keys.insert(code);
        } else {
            self.pressed_keys.remove(&code);
        }
        self.devices()?
            .keyboard
            .send(&[(EV_KEY, code, pressed.into())])
    }

    /// Types `text` with the keys of a US keyboard. Characters without one
    /// are skipped. Keys go out a few milliseconds apart: the display server
    /// reads the device's queue in batches, and a queue that overflows drops
    /// keys, which can leave Shift taps that turn on Sticky Keys.
    fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        self.release_all()?;
        let mut skipped = 0;
        let mut shifted = false;
        let pace = || std::thread::sleep(TYPING_INTERVAL);
        for character in text.chars() {
            let character = if character == '\r' { '\n' } else { character };
            let Some((shift, code)) = keymap::us_keys(character) else {
                skipped += 1;
                continue;
            };
            let keyboard = &self.devices()?.keyboard;
            // Shift stays down across a run of shifted characters.
            if shift != shifted {
                keyboard.send(&[(EV_KEY, KEY_LEFTSHIFT, shift.into())])?;
                shifted = shift;
                pace();
            }
            keyboard.send(&[(EV_KEY, code, 1)])?;
            pace();
            keyboard.send(&[(EV_KEY, code, 0)])?;
            pace();
        }
        if shifted {
            self.devices()?
                .keyboard
                .send(&[(EV_KEY, KEY_LEFTSHIFT, 0)])?;
        }
        if skipped > 0 {
            tracing::info!(skipped, "typed text had characters without a US key");
        }
        Ok(())
    }

    pub(crate) fn release_all(&mut self) -> anyhow::Result<()> {
        if self.devices.is_none() {
            return Ok(());
        }
        let mut failure = None;
        for code in std::mem::take(&mut self.pressed_keys) {
            if let Err(error) = self.key(code, false) {
                failure = Some(error);
            }
            self.pressed_keys.remove(&code);
        }
        for code in std::mem::take(&mut self.pressed_buttons) {
            if let Err(error) = self.button(code, false) {
                failure = Some(error);
            }
            self.pressed_buttons.remove(&code);
        }
        failure.map_or(Ok(()), Err)
    }

    pub(crate) fn blocked(&self) -> bool {
        self.blocked
    }

    /// Blocks or unblocks the local keyboard and pointer by grabbing them,
    /// which needs root: the coordinator does it for its helpers, and a
    /// console Agent can only when it runs as root.
    pub(crate) fn set_blocked(&mut self, blocked: bool) -> anyhow::Result<()> {
        if blocked {
            self.release_all()?;
        }
        super::input_block::set_blocked(blocked)?;
        self.blocked = blocked;
        Ok(())
    }
}

fn button_code(button: PointerButton) -> u16 {
    match button {
        PointerButton::Left => BTN_LEFT,
        PointerButton::Right => BTN_RIGHT,
        PointerButton::Middle => BTN_MIDDLE,
        PointerButton::Back => BTN_SIDE,
        PointerButton::Forward => BTN_EXTRA,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::{DesktopSession, DisplayId};

    fn display(id: u32, x: i32, y: i32, width: u32, height: u32) -> Display {
        Display {
            session: DesktopSession::Console,
            id: DisplayId(id),
            name: format!("{id}"),
            x,
            y,
            width,
            height,
            primary: id == 1,
        }
    }

    fn controller(active: Display, displays: Vec<Display>) -> InputController {
        let mut controller = InputController {
            files: None,
            devices: None,
            active_display: None,
            displays: Vec::new(),
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            wheel: (0, 0),
            blocked: false,
        };
        controller.set_active_display(active, displays).unwrap();
        controller
    }

    #[test]
    fn points_map_onto_the_whole_desktop() {
        let left = display(1, 0, 0, 1920, 1080);
        let right = display(2, 1920, 0, 1920, 1080);
        let single = controller(left.clone(), vec![left.clone()]);
        assert_eq!(single.position(0, 0).unwrap(), (17, 30));
        assert_eq!(single.position(u16::MAX, u16::MAX).unwrap().0, 65_518);
        let dual = controller(right.clone(), vec![left, right]);
        // The right display's left edge is the desktop's middle.
        let (x, _) = dual.position(0, 0).unwrap();
        assert_eq!(x, (1920.5 * 65_536.0 / 3840.0) as i32);
    }

    #[test]
    fn displays_left_of_the_origin_map_from_the_left_edge() {
        let left = display(2, -1280, 0, 1280, 1024);
        let main = display(1, 0, 0, 1920, 1080);
        let dual = controller(left.clone(), vec![left, main]);
        assert_eq!(dual.position(0, 0).unwrap().0, 10);
    }

    #[test]
    fn device_structures_have_the_kernel_layout() {
        assert_eq!(std::mem::size_of::<DeviceDescription>(), 1116);
        assert_eq!(std::mem::size_of::<Event>(), 24);
    }

    /// Needs root, or write access to /dev/uinput.
    #[test]
    #[ignore = "creates virtual input devices"]
    fn creates_the_virtual_devices() {
        let devices = Devices::create(UinputFiles::take().unwrap()).unwrap();
        devices.pointer.send(&[(EV_ABS, ABS_X, 100)]).unwrap();
        devices.keyboard.send(&[(EV_KEY, 42, 1)]).unwrap();
        devices.keyboard.send(&[(EV_KEY, 42, 0)]).unwrap();
        let names = std::fs::read_to_string("/proc/bus/input/devices").unwrap();
        assert!(names.contains("MeshRMM virtual pointer"), "{names}");
    }
}
