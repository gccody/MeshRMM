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

use meshrmm_protocol::{ChromaMode, CredentialState, QualityPreset};

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
    pub chat_available: bool,
    pub chat_unread: usize,
    /// The latest file transfer's progress or result.
    pub file_status: String,
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
    TypeClipboard,
    Files,
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
    Clipboard,
    Folder,
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

/// The toolbar's items, from the left.
pub fn items(state: &State) -> Vec<Item> {
    let mut items = Vec::with_capacity(16);
    let session = state
        .sessions
        .get(state.session)
        .cloned()
        .unwrap_or_default();
    let mut user = Item::new(
        Action::User,
        Icon::User,
        format!("User session: {session}"),
        0,
    );
    user.trailing = false;
    user.menu = state.sessions.len() > 1;
    user.chevron = user.menu;
    user.enabled = user.menu;
    if state.sessions.len() > 1 {
        user.label = Some(session);
    }
    items.push(user);

    let display = state
        .displays
        .get(state.display)
        .cloned()
        .unwrap_or_default();
    let mut monitor = Item::new(
        Action::Display,
        Icon::Display,
        format!("Monitor: {display}"),
        0,
    );
    monitor.trailing = false;
    monitor.label = Some(short_display_label(&display, state.display));
    monitor.menu = state.displays.len() > 1;
    monitor.chevron = monitor.menu;
    monitor.enabled = monitor.menu;
    items.push(monitor);

    let mut quality_tip = format!("Quality: {}", quality_label(state.quality));
    if let Some((ChromaMode::Yuv444, _)) = state.chroma {
        quality_tip.push_str(", 4:4:4 color");
    }
    let mut quality = Item::new(Action::Quality, Icon::Quality, quality_tip, 0).with_menu();
    quality.trailing = false;
    items.push(quality);

    if state.recording {
        let mut recording = Item::new(
            Action::Recording,
            Icon::Record,
            "Recording to Downloads. Click to stop and save.",
            1,
        );
        recording.label = Some("REC".into());
        recording.tone = Tone::Recording;
        items.push(recording);
    }

    let credentials = &state.credentials;
    let mut key = Item::new(
        Action::Credentials,
        Icon::Key,
        if credentials.message.is_empty() {
            "Credentials".to_owned()
        } else {
            format!("Credentials: {}", credentials.message)
        },
        2,
    )
    .with_menu();
    key.enabled = !state.input_blocked;
    key.active = credentials.prompt_active;
    key.badge = credentials.can_autofill.then_some(Badge::Attention);
    items.push(key);
    let mut secure_attention = Item::new(
        Action::SecureAttention,
        Icon::Keyboard,
        "Send Ctrl+Alt+Del",
        2,
    );
    secure_attention.enabled = !state.input_blocked;
    items.push(secure_attention);
    let mut type_clipboard = Item::new(
        Action::TypeClipboard,
        Icon::Clipboard,
        "Type clipboard text into the remote computer",
        2,
    );
    type_clipboard.enabled = !state.input_blocked;
    items.push(type_clipboard);

    items.push(
        Item::new(
            Action::Files,
            Icon::Folder,
            if state.file_status.is_empty() {
                "Send or receive files".to_owned()
            } else {
                format!("Files: {}", state.file_status)
            },
            3,
        )
        .with_menu(),
    );
    let mut chat = Item::new(
        Action::Chat,
        Icon::Chat,
        if !state.chat_available {
            "Chat is unavailable until the agent connects".to_owned()
        } else if state.chat_unread > 0 {
            format!(
                "Chat: {} unread {}",
                state.chat_unread,
                if state.chat_unread == 1 {
                    "message"
                } else {
                    "messages"
                }
            )
        } else {
            "Chat".to_owned()
        },
        3,
    );
    chat.enabled = state.chat_available;
    chat.badge = (state.chat_unread > 0).then_some(Badge::Unread);
    items.push(chat);

    let mut diagnostics = Item::new(Action::Diagnostics, Icon::Pulse, "Diagnostics", 4);
    diagnostics.active = state.diagnostics;
    items.push(diagnostics);
    let mut settings = Item::new(
        Action::Settings,
        Icon::Gear,
        if state.settings_menu {
            "Session controls"
        } else {
            "Settings"
        },
        4,
    );
    settings.menu = state.settings_menu;
    items.push(settings);

    if let Some(maximized) = state.caption {
        for (action, icon, tooltip, tone) in [
            (Action::Minimize, Icon::Minimize, "Minimize", Tone::Caption),
            (
                Action::Maximize,
                if maximized {
                    Icon::Restore
                } else {
                    Icon::Maximize
                },
                if maximized { "Restore" } else { "Maximize" },
                Tone::Caption,
            ),
            (Action::Close, Icon::Close, "Close", Tone::CloseCaption),
        ] {
            let mut caption = Item::new(action, icon, tooltip, 5);
            caption.tone = tone;
            items.push(caption);
        }
    }
    items
}

