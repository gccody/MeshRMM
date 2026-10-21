//! The pointer's shape, named for the viewer. X11 names the cursor itself;
//! Wayland compositors send only its image, which is matched against the
//! images of the session's Xcursor theme, the theme the compositor draws
//! its cursors from.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use meshrmm_protocol::CursorShape;

/// The Xcursor names each shape goes by in cursor themes, the CSS name and
/// the X11 font name first.
const NAMES: &[(CursorShape, &[&str])] = &[
    (
        CursorShape::Default,
        &["default", "left_ptr", "arrow", "top_left_arrow"],
    ),
    (
        CursorShape::Text,
        &["text", "xterm", "ibeam", "vertical-text"],
    ),
    (CursorShape::Wait, &["wait", "watch"]),
    (
        CursorShape::Progress,
        &["progress", "left_ptr_watch", "half-busy"],
    ),
    (
        CursorShape::Crosshair,
        &["crosshair", "cross", "tcross", "cell"],
    ),
    (
        CursorShape::Pointer,
        &["pointer", "hand2", "hand", "hand1", "pointing_hand"],
    ),
    (CursorShape::Help, &["help", "question_arrow", "whats_this"]),
    (
        CursorShape::Move,
        &["move", "fleur", "all-scroll", "grabbing", "size_all"],
    ),
    (
        CursorShape::NotAllowed,
        &[
            "not-allowed",
            "crossed_circle",
            "no-drop",
            "forbidden",
            "circle",
        ],
    ),
    (
        CursorShape::ResizeWestEast,
        &[
            "ew-resize",
            "sb_h_double_arrow",
            "h_double_arrow",
            "col-resize",
            "size_hor",
            "left_side",
            "right_side",
            "e-resize",
            "w-resize",
        ],
    ),
    (
        CursorShape::ResizeNorthSouth,
        &[
            "ns-resize",
            "sb_v_double_arrow",
            "v_double_arrow",
            "row-resize",
            "size_ver",
            "top_side",
            "bottom_side",
            "n-resize",
            "s-resize",
        ],
    ),
    (
        CursorShape::ResizeNorthWestSouthEast,
        &[
            "nwse-resize",
            "size_fdiag",
            "top_left_corner",
            "bottom_right_corner",
            "nw-resize",
            "se-resize",
        ],
    ),
    (
        CursorShape::ResizeNorthEastSouthWest,
        &[
            "nesw-resize",
            "size_bdiag",
            "top_right_corner",
            "bottom_left_corner",
            "ne-resize",
            "sw-resize",
        ],
    ),
    (
        CursorShape::UpArrow,
        &["sb_up_arrow", "up_arrow", "center_ptr"],
    ),
];

/// The shape of the cursor a theme names `name`.
pub(crate) fn shape_named(name: &str) -> CursorShape {
    NAMES
        .iter()
        .find(|(_, names)| names.contains(&name))
        .map_or(CursorShape::Default, |(shape, _)| *shape)
}

/// The largest mean difference, in alpha levels, between a cursor image and
/// a theme image of its size that still counts as a match.
const SAME_SIZE_TOLERANCE: u32 = 6;
/// The same, for a theme image scaled to the cursor's size.
const SCALED_TOLERANCE: u32 = 20;

/// One image of an Xcursor file: its size and the alpha of each pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mask {
    width: usize,
    height: usize,
    alpha: Vec<u8>,
}

/// Names cursor images by the theme cursor they match, remembering the
/// answer for each image.
#[derive(Default)]
pub(crate) struct ThemeShapes {
    /// Each shape's images in the session's theme, loaded on first use.
    theme: Option<Vec<(CursorShape, Vec<Mask>)>>,
    known: HashMap<(usize, usize, u64), CursorShape>,
}

