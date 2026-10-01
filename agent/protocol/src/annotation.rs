use serde::{Deserialize, Serialize};

use crate::DisplayId;

/// A technician's drawing over the viewed display. The Agent shows it to the
/// device's user above every window, and the capture includes it, so the
/// technician sees it in the video. It is not input: view-only viewers may
/// send it. Points are normalized to the display like pointer input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Annotation {
    /// Starts a stroke at the point.
    Start {
        display_id: DisplayId,
        x: u16,
        y: u16,
    },
    /// Extends the latest stroke to the point.
    Extend {
        display_id: DisplayId,
        x: u16,
        y: u16,
    },
    /// Erases every stroke.
    Clear,
}

impl Annotation {
    pub fn display_id(&self) -> Option<DisplayId> {
        match self {
            Self::Start { display_id, .. } | Self::Extend { display_id, .. } => Some(*display_id),
            Self::Clear => None,
        }
    }

    pub fn set_display_id(&mut self, id: DisplayId) {
        match self {
            Self::Start { display_id, .. } | Self::Extend { display_id, .. } => *display_id = id,
            Self::Clear => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionMessage;

    #[test]
    fn annotations_round_trip_after_the_audio_preference() {
        for annotation in [
            Annotation::Start {
                display_id: DisplayId(2),
                x: 0,
                y: 65_535,
            },
            Annotation::Extend {
                display_id: DisplayId(2),
                x: 32_768,
                y: 1,
            },
            Annotation::Clear,
        ] {
            let message = SessionMessage::Annotate(annotation);
            let bytes = message.encode().unwrap();
            assert_eq!(bytes[0], 40);
            assert_eq!(SessionMessage::decode(&bytes).unwrap(), message);
        }
    }

    #[test]
    fn clearing_names_no_display() {
        let mut annotation = Annotation::Extend {
            display_id: DisplayId(2),
            x: 1,
            y: 2,
        };
        annotation.set_display_id(DisplayId(7));
        assert_eq!(annotation.display_id(), Some(DisplayId(7)));
        let mut clear = Annotation::Clear;
        clear.set_display_id(DisplayId(7));
        assert_eq!(clear.display_id(), None);
    }
}
