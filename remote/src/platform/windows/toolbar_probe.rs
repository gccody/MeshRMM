//! An interactive check of the viewer's own toolbar on a real window: its
//! items, hit testing, tooltips, clicks, menus and caption buttons. It needs
//! an interactive desktop:
//!
//! ```text
//! cargo test -p meshrmm-remote -- --ignored --nocapture toolbar_probe
//! ```
//!
//! Set `MESHRMM_TOOLBAR_PROBE_CAPTURE` to a directory to save screenshots of
//! the window and of an open menu there.

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, ClientToScreen, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ,
    ReleaseDC, SelectObject,
};
use windows::Win32::UI::Controls::TTM_GETTOOLCOUNT;

use super::pipeline::Presentation;
use super::reset_probe::{present_synthetic, pump};
use super::window::{probe_state, probe_toolbar, probe_toolbar_state};
use super::*;
use crate::toolbar::{Action, Icon};
use meshrmm_protocol::{DesktopSession, DisplayId, PixelFormat};

/// Where the menu timer saves its screenshot.
static MENU_CAPTURE: Mutex<Option<String>> = Mutex::new(None);

fn display(session: DesktopSession, id: u32, width: u32, height: u32) -> Display {
    Display {
        session,
        id: DisplayId(id),
        name: format!(r"\\.\DISPLAY{id}"),
        x: 0,
        y: 0,
        width,
        height,
        primary: id == 1,
    }
}

fn capture_dir() -> Option<String> {
    std::env::var("MESHRMM_TOOLBAR_PROBE_CAPTURE").ok()
}

/// Writes 32-bit top-down pixels as a BMP file.
fn write_bmp(path: &str, width: i32, height: i32, pixels: &[u32]) {
    let size = 54 + pixels.len() * 4;
    let mut file = Vec::with_capacity(size);
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&(size as u32).to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&54_u32.to_le_bytes());
    file.extend_from_slice(&40_u32.to_le_bytes());
    file.extend_from_slice(&width.to_le_bytes());
    file.extend_from_slice(&(-height).to_le_bytes());
    file.extend_from_slice(&1_u16.to_le_bytes());
    file.extend_from_slice(&32_u16.to_le_bytes());
    file.extend_from_slice(&[0; 24]);
    for pixel in pixels {
        file.extend_from_slice(&pixel.to_le_bytes());
    }
    std::fs::write(path, file).unwrap();
    println!("saved {path}");
}

/// Captures an area of the screen.
unsafe fn capture(bounds: RECT) -> (i32, i32, Vec<u32>) {
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let _ = windows::Win32::Graphics::Gdi::BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            bounds.left,
            bounds.top,
            windows::Win32::Graphics::Gdi::SRCCOPY,
        );
        SelectObject(memory, previous);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0_u32; (width * height) as usize];
        GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        (width, height, pixels)
    }
}

unsafe fn save_window(window: HWND, name: &str) {
    let Some(dir) = capture_dir() else {
        return;
    };
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(window, &mut bounds) }.unwrap();
    // The toolbar and a little of the video under it.
    bounds.bottom = bounds.top + (bounds.bottom - bounds.top).min(160);
    // The screen, so the window frame shows as DWM draws it.
    let (width, height, pixels) = unsafe { capture(bounds) };
    write_bmp(&format!(r"{dir}\{name}.bmp"), width, height, &pixels);
}

/// Runs inside the menu's modal loop: saves the screen and closes the menu.
unsafe extern "system" fn close_menu(window: HWND, _message: u32, id: usize, _time: u32) {
    unsafe {
        let _ = KillTimer(Some(window), id);
        if let Some(path) = MENU_CAPTURE.lock().unwrap().take() {
            let mut bounds = RECT::default();
            let _ = GetWindowRect(window, &mut bounds);
            bounds.bottom = bounds.top + (bounds.bottom - bounds.top).min(420);
            let (width, height, pixels) = capture(bounds);
            write_bmp(&path, width, height, &pixels);
        }
        let _ = EndMenu();
    }
}

fn lparam_at(x: i32, y: i32) -> LPARAM {
    LPARAM(((x as u16 as usize) | ((y as u16 as usize) << 16)) as isize)
}

fn center(rect: RECT) -> (i32, i32) {
    ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2)
}

