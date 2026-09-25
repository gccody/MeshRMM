//! Translates Mac key codes and modifier state to Windows scan codes.
//!
//! Modifiers are tracked per physical key from the event's device-dependent
//! flag bits, so releasing Right Shift while Left Shift is held keeps Shift
//! down on the remote. Command is only sent once a key, click or scroll uses
//! it. Pressed and released alone it becomes a Windows key tap. Cmd-Tab and
//! Cmd-Space also reach the viewer as a lone Command press, so the view cancels
//! the tap when the system saw a key go down that the viewer never received.

/// A Windows keyboard scan code and whether it has the E0 prefix.
pub(super) type ScanCode = (u16, bool);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RemoteKey {
    pub(super) scan_code: u16,
    pub(super) extended: bool,
    pub(super) pressed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandKey {
    /// Command sends the Windows key.
    Windows,
    /// Command sends Ctrl, so Cmd-C and Cmd-V copy and paste on the remote.
    Control,
}

// NSEvent device-dependent modifier bits (IOKit NX_DEVICE*KEYMASK) and the
// device-independent flags used when an event carries no device bits.
const DEVICE_LEFT_CONTROL: u64 = 0x0000_0001;
const DEVICE_LEFT_SHIFT: u64 = 0x0000_0002;
const DEVICE_RIGHT_SHIFT: u64 = 0x0000_0004;
const DEVICE_LEFT_COMMAND: u64 = 0x0000_0008;
const DEVICE_RIGHT_COMMAND: u64 = 0x0000_0010;
const DEVICE_LEFT_OPTION: u64 = 0x0000_0020;
const DEVICE_RIGHT_OPTION: u64 = 0x0000_0040;
const DEVICE_RIGHT_CONTROL: u64 = 0x0000_2000;
const SHIFT: u64 = 1 << 17;
const CONTROL: u64 = 1 << 18;
const OPTION: u64 = 1 << 19;
const COMMAND: u64 = 1 << 20;

const CAPS_LOCK_KEY: u16 = 57;

#[derive(Clone, Copy)]
enum Modifier {
    LeftShift,
    RightShift,
    LeftControl,
    RightControl,
    LeftOption,
    RightOption,
    LeftCommand,
    RightCommand,
}

/// In declaration order, so a key's twin on the other side is at `index ^ 1`.
const MODIFIERS: [Modifier; 8] = [
    Modifier::LeftShift,
    Modifier::RightShift,
    Modifier::LeftControl,
    Modifier::RightControl,
    Modifier::LeftOption,
    Modifier::RightOption,
    Modifier::LeftCommand,
    Modifier::RightCommand,
];

impl Modifier {
    fn index(self) -> usize {
        self as usize
    }

    fn is_command(self) -> bool {
        matches!(self, Self::LeftCommand | Self::RightCommand)
    }

    /// The device bit for this key, and the flag shared with its twin.
    fn bits(self) -> (u64, u64) {
        match self {
            Self::LeftShift => (DEVICE_LEFT_SHIFT, SHIFT),
            Self::RightShift => (DEVICE_RIGHT_SHIFT, SHIFT),
            Self::LeftControl => (DEVICE_LEFT_CONTROL, CONTROL),
            Self::RightControl => (DEVICE_RIGHT_CONTROL, CONTROL),
            Self::LeftOption => (DEVICE_LEFT_OPTION, OPTION),
            Self::RightOption => (DEVICE_RIGHT_OPTION, OPTION),
            Self::LeftCommand => (DEVICE_LEFT_COMMAND, COMMAND),
            Self::RightCommand => (DEVICE_RIGHT_COMMAND, COMMAND),
        }
    }

    fn is_left(self) -> bool {
        matches!(
            self,
            Self::LeftShift | Self::LeftControl | Self::LeftOption | Self::LeftCommand
        )
    }

    fn scan_code(self, command: CommandKey) -> ScanCode {
        match (self, command) {
            (Self::LeftShift, _) => (0x2a, false),
            (Self::RightShift, _) => (0x36, false),
            (Self::LeftControl, _) | (Self::LeftCommand, CommandKey::Control) => (0x1d, false),
            (Self::RightControl, _) | (Self::RightCommand, CommandKey::Control) => (0x1d, true),
            (Self::LeftOption, _) => (0x38, false),
            (Self::RightOption, _) => (0x38, true),
            (Self::LeftCommand, CommandKey::Windows) => (0x5b, true),
            (Self::RightCommand, CommandKey::Windows) => (0x5c, true),
        }
    }
}

pub(super) struct Keyboard {
    command: CommandKey,
    /// Physically held modifiers, indexed by `Modifier`.
    held: [bool; 8],
    /// Held Command keys that a key, click or scroll has used.
    engaged: [bool; 8],
    /// The Command key held alone, which becomes a tap if released unused.
    tap: Option<Modifier>,
    /// A Windows key tap for the view to send.
    pending_tap: Option<ScanCode>,
    /// Scan codes the remote currently has down for modifiers.
    down: Vec<ScanCode>,
}

impl Keyboard {
    pub(super) fn new(command: CommandKey) -> Self {
        Self {
            command,
            held: [false; 8],
            engaged: [false; 8],
            tap: None,
            pending_tap: None,
            down: Vec::new(),
        }
    }

    /// Changes the Command mapping. Returns releases for anything it had down.
    pub(super) fn set_command_key(&mut self, command: CommandKey) -> Vec<RemoteKey> {
        let released = self.release_all();
        self.command = command;
        released
    }

    /// Updates the held modifiers from an event's modifier flags.
    pub(super) fn sync(&mut self, flags: u64) -> Vec<RemoteKey> {
        self.sync_flags(flags, false)
    }

    /// `taps` is whether a Command release here may be a tap: only a
    /// `flagsChanged:` event for the key itself says it was just released.
    fn sync_flags(&mut self, flags: u64, taps: bool) -> Vec<RemoteKey> {
        let was_held = self.held;
        let has_device_bits = MODIFIERS
            .iter()
            .any(|modifier| flags & modifier.bits().0 != 0);
        for modifier in MODIFIERS {
            let (device, shared) = modifier.bits();
            // Events without device bits only say that some key of the pair is
            // held; attribute it to the left key unless the right one is held.
            let index = modifier.index();
            let held = if has_device_bits {
                flags & device != 0
            } else if flags & shared == 0 {
                false
            } else if self.held[index] || self.held[index ^ 1] {
                self.held[index]
            } else {
                modifier.is_left()
            };
            self.held[index] = held;
            if !held {
                self.engaged[index] = false;
            }
        }
        self.track_tap(was_held, taps);
        self.update()
    }

    fn track_tap(&mut self, was_held: [bool; 8], taps: bool) {
        let alone = |held: &[bool; 8], modifier: Modifier| {
            held.iter()
                .enumerate()
                .all(|(index, &down)| down == (index == modifier.index()))
        };
        for modifier in [Modifier::LeftCommand, Modifier::RightCommand] {
            if !was_held[modifier.index()] && self.held[modifier.index()] {
                self.tap = (self.command == CommandKey::Windows && alone(&self.held, modifier))
                    .then_some(modifier);
            }
        }
        let Some(modifier) = self.tap else {
            return;
        };
        if self.held[modifier.index()] {
            if !alone(&self.held, modifier) {
                // Another modifier joined: a chord, not a tap.
                self.tap = None;
            }
        } else {
            self.tap = None;
            if taps && alone(&was_held, modifier) {
                self.pending_tap = Some(modifier.scan_code(self.command));
            }
        }
    }

    /// The Windows key tap from a Command key pressed and released alone.
    pub(super) fn take_command_tap(&mut self) -> Option<ScanCode> {
        self.pending_tap.take()
    }

    /// A key went down that the viewer did not receive, such as the Tab of
    /// Cmd-Tab: a held Command key is no longer a tap.
    pub(super) fn cancel_command_tap(&mut self) {
        self.tap = None;
    }

    /// A `flagsChanged:` event for `key_code`.
    pub(super) fn flags_changed(&mut self, key_code: u16, flags: u64) -> Vec<RemoteKey> {
        if key_code == CAPS_LOCK_KEY {
            // AppKit reports Caps Lock state, not key presses; send a tap.
            let (scan_code, extended) = (0x3a, false);
            return vec![
                RemoteKey {
                    scan_code,
                    extended,
                    pressed: true,
                },
                RemoteKey {
                    scan_code,
                    extended,
                    pressed: false,
                },
            ];
        }
        self.sync_flags(flags, true)
    }

    /// Call before sending a non-modifier key press, click or scroll: held
    /// Command keys now take part and are sent.
    pub(super) fn engage_command(&mut self) -> Vec<RemoteKey> {
        self.tap = None;
        for modifier in [Modifier::LeftCommand, Modifier::RightCommand] {
            self.engaged[modifier.index()] = self.held[modifier.index()];
        }
        self.update()
    }

    /// Forgets all state after the view released every remote key.
    pub(super) fn reset(&mut self) {
        self.held = [false; 8];
        self.engaged = [false; 8];
        self.tap = None;
        self.pending_tap = None;
        self.down.clear();
    }

    fn release_all(&mut self) -> Vec<RemoteKey> {
        let released = self
            .down
            .drain(..)
            .map(|(scan_code, extended)| RemoteKey {
                scan_code,
                extended,
                pressed: false,
            })
            .collect();
        self.held = [false; 8];
        self.engaged = [false; 8];
        self.tap = None;
        self.pending_tap = None;
        released
    }

    fn update(&mut self) -> Vec<RemoteKey> {
        let mut wanted = Vec::new();
        for modifier in MODIFIERS {
            let index = modifier.index();
            let active = self.held[index] && (!modifier.is_command() || self.engaged[index]);
            let scan = modifier.scan_code(self.command);
            if active && !wanted.contains(&scan) {
                wanted.push(scan);
            }
        }
        let mut keys = Vec::new();
        for &(scan_code, extended) in &self.down {
            if !wanted.contains(&(scan_code, extended)) {
                keys.push(RemoteKey {
                    scan_code,
                    extended,
                    pressed: false,
                });
            }
        }
        for &(scan_code, extended) in &wanted {
            if !self.down.contains(&(scan_code, extended)) {
                keys.push(RemoteKey {
                    scan_code,
                    extended,
                    pressed: true,
                });
            }
        }
        self.down = wanted;
        keys
    }
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn LMGetKbdType() -> u8;
    fn KBGetLayoutType(keyboard_type: i16) -> u32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCounterForEventType(state_id: i32, event_type: u32) -> u32;
}

/// `kCGEventSourceStateHIDSystemState` and `kCGEventKeyDown`.
const HID_SYSTEM_STATE: i32 = 1;
const KEY_DOWN: u32 = 10;

/// How many keys have gone down system-wide, including ones the system
/// handles itself, such as the Tab of Cmd-Tab. Needs no permission.
pub(super) fn system_key_downs() -> u32 {
    unsafe { CGEventSourceCounterForEventType(HID_SYSTEM_STATE, KEY_DOWN) }
}

/// HIToolbox's `kKeyboardISO` physical layout type.
const KEYBOARD_ISO: u32 = u32::from_be_bytes(*b"ISO ");

/// Whether the keyboard that produced the latest event has the ISO layout.
pub(super) fn keyboard_is_iso() -> bool {
    unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == KEYBOARD_ISO }
}

