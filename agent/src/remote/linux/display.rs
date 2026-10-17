//! Displays as the viewer sees them. Positions and sizes are in the desktop's
//! logical layout, the coordinates the display server places windows and the
//! pointer in, so they match the input device's mapping even with scaling.
use anyhow::Context;
use meshrmm_protocol::{DesktopSession, Display, DisplayId};

/// A display a capture backend found, with the output name it captures by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Output {
    /// The connector, such as `HDMI-1` or `Virtual-1`.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

impl Output {
    /// The display's ID, the same for as long as the connector keeps its name.
    pub(crate) fn id(&self) -> DisplayId {
        DisplayId(stable_id(&self.name))
    }

    pub(crate) fn display(&self) -> Display {
        Display {
            session: DesktopSession::Console,
            id: self.id(),
            name: self.name.clone(),
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            primary: self.primary,
        }
    }
}

/// FNV-1a of the connector name, which never collides with the background
/// desktop's reserved ID in practice and stays the same across helpers.
fn stable_id(name: &str) -> u32 {
    let hash = name.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    // IDs 0 and the background desktop's are reserved.
    match hash {
        0 => 1,
        hash if hash == meshrmm_protocol::BACKGROUND_DISPLAY_ID.0 => hash ^ 1,
        hash => hash,
    }
}

/// The requested display, or the primary one, or the first.
pub(crate) fn choose(outputs: &[Output], requested: Option<DisplayId>) -> anyhow::Result<Output> {
    requested
        .and_then(|id| outputs.iter().find(|output| output.id() == id))
        .or_else(|| outputs.iter().find(|output| output.primary))
        .or_else(|| outputs.first())
        .cloned()
        .context("the display server reported no displays")
}

/// The smallest rectangle around every display: left, top, width, height.
pub(crate) fn bounds(displays: &[Display]) -> Option<(i32, i32, u32, u32)> {
    let left = displays.iter().map(|display| display.x).min()?;
    let top = displays.iter().map(|display| display.y).min()?;
    let right = displays
        .iter()
        .map(|display| display.x + display.width as i32)
        .max()?;
    let bottom = displays
        .iter()
        .map(|display| display.y + display.height as i32)
        .max()?;
    (right > left && bottom > top)
        .then(|| (left, top, (right - left) as u32, (bottom - top) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, x: i32, y: i32, primary: bool) -> Output {
        Output {
            name: name.to_owned(),
            x,
            y,
            width: 1920,
            height: 1080,
            primary,
        }
    }

    #[test]
    fn ids_follow_the_connector() {
        let first = output("HDMI-1", 0, 0, true);
        let moved = output("HDMI-1", 1920, 0, false);
        assert_eq!(first.id(), moved.id());
        assert_ne!(first.id(), output("DP-1", 0, 0, true).id());
        assert_ne!(first.id(), meshrmm_protocol::BACKGROUND_DISPLAY_ID);
    }

    #[test]
    fn chooses_the_requested_then_the_primary_display() {
        let outputs = [output("DP-1", 0, 0, false), output("DP-2", 1920, 0, true)];
        assert_eq!(choose(&outputs, None).unwrap().name, "DP-2");
        assert_eq!(
            choose(&outputs, Some(outputs[0].id())).unwrap().name,
            "DP-1"
        );
        assert_eq!(choose(&outputs, Some(DisplayId(7))).unwrap().name, "DP-2");
        assert!(choose(&[], None).is_err());
    }

    #[test]
    fn bounds_cover_displays_left_of_and_above_the_origin() {
        let displays = [
            output("A", -1920, 0, false).display(),
            output("B", 0, -200, true).display(),
        ];
        assert_eq!(bounds(&displays), Some((-1920, -200, 3840, 1280)));
        assert_eq!(bounds(&[]), None);
    }
}
