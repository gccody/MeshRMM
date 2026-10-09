//! The viewer toolbar. The viewer draws it itself, so it looks the same on
//! every platform: this module decides which items it shows, where they go,
//! how they are colored, and the vector icons the platform code strokes and
//! fills. The platform code only paints, tracks the mouse, and shows the
//! menus described here natively.
//!
//! Lengths are in logical pixels (96-DPI pixels on Windows, points on
//! macOS), with the origin at the toolbar's top-left corner.

// Linux builds only the tests of the viewer's shared code.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

mod icons;
mod items;
mod menu;

use meshrmm_protocol::{ChromaMode, CredentialState, QualityPreset};

pub use icons::icon;
pub use items::items;
pub use menu::{Command, MenuEntry, commands, menu, restart_confirmation, toolbox_menu};

/// The toolbar's height.
pub const HEIGHT: f64 = 40.0;
/// The font size of item labels.
pub const FONT_SIZE: f64 = 12.5;
/// The corner radius of item backgrounds.
pub const RADIUS: f64 = 6.0;

const BUTTON_HEIGHT: f64 = 28.0;
const ICON: f64 = 18.0;
const PADDING: f64 = 7.0;
const LABEL_GAP: f64 = 5.0;
const CHEVRON: f64 = 8.0;
const CHEVRON_GAP: f64 = 2.0;
const ITEM_GAP: f64 = 2.0;
const GROUP_GAP: f64 = 13.0;
const EDGE: f64 = 8.0;
const CAPTION_WIDTH: f64 = 46.0;
const BADGE_RADIUS: f64 = 3.5;

/// An opaque sRGB color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8);

pub const BACKGROUND: Color = Color(0x1b, 0x1b, 0x1d);
pub const BORDER: Color = Color(0x2e, 0x2e, 0x32);
pub const SEPARATOR: Color = Color(0x3a, 0x3a, 0x3e);
const FOREGROUND: Color = Color(0xe8, 0xe8, 0xea);
const DISABLED: Color = Color(0x68, 0x68, 0x6c);
const HOVER: Color = Color(0x2f, 0x2f, 0x33);
const PRESSED: Color = Color(0x3c, 0x3c, 0x41);
const ACTIVE: Color = Color(0x1f, 0x3a, 0x5c);
const ACCENT: Color = Color(0x4d, 0xa3, 0xff);
const RED: Color = Color(0xff, 0x45, 0x3a);
const CLOSE_HOVER: Color = Color(0xc4, 0x2b, 0x1c);
const CLOSE_PRESSED: Color = Color(0x9e, 0x23, 0x17);
const WHITE: Color = Color(0xff, 0xff, 0xff);

