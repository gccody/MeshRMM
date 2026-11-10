//! The keys and pointer buttons the viewer holds down on the device. Both
//! native windows track them so a lost focus, blocked input or a file drop
//! can release everything instead of leaving a key or button stuck down.

use meshrmm_protocol::{DisplayId, PointerButton, RemoteInput};

#[derive(Default)]
pub struct HeldInput {
    keys: Vec<(u16, bool)>,
    buttons: Vec<PointerButton>,
}

impl HeldInput {
    /// The key event to send, noting whether the key is now held.
    pub fn key(
        &mut self,
        display_id: DisplayId,
        scan_code: u16,
        extended: bool,
        pressed: bool,
    ) -> RemoteInput {
        let key = (scan_code, extended);
        if !pressed {
            self.keys.retain(|held| *held != key);
        } else if !self.keys.contains(&key) {
            self.keys.push(key);
        }
        RemoteInput::Key {
            display_id,
            scan_code,
            extended,
            pressed,
        }
    }

    /// The key event to send for a key the window reports, if any. Only a
    /// fresh press starts holding a key: an auto-repeat of a key the device
    /// does not hold, one already down when the viewer gained focus or after
    /// it released everything, is not sent, so the device does not press a
    /// key by itself that the technician pressed elsewhere. The release of a
    /// key the device does not hold is not sent either.
    #[cfg(any(windows, test))]
    pub fn key_event(
        &mut self,
        display_id: DisplayId,
        scan_code: u16,
        extended: bool,
        pressed: bool,
        repeat: bool,
    ) -> Option<RemoteInput> {
        let held = self.keys.contains(&(scan_code, extended));
        if (pressed && repeat && !held) || (!pressed && !held) {
            return None;
        }
        Some(self.key(display_id, scan_code, extended, pressed))
    }

    /// Releases the held keys that `still_down` says the technician no
    /// longer holds: Windows kept their key-ups, as it does on its secure
    /// desktop after Ctrl+Alt+Del.
    #[cfg(any(windows, test))]
    pub fn release_unheld(
        &mut self,
        display_id: DisplayId,
        mut still_down: impl FnMut(u16, bool) -> bool,
    ) -> Vec<RemoteInput> {
        let mut released = Vec::new();
        self.keys.retain(|&(scan_code, extended)| {
            let down = still_down(scan_code, extended);
            if !down {
                released.push(RemoteInput::Key {
                    display_id,
                    scan_code,
                    extended,
                    pressed: false,
                });
            }
            down
        });
        released
    }

    /// Whether a key is held on the device.
    #[cfg(any(windows, test))]
    pub fn keys_held(&self) -> bool {
        !self.keys.is_empty()
    }

    /// The button event for a pointer at `position`, the point in the video,
    /// or `None` outside it. Presses outside the video are ignored. A release
    /// there still ends a drag that began on the video, without moving the
    /// remote pointer to the edge.
    pub fn button(
        &mut self,
        display_id: DisplayId,
        position: Option<(u16, u16)>,
        button: PointerButton,
        pressed: bool,
    ) -> Option<RemoteInput> {
        let input = match position {
            Some((x, y)) => RemoteInput::PointerButtonAt {
                display_id,
                x,
                y,
                button,
                pressed,
            },
            None if !pressed && self.buttons.contains(&button) => RemoteInput::PointerButton {
                display_id,
                button,
                pressed: false,
            },
            None => return None,
        };
        if !pressed {
            self.buttons.retain(|held| *held != button);
        } else if !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
        Some(input)
    }

    /// Whether a drag is in progress, which keeps the Windows mouse capture.
    #[cfg(any(windows, test))]
    pub fn buttons_held(&self) -> bool {
        !self.buttons.is_empty()
    }

    /// Releases every held key, then every held button, and forgets them.
    pub fn release_all(&mut self, display_id: DisplayId) -> Vec<RemoteInput> {
        let keys = self
            .keys
            .drain(..)
            .map(|(scan_code, extended)| RemoteInput::Key {
                display_id,
                scan_code,
                extended,
                pressed: false,
            });
        let buttons = self
            .buttons
            .drain(..)
            .map(|button| RemoteInput::PointerButton {
                display_id,
                button,
                pressed: false,
            });
        keys.chain(buttons).collect()
    }
}

