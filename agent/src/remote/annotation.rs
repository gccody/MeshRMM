//! The technician's drawing over the viewed display. Unlike the display
//! border, it is not excluded from capture: the device's user and the
//! technician see the same strokes. The overlay is a click-through,
//! color-keyed window that exists only while there is a drawing.
use std::collections::VecDeque;

/// The most points kept; the oldest strokes go first.
const MAX_POINTS: usize = 32_768;

/// Maps a normalized point to pixels from the display's top-left corner.
fn display_pixel(width: u32, height: u32, x: u16, y: u16) -> (i32, i32) {
    let scale = |value: u16, extent: u32| {
        (i64::from(value) * i64::from(extent.saturating_sub(1)) / 65_535) as i32
    };
    (scale(x, width), scale(y, height))
}

/// Which part of the overlay a change repaints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dirty {
    /// Left, top, right and bottom, in overlay pixels.
    Rect(i32, i32, i32, i32),
    All,
}

/// Strokes in overlay pixels.
#[derive(Debug)]
struct Strokes {
    strokes: VecDeque<Vec<(i32, i32)>>,
    points: usize,
    pen_width: i32,
}

impl Strokes {
    fn new(pen_width: i32) -> Self {
        Self {
            strokes: VecDeque::new(),
            points: 0,
            pen_width,
        }
    }

    /// Adds a point, to a new stroke or the latest one, and returns what to
    /// repaint.
    fn add(&mut self, point: (i32, i32), start: bool) -> Option<Dirty> {
        let from = match self.strokes.back_mut() {
            Some(stroke) if !start => {
                let last = *stroke.last()?;
                if last == point {
                    return None;
                }
                stroke.push(point);
                last
            }
            _ => {
                self.strokes.push_back(vec![point]);
                point
            }
        };
        self.points += 1;
        let mut trimmed = false;
        while self.points > MAX_POINTS && self.strokes.len() > 1 {
            if let Some(oldest) = self.strokes.pop_front() {
                self.points -= oldest.len();
                trimmed = true;
            }
        }
        if trimmed {
            return Some(Dirty::All);
        }
        // Round caps and joins reach half the width past the points.
        let margin = self.pen_width / 2 + 2;
        Some(Dirty::Rect(
            from.0.min(point.0) - margin,
            from.1.min(point.1) - margin,
            from.0.max(point.0) + margin + 1,
            from.1.max(point.1) + margin + 1,
        ))
    }
}

#[cfg(windows)]
pub use overlay::AnnotationOverlay;

#[cfg(windows)]
mod overlay {
    use super::*;
    use meshrmm_protocol::Display;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use windows::{
        Win32::{
            Foundation::*,
            Graphics::Gdi::*,
            System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
            UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
        },
        core::w,
    };

    /// The transparent color: the overlay shows only its strokes.
    const KEY: COLORREF = COLORREF(0x00ff_00ff);
    /// The display border's red.
    const INK: COLORREF = COLORREF(0x0035_35e5);
    /// The stroke width at 96 DPI.
    const PEN_WIDTH: i32 = 5;

    thread_local! {
        /// The strokes the overlay on this thread paints.
        static PAINTED: RefCell<Option<Arc<Mutex<Strokes>>>> = const { RefCell::new(None) };
    }

    pub struct AnnotationOverlay {
        thread_id: u32,
        window: usize,
        width: u32,
        height: u32,
        strokes: Arc<Mutex<Strokes>>,
        thread: Option<JoinHandle<()>>,
    }

