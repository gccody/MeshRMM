use serde::{Deserialize, Serialize};

use crate::DisplayId;

/// Input coordinates are normalized to a display rather than the full virtual
/// desktop. This keeps pointer input correctly bound to the displayed monitor,
/// including monitors with negative desktop coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl RemoteInput {
    pub fn display_id(self) -> DisplayId {
        match self {
            Self::PointerMove { display_id, .. }
            | Self::PointerButton { display_id, .. }
            | Self::PointerButtonAt { display_id, .. }
            | Self::Wheel { display_id, .. }
            | Self::WheelAt { display_id, .. }
            | Self::Key { display_id, .. } => display_id,
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
