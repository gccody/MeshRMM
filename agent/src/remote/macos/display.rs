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
/// The name of the display that shows every display at once, as Windows
/// names it.
pub(crate) const ALL_DISPLAYS: &str = "All monitors";
/// All displays' picture is scaled to fit this, which every encoder and
/// viewer handles.
const LARGEST_PICTURE: (f64, f64) = (3840.0, 2160.0);

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
    let mut displays = ids[..count as usize]
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
    if let Some(all) = all_displays(&displays) {
        displays.push(all);
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
            display.id != meshrmm_protocol::ALL_DISPLAYS_ID
                && x >= f64::from(display.x)
                && y >= f64::from(display.y)
                && x < f64::from(display.x) + f64::from(display.width)
                && y < f64::from(display.y) + f64::from(display.height)
        })
        .map(|display| display.id)
}

/// A display covering every display, for viewing them all at once, when
/// there are several: the bounding box of their positions.
fn all_displays(displays: &[Display]) -> Option<Display> {
    if displays.len() < 2 {
        return None;
    }
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
    Some(Display {
        session: DesktopSession::Console,
        id: meshrmm_protocol::ALL_DISPLAYS_ID,
        name: ALL_DISPLAYS.to_owned(),
        x: left,
        y: top,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
        primary: false,
    })
}

/// The real displays a capture of `display` shows: each of them for All
/// displays, otherwise `display` itself.
pub(crate) fn members(display: &Display, displays: &[Display]) -> Vec<Display> {
    if display.id == meshrmm_protocol::ALL_DISPLAYS_ID {
        displays
            .iter()
            .filter(|member| member.id != meshrmm_protocol::ALL_DISPLAYS_ID)
            .cloned()
            .collect()
    } else {
        vec![display.clone()]
    }
}

/// Where each display sits in All displays' picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub width: usize,
    pub height: usize,
    /// Each member's left, top, width and height in the picture, in pixels.
    pub rects: Vec<(usize, usize, usize, usize)>,
}

/// Lays out `members` in one picture at the finest of their `scales` (pixels
/// per point), scaled down to fit [`LARGEST_PICTURE`]. Every edge is even,
/// as 4:2:0 needs.
pub(crate) fn layout(members: &[Display], scales: &[f64]) -> Option<Layout> {
    let all = all_displays(members)?;
    let finest = scales.iter().copied().fold(1.0_f64, f64::max);
    let scale = finest
        .min(LARGEST_PICTURE.0 / f64::from(all.width))
        .min(LARGEST_PICTURE.1 / f64::from(all.height));
    let even = |value: f64| (value.round() as usize) & !1;
    // A size is at least one 4:2:0 block.
    let size = |value: f64| even(value).max(2);
    let (width, height) = (
        size(f64::from(all.width) * scale),
        size(f64::from(all.height) * scale),
    );
    let rects = members
        .iter()
        .map(|member| {
            let left = even(f64::from(member.x - all.x) * scale).min(width - 2);
            let top = even(f64::from(member.y - all.y) * scale).min(height - 2);
            (
                left,
                top,
                size(f64::from(member.width) * scale).min(width - left),
                size(f64::from(member.height) * scale).min(height - top),
            )
        })
        .collect();
    Some(Layout {
        width,
        height,
        rects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, x: i32, y: i32, width: u32, height: u32) -> Display {
        Display {
            session: DesktopSession::Console,
            id: DisplayId(id),
            name: format!("Display {id}"),
            x,
            y,
            width,
            height,
            primary: id == 1,
        }
    }

    #[test]
    fn several_displays_add_one_that_covers_them_all() {
        assert!(all_displays(&[display(1, 0, 0, 1512, 982)]).is_none());
        let displays = [
            display(1, 0, 0, 1512, 982),
            display(2, 1512, -300, 2560, 1440),
        ];
        let all = all_displays(&displays).unwrap();
        assert_eq!(all.id, meshrmm_protocol::ALL_DISPLAYS_ID);
        assert_eq!((all.x, all.y, all.width, all.height), (0, -300, 4072, 1440));
        assert_eq!(all.selection_label(2), "All displays");
        let mut with_all = displays.to_vec();
        with_all.push(all.clone());
        assert_eq!(members(&all, &with_all), displays);
        assert_eq!(members(&displays[1], &with_all), [displays[1].clone()]);
        // The pointer is on a real display, not on All displays.
        assert_eq!(at(&with_all, 1600.0, 0.0), Some(DisplayId(2)));
    }

    #[test]
    fn all_displays_is_laid_out_at_the_finest_scale_that_fits() {
        // A Retina laptop beside a 1440p monitor: too wide for twice the points.
        let displays = [
            display(1, 0, 0, 1512, 982),
            display(2, 1512, -300, 2560, 1440),
        ];
        let layout = layout(&displays, &[2.0, 1.0]).unwrap();
        assert_eq!((layout.width, layout.height), (3840, 1358));
        assert_eq!(layout.rects[0], (0, 282, 1426, 926));
        assert_eq!(layout.rects[1], (1426, 0, 2414, 1358));
        // Two small displays keep the Retina scale.
        let small = [display(1, 0, 0, 800, 600), display(2, 800, 0, 800, 600)];
        let layout = super::layout(&small, &[2.0, 2.0]).unwrap();
        assert_eq!((layout.width, layout.height), (3200, 1200));
        assert_eq!(layout.rects, [(0, 0, 1600, 1200), (1600, 0, 1600, 1200)]);
    }
}
