//! Windows set-1 scan codes, as viewers send them, to Mac virtual key codes.
//! This is the inverse of the macOS viewer's table, with the Windows keys
//! becoming Command, as the viewer's Command key sends them.

/// A Mac modifier key and the event flag it holds down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Modifier {
    Shift,
    Control,
    Option,
    Command,
    CapsLock,
}

/// The Mac key code for a scan code. `iso` is whether the Mac's keyboard has
/// the ISO layout, where the key left of 1 is `kVK_ISO_Section`.
pub(super) fn key_code(scan_code: u16, extended: bool, iso: bool) -> Option<u16> {
    Some(match (scan_code, extended) {
        (0x29, false) if iso => 10,
        (0x29, false) => 50,
        (0x56, false) if iso => 50,
        (0x56, false) => 10,
        (0x1e, false) => 0,
        (0x1f, false) => 1,
        (0x20, false) => 2,
        (0x21, false) => 3,
        (0x23, false) => 4,
        (0x22, false) => 5,
        (0x2c, false) => 6,
        (0x2d, false) => 7,
        (0x2e, false) => 8,
        (0x2f, false) => 9,
        (0x30, false) => 11,
        (0x10, false) => 12,
        (0x11, false) => 13,
        (0x12, false) => 14,
        (0x13, false) => 15,
        (0x15, false) => 16,
        (0x14, false) => 17,
        (0x02, false) => 18,
        (0x03, false) => 19,
        (0x04, false) => 20,
        (0x05, false) => 21,
        (0x07, false) => 22,
        (0x06, false) => 23,
        (0x0d, false) => 24,
        (0x0a, false) => 25,
        (0x08, false) => 26,
        (0x0c, false) => 27,
        (0x09, false) => 28,
        (0x0b, false) => 29,
        (0x1b, false) => 30,
        (0x18, false) => 31,
        (0x16, false) => 32,
        (0x1a, false) => 33,
        (0x17, false) => 34,
        (0x19, false) => 35,
        (0x1c, false) => 36,
        (0x26, false) => 37,
        (0x24, false) => 38,
        (0x28, false) => 39,
        (0x25, false) => 40,
        (0x27, false) => 41,
        (0x2b, false) => 42,
        (0x33, false) => 43,
        (0x35, false) => 44,
        (0x31, false) => 45,
        (0x32, false) => 46,
        (0x34, false) => 47,
        (0x0f, false) => 48,
        (0x39, false) => 49,
        (0x0e, false) => 51,
        (0x01, false) => 53,
        // Modifiers.
        (0x5c, true) => 54,
        (0x5b, true) => 55,
        (0x2a, false) => 56,
        (0x3a, false) => 57,
        (0x38, false) => 58,
        (0x1d, false) => 59,
        (0x36, false) => 60,
        (0x38, true) => 61,
        (0x1d, true) => 62,
        // Keypad.
        (0x53, false) => 65,
        (0x37, false) => 67,
        (0x4e, false) => 69,
        (0x45, false) => 71,
        (0x35, true) => 75,
        (0x1c, true) => 76,
        (0x4a, false) => 78,
        (0x52, false) => 82,
        (0x4f, false) => 83,
        (0x50, false) => 84,
        (0x51, false) => 85,
        (0x4b, false) => 86,
        (0x4c, false) => 87,
        (0x4d, false) => 88,
        (0x47, false) => 89,
        (0x48, false) => 91,
        (0x49, false) => 92,
        // JIS: Yen, Ro, Eisu and Kana.
        (0x7d, false) => 93,
        (0x73, false) => 94,
        (0x71, false) => 102,
        (0x72, false) => 104,
        // Function keys.
        (0x3b, false) => 122,
        (0x3c, false) => 120,
        (0x3d, false) => 99,
        (0x3e, false) => 118,
        (0x3f, false) => 96,
        (0x40, false) => 97,
        (0x41, false) => 98,
        (0x42, false) => 100,
        (0x43, false) => 101,
        (0x44, false) => 109,
        (0x57, false) => 103,
        (0x58, false) => 111,
        (0x64, false) => 105,
        (0x65, false) => 107,
        (0x66, false) => 113,
        (0x67, false) => 106,
        (0x68, false) => 64,
        (0x69, false) => 79,
        (0x6a, false) => 80,
        (0x6b, false) => 90,
        // Navigation; Insert becomes Help, which sits in its place.
        (0x52, true) => 114,
        (0x47, true) => 115,
        (0x49, true) => 116,
        (0x53, true) => 117,
        (0x4f, true) => 119,
        (0x51, true) => 121,
        (0x4b, true) => 123,
        (0x4d, true) => 124,
        (0x50, true) => 125,
        (0x48, true) => 126,
        // Media keys.
        (0x20, true) => 74,
        (0x2e, true) => 73,
        (0x30, true) => 72,
        _ => return None,
    })
}

pub(super) fn modifier(key_code: u16) -> Option<Modifier> {
    Some(match key_code {
        54 | 55 => Modifier::Command,
        56 | 60 => Modifier::Shift,
        57 => Modifier::CapsLock,
        58 | 61 => Modifier::Option,
        59 | 62 => Modifier::Control,
        _ => return None,
    })
}

/// Whether the Mac's built-in or attached keyboard has the ISO layout.
pub(super) fn keyboard_is_iso() -> bool {
    const KEYBOARD_ISO: u32 = 1;
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn LMGetKbdType() -> u8;
        fn KBGetLayoutType(keyboard_type: i16) -> u32;
    }
    // SAFETY: both functions only read the current keyboard type.
    unsafe { KBGetLayoutType(i16::from(LMGetKbdType())) == KEYBOARD_ISO }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_modifiers_and_extended_keys() {
        assert_eq!(key_code(0x1e, false, false), Some(0)); // A
        assert_eq!(key_code(0x5b, true, false), Some(55)); // Windows key: Command
        assert_eq!(key_code(0x1d, true, false), Some(62)); // Right Control
        assert_eq!(key_code(0x48, true, false), Some(126)); // Up arrow
        assert_eq!(key_code(0x48, false, false), Some(91)); // Keypad 8
        assert_eq!(key_code(0x37, true, false), None); // Print Screen
    }

    #[test]
    fn swaps_the_iso_keys_like_the_viewer() {
        assert_eq!(key_code(0x29, false, false), Some(50));
        assert_eq!(key_code(0x29, false, true), Some(10));
        assert_eq!(key_code(0x56, false, true), Some(50));
    }

    #[test]
    fn identifies_modifier_keys() {
        assert_eq!(modifier(55), Some(Modifier::Command));
        assert_eq!(modifier(61), Some(Modifier::Option));
        assert_eq!(modifier(0), None);
    }
}
