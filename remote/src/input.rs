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
