//! Windows set-1 scan codes, as viewers send them, to Linux evdev key codes.
//! Linux numbered its keys after set 1, so most keys keep their code; the
//! extended keys and the higher codes need the table. The display server
//! then applies the user's keyboard layout, as for a physical keyboard.

/// The evdev code for a scan code, or `None` for keys Linux has no code for.
pub(super) fn key_code(scan_code: u16, extended: bool) -> Option<u16> {
    Some(match (scan_code, extended) {
        // Windows reports Num Lock as extended; the macOS viewer's Clear
        // key, in its place, is not.
        (0x45, _) => 69,
        (0x54, false) => 99,
        (code @ 0x01..=0x58, false) => code,
        (0x59, false) => 117,
        (0x64..=0x6e, false) => 183 + (scan_code - 0x64),
        (0x70, false) => 93,
        (0x73, false) => 89,
        (0x76, false) => 194,
        (0x79, false) => 92,
        (0x7b, false) => 94,
        (0x7d, false) => 124,
        (0x7e, false) => 121,
        (0xf1, false) => 123,
        (0xf2, false) => 122,
        // Extended keys.
        (0x1c, true) => 96,
        (0x1d, true) => 97,
        (0x35, true) => 98,
        (0x37, true) => 99,
        (0x38, true) => 100,
        (0x46, true) => 119,
        (0x47, true) => 102,
        (0x48, true) => 103,
        (0x49, true) => 104,
        (0x4b, true) => 105,
        (0x4d, true) => 106,
        (0x4f, true) => 107,
        (0x50, true) => 108,
        (0x51, true) => 109,
        (0x52, true) => 110,
        (0x53, true) => 111,
        (0x5b, true) => 125,
        (0x5c, true) => 126,
        (0x5d, true) => 127,
        (0x5e, true) => 116,
        (0x5f, true) => 142,
        (0x63, true) => 143,
        // Media and browser keys.
        (0x10, true) => 165,
        (0x19, true) => 163,
        (0x20, true) => 113,
        (0x21, true) => 140,
        (0x22, true) => 164,
        (0x24, true) => 166,
        (0x2e, true) => 114,
        (0x30, true) => 115,
        (0x32, true) => 172,
        (0x65, true) => 217,
        (0x66, true) => 156,
        (0x67, true) => 173,
        (0x68, true) => 128,
        (0x69, true) => 159,
        (0x6a, true) => 158,
        (0x6b, true) => 157,
        (0x6c, true) => 155,
        (0x6d, true) => 226,
        _ => return None,
    })
}

/// The evdev codes of the keys that type `character` on a US keyboard, with
/// Shift first when it is needed. Used for typed text the display server
/// cannot take directly; other layouts get their own keys' characters.
pub(super) fn us_keys(character: char) -> Option<(bool, u16)> {
    const SHIFT: bool = true;
    let plain = |code| Some((false, code));
    let shifted = |code| Some((SHIFT, code));
    match character {
        'a'..='z' => plain(LETTERS[character as usize - 'a' as usize]),
        'A'..='Z' => shifted(LETTERS[character as usize - 'A' as usize]),
        '1'..='9' => plain(2 + (character as u16 - '1' as u16)),
        '0' => plain(11),
        ' ' => plain(57),
        '\n' => plain(28),
        '\t' => plain(15),
        '-' => plain(12),
        '=' => plain(13),
        '[' => plain(26),
        ']' => plain(27),
        ';' => plain(39),
        '\'' => plain(40),
        '`' => plain(41),
        '\\' => plain(43),
        ',' => plain(51),
        '.' => plain(52),
        '/' => plain(53),
        '!' => shifted(2),
        '@' => shifted(3),
        '#' => shifted(4),
        '$' => shifted(5),
        '%' => shifted(6),
        '^' => shifted(7),
        '&' => shifted(8),
        '*' => shifted(9),
        '(' => shifted(10),
        ')' => shifted(11),
        '_' => shifted(12),
        '+' => shifted(13),
        '{' => shifted(26),
        '}' => shifted(27),
        ':' => shifted(39),
        '"' => shifted(40),
        '~' => shifted(41),
        '|' => shifted(43),
        '<' => shifted(51),
        '>' => shifted(52),
        '?' => shifted(53),
        _ => None,
    }
}

const LETTERS: [u16; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// Every key code the virtual keyboard can send.
pub(super) fn all_key_codes() -> impl Iterator<Item = u16> {
    1..=248
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_plain_extended_and_media_keys() {
        assert_eq!(key_code(0x1e, false), Some(30)); // A
        assert_eq!(key_code(0x1c, false), Some(28)); // Enter
        assert_eq!(key_code(0x1c, true), Some(96)); // Keypad Enter
        assert_eq!(key_code(0x48, true), Some(103)); // Up arrow
        assert_eq!(key_code(0x48, false), Some(72)); // Keypad 8
        assert_eq!(key_code(0x5b, true), Some(125)); // Windows key: Super
        assert_eq!(key_code(0x38, true), Some(100)); // Right Alt
        assert_eq!(key_code(0x37, true), Some(99)); // Print Screen
        assert_eq!(key_code(0x57, false), Some(87)); // F11
        assert_eq!(key_code(0x64, false), Some(183)); // F13
        assert_eq!(key_code(0x76, false), Some(194)); // F24
        assert_eq!(key_code(0x30, true), Some(115)); // Volume up
        assert_eq!(key_code(0x45, true), Some(69)); // Num Lock
        assert_eq!(key_code(0x7f, true), None);
    }

    #[test]
    fn types_ascii_on_us_keys() {
        assert_eq!(us_keys('a'), Some((false, 30)));
        assert_eq!(us_keys('Q'), Some((true, 16)));
        assert_eq!(us_keys('0'), Some((false, 11)));
        assert_eq!(us_keys('?'), Some((true, 53)));
        assert_eq!(us_keys('é'), None);
    }
}
