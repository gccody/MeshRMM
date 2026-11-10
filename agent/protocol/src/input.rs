use serde::{Deserialize, Serialize};

use crate::DisplayId;

/// The most characters "Type clipboard" types. A Linux Agent types a
/// character in 8 ms, 12 ms when it also presses or releases Shift, so the
/// longest text takes 40 seconds, and at most a minute.
pub const MAX_TYPED_TEXT_CHARS: usize = 5_000;
/// The most UTF-8 bytes a `TypeText` may carry: every character of the
/// longest text could take four.
pub const MAX_TYPED_TEXT_BYTES: usize = 4 * MAX_TYPED_TEXT_CHARS;

/// Why `text` cannot be typed, said for the technician, or `None` when it can.
pub fn typed_text_problem(text: &str) -> Option<String> {
    if text.contains('\0') {
        return Some("the text contains a null character".into());
    }
    let characters = text.chars().count();
    (characters > MAX_TYPED_TEXT_CHARS).then(|| {
        format!(
            "the text has {characters} characters, and at most {MAX_TYPED_TEXT_CHARS} can be typed"
        )
    })
}

/// Platform-neutral cursor shapes that can be represented by native viewers.
/// Endpoint-specific or custom cursors are sent as `Default`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    #[default]
    Default,
    Text,
    Wait,
    Crosshair,
    UpArrow,
    ResizeNorthWestSouthEast,
    ResizeNorthEastSouthWest,
    ResizeWestEast,
    ResizeNorthSouth,
    Move,
    NotAllowed,
    Pointer,
    Progress,
    Help,
    Pin,
    Person,
}

/// Input coordinates are normalized to a display rather than the full virtual
/// desktop. This keeps pointer input correctly bound to the displayed monitor,
/// including monitors with negative desktop coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteInput {
    PointerMove {
        display_id: DisplayId,
        x: u16,
        y: u16,
    },
    PointerButton {
        display_id: DisplayId,
        button: PointerButton,
        pressed: bool,
    },
    /// A pointer button event paired with its intended position. The Agent
    /// injects the move and button action in one Windows `SendInput` batch so
    /// physical endpoint activity cannot split a remote click across two
    /// different cursor positions.
    PointerButtonAt {
        display_id: DisplayId,
        x: u16,
        y: u16,
        button: PointerButton,
        pressed: bool,
    },
    Wheel {
        display_id: DisplayId,
        horizontal: i16,
        vertical: i16,
    },
    /// A wheel event paired with its intended position for the same atomic
    /// local/remote collaboration semantics as `PointerButtonAt`.
    WheelAt {
        display_id: DisplayId,
        x: u16,
        y: u16,
        horizontal: i16,
        vertical: i16,
    },
    /// A Windows set-1 scan code. The Windows viewer obtains this directly
    /// from the native key message and the macOS viewer translates its native
    /// hardware key code before sending it.
    Key {
        display_id: DisplayId,
        scan_code: u16,
        extended: bool,
        pressed: bool,
    },
    /// Type literal text as keyboard events on the active desktop. The Agent
    /// types it a piece at a time and says so with `Typing` while it does; a
    /// `StopTyping`, a new text or a key the technician presses stops it. At
    /// most [`MAX_TYPED_TEXT_CHARS`] characters.
    TypeText { display_id: DisplayId, text: String },
}

impl RemoteInput {
    pub fn set_display_id(&mut self, id: DisplayId) {
        match self {
            Self::PointerMove { display_id, .. }
            | Self::PointerButton { display_id, .. }
            | Self::PointerButtonAt { display_id, .. }
            | Self::Wheel { display_id, .. }
            | Self::WheelAt { display_id, .. }
            | Self::Key { display_id, .. }
            | Self::TypeText { display_id, .. } => *display_id = id,
        }
    }

    pub fn display_id(&self) -> DisplayId {
        match self {
            Self::PointerMove { display_id, .. }
            | Self::PointerButton { display_id, .. }
            | Self::PointerButtonAt { display_id, .. }
            | Self::Wheel { display_id, .. }
            | Self::WheelAt { display_id, .. }
            | Self::Key { display_id, .. }
            | Self::TypeText { display_id, .. } => *display_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[cfg(test)]
mod tests {
    use crate::SessionMessage;

    use super::*;

    #[test]
    fn typed_text_is_bounded_and_preserves_unicode() {
        let message = SessionMessage::Input(RemoteInput::TypeText {
            display_id: DisplayId(2),
            text: "päss🔑\r\nnext\tfield".into(),
        });
        assert_eq!(
            SessionMessage::decode(&message.encode().unwrap()).unwrap(),
            message
        );
        for text in ["x".repeat(MAX_TYPED_TEXT_BYTES + 1), "bad\0text".into()] {
            let message = SessionMessage::Input(RemoteInput::TypeText {
                display_id: DisplayId(2),
                text,
            });
            assert!(SessionMessage::decode(&message.encode().unwrap()).is_err());
        }
        let message = SessionMessage::Input(RemoteInput::TypeText {
            display_id: DisplayId(2),
            text: "🔑".repeat(MAX_TYPED_TEXT_CHARS),
        });
        let bytes = message.encode().unwrap();
        assert!(bytes.len() < 65536);
        assert_eq!(SessionMessage::decode(&bytes).unwrap(), message);
    }

    #[test]
    fn typed_text_is_limited_in_characters() {
        assert_eq!(typed_text_problem(&"🔑".repeat(MAX_TYPED_TEXT_CHARS)), None);
        assert_eq!(
            typed_text_problem(&"a".repeat(MAX_TYPED_TEXT_CHARS + 1)).as_deref(),
            Some("the text has 5001 characters, and at most 5000 can be typed")
        );
        assert!(typed_text_problem("a\0").is_some());
    }

    #[test]
    fn input_round_trips_through_control_channel() {
        let message = SessionMessage::Input(RemoteInput::PointerMove {
            display_id: DisplayId(2),
            x: 32_768,
            y: 65_535,
        });
        let bytes = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&bytes).unwrap(), message);
    }

    #[test]
    fn positioned_button_round_trips_through_control_channel() {
        let message = SessionMessage::Input(RemoteInput::PointerButtonAt {
            display_id: DisplayId(2),
            x: 12_345,
            y: 54_321,
            button: PointerButton::Left,
            pressed: true,
        });
        let bytes = message.encode().unwrap();
        assert_eq!(SessionMessage::decode(&bytes).unwrap(), message);
    }
}
