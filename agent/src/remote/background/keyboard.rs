//! Keyboard rules that hold however keys reach Session 0 apps.
use meshrmm_protocol::RemoteInput;

/// Scan codes of the left and right Windows keys, both extended.
const WINDOWS_KEYS: [u16; 2] = [0x5b, 0x5c];

/// Session 0 has no shell to consume Windows-key shortcuts, so without this,
/// Win+R would reach the app as a plain R and type "r". The Windows keys, and
/// every key pressed while one is held, are dropped along with their releases.
#[derive(Default)]
pub(super) struct ShellKeys {
    /// (scan code, extended) of each key whose press was dropped.
    dropped: Vec<(u16, bool)>,
}

impl ShellKeys {
    /// Whether `event` should reach the application.
    pub(super) fn deliver(&mut self, event: &RemoteInput) -> bool {
        let RemoteInput::Key {
            scan_code,
            extended,
            pressed,
            ..
        } = *event
        else {
            return true;
        };
        let key = (scan_code, extended);
        if pressed {
            let windows = extended && WINDOWS_KEYS.contains(&scan_code);
            let held = self
                .dropped
                .iter()
                .any(|&(scan, extended)| extended && WINDOWS_KEYS.contains(&scan));
            if !windows && !held {
                return true;
            }
            if !self.dropped.contains(&key) {
                self.dropped.push(key);
            }
            false
        } else if let Some(index) = self.dropped.iter().position(|dropped| *dropped == key) {
            self.dropped.swap_remove(index);
            false
        } else {
            true
        }
    }

    pub(super) fn release(&mut self) {
        self.dropped.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::DisplayId;

    fn key(scan_code: u16, extended: bool, pressed: bool) -> RemoteInput {
        RemoteInput::Key {
            display_id: DisplayId(0),
            scan_code,
            extended,
            pressed,
        }
    }

    #[test]
    fn windows_shortcuts_are_dropped() {
        let mut keys = ShellKeys::default();
        let shift = [key(0x2a, false, true), key(0x2a, false, false)];
        assert!(keys.deliver(&shift[0]));
        // Win+R, with auto-repeat, and R released after Win.
        for (event, delivered) in [
            (key(0x5b, true, true), false),
            (key(0x13, false, true), false),
            (key(0x5b, true, true), false),
            (key(0x13, false, true), false),
            (key(0x5b, true, false), false),
            // Shift was pressed before Win, so its release still counts.
            (shift[1].clone(), true),
            (key(0x13, false, false), false),
            (key(0x13, false, true), true),
            (key(0x13, false, false), true),
        ] {
            assert_eq!(keys.deliver(&event), delivered, "{event:?}");
        }
        // Scan code 0x5b without the extended flag is not a Windows key.
        assert!(keys.deliver(&key(0x5b, false, true)));
        assert!(!keys.deliver(&key(0x5c, true, true)));
        keys.release();
        assert!(keys.deliver(&key(0x13, false, true)));
        assert!(keys.deliver(&RemoteInput::TypeText {
            display_id: DisplayId(0),
            text: "r".into(),
        }));
    }
}