/// Whether a WM_WTSSESSION_CHANGE event takes the keyboard from the viewer:
/// the workstation locking (Windows+L among others) or the session leaving
/// its console or remote connection. The window keeps its focus through
/// these, and the key-ups go elsewhere.
#[cfg(any(windows, test))]
pub fn releases_on_session_change(event: u32) -> bool {
    const WTS_CONSOLE_DISCONNECT: u32 = 2;
    const WTS_REMOTE_DISCONNECT: u32 = 4;
    const WTS_SESSION_LOGOFF: u32 = 6;
    const WTS_SESSION_LOCK: u32 = 7;
    matches!(
        event,
        WTS_CONSOLE_DISCONNECT | WTS_REMOTE_DISCONNECT | WTS_SESSION_LOGOFF | WTS_SESSION_LOCK
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: DisplayId = DisplayId(3);

    fn key_up(scan_code: u16, extended: bool) -> RemoteInput {
        RemoteInput::Key {
            display_id: DISPLAY,
            scan_code,
            extended,
            pressed: false,
        }
    }

    fn button_up(button: PointerButton) -> RemoteInput {
        RemoteInput::PointerButton {
            display_id: DISPLAY,
            button,
            pressed: false,
        }
    }

    #[test]
    fn releases_held_keys_then_buttons_once() {
        let mut held = HeldInput::default();
        held.key(DISPLAY, 0x1d, false, true);
        held.key(DISPLAY, 0x5b, true, true);
        held.key(DISPLAY, 0x5b, true, true);
        held.key(DISPLAY, 0x1e, false, true);
        held.key(DISPLAY, 0x1e, false, false);
        held.button(DISPLAY, Some((1, 2)), PointerButton::Left, true);
        held.button(DISPLAY, Some((1, 2)), PointerButton::Right, true);
        held.button(DISPLAY, Some((1, 2)), PointerButton::Right, false);

        assert_eq!(
            held.release_all(DISPLAY),
            vec![
                key_up(0x1d, false),
                key_up(0x5b, true),
                button_up(PointerButton::Left)
            ]
        );
        assert!(held.release_all(DISPLAY).is_empty());
        assert!(!held.buttons_held());
    }

    #[test]
    fn key_events_carry_the_key_and_display() {
        let mut held = HeldInput::default();
        assert_eq!(
            held.key(DISPLAY, 0x2a, false, true),
            RemoteInput::Key {
                display_id: DISPLAY,
                scan_code: 0x2a,
                extended: false,
                pressed: true,
            }
        );
        assert_eq!(held.key(DISPLAY, 0x2a, false, false), key_up(0x2a, false));
    }

    fn key_down(scan_code: u16, extended: bool) -> RemoteInput {
        RemoteInput::Key {
            display_id: DISPLAY,
            scan_code,
            extended,
            pressed: true,
        }
    }

    const CTRL: u16 = 0x1d;
    const ALT: u16 = 0x38;
    const ESC: u16 = 0x01;
    const WIN: u16 = 0x5b;
    const VK_ESCAPE: u32 = 0x1b;
    const VK_LWIN: u32 = 0x5b;
    const WTS_SESSION_LOCK: u32 = 7;
    const WTS_SESSION_UNLOCK: u32 = 8;

    /// The virtual key Windows reports for a scan code.
    fn virtual_key(scan_code: u16) -> u32 {
        match scan_code {
            ESC => VK_ESCAPE,
            other => u32::from(other),
        }
    }

    /// The viewer's keyboard, as the Windows window drives it: keys the
    /// low-level hook takes from Windows, keys the window gets, and the
    /// device's view of both.
    #[derive(Default)]
    struct Viewer {
        hook: crate::shortcuts::SystemShortcuts,
        held: HeldInput,
        sent: Vec<RemoteInput>,
    }

    impl Viewer {
        /// A key the hook takes: the Windows keys, Alt+Tab, Alt+Esc, Ctrl+Esc.
        fn hooked(&mut self, scan_code: u16, extended: bool, pressed: bool, control: bool) {
            let taken = self
                .hook
                .take(virtual_key(scan_code), pressed, false, control, false);
            assert!(taken);
            self.sent.extend(
                self.held
                    .key_event(DISPLAY, scan_code, extended, pressed, false),
            );
        }

        /// A WM_KEYDOWN or WM_KEYUP; `repeat` is bit 30 of its LPARAM.
        fn window_key(&mut self, scan_code: u16, pressed: bool, repeat: bool) {
            self.sent.extend(
                self.held
                    .key_event(DISPLAY, scan_code, false, pressed, repeat),
            );
        }

        /// The window loses focus or is minimised.
        fn release(&mut self) {
            self.hook.clear();
            self.sent.extend(self.held.release_all(DISPLAY));
        }

        /// A WM_WTSSESSION_CHANGE.
        fn session_change(&mut self, event: u32) {
            if releases_on_session_change(event) {
                self.release();
            }
        }

        /// What the window loop checks once its messages are handled.
        fn check(&mut self, physically_down: &[u16]) {
            let hook = &self.hook;
            self.sent
                .extend(self.held.release_unheld(DISPLAY, |scan_code, _| {
                    hook.holds(virtual_key(scan_code)) || physically_down.contains(&scan_code)
                }));
        }
    }

    #[test]
    fn focus_loss_with_ctrl_and_a_hooked_windows_key_releases_both_and_repeats_press_nothing() {
        let mut viewer = Viewer::default();
        viewer.window_key(CTRL, true, false);
        viewer.window_key(CTRL, true, true);
        viewer.hooked(WIN, true, true, false);
        viewer.release();
        // The technician still holds Ctrl when focus returns: its repeats
        // and its release reach the window, and press nothing on the device.
        viewer.window_key(CTRL, true, true);
        viewer.window_key(CTRL, false, false);
        assert_eq!(
            viewer.sent,
            vec![
                key_down(CTRL, false),
                key_down(CTRL, false),
                key_down(WIN, true),
                key_up(CTRL, false),
                key_up(WIN, true),
            ]
        );
        assert!(!viewer.held.keys_held());
    }

    #[test]
    fn windows_l_leaves_nothing_down() {
        let mut viewer = Viewer::default();
        viewer.hooked(WIN, true, true, false);
        // The hook holds the Windows key, which Windows' own key state never
        // saw go down.
        viewer.check(&[]);
        assert_eq!(viewer.sent, vec![key_down(WIN, true)]);
        // Windows takes L and locks the workstation: the window keeps its
        // focus, the key-ups go to the lock screen, and the viewer hears of
        // the lock through WM_WTSSESSION_CHANGE.
        viewer.session_change(WTS_SESSION_LOCK);
        viewer.session_change(WTS_SESSION_UNLOCK);
        viewer.check(&[]);
        assert!(!viewer.hook.holds(VK_LWIN));
        assert_eq!(viewer.sent, vec![key_down(WIN, true), key_up(WIN, true)]);
    }

    #[test]
    fn ctrl_alt_del_leaves_nothing_down() {
        let mut viewer = Viewer::default();
        viewer.window_key(CTRL, true, false);
        viewer.window_key(ALT, true, false);
        // Ctrl and Alt are down; the hook holds neither, and is not asked
        // about them while they are physically down.
        viewer.check(&[CTRL, ALT]);
        // Delete never reaches the window: Windows shows its secure desktop,
        // which takes the key-ups, and the window keeps its focus.
        viewer.check(&[]);
        viewer.check(&[]);
        viewer.window_key(CTRL, false, false);
        assert_eq!(
            viewer.sent,
            vec![
                key_down(CTRL, false),
                key_down(ALT, false),
                key_up(CTRL, false),
                key_up(ALT, false),
            ]
        );
        assert!(!viewer.held.keys_held());
    }

    #[test]
    fn hooked_keys_stay_down_while_the_hook_holds_them() {
        let mut viewer = Viewer::default();
        // Ctrl+Esc: Ctrl reaches the window, Esc is taken by the hook, and
        // Windows' own key state never sees Esc go down.
        viewer.window_key(CTRL, true, false);
        viewer.hooked(ESC, false, true, true);
        viewer.check(&[CTRL]);
        viewer.hooked(ESC, false, false, true);
        viewer.window_key(CTRL, false, false);
        assert_eq!(
            viewer.sent,
            vec![
                key_down(CTRL, false),
                key_down(ESC, false),
                key_up(ESC, false),
                key_up(CTRL, false),
            ]
        );
    }

    #[test]
    fn buttons_outside_the_video_only_finish_a_drag() {
        let mut held = HeldInput::default();
        assert_eq!(held.button(DISPLAY, None, PointerButton::Left, true), None);
        assert_eq!(held.button(DISPLAY, None, PointerButton::Left, false), None);
        assert!(!held.buttons_held());

        assert_eq!(
            held.button(DISPLAY, Some((10, 20)), PointerButton::Left, true),
            Some(RemoteInput::PointerButtonAt {
                display_id: DISPLAY,
                x: 10,
                y: 20,
                button: PointerButton::Left,
                pressed: true,
            })
        );
        assert!(held.buttons_held());
        // The drag ends outside the video.
        assert_eq!(
            held.button(DISPLAY, None, PointerButton::Left, false),
            Some(button_up(PointerButton::Left))
        );
        assert!(!held.buttons_held());
        assert!(held.release_all(DISPLAY).is_empty());
    }
}
