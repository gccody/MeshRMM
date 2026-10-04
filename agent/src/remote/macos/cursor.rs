//! The shape of the system cursor, for viewers that draw their own pointer
//! while the technician controls the Mac. macOS names no cursors across
//! applications, so the current one is matched against the standard AppKit
//! cursors by image; any other cursor is reported as the default arrow.
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use meshrmm_protocol::CursorShape;
use objc2::rc::Retained;
use objc2_app_kit::NSCursor;

thread_local! {
    /// Standard cursors by image fingerprint, and the last cursor seen.
    static CURSORS: RefCell<Option<Cursors>> = const { RefCell::new(None) };
}

struct Cursors {
    standard: Vec<(u64, CursorShape)>,
    last: Option<(Retained<NSCursor>, CursorShape)>,
}

fn fingerprint(cursor: &NSCursor) -> u64 {
    let mut hasher = DefaultHasher::new();
    let hot_spot = cursor.hotSpot();
    (hot_spot.x.to_bits(), hot_spot.y.to_bits()).hash(&mut hasher);
    if let Some(data) = cursor.image().TIFFRepresentation() {
        data.to_vec().hash(&mut hasher);
    }
    hasher.finish()
}

/// The shape of the cursor macOS shows now.
pub(crate) fn current() -> CursorShape {
    super::on_main(|_| {
        CURSORS.with(|cursors| {
            let mut cursors = cursors.borrow_mut();
            // Applications still show the deprecated resize cursors.
            #[allow(deprecated)]
            let cursors = cursors.get_or_insert_with(|| Cursors {
                standard: [
                    (NSCursor::IBeamCursor(), CursorShape::Text),
                    (NSCursor::IBeamCursorForVerticalLayout(), CursorShape::Text),
                    (NSCursor::pointingHandCursor(), CursorShape::Pointer),
                    (NSCursor::crosshairCursor(), CursorShape::Crosshair),
                    (NSCursor::openHandCursor(), CursorShape::Move),
                    (NSCursor::closedHandCursor(), CursorShape::Move),
                    (
                        NSCursor::resizeLeftRightCursor(),
                        CursorShape::ResizeWestEast,
                    ),
                    (NSCursor::columnResizeCursor(), CursorShape::ResizeWestEast),
                    (
                        NSCursor::resizeUpDownCursor(),
                        CursorShape::ResizeNorthSouth,
                    ),
                    (NSCursor::rowResizeCursor(), CursorShape::ResizeNorthSouth),
                    (
                        NSCursor::operationNotAllowedCursor(),
                        CursorShape::NotAllowed,
                    ),
                ]
                .iter()
                .map(|(cursor, shape)| (fingerprint(cursor), *shape))
                .collect(),
                last: None,
            });
            #[allow(deprecated)]
            let Some(cursor) = NSCursor::currentSystemCursor() else {
                return CursorShape::Default;
            };
            if let Some((last, shape)) = &cursors.last
                && Retained::as_ptr(last) == Retained::as_ptr(&cursor)
            {
                return *shape;
            }
            let print = fingerprint(&cursor);
            let shape = cursors
                .standard
                .iter()
                .find(|(standard, _)| *standard == print)
                .map_or(CursorShape::Default, |(_, shape)| *shape);
            cursors.last = Some((cursor, shape));
            shape
        })
    })
}
