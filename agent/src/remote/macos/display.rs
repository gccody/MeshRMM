//! The Mac's active displays. Positions and sizes are in the global display
//! coordinate space Quartz events use: points, with the origin at the top
//! left of the main display. Video is captured in pixels.
use anyhow::{Context, bail};
use meshrmm_protocol::{DesktopSession, Display, DisplayId};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin,
    CGDisplayIsMain, CGDisplayMode, CGError, CGGetActiveDisplayList,
};

const MAX_DISPLAYS: usize = 32;

pub(crate) fn enumerate() -> anyhow::Result<Vec<Display>> {
    let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS];
    let mut count = 0;
    // SAFETY: the list holds MAX_DISPLAYS entries and `count` is a valid out pointer.
    let error =
        unsafe { CGGetActiveDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if error != CGError::Success {
        bail!("Quartz could not list the active displays ({})", error.0);
    }
    let mut external = 0;
    let displays = ids[..count as usize]
        .iter()
        .map(|&id| {
            let bounds = CGDisplayBounds(id);
            let name = if CGDisplayIsBuiltin(id) {
                "Built-in Display".to_owned()
            } else {
                external += 1;
                format!("Display {external}")
            };
            Display {
                session: DesktopSession::Console,
                id: DisplayId(id),
                name,
                x: bounds.origin.x.round() as i32,
                y: bounds.origin.y.round() as i32,
                width: bounds.size.width.round() as u32,
                height: bounds.size.height.round() as u32,
                primary: CGDisplayIsMain(id),
            }
        })
        .collect::<Vec<_>>();
    if displays.is_empty() {
        bail!("macOS reported no active displays");
    }
    Ok(displays)
}

pub(crate) fn choose(
    displays: &[Display],
    requested: Option<DisplayId>,
) -> anyhow::Result<Display> {
    requested
        .and_then(|id| displays.iter().find(|display| display.id == id))
        .or_else(|| displays.iter().find(|display| display.primary))
        .or_else(|| displays.first())
        .cloned()
        .context("macOS reported no active displays")
}

/// The display's size in pixels, which is larger than its size in points on
/// Retina displays.
pub(crate) fn pixel_size(display: &Display) -> (u32, u32) {
    let mode = CGDisplayCopyDisplayMode(display.id.0);
    let width = CGDisplayMode::pixel_width(mode.as_deref()) as u32;
    let height = CGDisplayMode::pixel_height(mode.as_deref()) as u32;
    if width == 0 || height == 0 {
        (display.width, display.height)
    } else {
        (width, height)
    }
}

/// The display under a point in global coordinates.
pub(crate) fn at(displays: &[Display], x: f64, y: f64) -> Option<DisplayId> {
    displays
        .iter()
        .find(|display| {
            x >= f64::from(display.x)
                && y >= f64::from(display.y)
                && x < f64::from(display.x) + f64::from(display.width)
                && y < f64::from(display.y) + f64::from(display.height)
        })
        .map(|display| display.id)
}