unsafe fn items(window: HWND) -> Vec<(crate::toolbar::Item, RECT)> {
    unsafe { probe_toolbar(window) }.unwrap().2
}

unsafe fn rect_of(window: HWND, action: Action) -> RECT {
    unsafe { items(window) }
        .into_iter()
        .find(|(item, _)| item.action == action)
        .unwrap_or_else(|| panic!("no {action:?} item"))
        .1
}

/// Clicks an item the way the mouse does. A menu item opens its menu, which
/// a timer closes after saving a screenshot as `menu_capture`.
unsafe fn click(window: HWND, action: Action, menu_capture: Option<&str>) {
    let toolbar = unsafe { probe_toolbar(window) }.unwrap().0;
    let (x, y) = center(unsafe { rect_of(window, action) });
    if let Some(name) = menu_capture {
        *MENU_CAPTURE.lock().unwrap() = capture_dir().map(|dir| format!(r"{dir}\{name}.bmp"));
        unsafe { SetTimer(Some(window), 77, 500, Some(close_menu)) };
    }
    unsafe {
        SendMessageW(
            toolbar,
            WM_MOUSEMOVE,
            Some(WPARAM(0)),
            Some(lparam_at(x, y)),
        );
        SendMessageW(
            toolbar,
            WM_LBUTTONDOWN,
            Some(WPARAM(1)),
            Some(lparam_at(x, y)),
        );
        SendMessageW(
            toolbar,
            WM_LBUTTONUP,
            Some(WPARAM(0)),
            Some(lparam_at(x, y)),
        );
        pump(window, Duration::from_millis(150));
    }
}

/// The probe window and the session state its toolbar acts on.
struct ToolbarProbe {
    sent: Arc<Mutex<Vec<SessionMessage>>>,
    chat: meshrmm_chat::ChatSession,
    control: ControlSink,
    presentation: Presentation,
    window: HWND,
}

#[test]
#[ignore = "needs an interactive desktop and a D3D11 video device"]
fn toolbar_probe_draws_and_handles_the_viewer_toolbar() {
    unsafe { run_probe() }
}

unsafe fn run_probe() {
    super::enable_dpi_awareness();
    let probe = unsafe { ToolbarProbe::open() };
    let (toolbar, tooltip, item_count) = unsafe { probe.check_items() };
    unsafe { probe.check_hit_testing(toolbar) };

    // One tooltip per item.
    if tooltip.is_invalid() {
        println!("no tooltip control");
    } else {
        let tools = unsafe { SendMessageW(tooltip, TTM_GETTOOLCOUNT, None, None) }.0;
        assert_eq!(tools as usize, item_count);
    }

    // Hover highlights an item.
    let (x, y) = center(unsafe { rect_of(probe.window, Action::Settings) });
    unsafe {
        SendMessageW(
            toolbar,
            WM_MOUSEMOVE,
            Some(WPARAM(0)),
            Some(lparam_at(x, y)),
        );
        pump(probe.window, Duration::from_millis(100));
        save_window(probe.window, "toolbar_hover");
    }

    unsafe { probe.check_buttons() };
    unsafe { probe.check_annotation() };
    unsafe { probe.check_unread_badge_and_menus() };
    unsafe { probe.check_caption_buttons() };
    println!("toolbar probe passed");
    drop(probe.presentation);
}

impl ToolbarProbe {
    unsafe fn open() -> Self {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let chat = meshrmm_chat::ChatSession::default();
        chat.set_available(true);
        let console = |id, width, height| display(DesktopSession::Console, id, width, height);
        let first = console(1, 1280, 720);
        let displays = vec![
            first.clone(),
            console(2, 1920, 1080),
            display(
                DesktopSession::Rdp {
                    id: 2,
                    user: "probe".into(),
                },
                7,
                1600,
                900,
            ),
        ];
        let format = VideoFormat {
            width: 1280,
            height: 720,
            frames_per_second: 60,
            codec: Codec::H264,
            pixel_format: PixelFormat::Nv12,
            bitrate_bits_per_second: 12_000_000,
        };
        let control = test_sink(Arc::clone(&sent), chat.clone());
        let mut presentation = unsafe {
            Presentation::new(
                format,
                first,
                displays,
                control.clone(),
                DebugInfo::new("toolbar-probe"),
            )
        }
        .expect("the probe window and renderer");
        let window = presentation.window();
        unsafe { pump(window, Duration::from_millis(300)) };
        unsafe { present_synthetic(&mut presentation, 1280, 720, PixelFormat::Nv12) }.unwrap();
        unsafe { pump(window, Duration::from_millis(200)) };
        Self {
            sent,
            chat,
            control,
            presentation,
            window,
        }
    }