/// What the viewer shows in the toolbar. The platform code fills it in
/// from its window and session state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The desktop sessions' labels, and the one shown.
    pub sessions: Vec<String>,
    pub session: usize,
    /// The shown session's display labels, the one shown, and the one the
    /// device's pointer is on while its local user controls the input.
    pub displays: Vec<String>,
    pub display: usize,
    pub pointer_display: Option<usize>,
    pub quality: QualityPreset,
    /// The color mode and whether 4:4:4 is available, where the viewer
    /// offers the choice.
    pub chroma: Option<(ChromaMode, bool)>,
    pub credentials: CredentialState,
    /// The technician's input is blocked.
    pub input_blocked: bool,
    /// The mouse draws on the device's screen, and whether the shown
    /// display can show a drawing.
    pub annotating: bool,
    pub annotation_available: bool,
    pub chat_available: bool,
    pub chat_unread: usize,
    /// `Some(safe_mode)` once the agent reports it can restart its computer,
    /// with whether Windows is in Safe Mode.
    pub power: Option<bool>,
    /// The agent runs on a Mac: no Ctrl+Alt+Del and no Safe Mode.
    pub device_is_mac: bool,
    /// The latest file transfer's progress or result.
    pub file_status: String,
    /// The session can use the technician's toolbox, whether a run or file
    /// is in progress, and the latest one's progress or result.
    pub toolbox_available: bool,
    pub toolbox_busy: bool,
    pub toolbox_status: String,
    pub recording: bool,
    pub diagnostics: bool,
    /// Whether the settings item opens a menu rather than a window.
    pub settings_menu: bool,
    /// `Some(maximized)` when the toolbar draws the window's caption buttons.
    pub caption: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    User,
    Display,
    Quality,
    Recording,
    Credentials,
    SecureAttention,
    Power,
    TypeClipboard,
    Annotate,
    Files,
    Toolbox,
    Chat,
    Diagnostics,
    Settings,
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    User,
    Display,
    Quality,
    Record,
    Key,
    Keyboard,
    Power,
    Clipboard,
    Pen,
    Folder,
    Toolbox,
    Chat,
    Pulse,
    Gear,
    Minimize,
    Maximize,
    Restore,
    Close,
    Chevron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Recording,
    Caption,
    CloseCaption,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Unread chat messages.
    Unread,
    /// Something is waiting for the technician, like a credential prompt.
    Attention,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub action: Action,
    pub icon: Icon,
    pub label: Option<String>,
    pub tooltip: String,
    /// Opens a menu, on mouse-down; other items act on mouse-up.
    pub menu: bool,
    /// Draws the menu arrow.
    pub chevron: bool,
    pub enabled: bool,
    /// A toggle that is on, or an operation in progress.
    pub active: bool,
    pub badge: Option<Badge>,
    pub tone: Tone,
    /// Items of one group sit together; groups are divided by a line.
    group: u8,
    trailing: bool,
}

impl Item {
    fn new(action: Action, icon: Icon, tooltip: impl Into<String>, group: u8) -> Self {
        Self {
            action,
            icon,
            label: None,
            tooltip: tooltip.into(),
            menu: false,
            chevron: false,
            enabled: true,
            active: false,
            badge: None,
            tone: Tone::Normal,
            group,
            trailing: true,
        }
    }

    fn with_menu(mut self) -> Self {
        self.menu = true;
        self.chevron = true;
        self
    }
}

pub fn quality_label(preset: QualityPreset) -> &'static str {
    match preset {
        QualityPreset::UltraDataSaver => "Ultra data saver",
        QualityPreset::DataSaver => "Data saver",
        QualityPreset::Balanced => "Balanced",
        QualityPreset::BestQuality => "Best quality",
    }
}

const QUALITY_PRESETS: [QualityPreset; 4] = [
    QualityPreset::UltraDataSaver,
    QualityPreset::DataSaver,
    QualityPreset::Balanced,
    QualityPreset::BestQuality,
];

/// A rectangle in toolbar coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }
}

/// Where the items and the lines between their groups go.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub rects: Vec<Rect>,
    /// Labels fitted to the available width; full text stays in item tooltips.
    pub labels: Vec<Option<String>>,
    /// The horizontal centers of the lines between groups.
    pub separators: Vec<f64>,
}

impl Layout {
    /// The item under a point, if any. The rest of the toolbar moves the
    /// window.
    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        self.rects.iter().position(|rect| rect.contains(x, y))
    }
}

fn is_caption(item: &Item) -> bool {
    matches!(item.tone, Tone::Caption | Tone::CloseCaption)
}

/// The width of an item whose label is `text_width` wide.
fn item_width(item: &Item, text_width: f64) -> f64 {
    if is_caption(item) {
        return CAPTION_WIDTH;
    }
    let mut width = 2.0 * PADDING + ICON;
    if item.label.is_some() {
        width += LABEL_GAP + text_width.ceil();
    }
    if item.chevron {
        width += CHEVRON_GAP + CHEVRON;
    }
    width
}

