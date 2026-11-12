//! Linux evdev key codes, as the display server reports a physical key, to
//! the Windows set-1 scan codes viewers send. It is the Linux Agent's table
//! (`agent/src/remote/linux/keymap.rs`) the other way round, so a key
//! pressed on a Linux viewer is the same key on a Linux device, and the key
//! in the same place on a Windows or macOS one.

/// A Windows keyboard scan code and whether it has the E0 prefix.
pub(super) type ScanCode = (u16, bool);

/// The scan code for an evdev key code, or `None` for keys Windows has no
/// code for.
pub(super) fn scan_code(key_code: u32) -> Option<ScanCode> {
    let plain = |code: u16| Some((code, false));
    let extended = |code: u16| Some((code, true));
    match key_code {
        // Windows reports Num Lock as extended.
        69 => extended(0x45),
        // Linux numbered its keys after set 1, so most keep their code.
        1..=83 | 85..=88 => plain(key_code as u16),
        89 => plain(0x73),
        92 => plain(0x79),
        93 => plain(0x70),
        94 => plain(0x7b),
        96 => extended(0x1c),
        97 => extended(0x1d),
        98 => extended(0x35),
        99 => extended(0x37),
        100 => extended(0x38),
        102 => extended(0x47),
        103 => extended(0x48),
        104 => extended(0x49),
        105 => extended(0x4b),
        106 => extended(0x4d),
        107 => extended(0x4f),
        108 => extended(0x50),
        109 => extended(0x51),
        110 => extended(0x52),
        111 => extended(0x53),
        113 => extended(0x20),
        114 => extended(0x2e),
        115 => extended(0x30),
        116 => extended(0x5e),
        117 => plain(0x59),
        // Pause, which Windows reports as Num Lock's code without the prefix.
        119 => plain(0x45),
        121 => plain(0x7e),
        122 => plain(0xf2),
        123 => plain(0xf1),
        124 => plain(0x7d),
        125 => extended(0x5b),
        126 => extended(0x5c),
        127 => extended(0x5d),
        128 => extended(0x68),
        140 => extended(0x21),
        142 => extended(0x5f),
        143 => extended(0x63),
        155 => extended(0x6c),
        156 => extended(0x66),
        157 => extended(0x6b),
        158 => extended(0x6a),
        159 => extended(0x69),
        163 => extended(0x19),
        164 => extended(0x22),
        165 => extended(0x10),
        166 => extended(0x24),
        172 => extended(0x32),
        173 => extended(0x67),
        // F13 to F23, then F24.
        183..=193 => plain(0x64 + (key_code - 183) as u16),
        194 => plain(0x76),
        217 => extended(0x65),
        226 => extended(0x6d),
        _ => None,
    }
}

/// V, which with Ctrl pastes copied files.
pub(super) const V: ScanCode = (0x2f, false);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_plain_extended_and_media_keys() {
        assert_eq!(scan_code(30), Some((0x1e, false))); // A
        assert_eq!(scan_code(28), Some((0x1c, false))); // Enter
        assert_eq!(scan_code(96), Some((0x1c, true))); // Keypad Enter
        assert_eq!(scan_code(103), Some((0x48, true))); // Up arrow
        assert_eq!(scan_code(72), Some((0x48, false))); // Keypad 8
        assert_eq!(scan_code(125), Some((0x5b, true))); // Super: Windows key
        assert_eq!(scan_code(100), Some((0x38, true))); // Right Alt
        assert_eq!(scan_code(99), Some((0x37, true))); // Print Screen
        assert_eq!(scan_code(87), Some((0x57, false))); // F11
        assert_eq!(scan_code(183), Some((0x64, false))); // F13
        assert_eq!(scan_code(194), Some((0x76, false))); // F24
        assert_eq!(scan_code(115), Some((0x30, true))); // Volume up
        assert_eq!(scan_code(69), Some((0x45, true))); // Num Lock
        assert_eq!(scan_code(119), Some((0x45, false))); // Pause
        assert_eq!(scan_code(0), None);
        assert_eq!(scan_code(84), None);
        assert_eq!(scan_code(240), None);
    }

    #[test]
    fn no_two_keys_share_a_scan_code() {
        let mut seen = std::collections::HashMap::new();
        for key_code in 0..512 {
            if let Some(scan_code) = scan_code(key_code) {
                assert_eq!(
                    seen.insert(scan_code, key_code),
                    None,
                    "{key_code} repeats {scan_code:?}"
                );
            }
        }
    }
}