/// "1" for "Display 1", and the first word of other labels.
fn short_display_label(label: &str, index: usize) -> String {
    match label.strip_prefix("Display ") {
        Some(_) => (index + 1).to_string(),
        None => label
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned(),
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
    let widths: Vec<f64> = items
        .iter()
        .map(|item| item_width(item, item.label.as_deref().map_or(0.0, measure)))
        .collect();
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
    Layout { rects, separators }
}

/// The narrowest toolbar that fits `items` without overlap.
#[cfg(test)]
fn minimum_width(items: &[Item], leading_inset: f64, measure: &dyn Fn(&str) -> f64) -> f64 {
    let layout = layout(items, 0.0, leading_inset, measure);
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

fn grid_rounded_rect(x: f64, y: f64, width: f64, height: f64, radius: f64) -> Vec<Segment> {
    rounded_rect(Rect::new(x, y, width, height), radius)
}

/// The icon's shapes on a [`GRID`]-unit square grid.
pub fn icon(icon: Icon) -> Vec<Shape> {
    match icon {
        Icon::User => vec![
            stroke(circle(10.0, 6.5, 3.2)),
            stroke(vec![
                Segment::Move(3.5, 17.5),
                Segment::Cubic(3.5, 13.9, 6.4, 11.5, 10.0, 11.5),
                Segment::Cubic(13.6, 11.5, 16.5, 13.9, 16.5, 17.5),
            ]),
        ],
        Icon::Display => vec![
            stroke(grid_rounded_rect(2.5, 3.5, 15.0, 10.5, 1.8)),
            stroke(polyline(&[(10.0, 14.0), (10.0, 16.5)])),
            stroke(polyline(&[(6.5, 16.5), (13.5, 16.5)])),
        ],
        Icon::Quality => vec![
            stroke(arc(10.0, 12.5, 7.0, 150.0, 390.0)),
            stroke(polyline(&[(10.0, 12.5), (13.6, 8.4)])),
            fill(circle(10.0, 12.5, 1.4)),
        ],
        Icon::Record => vec![fill(circle(10.0, 10.0, 4.5))],
        Icon::Key => vec![
            stroke(circle(6.5, 13.5, 3.5)),
            stroke(polyline(&[(9.0, 11.0), (16.5, 3.5)])),
            stroke(polyline(&[(14.0, 6.0), (15.8, 7.8)])),
            stroke(polyline(&[(11.9, 8.1), (13.4, 9.6)])),
        ],
        Icon::Keyboard => {
            let mut shapes = vec![
                stroke(grid_rounded_rect(2.0, 5.0, 16.0, 10.0, 2.0)),
                stroke(polyline(&[(6.5, 11.8), (13.5, 11.8)])),
            ];
            for x in [5.5, 8.5, 11.5, 14.5] {
                shapes.push(fill(circle(x, 8.4, 0.9)));
            }
            shapes
        }
        Icon::Clipboard => vec![
            stroke(vec![
                Segment::Move(7.5, 4.0),
                Segment::Line(6.3, 4.0),
                Segment::Cubic(5.3, 4.0, 4.5, 4.8, 4.5, 5.8),
                Segment::Line(4.5, 15.7),
                Segment::Cubic(4.5, 16.7, 5.3, 17.5, 6.3, 17.5),
                Segment::Line(13.7, 17.5),
                Segment::Cubic(14.7, 17.5, 15.5, 16.7, 15.5, 15.7),
                Segment::Line(15.5, 5.8),
                Segment::Cubic(15.5, 4.8, 14.7, 4.0, 13.7, 4.0),
                Segment::Line(12.5, 4.0),
            ]),
            stroke(grid_rounded_rect(7.5, 2.5, 5.0, 3.0, 1.0)),
            stroke(polyline(&[(7.5, 9.5), (12.5, 9.5)])),
            stroke(polyline(&[(7.5, 12.8), (10.8, 12.8)])),
        ],
        Icon::Folder => vec![stroke(vec![
            Segment::Move(2.5, 6.0),
            Segment::Cubic(2.5, 5.2, 3.2, 4.5, 4.0, 4.5),
            Segment::Line(7.6, 4.5),
            Segment::Line(9.4, 6.5),
            Segment::Line(16.0, 6.5),
            Segment::Cubic(16.8, 6.5, 17.5, 7.2, 17.5, 8.0),
            Segment::Line(17.5, 14.5),
            Segment::Cubic(17.5, 15.3, 16.8, 16.0, 16.0, 16.0),
            Segment::Line(4.0, 16.0),
            Segment::Cubic(3.2, 16.0, 2.5, 15.3, 2.5, 14.5),
            Segment::Close,
        ])],
        Icon::Chat => vec![stroke(vec![
            Segment::Move(4.5, 3.5),
            Segment::Line(15.5, 3.5),
            Segment::Cubic(16.6, 3.5, 17.5, 4.4, 17.5, 5.5),
            Segment::Line(17.5, 11.5),
            Segment::Cubic(17.5, 12.6, 16.6, 13.5, 15.5, 13.5),
            Segment::Line(10.5, 13.5),
            Segment::Line(6.5, 16.8),
            Segment::Line(6.5, 13.5),
            Segment::Line(4.5, 13.5),
            Segment::Cubic(3.4, 13.5, 2.5, 12.6, 2.5, 11.5),
            Segment::Line(2.5, 5.5),
            Segment::Cubic(2.5, 4.4, 3.4, 3.5, 4.5, 3.5),
            Segment::Close,
        ])],
        Icon::Pulse => vec![stroke(polyline(&[
            (2.0, 10.5),
            (5.5, 10.5),
            (7.5, 5.0),
            (11.0, 15.5),
            (13.0, 10.5),
            (18.0, 10.5),
        ]))],
        Icon::Gear => {
            let mut outline = Vec::with_capacity(32);
            for tooth in 0..8 {
                let center = f64::from(tooth) * 45.0;
                for (radius, offset) in [(6.3, -13.0), (8.4, -8.0), (8.4, 8.0), (6.3, 13.0)] {
                    let angle = (center + offset).to_radians();
                    outline.push((10.0 + radius * angle.cos(), 10.0 + radius * angle.sin()));
                }
            }
            vec![stroke(polygon(&outline)), stroke(circle(10.0, 10.0, 2.6))]
        }
        Icon::Minimize => vec![caption(polyline(&[(5.0, 10.0), (15.0, 10.0)]))],
        Icon::Maximize => vec![caption(polygon(&[
            (5.5, 5.5),
            (14.5, 5.5),
            (14.5, 14.5),
            (5.5, 14.5),
        ]))],
        Icon::Restore => vec![
            caption(polygon(&[
                (5.5, 7.5),
                (12.5, 7.5),
                (12.5, 14.5),
                (5.5, 14.5),
            ])),
            caption(polyline(&[
                (7.5, 7.5),
                (7.5, 5.5),
                (14.5, 5.5),
                (14.5, 12.5),
                (12.5, 12.5),
            ])),
        ],
        Icon::Close => vec![
            caption(polyline(&[(5.5, 5.5), (14.5, 14.5)])),
            caption(polyline(&[(14.5, 5.5), (5.5, 14.5)])),
        ],
        Icon::Chevron => vec![Shape {
            segments: polyline(&[(4.5, 7.5), (10.0, 13.0), (15.5, 7.5)]),
            paint: Paint::Stroke(3.0),
        }],
    }
}

/// Caption glyphs are thin, like the system's.
fn caption(segments: Vec<Segment>) -> Shape {
    Shape {
        segments,
        paint: Paint::Stroke(1.0),
    }
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

/// What a menu item does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Session(usize),
    Display(usize),
    Quality(QualityPreset),
    Chroma(ChromaMode),
    PromptCredentials,
    AutofillCredentials,
    ForgetCredentials,
    SendFiles,
    ReceiveFiles,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MenuEntry {
    Item {
        label: String,
        checked: bool,
        enabled: bool,
        /// `None` for status lines.
        command: Option<Command>,
    },
    Separator,
}

fn entry(label: impl Into<String>, checked: bool, enabled: bool, command: Command) -> MenuEntry {
    MenuEntry::Item {
        label: label.into(),
        checked,
        enabled,
        command: Some(command),
    }
}

fn status(label: impl Into<String>) -> MenuEntry {
    MenuEntry::Item {
        label: label.into(),
        checked: false,
        enabled: false,
        command: None,
    }
}

/// The menu an item opens. The settings menu is the platform's own.
pub fn menu(action: Action, state: &State) -> Vec<MenuEntry> {
    match action {
        Action::User => state
            .sessions
            .iter()
            .enumerate()
            .map(|(index, label)| {
                entry(
                    label.clone(),
                    index == state.session,
                    true,
                    Command::Session(index),
                )
            })
            .collect(),
        Action::Display => state
            .displays
            .iter()
            .enumerate()
            .map(|(index, label)| {
                let label = if state.pointer_display == Some(index) {
                    format!("➤ {label}")
                } else {
                    label.clone()
                };
                entry(label, index == state.display, true, Command::Display(index))
            })
            .collect(),
        Action::Quality => {
            let mut entries: Vec<MenuEntry> = QUALITY_PRESETS
                .into_iter()
                .map(|preset| {
                    entry(
                        quality_label(preset),
                        preset == state.quality,
                        true,
                        Command::Quality(preset),
                    )
                })
                .collect();
            if let Some((chroma, crisp_available)) = state.chroma {
                entries.push(MenuEntry::Separator);
                entries.push(entry(
                    "4:2:0 efficient color",
                    chroma == ChromaMode::Yuv420,
                    true,
                    Command::Chroma(ChromaMode::Yuv420),
                ));
                entries.push(entry(
                    "4:4:4 crisp color",
                    chroma == ChromaMode::Yuv444,
                    crisp_available,
                    Command::Chroma(ChromaMode::Yuv444),
                ));
            }
            entries
        }
        Action::Credentials => {
            let credentials = &state.credentials;
            let allowed = !state.input_blocked;
            let mut entries = Vec::new();
            if !credentials.message.is_empty() {
                entries.push(status(credentials.message.clone()));
                entries.push(MenuEntry::Separator);
            }
            if credentials.can_autofill {
                entries.push(entry(
                    "Autofill saved credentials",
                    false,
                    allowed,
                    Command::AutofillCredentials,
                ));
            }
            entries.push(entry(
                "Prompt for credentials",
                false,
                allowed && credentials.available && !credentials.prompt_active,
                Command::PromptCredentials,
            ));
            entries.push(entry(
                "Forget saved credentials",
                false,
                allowed && credentials.saved && !credentials.prompt_active,
                Command::ForgetCredentials,
            ));
            entries
        }
        Action::Files => {
            let mut entries = vec![
                entry("Send files…", false, true, Command::SendFiles),
                entry("Receive files…", false, true, Command::ReceiveFiles),
            ];
            if !state.file_status.is_empty() {
                entries.push(MenuEntry::Separator);
                entries.push(status(state.file_status.clone()));
            }
            entries
        }
        _ => Vec::new(),
    }
}

/// The commands of `entries`, in order, for platforms that number menu
/// items. Status lines and separators have none.
pub fn commands(entries: &[MenuEntry]) -> Vec<Option<Command>> {
    entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Item { command, .. } => *command,
            MenuEntry::Separator => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a 12.5-pixel UI font.
    fn measure(text: &str) -> f64 {
        text.chars().count() as f64 * 7.0
    }

    fn state() -> State {
        State {
            sessions: vec!["Console".into(), "probe (RDP 2)".into()],
            session: 0,
            displays: vec!["Display 1".into(), "Display 2".into()],
            display: 1,
            pointer_display: Some(0),
            chroma: Some((ChromaMode::Yuv420, true)),
            chat_available: true,
            caption: Some(false),
            ..State::default()
        }
    }

    fn find(items: &[Item], action: Action) -> Option<&Item> {
        items.iter().find(|item| item.action == action)
    }

    #[test]
    fn items_are_icons_with_short_labels() {
        let items = items(&state());
        let labels: Vec<_> = items
            .iter()
            .filter_map(|item| item.label.as_deref())
            .collect();
        assert_eq!(labels, ["Console", "2"]);
        assert!(items.iter().all(|item| !item.tooltip.is_empty()));
        let single = items_for(|state| {
            state.sessions.truncate(1);
            state.displays.truncate(1);
            state.display = 0;
        });
        let user = find(&single, Action::User).unwrap();
        assert_eq!(user.label, None);
        assert!(!user.enabled && !user.chevron);
        assert_eq!(user.tooltip, "User session: Console");
        let display = find(&single, Action::Display).unwrap();
        assert_eq!(display.label.as_deref(), Some("1"));
        assert!(!display.enabled);
    }

    fn items_for(change: impl FnOnce(&mut State)) -> Vec<Item> {
        let mut state = state();
        change(&mut state);
        items(&state)
    }

    #[test]
    fn items_follow_the_session_state() {
        let items = items_for(|state| {
            state.recording = true;
            state.diagnostics = true;
            state.chat_unread = 2;
            state.credentials.can_autofill = true;
            state.credentials.message = "Waiting for the remote user…".into();
            state.input_blocked = true;
        });
        let recording = find(&items, Action::Recording).unwrap();
        assert_eq!(recording.label.as_deref(), Some("REC"));
        assert_eq!(style(recording, false, false).foreground, RED);
        assert!(find(&items, Action::Diagnostics).unwrap().active);
        let chat = find(&items, Action::Chat).unwrap();
        assert_eq!(chat.badge, Some(Badge::Unread));
        assert_eq!(chat.tooltip, "Chat: 2 unread messages");
        let key = find(&items, Action::Credentials).unwrap();
        assert_eq!(key.badge, Some(Badge::Attention));
        assert_eq!(key.tooltip, "Credentials: Waiting for the remote user…");
        for action in [
            Action::Credentials,
            Action::SecureAttention,
            Action::TypeClipboard,
        ] {
            assert!(!find(&items, action).unwrap().enabled, "{action:?}");
        }
        assert!(find(&items_for(|_| {}), Action::Recording).is_none());
        let maximized = items_for(|state| state.caption = Some(true));
        assert_eq!(
            find(&maximized, Action::Maximize).unwrap().icon,
            Icon::Restore
        );
        assert!(find(&items_for(|state| state.caption = None), Action::Close).is_none());
    }

    #[test]
    fn layout_places_groups_without_overlap() {
        let items = items_for(|state| state.recording = true);
        let layout = layout(&items, 1000.0, 0.0, &measure);
        assert_eq!(layout.rects.len(), items.len());
        let mut sorted: Vec<_> = layout.rects.clone();
        sorted.sort_by(|a, b| a.x.total_cmp(&b.x));
        for pair in sorted.windows(2) {
            assert!(pair[0].right() <= pair[1].x, "{pair:?}");
        }
        // The caption buttons fill the top-right corner.
        let close = layout.rects[items.len() - 1];
        assert_eq!(
            (close.right(), close.y, close.height),
            (1000.0, 0.0, HEIGHT)
        );
        // One line between each pair of trailing groups before the captions.
        assert_eq!(layout.separators.len(), 3);
        // Each line is centered in the gap between two items.
        for x in &layout.separators {
            let left = sorted
                .iter()
                .filter(|rect| rect.right() <= *x)
                .map(|rect| rect.right());
            let right = sorted.iter().filter(|rect| rect.x >= *x).map(|rect| rect.x);
            let (left, right) = (
                left.fold(f64::MIN, f64::max),
                right.fold(f64::MAX, f64::min),
            );
            assert!(
                ((x - left) - (right - x)).abs() < 0.01,
                "{left} {x} {right}"
            );
        }
        for rect in &layout.rects {
            assert!(rect.y >= 0.0 && rect.bottom() <= HEIGHT);
        }
        assert_eq!(layout.hit(close.x + 1.0, 1.0), Some(items.len() - 1));
        // The space between the two sides moves the window.
        assert_eq!(layout.hit(500.0, 20.0), None);
    }

    #[test]
    fn layout_leaves_room_for_the_macos_window_buttons() {
        let items = items_for(|state| state.caption = None);
        let layout = layout(&items, 900.0, 84.0, &measure);
        assert!(layout.rects[0].x >= 84.0);
        assert!(layout.rects.last().unwrap().right() <= 900.0 - EDGE);
    }

    #[test]
    fn minimum_width_fits_every_item() {
        let items = items_for(|state| state.recording = true);
        let width = minimum_width(&items, 0.0, &measure);
        assert!(width < 760.0, "{width}");
        let layout = layout(&items, width, 0.0, &measure);
        let mut sorted: Vec<_> = layout.rects.clone();
        sorted.sort_by(|a, b| a.x.total_cmp(&b.x));
        for pair in sorted.windows(2) {
            assert!(pair[0].right() <= pair[1].x, "{pair:?}");
        }
    }

    #[test]
    fn parts_fit_inside_their_item() {
        let items = items(&state());
        let layout = layout(&items, 1000.0, 0.0, &measure);
        for (item, rect) in items.iter().zip(&layout.rects) {
            let text = item.label.as_deref().map_or(0.0, measure);
            let parts = parts(item, *rect, text);
            for part in [Some(parts.icon), parts.label, parts.chevron]
                .into_iter()
                .flatten()
            {
                assert!(
                    part.x >= rect.x && part.right() <= rect.right() + 0.01,
                    "{item:?}"
                );
            }
        }
    }

    #[test]
    fn styles_show_hover_press_and_disabled_states() {
        let items = items(&state());
        let chat = find(&items, Action::Chat).unwrap();
        assert_eq!(style(chat, false, false).background, None);
        assert_eq!(style(chat, true, false).background, Some(HOVER));
        assert_eq!(style(chat, true, true).background, Some(PRESSED));
        // Pressed, then dragged off the item.
        assert_eq!(style(chat, false, true).background, None);
        let close = find(&items, Action::Close).unwrap();
        assert_eq!(style(close, true, false).background, Some(CLOSE_HOVER));
        assert_eq!(style(close, true, false).foreground, WHITE);
        assert_eq!(style(close, true, false).radius, 0.0);
        let user = find(&items_for(|state| state.sessions.truncate(1)), Action::User)
            .unwrap()
            .clone();
        assert_eq!(style(&user, true, true).background, None);
        assert_eq!(style(&user, true, true).foreground, DISABLED);
    }

    #[test]
    fn icons_stay_inside_the_grid() {
        let all = [
            Icon::User,
            Icon::Display,
            Icon::Quality,
            Icon::Record,
            Icon::Key,
            Icon::Keyboard,
            Icon::Clipboard,
            Icon::Folder,
            Icon::Chat,
            Icon::Pulse,
            Icon::Gear,
            Icon::Minimize,
            Icon::Maximize,
            Icon::Restore,
            Icon::Close,
            Icon::Chevron,
        ];
        for icon in all {
            let shapes = super::icon(icon);
            assert!(!shapes.is_empty());
            for shape in shapes {
                assert!(matches!(shape.segments.first(), Some(Segment::Move(..))));
                for segment in shape.segments {
                    let points: Vec<(f64, f64)> = match segment {
                        Segment::Move(x, y) | Segment::Line(x, y) => vec![(x, y)],
                        Segment::Cubic(ax, ay, bx, by, x, y) => vec![(ax, ay), (bx, by), (x, y)],
                        Segment::Close => vec![],
                    };
                    for (x, y) in points {
                        assert!(
                            (0.5..=19.5).contains(&x) && (0.5..=19.5).contains(&y),
                            "{icon:?}: ({x}, {y})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn circles_are_round() {
        let segments = circle(10.0, 10.0, 5.0);
        assert_eq!(segments.len(), 6);
        let Segment::Cubic(_, _, _, _, x, y) = segments[1] else {
            panic!("{segments:?}");
        };
        assert!(
            (x - 10.0).abs() < 1e-9 && (y - 15.0).abs() < 1e-9,
            "({x}, {y})"
        );
        let (placed, paint) = place(&stroke(segments), Rect::new(100.0, 10.0, 40.0, 40.0));
        assert_eq!(placed[0], Segment::Move(130.0, 30.0));
        assert_eq!(paint, Paint::Stroke(3.0));
    }

    #[test]
    fn menus_list_choices_and_status() {
        let state = State {
            credentials: CredentialState {
                available: true,
                saved: true,
                prompt_active: false,
                can_autofill: true,
                message: "Saved".into(),
            },
            file_status: "Sent 2 files".into(),
            quality: QualityPreset::BestQuality,
            ..state()
        };
        let labels = |entries: Vec<MenuEntry>| -> Vec<(String, bool, bool)> {
            entries
                .into_iter()
                .map(|entry| match entry {
                    MenuEntry::Item {
                        label,
                        checked,
                        enabled,
                        ..
                    } => (label, checked, enabled),
                    MenuEntry::Separator => ("-".into(), false, false),
                })
                .collect()
        };
        assert_eq!(
            labels(menu(Action::Display, &state)),
            [
                ("➤ Display 1".into(), false, true),
                ("Display 2".into(), true, true)
            ]
        );
        let quality = menu(Action::Quality, &state);
        assert_eq!(quality.len(), 7);
        assert_eq!(
            commands(&quality)[3],
            Some(Command::Quality(QualityPreset::BestQuality))
        );
        assert_eq!(labels(quality)[3], ("Best quality".into(), true, true));
        assert!(menu(Action::Quality, &State::default()).len() == 4);
        let credentials = menu(Action::Credentials, &state);
        assert_eq!(
            commands(&credentials),
            [
                None,
                None,
                Some(Command::AutofillCredentials),
                Some(Command::PromptCredentials),
                Some(Command::ForgetCredentials)
            ]
        );
        let blocked = State {
            input_blocked: true,
            ..state.clone()
        };
        assert!(
            labels(menu(Action::Credentials, &blocked))
                .iter()
                .all(|(_, _, enabled)| !enabled)
        );
        assert_eq!(
            labels(menu(Action::Files, &state)).last().unwrap().0,
            "Sent 2 files"
        );
        assert!(menu(Action::Settings, &state).is_empty());
    }
}