impl ThemeShapes {
    /// The shape of a cursor image of `width` by `height` whose pixels have
    /// the given alpha values, or the default arrow when no theme cursor
    /// matches it.
    pub(crate) fn classify(&mut self, width: usize, height: usize, alpha: &[u8]) -> CursorShape {
        let key = (width, height, fingerprint(alpha));
        if let Some(shape) = self.known.get(&key) {
            return *shape;
        }
        let theme = self.theme.get_or_insert_with(|| {
            let theme = load_theme(&theme_name());
            tracing::info!(
                shapes = theme.len(),
                "loaded the cursor theme to name Wayland cursors"
            );
            theme
        });
        let shape = best_match(theme, width, height, alpha).unwrap_or(CursorShape::Default);
        self.known.insert(key, shape);
        shape
    }
}

fn fingerprint(alpha: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    alpha.hash(&mut hasher);
    hasher.finish()
}

/// The shape whose theme image is closest to the cursor, within tolerance.
fn best_match(
    theme: &[(CursorShape, Vec<Mask>)],
    width: usize,
    height: usize,
    alpha: &[u8],
) -> Option<CursorShape> {
    if width == 0 || height == 0 || alpha.len() != width * height {
        return None;
    }
    let mut best: Option<(u32, CursorShape)> = None;
    for (shape, masks) in theme {
        let same_size: Vec<&Mask> = masks
            .iter()
            .filter(|mask| (mask.width, mask.height) == (width, height))
            .collect();
        let difference = if same_size.is_empty() {
            // The nearest size, scaled to the cursor's.
            masks
                .iter()
                .min_by_key(|mask| mask.width.abs_diff(width) + mask.height.abs_diff(height))
                .map(|mask| difference(&scale(mask, width, height), alpha))
                .filter(|difference| *difference <= SCALED_TOLERANCE)
        } else {
            same_size
                .iter()
                .map(|mask| difference(&mask.alpha, alpha))
                .min()
                .filter(|difference| *difference <= SAME_SIZE_TOLERANCE)
        };
        if let Some(difference) = difference
            && best.is_none_or(|(least, _)| difference < least)
        {
            best = Some((difference, *shape));
        }
    }
    best.map(|(_, shape)| shape)
}

/// The mean absolute difference of two alpha masks of one size.
fn difference(first: &[u8], second: &[u8]) -> u32 {
    let total: u64 = first
        .iter()
        .zip(second)
        .map(|(a, b)| u64::from(a.abs_diff(*b)))
        .sum();
    (total / first.len().max(1) as u64) as u32
}

/// `mask` resized to `width` by `height`, nearest pixel.
fn scale(mask: &Mask, width: usize, height: usize) -> Vec<u8> {
    let mut scaled = Vec::with_capacity(width * height);
    for y in 0..height {
        let source_y = y * mask.height / height;
        for x in 0..width {
            scaled.push(mask.alpha[source_y * mask.width + x * mask.width / width]);
        }
    }
    scaled
}

/// The session's cursor theme: `XCURSOR_THEME`, or the desktop's setting,
/// or the system default.
fn theme_name() -> String {
    if let Ok(theme) = std::env::var("XCURSOR_THEME")
        && !theme.is_empty()
    {
        return theme;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if desktop.split(':').any(|name| name == "KDE") {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")));
        if let Some(theme) = config
            .and_then(|config| std::fs::read_to_string(config.join("kcminputrc")).ok())
            .and_then(|text| ini_value(&text, "Mouse", "cursorTheme"))
        {
            return theme;
        }
        return "breeze_cursors".to_owned();
    }
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "cursor-theme"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .trim_matches('\'')
                .to_owned()
        })
        .filter(|theme| !theme.is_empty())
        .unwrap_or_else(|| "default".to_owned())
}