/// Places the items in a toolbar `width` wide. The first `leading_inset`
/// pixels are left free, for the macOS window buttons. `measure` returns a
/// label's width in the label font.
pub fn layout(
    items: &[Item],
    width: f64,
    leading_inset: f64,
    measure: &dyn Fn(&str) -> f64,
) -> Layout {
    let mut labels: Vec<_> = items.iter().map(|item| item.label.clone()).collect();
    let mut widths: Vec<f64> = items
        .iter()
        .map(|item| item_width(item, item.label.as_deref().map_or(0.0, measure)))
        .collect();
    let natural = layout_widths(items, width, leading_inset, &widths);
    let leading_end = items
        .iter()
        .zip(&natural.rects)
        .filter(|(item, _)| !item.trailing)
        .map(|(_, rect)| rect.right())
        .fold(leading_inset, f64::max);
    let trailing_start = items
        .iter()
        .zip(&natural.rects)
        .filter(|(item, _)| item.trailing)
        .map(|(_, rect)| rect.x)
        .fold(width, f64::min);
    let mut excess = (leading_end + GROUP_GAP - trailing_start).max(0.0);
    // Session and display names can be arbitrarily long. Preserve all the
    // controls, including REC, by shortening only the leading labels.
    for (index, item) in items.iter().enumerate().filter(|(_, item)| !item.trailing) {
        if excess <= 0.0 {
            break;
        }
        if let Some(label) = &labels[index] {
            let old_width = widths[index];
            let budget = (old_width - item_width(item, 0.0) - excess).max(0.0);
            let fitted = fit_label(label, budget, measure);
            widths[index] = item_width(item, measure(&fitted));
            excess = (excess - (old_width - widths[index])).max(0.0);
            labels[index] = Some(fitted);
        }
    }
    let mut layout = layout_widths(items, width, leading_inset, &widths);
    layout.labels = labels;
    layout
}

fn fit_label(text: &str, width: f64, measure: &dyn Fn(&str) -> f64) -> String {
    if measure(text).ceil() <= width {
        return text.to_owned();
    }
    if measure("…").ceil() > width {
        return String::new();
    }
    let mut fitted = text.to_owned();
    while fitted.pop().is_some() {
        let candidate = format!("{fitted}…");
        if measure(&candidate).ceil() <= width {
            return candidate;
        }
    }
    "…".into()
}

fn layout_widths(items: &[Item], width: f64, leading_inset: f64, widths: &[f64]) -> Layout {
    let top = ((HEIGHT - BUTTON_HEIGHT) / 2.0).round();
    let mut rects = vec![Rect::default(); items.len()];
    let mut separators = Vec::new();

    let mut x = leading_inset + EDGE;
    let mut previous_group = None;
    for (index, item) in items.iter().enumerate().filter(|(_, item)| !item.trailing) {
        if previous_group.is_some_and(|group| group != item.group) {
            separators.push(x - ITEM_GAP + GROUP_GAP / 2.0);
            x += GROUP_GAP - ITEM_GAP;
        }
        previous_group = Some(item.group);
        rects[index] = Rect::new(x, top, widths[index], BUTTON_HEIGHT);
        x += widths[index] + ITEM_GAP;
    }

    let mut x = width;
    let mut previous: Option<&Item> = None;
    for (index, item) in items
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, item)| item.trailing)
    {
        match previous {
            None if !is_caption(item) => x -= EDGE,
            Some(next) if is_caption(next) && !is_caption(item) => x -= EDGE,
            Some(next) if next.group != item.group => {
                x -= GROUP_GAP - ITEM_GAP;
                separators.push(x + GROUP_GAP / 2.0);
            }
            _ => {}
        }
        if is_caption(item) {
            x -= widths[index];
            rects[index] = Rect::new(x, 0.0, widths[index], HEIGHT);
        } else {
            x -= widths[index];
            rects[index] = Rect::new(x, top, widths[index], BUTTON_HEIGHT);
            x -= ITEM_GAP;
        }
        previous = Some(item);
    }
    Layout {
        rects,
        separators,
        labels: Vec::new(),
    }
}

/// The narrowest toolbar that fits `items` without overlap.
#[cfg(test)]
fn minimum_width(items: &[Item], leading_inset: f64, measure: &dyn Fn(&str) -> f64) -> f64 {
    let widths: Vec<_> = items
        .iter()
        .map(|item| item_width(item, item.label.as_deref().map_or(0.0, measure)))
        .collect();
    let layout = layout_widths(items, 0.0, leading_inset, &widths);
    let leading_end = items
        .iter()
        .zip(&layout.rects)
        .filter(|(item, _)| !item.trailing)
        .map(|(_, rect)| rect.right())
        .fold(leading_inset, f64::max);
    let trailing_start = items
        .iter()
        .zip(&layout.rects)
        .filter(|(item, _)| item.trailing)
        .map(|(_, rect)| rect.x)
        .fold(0.0, f64::min);
    (leading_end - trailing_start + GROUP_GAP).ceil()
}