    /// Checks the items and their layout. Returns the toolbar, its tooltip
    /// control and the number of items.
    unsafe fn check_items(&self) -> (HWND, HWND, usize) {
        let window = self.window;
        let (toolbar, tooltip, initial) = unsafe { probe_toolbar(window) }.unwrap();
        let actions: Vec<Action> = initial.iter().map(|(item, _)| item.action).collect();
        println!("items: {actions:?}");
        assert_eq!(
            actions,
            [
                Action::User,
                Action::Display,
                Action::Quality,
                Action::Credentials,
                Action::SecureAttention,
                Action::Power,
                Action::TypeClipboard,
                Action::Annotate,
                Action::Files,
                Action::Chat,
                Action::Diagnostics,
                Action::Settings,
                Action::Minimize,
                Action::Maximize,
                Action::Close,
            ]
        );
        let dpi = unsafe { window::window_dpi(window) };
        let (width, _) = {
            let mut client = RECT::default();
            unsafe { GetClientRect(window, &mut client) }.unwrap();
            (client.right, client.bottom)
        };
        println!("dpi {dpi}, client width {width}");
        let close = initial.last().unwrap().1;
        assert_eq!(close.right, width, "the close button is in the corner");
        assert_eq!(close.top, 0);
        for pair in initial.windows(2) {
            assert!(pair[0].1.right <= pair[1].1.left, "{pair:?}");
        }
        unsafe { save_window(window, "toolbar") };
        (toolbar, tooltip, initial.len())
    }

    /// Items take clicks; the space between them moves the window.
    unsafe fn check_hit_testing(&self, toolbar: HWND) {
        let window = self.window;
        let hit = |x: i32, y: i32| {
            let mut point = windows::Win32::Foundation::POINT { x, y };
            let _ = unsafe { ClientToScreen(window, &mut point) };
            let position = lparam_at(point.x, point.y);
            (
                unsafe { SendMessageW(toolbar, WM_NCHITTEST, None, Some(position)) }.0,
                unsafe { SendMessageW(window, WM_NCHITTEST, None, Some(position)) }.0,
            )
        };
        let (x, y) = center(unsafe { rect_of(window, Action::Chat) });
        assert_eq!(hit(x, y).0, HTCLIENT as isize);
        let gap_x = (unsafe { rect_of(window, Action::Quality) }.right
            + unsafe { rect_of(window, Action::Credentials) }.left)
            / 2;
        assert_eq!(hit(gap_x, y), (-1, HTCAPTION as isize));
    }

    /// Buttons act on click.
    unsafe fn check_buttons(&self) {
        let (window, sent, chat) = (self.window, &self.sent, &self.chat);
        sent.lock().unwrap().clear();
        unsafe { click(window, Action::SecureAttention, None) };
        assert!(
            sent.lock()
                .unwrap()
                .contains(&SessionMessage::SendSecureAttention),
            "{:?}",
            sent.lock().unwrap()
        );
        unsafe { click(window, Action::Diagnostics, None) };
        assert!(unsafe { probe_toolbar_state(window) }.unwrap().diagnostics);
        let diagnostics = unsafe { items(window) }
            .into_iter()
            .find(|(item, _)| item.action == Action::Diagnostics)
            .unwrap()
            .0;
        assert!(diagnostics.active);
        unsafe { click(window, Action::Diagnostics, None) };
        assert!(!unsafe { probe_toolbar_state(window) }.unwrap().diagnostics);
        unsafe { click(window, Action::Chat, None) };
        assert!(chat.visible(), "the chat item opens the chat popup");
        unsafe { click(window, Action::Chat, None) };
        assert!(!chat.visible(), "the chat item closes the chat popup");
    }