/// The value of `key` in `section` of an INI-style file.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut current = "";
    for line in text.lines().map(str::trim) {
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            current = name;
        } else if current == section
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// Where Xcursor themes are looked for, as libXcursor does.
fn theme_directories() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("XCURSOR_PATH") {
        return std::env::split_paths(&path).collect();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut directories = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(data) => directories.push(PathBuf::from(data).join("icons")),
        None => directories.extend(home.as_ref().map(|home| home.join(".local/share/icons"))),
    }
    directories.extend(home.as_ref().map(|home| home.join(".icons")));
    let data = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|data| !data.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    directories.extend(data.split(':').map(|data| Path::new(data).join("icons")));
    directories.push(PathBuf::from("/usr/share/pixmaps"));
    directories
}

/// Each shape's images in `theme` and the themes it inherits.
fn load_theme(theme: &str) -> Vec<(CursorShape, Vec<Mask>)> {
    let directories = theme_directories();
    NAMES
        .iter()
        .filter_map(|(shape, names)| {
            let masks: Vec<Mask> = names
                .iter()
                .find_map(|name| find_cursor(&directories, theme, name, 0))
                .and_then(|path| std::fs::read(path).ok())
                .map(|bytes| parse_xcursor(&bytes))
                .unwrap_or_default();
            (!masks.is_empty()).then_some((*shape, masks))
        })
        .collect()
}