/// Maps a non-modifier Mac key code. `iso` is whether the keyboard has the
/// ISO layout, where macOS reports the key left of 1 as `kVK_ISO_Section`
/// and the key beside Left Shift as `kVK_ANSI_Grave`.
pub(super) fn scan_code(code: u16, iso: bool) -> Option<ScanCode> {
    Some(match code {
        10 if iso => (0x29, false),
        10 => (0x56, false),
        50 if iso => (0x56, false),
        50 => (0x29, false),
        0 => (0x1e, false),
        1 => (0x1f, false),
        2 => (0x20, false),
        3 => (0x21, false),
        4 => (0x23, false),
        5 => (0x22, false),
        6 => (0x2c, false),
        7 => (0x2d, false),
        8 => (0x2e, false),
        9 => (0x2f, false),
        11 => (0x30, false),
        12 => (0x10, false),
        13 => (0x11, false),
        14 => (0x12, false),
        15 => (0x13, false),
        16 => (0x15, false),
        17 => (0x14, false),
        18 => (0x02, false),
        19 => (0x03, false),
        20 => (0x04, false),
        21 => (0x05, false),
        22 => (0x07, false),
        23 => (0x06, false),
        24 => (0x0d, false),
        25 => (0x0a, false),
        26 => (0x08, false),
        27 => (0x0c, false),
        28 => (0x09, false),
        29 => (0x0b, false),
        30 => (0x1b, false),
        31 => (0x18, false),
        32 => (0x16, false),
        33 => (0x1a, false),
        34 => (0x17, false),
        35 => (0x19, false),
        36 => (0x1c, false),
        37 => (0x26, false),
        38 => (0x24, false),
        39 => (0x28, false),
        40 => (0x25, false),
        41 => (0x27, false),
        42 => (0x2b, false),
        43 => (0x33, false),
        44 => (0x35, false),
        45 => (0x31, false),
        46 => (0x32, false),
        47 => (0x34, false),
        48 => (0x0f, false),
        49 => (0x39, false),
        51 => (0x0e, false),
        53 => (0x01, false),
        65 => (0x53, false),
        67 => (0x37, false),
        69 => (0x4e, false),
        71 => (0x45, false),
        75 => (0x35, true),
        76 => (0x1c, true),
        78 => (0x4a, false),
        81 => (0x0d, false),
        82 => (0x52, false),
        83 => (0x4f, false),
        84 => (0x50, false),
        85 => (0x51, false),
        86 => (0x4b, false),
        87 => (0x4c, false),
        88 => (0x4d, false),
        89 => (0x47, false),
        91 => (0x48, false),
        92 => (0x49, false),
        // JIS: Yen, Ro (underscore), Eisu and Kana, mapped as Chromium does.
        93 => (0x7d, false),
        94 => (0x73, false),
        96 => (0x3f, false),
        97 => (0x40, false),
        98 => (0x41, false),
        99 => (0x3d, false),
        100 => (0x42, false),
        101 => (0x43, false),
        102 => (0x71, false),
        103 => (0x57, false),
        104 => (0x72, false),
        109 => (0x44, false),
        111 => (0x58, false),
        114 => (0x52, true),
        115 => (0x47, true),
        116 => (0x49, true),
        117 => (0x53, true),
        118 => (0x3e, false),
        119 => (0x4f, true),
        120 => (0x3c, false),
        121 => (0x51, true),
        122 => (0x3b, false),
        123 => (0x4b, true),
        124 => (0x4d, true),
        125 => (0x50, true),
        126 => (0x48, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(scan_code: u16, extended: bool) -> RemoteKey {
        RemoteKey {
            scan_code,
            extended,
            pressed: true,
        }
    }

    fn release(scan_code: u16, extended: bool) -> RemoteKey {
        RemoteKey {
            scan_code,
            extended,
            pressed: false,
        }
    }

    const LEFT_COMMAND_KEY: u16 = 55;
    const RIGHT_COMMAND_KEY: u16 = 54;
    const LEFT_CMD: u64 = COMMAND | DEVICE_LEFT_COMMAND;

    #[test]
    fn command_alone_is_held_back_and_released_as_a_windows_tap() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        // Cmd-Tab looks the same: AppKit sees Command go down and up, never
        // the Tab. The view cancels the tap when the system saw the Tab.
        assert!(
            keyboard
                .flags_changed(LEFT_COMMAND_KEY, LEFT_CMD)
                .is_empty()
        );
        assert_eq!(keyboard.take_command_tap(), None, "not while held");
        assert!(keyboard.flags_changed(LEFT_COMMAND_KEY, 0).is_empty());
        assert_eq!(keyboard.take_command_tap(), Some((0x5b, true)));
        assert_eq!(keyboard.take_command_tap(), None, "taken once");
        keyboard.flags_changed(RIGHT_COMMAND_KEY, COMMAND | DEVICE_RIGHT_COMMAND);
        keyboard.flags_changed(RIGHT_COMMAND_KEY, 0);
        assert_eq!(keyboard.take_command_tap(), Some((0x5c, true)));
    }

    #[test]
    fn command_that_was_used_or_chorded_is_not_a_tap() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.engage_command();
        keyboard.flags_changed(LEFT_COMMAND_KEY, 0);
        assert_eq!(keyboard.take_command_tap(), None, "Cmd-R");

        let shift = SHIFT | DEVICE_LEFT_SHIFT;
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.flags_changed(56, LEFT_CMD | shift);
        keyboard.flags_changed(56, LEFT_CMD);
        keyboard.flags_changed(LEFT_COMMAND_KEY, 0);
        assert_eq!(keyboard.take_command_tap(), None, "Cmd-Shift");

        keyboard.flags_changed(56, shift);
        keyboard.flags_changed(LEFT_COMMAND_KEY, shift | LEFT_CMD);
        keyboard.flags_changed(LEFT_COMMAND_KEY, shift);
        assert_eq!(keyboard.take_command_tap(), None, "Shift-Cmd");
    }

    #[test]
    fn command_tap_is_dropped_by_reset_and_missed_releases() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.flags_changed(LEFT_COMMAND_KEY, 0);
        keyboard.reset();
        assert_eq!(keyboard.take_command_tap(), None);
        // A release seen only through a later event's flags is not a tap.
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.sync(0);
        assert_eq!(keyboard.take_command_tap(), None);
    }

    #[test]
    fn command_tap_is_cancelled_by_a_key_the_system_took() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.cancel_command_tap();
        keyboard.flags_changed(LEFT_COMMAND_KEY, 0);
        assert_eq!(keyboard.take_command_tap(), None, "Cmd-Tab");
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.flags_changed(LEFT_COMMAND_KEY, 0);
        assert_eq!(
            keyboard.take_command_tap(),
            Some((0x5b, true)),
            "next press"
        );
    }

    #[test]
    fn command_sending_control_never_taps() {
        let mut keyboard = Keyboard::new(CommandKey::Control);
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.flags_changed(LEFT_COMMAND_KEY, 0);
        assert_eq!(keyboard.take_command_tap(), None);
    }

    #[test]
    fn command_is_sent_once_a_key_uses_it_and_released_after() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        assert!(
            keyboard
                .flags_changed(LEFT_COMMAND_KEY, LEFT_CMD)
                .is_empty()
        );
        assert_eq!(keyboard.engage_command(), vec![press(0x5b, true)]);
        assert!(keyboard.engage_command().is_empty(), "sent only once");
        assert_eq!(
            keyboard.flags_changed(LEFT_COMMAND_KEY, 0),
            vec![release(0x5b, true)]
        );
    }

    #[test]
    fn command_can_send_control_instead() {
        let mut keyboard = Keyboard::new(CommandKey::Control);
        keyboard.flags_changed(RIGHT_COMMAND_KEY, COMMAND | DEVICE_RIGHT_COMMAND);
        assert_eq!(keyboard.engage_command(), vec![press(0x1d, true)]);
        assert_eq!(
            keyboard.set_command_key(CommandKey::Windows),
            vec![release(0x1d, true)]
        );
    }

    #[test]
    fn command_mapped_to_control_does_not_release_a_held_control() {
        let mut keyboard = Keyboard::new(CommandKey::Control);
        let control = CONTROL | DEVICE_LEFT_CONTROL;
        assert_eq!(
            keyboard.flags_changed(59, control),
            vec![press(0x1d, false)]
        );
        keyboard.flags_changed(LEFT_COMMAND_KEY, control | LEFT_CMD);
        assert!(keyboard.engage_command().is_empty());
        assert!(keyboard.flags_changed(LEFT_COMMAND_KEY, control).is_empty());
        assert_eq!(keyboard.flags_changed(59, 0), vec![release(0x1d, false)]);
    }

    #[test]
    fn releasing_one_shift_keeps_the_other_down() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        let left = SHIFT | DEVICE_LEFT_SHIFT;
        let both = left | DEVICE_RIGHT_SHIFT;
        assert_eq!(keyboard.flags_changed(56, left), vec![press(0x2a, false)]);
        assert_eq!(keyboard.flags_changed(60, both), vec![press(0x36, false)]);
        // The shared Shift flag is still set; only the device bit says which.
        assert_eq!(keyboard.flags_changed(60, left), vec![release(0x36, false)]);
        assert_eq!(keyboard.flags_changed(56, 0), vec![release(0x2a, false)]);
    }

    #[test]
    fn right_side_modifiers_keep_their_own_scan_codes() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        assert_eq!(
            keyboard.flags_changed(61, OPTION | DEVICE_RIGHT_OPTION),
            vec![press(0x38, true)]
        );
        assert_eq!(
            keyboard.flags_changed(
                62,
                OPTION | DEVICE_RIGHT_OPTION | CONTROL | DEVICE_RIGHT_CONTROL
            ),
            vec![press(0x1d, true)]
        );
    }

    #[test]
    fn events_without_device_bits_fall_back_to_the_shared_flags() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        assert_eq!(keyboard.sync(SHIFT), vec![press(0x2a, false)]);
        assert!(keyboard.sync(SHIFT).is_empty());
        assert_eq!(keyboard.sync(0), vec![release(0x2a, false)]);
    }

    #[test]
    fn a_missed_release_is_caught_up_by_the_next_event() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        keyboard.flags_changed(LEFT_COMMAND_KEY, LEFT_CMD);
        keyboard.engage_command();
        // Command was released while another app was active.
        assert_eq!(keyboard.sync(0), vec![release(0x5b, true)]);
        keyboard.reset();
        assert!(keyboard.sync(0).is_empty());
    }

    #[test]
    fn caps_lock_is_a_tap() {
        let mut keyboard = Keyboard::new(CommandKey::Windows);
        assert_eq!(
            keyboard.flags_changed(CAPS_LOCK_KEY, 1 << 16),
            vec![press(0x3a, false), release(0x3a, false)]
        );
    }

    #[test]
    fn iso_keyboards_swap_the_section_and_grave_keys() {
        // ANSI: the key left of 1 is grave.
        assert_eq!(scan_code(50, false), Some((0x29, false)));
        // ISO: left of 1 is reported as section, the key beside Left Shift as grave.
        assert_eq!(scan_code(10, true), Some((0x29, false)));
        assert_eq!(scan_code(50, true), Some((0x56, false)));
    }

    #[test]
    fn jis_keys_are_mapped() {
        assert_eq!(scan_code(93, false), Some((0x7d, false)));
        assert_eq!(scan_code(94, false), Some((0x73, false)));
        assert_eq!(scan_code(102, false), Some((0x71, false)));
        assert_eq!(scan_code(104, false), Some((0x72, false)));
    }

    #[test]
    fn modifier_key_codes_are_not_regular_keys() {
        for code in [54, 55, 56, 57, 58, 59, 60, 61, 62] {
            assert_eq!(scan_code(code, false), None, "key code {code}");
        }
    }
}