    /// View-only sessions annotate: the mouse draws and erases, and sends
    /// no input.
    unsafe fn check_annotation(&self) {
        use meshrmm_protocol::Annotation;
        let (window, sent) = (self.window, &self.sent);
        self.control.set_technician_blocked(true);
        sent.lock().unwrap().clear();
        unsafe { click(window, Action::Annotate, None) };
        assert!(unsafe { probe_toolbar_state(window) }.unwrap().annotating);
        let video = unsafe { probe_state(window) }.unwrap().video.unwrap();
        let at = |x: i32, y: i32| {
            lparam_at(
                video.left + video.width * x / 4,
                video.top + video.height * y / 4,
            )
        };
        unsafe {
            SendMessageW(window, WM_LBUTTONDOWN, Some(WPARAM(1)), Some(at(1, 1)));
            SendMessageW(window, WM_MOUSEMOVE, Some(WPARAM(1)), Some(at(3, 1)));
            SendMessageW(window, WM_LBUTTONUP, Some(WPARAM(0)), Some(at(3, 1)));
            // Moving without the button draws nothing.
            SendMessageW(window, WM_MOUSEMOVE, Some(WPARAM(0)), Some(at(3, 3)));
            SendMessageW(window, WM_RBUTTONDOWN, Some(WPARAM(2)), Some(at(2, 2)));
            SendMessageW(window, WM_RBUTTONUP, Some(WPARAM(0)), Some(at(2, 2)));
            pump(window, Duration::from_millis(100));
            save_window(window, "toolbar_annotating");
        }
        unsafe { click(window, Action::Annotate, None) };
        assert!(!unsafe { probe_toolbar_state(window) }.unwrap().annotating);
        let annotations = sent
            .lock()
            .unwrap()
            .iter()
            .map(|message| match message {
                SessionMessage::Annotate(annotation) => *annotation,
                other => panic!("annotating sent {other:?}"),
            })
            .collect::<Vec<_>>();
        assert!(
            matches!(
                annotations.as_slice(),
                [
                    Annotation::Start {
                        display_id: DisplayId(1),
                        x: 16_200..=16_600,
                        y: 16_200..=16_600
                    },
                    Annotation::Extend {
                        display_id: DisplayId(1),
                        x: 49_000..=49_400,
                        ..
                    },
                    Annotation::Clear,
                    Annotation::Clear,
                ]
            ),
            "{annotations:?}"
        );
        self.control.set_technician_blocked(false);
    }

    unsafe fn check_unread_badge_and_menus(&self) {
        let (window, sent) = (self.window, &self.sent);
        // Unread messages badge the chat item.
        self.chat.receive("Hello from the probe".into());
        unsafe { pump(window, Duration::from_millis(100)) };
        let chat_item = unsafe { items(window) }
            .into_iter()
            .find(|(item, _)| item.action == Action::Chat)
            .unwrap()
            .0;
        assert_eq!(chat_item.badge, Some(crate::toolbar::Badge::Unread));

        // Menus open under their item and close without choosing.
        sent.lock().unwrap().clear();
        unsafe { click(window, Action::Display, Some("menu_display")) };
        unsafe { click(window, Action::Quality, Some("menu_quality")) };
        unsafe { click(window, Action::User, Some("menu_user")) };
        assert!(
            !sent
                .lock()
                .unwrap()
                .iter()
                .any(|message| matches!(message, SessionMessage::SelectDisplay { .. })),
            "{:?}",
            sent.lock().unwrap()
        );
        unsafe { save_window(window, "toolbar_badge") };
    }

    /// The caption buttons maximize and restore the window.
    unsafe fn check_caption_buttons(&self) {
        let window = self.window;
        unsafe { click(window, Action::Maximize, None) };
        unsafe { pump(window, Duration::from_millis(300)) };
        assert!(unsafe { IsZoomed(window) }.as_bool());
        let restore = unsafe { items(window) }
            .into_iter()
            .find(|(item, _)| item.action == Action::Maximize)
            .unwrap();
        assert_eq!(restore.0.icon, Icon::Restore);
        let mut client = RECT::default();
        unsafe { GetClientRect(window, &mut client) }.unwrap();
        assert_eq!(
            unsafe { rect_of(window, Action::Close) }.right,
            client.right
        );
        unsafe { save_window(window, "toolbar_maximized") };
        unsafe { click(window, Action::Maximize, None) };
        unsafe { pump(window, Duration::from_millis(300)) };
        assert!(!unsafe { IsZoomed(window) }.as_bool());
    }
}
