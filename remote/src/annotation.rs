//! Drawing on the device's screen. While the technician annotates, the left
//! button draws over the video instead of clicking on the device, and the
//! right button erases the drawing. Annotations are not input, so view-only
//! sessions can annotate too. The Agent draws the strokes on the device's
//! screen, where its user sees them and the video shows them.

// Linux builds only the tests of the viewer's shared code.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use meshrmm_protocol::{Annotation, DisplayId, SessionMessage};

#[derive(Debug, Default)]
pub struct Annotator {
    enabled: bool,
    /// The left button is down and draws a stroke.
    drawing: bool,
}

impl Annotator {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Turns annotating on or off. Turning it off erases the drawing.
    pub fn toggle(&mut self) -> Option<SessionMessage> {
        self.enabled = !self.enabled;
        self.drawing = false;
        (!self.enabled).then_some(SessionMessage::Annotate(Annotation::Clear))
    }

    /// Stops annotating without a message, for a display that cannot show a
    /// drawing. The Agent erases the drawing when the display changes.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.drawing = false;
    }

    /// The left button went down at `position`, `None` outside the video.
    pub fn start(
        &mut self,
        display_id: DisplayId,
        position: Option<(u16, u16)>,
    ) -> Option<SessionMessage> {
        let (x, y) = position.filter(|_| self.enabled)?;
        self.drawing = true;
        Some(SessionMessage::Annotate(Annotation::Start {
            display_id,
            x,
            y,
        }))
    }

    /// The pointer moved to `position` while the left button is down.
    pub fn extend(
        &mut self,
        display_id: DisplayId,
        position: Option<(u16, u16)>,
    ) -> Option<SessionMessage> {
        let (x, y) = position.filter(|_| self.drawing)?;
        Some(SessionMessage::Annotate(Annotation::Extend {
            display_id,
            x,
            y,
        }))
    }

    /// The left button went up, or the window lost the mouse.
    pub fn finish(&mut self) {
        self.drawing = false;
    }

    /// Erases the drawing while annotating.
    pub fn clear(&mut self) -> Option<SessionMessage> {
        self.drawing = false;
        self.enabled
            .then_some(SessionMessage::Annotate(Annotation::Clear))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISPLAY: DisplayId = DisplayId(2);

    fn annotation(message: Option<SessionMessage>) -> Option<Annotation> {
        match message? {
            SessionMessage::Annotate(annotation) => Some(annotation),
            other => panic!("unexpected message {other:?}"),
        }
    }

    #[test]
    fn nothing_is_drawn_until_annotating_starts() {
        let mut annotator = Annotator::default();
        assert!(!annotator.enabled());
        assert_eq!(annotator.start(DISPLAY, Some((1, 2))), None);
        assert_eq!(annotator.extend(DISPLAY, Some((3, 4))), None);
        assert_eq!(annotator.clear(), None);
    }

    #[test]
    fn a_drag_draws_one_stroke() {
        let mut annotator = Annotator::default();
        assert_eq!(annotator.toggle(), None);
        // Moving without the button down draws nothing.
        assert_eq!(annotator.extend(DISPLAY, Some((3, 4))), None);
        assert_eq!(
            annotation(annotator.start(DISPLAY, Some((1, 2)))),
            Some(Annotation::Start {
                display_id: DISPLAY,
                x: 1,
                y: 2
            })
        );
        assert_eq!(
            annotation(annotator.extend(DISPLAY, Some((3, 4)))),
            Some(Annotation::Extend {
                display_id: DISPLAY,
                x: 3,
                y: 4
            })
        );
        // Outside the video, where the platform has no point.
        assert_eq!(annotator.extend(DISPLAY, None), None);
        annotator.finish();
        assert_eq!(annotator.extend(DISPLAY, Some((5, 6))), None);
    }

    #[test]
    fn a_press_outside_the_video_starts_no_stroke() {
        let mut annotator = Annotator::default();
        annotator.toggle();
        assert_eq!(annotator.start(DISPLAY, None), None);
        assert_eq!(annotator.extend(DISPLAY, Some((3, 4))), None);
    }

    #[test]
    fn stopping_or_clearing_erases_the_drawing() {
        let mut annotator = Annotator::default();
        annotator.toggle();
        annotator.start(DISPLAY, Some((1, 2)));
        assert_eq!(annotation(annotator.clear()), Some(Annotation::Clear));
        assert_eq!(annotator.extend(DISPLAY, Some((3, 4))), None);
        assert!(annotator.enabled());
        assert_eq!(annotation(annotator.toggle()), Some(Annotation::Clear));
        assert!(!annotator.enabled());
    }

    #[test]
    fn disabling_sends_nothing() {
        let mut annotator = Annotator::default();
        annotator.toggle();
        annotator.start(DISPLAY, Some((1, 2)));
        annotator.disable();
        assert!(!annotator.enabled());
        assert_eq!(annotator.extend(DISPLAY, Some((3, 4))), None);
        assert_eq!(annotator.clear(), None);
    }
}