/// Where an item's parts go inside its rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Parts {
    pub icon: Rect,
    pub label: Option<Rect>,
    pub chevron: Option<Rect>,
    /// The badge's center.
    pub badge: Option<(f64, f64)>,
}

pub fn parts(item: &Item, rect: Rect, text_width: f64) -> Parts {
    let icon_size = if is_caption(item) { 20.0 } else { ICON };
    let icon_x = if is_caption(item) {
        rect.x + (rect.width - icon_size) / 2.0
    } else {
        rect.x + PADDING
    };
    let icon = Rect::new(
        icon_x.round(),
        (rect.y + (rect.height - icon_size) / 2.0).round(),
        icon_size,
        icon_size,
    );
    let mut x = icon.right();
    let label = item.label.as_ref().map(|_| {
        let label = Rect::new(x + LABEL_GAP, rect.y, text_width.ceil(), rect.height);
        x = label.right();
        label
    });
    let chevron = item.chevron.then(|| {
        Rect::new(
            x + CHEVRON_GAP,
            (rect.y + (rect.height - CHEVRON) / 2.0).round(),
            CHEVRON,
            CHEVRON,
        )
    });
    let badge = item
        .badge
        .map(|_| (icon.right() - 1.0, icon.y + BADGE_RADIUS - 0.5));
    Parts {
        icon,
        label,
        chevron,
        badge,
    }
}

/// How to paint an item in its current mouse state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub background: Option<Color>,
    /// Caption buttons fill their whole rectangle.
    pub radius: f64,
    pub foreground: Color,
    /// The label, which recording draws in red like its dot.
    pub label: Color,
    pub badge: Option<Color>,
    pub badge_radius: f64,
    /// The ring around the badge, which separates it from the icon.
    pub badge_ring: Color,
}

pub fn style(item: &Item, hovered: bool, pressed: bool) -> Style {
    let caption = is_caption(item);
    let pressed = pressed && hovered;
    let background = match (item.enabled, item.tone) {
        (false, _) => None,
        (true, Tone::CloseCaption) if pressed => Some(CLOSE_PRESSED),
        (true, Tone::CloseCaption) if hovered => Some(CLOSE_HOVER),
        _ if pressed => Some(PRESSED),
        _ if hovered => Some(HOVER),
        _ if item.active => Some(ACTIVE),
        _ => None,
    };
    let foreground = if !item.enabled {
        DISABLED
    } else if item.tone == Tone::CloseCaption && hovered {
        WHITE
    } else if item.tone == Tone::Recording {
        RED
    } else if item.active {
        ACCENT
    } else {
        FOREGROUND
    };
    Style {
        background,
        radius: if caption { 0.0 } else { RADIUS },
        foreground,
        label: foreground,
        badge: item.badge.map(|badge| match badge {
            Badge::Unread => RED,
            Badge::Attention => ACCENT,
        }),
        badge_radius: BADGE_RADIUS,
        badge_ring: background.unwrap_or(BACKGROUND),
    }
}

/// A path segment in icon-grid or toolbar coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    Move(f64, f64),
    Line(f64, f64),
    /// Two control points and the end point.
    Cubic(f64, f64, f64, f64, f64, f64),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Paint {
    /// Round caps and joins, `width` grid units wide.
    Stroke(f64),
    Fill,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub segments: Vec<Segment>,
    pub paint: Paint,
}

/// The icon grid's size. Icons are drawn into a square this many units wide.
pub const GRID: f64 = 20.0;
const STROKE: f64 = 1.5;

fn stroke(segments: Vec<Segment>) -> Shape {
    Shape {
        segments,
        paint: Paint::Stroke(STROKE),
    }
}