/// The file of cursor `name` in `theme`, or in a theme it inherits.
fn find_cursor(directories: &[PathBuf], theme: &str, name: &str, depth: u32) -> Option<PathBuf> {
    if depth > 8 {
        return None;
    }
    for directory in directories {
        let path = directory.join(theme).join("cursors").join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    directories
        .iter()
        .filter_map(|directory| {
            std::fs::read_to_string(directory.join(theme).join("index.theme")).ok()
        })
        .filter_map(|text| ini_value(&text, "Icon Theme", "Inherits"))
        .flat_map(|inherits| {
            inherits
                .split([',', ';'])
                .map(|parent| parent.trim().to_owned())
                .filter(|parent| !parent.is_empty() && parent != theme)
                .collect::<Vec<_>>()
        })
        .find_map(|parent| find_cursor(directories, &parent, name, depth + 1))
}

/// The images of an Xcursor file, each frame of an animated one included.
fn parse_xcursor(bytes: &[u8]) -> Vec<Mask> {
    const IMAGE: u32 = 0xfffd_0002;
    let word = |offset: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(offset..offset + 4)?.try_into().ok()?,
        ))
    };
    let mut masks = Vec::new();
    if bytes.get(..4) != Some(b"Xcur") {
        return masks;
    }
    let (Some(header), Some(entries)) = (word(4), word(12)) else {
        return masks;
    };
    for entry in 0..entries.min(1024) as usize {
        let at = header as usize + entry * 12;
        let (Some(kind), Some(position)) = (word(at), word(at + 8)) else {
            break;
        };
        let position = position as usize;
        if kind != IMAGE || word(position + 4) != Some(IMAGE) {
            continue;
        }
        let (Some(chunk), Some(width), Some(height)) =
            (word(position), word(position + 16), word(position + 20))
        else {
            continue;
        };
        let (width, height) = (width as usize, height as usize);
        if width == 0 || height == 0 || width > 1024 || height > 1024 {
            continue;
        }
        let pixels = position + chunk as usize;
        let Some(data) = bytes.get(pixels..pixels + width * height * 4) else {
            continue;
        };
        // Pixels are little-endian ARGB: alpha is the last byte of each.
        let alpha = data.chunks_exact(4).map(|pixel| pixel[3]).collect();
        masks.push(Mask {
            width,
            height,
            alpha,
        });
    }
    masks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_cursor_themes_map_to_shapes() {
        assert_eq!(shape_named("xterm"), CursorShape::Text);
        assert_eq!(shape_named("hand2"), CursorShape::Pointer);
        assert_eq!(
            shape_named("sb_h_double_arrow"),
            CursorShape::ResizeWestEast
        );
        assert_eq!(shape_named("left_ptr"), CursorShape::Default);
        assert_eq!(shape_named(""), CursorShape::Default);
    }

    /// An Xcursor file with one image per mask, at nominal size `width`.
    fn xcursor(masks: &[(usize, usize, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = b"Xcur".to_vec();
        for word in [16_u32, 0x1_0000, masks.len() as u32] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        let mut position = 16 + 12 * masks.len();
        let mut chunks = Vec::new();
        for (width, height, alpha) in masks {
            for word in [0xfffd_0002_u32, *width as u32, position as u32] {
                bytes.extend_from_slice(&word.to_le_bytes());
            }
            let mut chunk = Vec::new();
            for word in [
                36_u32,
                0xfffd_0002,
                *width as u32,
                1,
                *width as u32,
                *height as u32,
                0,
                0,
                0,
            ] {
                chunk.extend_from_slice(&word.to_le_bytes());
            }
            for &value in alpha {
                chunk.extend_from_slice(&[0, 0, 0, value]);
            }
            position += chunk.len();
            chunks.extend(chunk);
        }
        bytes.extend(chunks);
        bytes
    }

    fn mask(width: usize, height: usize, inside: impl Fn(usize, usize) -> bool) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| if inside(x, y) { 255 } else { 0 })
            .collect()
    }

    #[test]
    fn reads_every_image_of_an_xcursor_file() {
        let bar = mask(4, 4, |x, _| x == 1);
        let file = xcursor(&[(4, 4, bar.clone()), (2, 2, vec![255, 0, 0, 255])]);
        let masks = parse_xcursor(&file);
        assert_eq!(masks.len(), 2);
        assert_eq!(
            masks[0],
            Mask {
                width: 4,
                height: 4,
                alpha: bar
            }
        );
        assert_eq!((masks[1].width, masks[1].height), (2, 2));
        assert!(parse_xcursor(b"Xcur").is_empty());
        assert!(parse_xcursor(b"nope").is_empty());
    }

    #[test]
    fn cursor_images_are_named_by_the_closest_theme_image() {
        let text = mask(24, 24, |x, y| (11..13).contains(&x) && (3..21).contains(&y));
        let arrow = mask(24, 24, |x, y| x <= y && y < 18);
        let resize = mask(24, 24, |x, y| (11..13).contains(&y) && (2..22).contains(&x));
        let theme = vec![
            (
                CursorShape::Default,
                vec![Mask {
                    width: 24,
                    height: 24,
                    alpha: arrow.clone(),
                }],
            ),
            (
                CursorShape::Text,
                vec![Mask {
                    width: 24,
                    height: 24,
                    alpha: text.clone(),
                }],
            ),
            (
                CursorShape::ResizeWestEast,
                vec![Mask {
                    width: 48,
                    height: 48,
                    alpha: scale(
                        &Mask {
                            width: 24,
                            height: 24,
                            alpha: resize.clone(),
                        },
                        48,
                        48,
                    ),
                }],
            ),
        ];
        assert_eq!(best_match(&theme, 24, 24, &text), Some(CursorShape::Text));
        assert_eq!(
            best_match(&theme, 24, 24, &arrow),
            Some(CursorShape::Default)
        );
        // Only a larger image of this one is in the theme.
        assert_eq!(
            best_match(&theme, 24, 24, &resize),
            Some(CursorShape::ResizeWestEast)
        );
        // Nothing like a filled square.
        assert_eq!(best_match(&theme, 24, 24, &[255; 24 * 24]), None);
        assert_eq!(best_match(&theme, 0, 0, &[]), None);
    }

    #[test]
    fn reads_ini_values_from_their_section() {
        let text = "[General]\ncursorTheme=wrong\n[Mouse]\ncursorTheme = Breeze_Light\n";
        assert_eq!(
            ini_value(text, "Mouse", "cursorTheme").as_deref(),
            Some("Breeze_Light")
        );
        assert_eq!(ini_value(text, "Mouse", "cursorSize"), None);
    }
}