    impl AnnotationOverlay {
        /// Shows an empty overlay over `display`.
        pub fn show(display: &Display) -> anyhow::Result<Self> {
            let (x, y) = (display.x, display.y);
            let width = display.width.clamp(1, i32::MAX as u32);
            let height = display.height.clamp(1, i32::MAX as u32);
            let strokes = Arc::new(Mutex::new(Strokes::new(PEN_WIDTH)));
            let painted = Arc::clone(&strokes);
            let (ready, started) = mpsc::sync_channel(1);
            let thread = thread::Builder::new()
                .name("annotation-overlay".into())
                .spawn(move || unsafe {
                    PAINTED.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&painted)));
                    match create_window(x, y, width as i32, height as i32) {
                        Ok(window) => {
                            let dpi = match GetDpiForWindow(window) {
                                0 => 96,
                                dpi => dpi as i32,
                            };
                            painted
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .pen_width = (PEN_WIDTH * dpi / 96).max(PEN_WIDTH);
                            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
                            if ready
                                .send(Ok((GetCurrentThreadId(), window.0 as usize)))
                                .is_ok()
                            {
                                let mut message = MSG::default();
                                while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                                    let _ = TranslateMessage(&message);
                                    DispatchMessageW(&message);
                                }
                            }
                            let _ = DestroyWindow(window);
                        }
                        Err(error) => {
                            let _ = ready.send(Err(error));
                        }
                    }
                })?;
            match started.recv() {
                Ok(Ok((thread_id, window))) => Ok(Self {
                    thread_id,
                    window,
                    width,
                    height,
                    strokes,
                    thread: Some(thread),
                }),
                result => {
                    let _ = thread.join();
                    anyhow::bail!("Could not show the annotation overlay: {result:?}")
                }
            }
        }

        fn hwnd(&self) -> HWND {
            HWND(self.window as *mut _)
        }

        /// Adds a normalized point, to a new stroke or the latest one.
        pub fn draw(&self, x: u16, y: u16, start: bool) {
            let point = display_pixel(self.width, self.height, x, y);
            let dirty = self
                .strokes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .add(point, start);
            unsafe {
                if start {
                    // Stay above windows that became topmost since.
                    let _ = SetWindowPos(
                        self.hwnd(),
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
                match dirty {
                    Some(Dirty::Rect(left, top, right, bottom)) => {
                        let rect = RECT {
                            left,
                            top,
                            right,
                            bottom,
                        };
                        let _ = InvalidateRect(Some(self.hwnd()), Some(&rect), false);
                    }
                    Some(Dirty::All) => {
                        let _ = InvalidateRect(Some(self.hwnd()), None, false);
                    }
                    None => {}
                }
            }
        }

        #[cfg(test)]
        fn stroke_count(&self) -> usize {
            self.strokes.lock().unwrap().strokes.len()
        }
    }

    impl Drop for AnnotationOverlay {
        fn drop(&mut self) {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    unsafe fn create_window(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> windows::core::Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("MeshRMMAnnotationOverlay");
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            });
            let window = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT
                    | WS_EX_LAYERED,
                class,
                w!("MeshRMM annotations"),
                WS_POPUP,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            if let Err(error) = SetLayeredWindowAttributes(window, KEY, 255, LWA_COLORKEY) {
                let _ = DestroyWindow(window);
                return Err(error);
            }
            Ok(window)
        }
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
                WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
                // WM_PAINT covers every pixel it repaints.
                WM_ERASEBKGND => LRESULT(1),
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut paint);
                    PAINTED.with(|slot| {
                        if let Some(strokes) = slot.borrow().as_ref() {
                            let strokes = strokes.lock().unwrap_or_else(|error| error.into_inner());
                            paint_strokes(dc, paint.rcPaint, &strokes);
                        }
                    });
                    let _ = EndPaint(hwnd, &paint);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
    }

    /// Paints `area` off screen, so a repaint never shows the key color
    /// without its strokes.
    unsafe fn paint_strokes(dc: HDC, area: RECT, strokes: &Strokes) {
        let (width, height) = (area.right - area.left, area.bottom - area.top);
        if width <= 0 || height <= 0 {
            return;
        }
        unsafe {
            let memory = CreateCompatibleDC(Some(dc));
            let bitmap = CreateCompatibleBitmap(dc, width, height);
            let previous_bitmap = SelectObject(memory, bitmap.into());
            let _ = SetViewportOrgEx(memory, -area.left, -area.top, None);
            let background = CreateSolidBrush(KEY);
            FillRect(memory, &area, background);
            let _ = DeleteObject(background.into());
            let brush = LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: INK,
                lbHatch: 0,
            };
            let pen = ExtCreatePen(
                PEN_STYLE(PS_GEOMETRIC.0 | PS_SOLID.0 | PS_ENDCAP_ROUND.0 | PS_JOIN_ROUND.0),
                strokes.pen_width as u32,
                &brush,
                None,
            );
            let dot = CreateSolidBrush(INK);
            let previous_pen = SelectObject(memory, pen.into());
            let previous_brush = SelectObject(memory, dot.into());
            let radius = (strokes.pen_width / 2).max(1);
            for stroke in &strokes.strokes {
                if let [(x, y)] = stroke.as_slice() {
                    // A line has no length to cap: draw a click as a dot.
                    let _ = Ellipse(memory, x - radius, y - radius, x + radius, y + radius);
                } else {
                    let points = stroke
                        .iter()
                        .map(|&(x, y)| POINT { x, y })
                        .collect::<Vec<_>>();
                    let _ = Polyline(memory, &points);
                }
            }
            SelectObject(memory, previous_pen);
            SelectObject(memory, previous_brush);
            let _ = DeleteObject(pen.into());
            let _ = DeleteObject(dot.into());
            let _ = BitBlt(
                dc,
                area.left,
                area.top,
                width,
                height,
                Some(memory),
                area.left,
                area.top,
                SRCCOPY,
            );
            SelectObject(memory, previous_bitmap);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(memory);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[ignore = "requires an interactive Windows desktop with DWM"]
        fn overlay_is_click_through_captured_and_destroyed_on_drop() {
            let display = Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: meshrmm_protocol::DisplayId(1),
                name: "Test".into(),
                x: 0,
                y: 0,
                width: 400,
                height: 300,
                primary: true,
            };
            let overlay = AnnotationOverlay::show(&display).unwrap();
            let hwnd = overlay.hwnd();
            unsafe {
                let mut affinity = 0;
                GetWindowDisplayAffinity(hwnd, &mut affinity).unwrap();
                assert_eq!(affinity, WDA_NONE.0);
                let style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
                assert_ne!(style & WS_EX_TRANSPARENT.0, 0);
                assert_ne!(style & WS_EX_NOACTIVATE.0, 0);
                assert_ne!(style & WS_EX_TOPMOST.0, 0);
                let mut rect = RECT::default();
                GetWindowRect(hwnd, &mut rect).unwrap();
                assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (400, 300));
            }
            overlay.draw(0, 0, true);
            overlay.draw(65_535, 65_535, false);
            overlay.draw(100, 100, true);
            assert_eq!(overlay.stroke_count(), 2);
            drop(overlay);
            assert!(!unsafe { IsWindow(Some(hwnd)).as_bool() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_map_to_display_pixels() {
        assert_eq!(display_pixel(1920, 1080, 0, 0), (0, 0));
        assert_eq!(display_pixel(1920, 1080, 65_535, 65_535), (1919, 1079));
        assert_eq!(display_pixel(1920, 1080, 32_768, 32_768), (959, 539));
        assert_eq!(display_pixel(0, 0, 65_535, 65_535), (0, 0));
    }

    #[test]
    fn strokes_repaint_only_the_new_segment() {
        let mut strokes = Strokes::new(6);
        assert_eq!(
            strokes.add((10, 20), true),
            Some(Dirty::Rect(5, 15, 16, 26))
        );
        assert_eq!(strokes.add((30, 5), false), Some(Dirty::Rect(5, 0, 36, 26)));
        // The pointer did not move.
        assert_eq!(strokes.add((30, 5), false), None);
        assert_eq!(strokes.strokes.len(), 1);
        strokes.add((50, 50), true);
        assert_eq!(strokes.strokes.len(), 2);
        assert_eq!(strokes.points, 3);
    }

    #[test]
    fn extending_without_a_stroke_starts_one() {
        let mut strokes = Strokes::new(6);
        assert!(strokes.add((1, 1), false).is_some());
        assert_eq!(strokes.strokes.len(), 1);
    }

    #[test]
    fn the_oldest_strokes_are_dropped_past_the_limit() {
        let mut strokes = Strokes::new(6);
        for index in 0..MAX_POINTS as i32 {
            strokes.add((index, 0), index == 0);
        }
        assert_eq!(strokes.points, MAX_POINTS);
        // The first stroke goes, even though it is the only earlier one.
        assert_eq!(strokes.add((0, 1), true), Some(Dirty::All));
        assert_eq!(strokes.strokes.len(), 1);
        assert_eq!(strokes.points, 1);
        assert!(matches!(strokes.add((0, 2), false), Some(Dirty::Rect(..))));
    }
}