fn fill(segments: Vec<Segment>) -> Shape {
    Shape {
        segments,
        paint: Paint::Fill,
    }
}

fn polyline(points: &[(f64, f64)]) -> Vec<Segment> {
    points
        .iter()
        .enumerate()
        .map(|(index, &(x, y))| {
            if index == 0 {
                Segment::Move(x, y)
            } else {
                Segment::Line(x, y)
            }
        })
        .collect()
}

fn polygon(points: &[(f64, f64)]) -> Vec<Segment> {
    let mut segments = polyline(points);
    segments.push(Segment::Close);
    segments
}

/// An arc from `start` to `end` degrees, clockwise on screen (y grows down).
fn arc(cx: f64, cy: f64, radius: f64, start: f64, end: f64) -> Vec<Segment> {
    let pieces = ((end - start).abs() / 90.0).ceil().max(1.0) as usize;
    let step = (end - start).to_radians() / pieces as f64;
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let point = |angle: f64| (cx + radius * angle.cos(), cy + radius * angle.sin());
    let mut angle = start.to_radians();
    let (x, y) = point(angle);
    let mut segments = vec![Segment::Move(x, y)];
    for _ in 0..pieces {
        let next = angle + step;
        let (x0, y0) = point(angle);
        let (x1, y1) = point(next);
        segments.push(Segment::Cubic(
            x0 - k * radius * angle.sin(),
            y0 + k * radius * angle.cos(),
            x1 + k * radius * next.sin(),
            y1 - k * radius * next.cos(),
            x1,
            y1,
        ));
        angle = next;
    }
    segments
}

pub fn circle(cx: f64, cy: f64, radius: f64) -> Vec<Segment> {
    let mut segments = arc(cx, cy, radius, 0.0, 360.0);
    segments.push(Segment::Close);
    segments
}

/// A rectangle with rounded corners, for icons and item backgrounds.
pub fn rounded_rect(rect: Rect, radius: f64) -> Vec<Segment> {
    let r = radius.min(rect.width / 2.0).min(rect.height / 2.0);
    let (left, top, right, bottom) = (rect.x, rect.y, rect.right(), rect.bottom());
    if r <= 0.0 {
        return polygon(&[(left, top), (right, top), (right, bottom), (left, bottom)]);
    }
    let corner = |cx: f64, cy: f64, start: f64| {
        arc(cx, cy, r, start, start + 90.0)
            .into_iter()
            .skip(1)
            .collect::<Vec<_>>()
    };
    let mut segments = vec![Segment::Move(left + r, top), Segment::Line(right - r, top)];
    segments.extend(corner(right - r, top + r, 270.0));
    segments.push(Segment::Line(right, bottom - r));
    segments.extend(corner(right - r, bottom - r, 0.0));
    segments.push(Segment::Line(left + r, bottom));
    segments.extend(corner(left + r, bottom - r, 90.0));
    segments.push(Segment::Line(left, top + r));
    segments.extend(corner(left + r, top + r, 180.0));
    segments.push(Segment::Close);
    segments
}

/// Maps icon-grid shapes into `rect`: the points, and the stroke width
/// scale.
pub fn place(shape: &Shape, rect: Rect) -> (Vec<Segment>, Paint) {
    let scale = rect.width / GRID;
    let x = |value: f64| rect.x + value * scale;
    let y = |value: f64| rect.y + value * scale;
    let segments = shape
        .segments
        .iter()
        .map(|segment| match *segment {
            Segment::Move(px, py) => Segment::Move(x(px), y(py)),
            Segment::Line(px, py) => Segment::Line(x(px), y(py)),
            Segment::Cubic(ax, ay, bx, by, px, py) => {
                Segment::Cubic(x(ax), y(ay), x(bx), y(by), x(px), y(py))
            }
            Segment::Close => Segment::Close,
        })
        .collect();
    let paint = match shape.paint {
        Paint::Stroke(width) => Paint::Stroke(width * scale),
        Paint::Fill => Paint::Fill,
    };
    (segments, paint)
}

#[cfg(test)]
mod tests;
