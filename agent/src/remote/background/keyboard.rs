//! Keyboard rules that hold however keys reach Session 0 apps.
use meshrmm_protocol::RemoteInput;

/// Scan codes of the left and right Windows keys, both extended.
const WINDOWS_KEYS: [u16; 2] = [0x5b, 0x5c];
const R: u16 = 0x13;

/// Where a key event goes.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    Application,
    Dropped,
    /// Win+R, which the workspace handles like the Windows shell.
    Run,
}

/// Session 0 has no shell to consume Windows-key shortcuts, so without this,
/// Win+R would reach the app as a plain R and type "r". The Windows keys, and
/// every key pressed while one is held, are dropped along with their releases.
/// Win+R opens the workspace's Run dialog.
#[derive(Default)]
pub(super) struct ShellKeys {
    /// (scan code, extended) of each key whose press was dropped.
    dropped: Vec<(u16, bool)>,
}

impl ShellKeys {
    pub(super) fn route(&mut self, event: &RemoteInput) -> Route {
        let RemoteInput::Key {
            scan_code,
            extended,
            pressed,
            ..
        } = *event
        else {
            return Route::Application;
        };
        let key = (scan_code, extended);
        if pressed {
            let windows = extended && WINDOWS_KEYS.contains(&scan_code);
            let held = self
                .dropped
                .iter()
                .any(|&(scan, extended)| extended && WINDOWS_KEYS.contains(&scan));
            if !windows && !held {
                return Route::Application;
            }
            if !self.dropped.contains(&key) {
                self.dropped.push(key);
            }
            if held && key == (R, false) {
                Route::Run
            } else {
                Route::Dropped
            }
        } else if let Some(index) = self.dropped.iter().position(|dropped| *dropped == key) {
            self.dropped.swap_remove(index);
            Route::Dropped
        } else {
            Route::Application
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
        use Route::*;
        let mut keys = ShellKeys::default();
        let shift = [key(0x2a, false, true), key(0x2a, false, false)];
        assert_eq!(keys.route(&shift[0]), Application);
        // Win+R, with auto-repeat, and R released after Win.
        for (event, route) in [
            (key(0x5b, true, true), Dropped),
            (key(0x13, false, true), Run),
            (key(0x5b, true, true), Dropped),
            (key(0x13, false, true), Run),
            (key(0x5b, true, false), Dropped),
            // Shift was pressed before Win, so its release still counts.
            (shift[1].clone(), Application),
            (key(0x13, false, false), Dropped),
            (key(0x13, false, true), Application),
            (key(0x13, false, false), Application),
            // Other Windows-key shortcuts do nothing.
            (key(0x5c, true, true), Dropped),
            (key(0x12, false, true), Dropped),
            (key(0x12, false, false), Dropped),
            (key(0x5c, true, false), Dropped),
        ] {
            assert_eq!(keys.route(&event), route, "{event:?}");
        }
        // Scan code 0x5b without the extended flag is not a Windows key.
        assert_eq!(keys.route(&key(0x5b, false, true)), Application);
        assert_eq!(keys.route(&key(0x5c, true, true)), Dropped);
        keys.release();
        assert_eq!(keys.route(&key(0x13, false, true)), Application);
        assert_eq!(
            keys.route(&RemoteInput::TypeText {
                display_id: DisplayId(0),
                text: "r".into(),
            }),
            Application
        );
    }
}
