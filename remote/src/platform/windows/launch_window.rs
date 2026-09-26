//! The small window shown from launch until the first remote desktop window
//! replaces it. Without it nothing appears on Windows until the video
//! starts, so a slow launch would not say what it is waiting on.
//!
//! The window runs on its own thread, so it keeps painting while the launch
//! blocks on the network or the previous viewer. Its Cancel button, Esc, and
//! close box ask the launch to end; the launch itself closes the window.

use std::sync::{Mutex, MutexGuard};

use super::window::{STATIC_CENTER, message_font, scale, set_font, window_dpi};
use super::*;
use windows::Win32::Graphics::Gdi::{
    COLOR_BTNFACE, DeleteObject, GetMonitorInfoW, GetSysColorBrush, HGDIOBJ,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::AdjustWindowRectExForDpi;

const WM_STATUS_CHANGED: u32 = WM_APP + 1;
const STATUS_LABEL_ID: i32 = 1;
/// The client area, in 96-DPI pixels. Two lines fit the longest status,
/// above the Cancel button.
const CLIENT_WIDTH: i32 = 460;
const CLIENT_HEIGHT: i32 = 150;
const MARGIN: i32 = 24;
const LABEL_HEIGHT: i32 = 48;
const BUTTON_WIDTH: i32 = 88;
const BUTTON_HEIGHT: i32 = 28;

enum State {
    Hidden,
    Opening,
    /// The window handle, which is not `Send`.
    Open(usize),
    Closed,
}

struct LaunchWindow {
    state: State,
    message: String,
    /// Whether the current phase can be cancelled.
    cancellable: bool,
    /// The user cancelled; the window says so until the launch closes it.
    cancelling: bool,
}

static LAUNCH_WINDOW: Mutex<LaunchWindow> = Mutex::new(LaunchWindow {
    state: State::Hidden,
    message: String::new(),
    cancellable: true,
    cancelling: false,
});

fn launch_window() -> MutexGuard<'static, LaunchWindow> {
    LAUNCH_WINDOW
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Shows `message`, and whether the launch can be cancelled now, opening the
/// window the first time. Does nothing once the launch has closed the window.
pub fn show_launch_status(message: String, cancellable: bool) {
    let mut launch = launch_window();
    launch.message = message;
    launch.cancellable = cancellable;
    match launch.state {
        State::Hidden => {
            launch.state = State::Opening;
            let thread = std::thread::Builder::new()
                .name("meshrmm-launch-status".into())
                .spawn(run_window);
            if let Err(error) = thread {
                tracing::warn!(%error, "could not show the viewer launch status");
                launch.state = State::Closed;
            }
        }
        State::Open(window) => {
            let _ = unsafe {
                PostMessageW(
                    Some(HWND(window as *mut c_void)),
                    WM_STATUS_CHANGED,
                    WPARAM(0),
                    LPARAM(0),
                )
            };
        }
        State::Opening | State::Closed => {}
    }
}

/// Closes the window for good: the remote desktop is showing, or the launch ended.
pub fn close_launch_status() {
    let previous = std::mem::replace(&mut launch_window().state, State::Closed);
    if let State::Open(window) = previous {
        let _ = unsafe {
            PostMessageW(
                Some(HWND(window as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

fn run_window() {
    let window = match unsafe { open_window() } {
        Ok(Some(window)) => window,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(error = ?error, "could not show the viewer launch status");
            launch_window().state = State::Closed;
            return;
        }
    };
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        // Esc becomes IDCANCEL, and Tab moves focus, as in a dialog.
        if unsafe { IsDialogMessageW(window, &message) }.as_bool() {
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Opens the window, or returns `None` if the launch closed it first.
unsafe fn open_window() -> anyhow::Result<Option<HWND>> {
    let module =
        unsafe { GetModuleHandleW(None) }.context("application module handle unavailable")?;
    let instance = HINSTANCE(module.0);
    let class = w!("MeshRmmRemoteLaunchStatus");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(launch_window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        hbrBackground: unsafe { GetSysColorBrush(COLOR_BTNFACE) },
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("launch status window class registration failed");
        }
    }
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("MeshRMM Remote"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CLIENT_WIDTH,
            CLIENT_HEIGHT,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .context("launch status window creation failed")?;
    let dpi = unsafe { window_dpi(window) };
    let font = unsafe { message_font(dpi) };
    // Deleted with the window.
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, font.0 as isize) };
    let label = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | STATIC_CENTER),
            scale(MARGIN, dpi),
            scale(MARGIN, dpi),
            scale(CLIENT_WIDTH - 2 * MARGIN, dpi),
            scale(LABEL_HEIGHT, dpi),
            Some(window),
            Some(HMENU(STATUS_LABEL_ID as *mut c_void)),
            Some(instance),
            None,
        )
    };
    let label = match label {
        Ok(label) => label,
        Err(error) => {
            let _ = unsafe { DestroyWindow(window) };
            return Err(error).context("launch status label creation failed");
        }
    };
    unsafe { set_font(label, font) };
    let cancel = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("BUTTON"),
            w!("Cancel"),
            WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_PUSHBUTTON as u32),
            scale((CLIENT_WIDTH - BUTTON_WIDTH) / 2, dpi),
            scale(CLIENT_HEIGHT - MARGIN - BUTTON_HEIGHT, dpi),
            scale(BUTTON_WIDTH, dpi),
            scale(BUTTON_HEIGHT, dpi),
            Some(window),
            Some(HMENU(IDCANCEL.0 as usize as *mut c_void)),
            Some(instance),
            None,
        )
    };
    let cancel = match cancel {
        Ok(cancel) => cancel,
        Err(error) => {
            let _ = unsafe { DestroyWindow(window) };
            return Err(error).context("launch status Cancel button creation failed");
        }
    };
    unsafe { set_font(cancel, font) };
    unsafe { center_on_primary_monitor(window, style, dpi) };

    {
        let mut launch = launch_window();
        if matches!(launch.state, State::Closed) {
            drop(launch);
            // Posts the quit message that ends the loop.
            let _ = unsafe { DestroyWindow(window) };
            return Ok(None);
        }
        launch.state = State::Open(window.0 as usize);
    }
    unsafe {
        refresh(window);
        let _ = ShowWindow(window, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(window);
    }
    Ok(Some(window))
}

/// Shows the current status and whether Cancel is available.
unsafe fn refresh(window: HWND) {
    let (text, enabled) = {
        let launch = launch_window();
        if launch.cancelling {
            (HSTRING::from("Cancelling…"), false)
        } else {
            (HSTRING::from(launch.message.as_str()), launch.cancellable)
        }
    };
    if let Ok(label) = unsafe { GetDlgItem(Some(window), STATUS_LABEL_ID) } {
        let _ = unsafe { SetWindowTextW(label, &text) };
    }
    if let Ok(cancel) = unsafe { GetDlgItem(Some(window), IDCANCEL.0) } {
        let _ = unsafe { EnableWindow(cancel, enabled) };
    }
}

/// Cancel, Esc, or the close box: ask the launch to end. It ends without an
/// error and closes this window.
unsafe fn cancel(window: HWND) {
    {
        let mut launch = launch_window();
        if launch.cancelling || !launch.cancellable {
            return;
        }
        launch.cancelling = true;
    }
    unsafe { refresh(window) };
    crate::shutdown::request("the user cancelled the connection");
}

unsafe fn center_on_primary_monitor(window: HWND, style: WINDOW_STYLE, dpi: u32) {
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: scale(CLIENT_WIDTH, dpi),
        bottom: scale(CLIENT_HEIGHT, dpi),
    };
    let _ = unsafe {
        AdjustWindowRectExForDpi(&mut frame, style, false, WINDOW_EX_STYLE::default(), dpi)
    };
    let width = frame.right - frame.left;
    let height = frame.bottom - frame.top;
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTOPRIMARY) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (x, y) = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let work = info.rcWork;
        (
            work.left + (work.right - work.left - width) / 2,
            work.top + (work.bottom - work.top - height) / 2,
        )
    } else {
        (CW_USEDEFAULT, CW_USEDEFAULT)
    };
    let _ = unsafe { SetWindowPos(window, None, x, y, width, height, SWP_NOZORDER) };
}

unsafe extern "system" fn launch_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_STATUS_CHANGED => {
            unsafe { refresh(window) };
            LRESULT(0)
        }
        WM_COMMAND if (wparam.0 & 0xffff) as i32 == IDCANCEL.0 => {
            unsafe { cancel(window) };
            LRESULT(0)
        }
        // The launch closes the window; the user's close box cancels.
        WM_CLOSE => {
            if matches!(launch_window().state, State::Closed) {
                let _ = unsafe { DestroyWindow(window) };
            } else {
                unsafe { cancel(window) };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let font = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) };
            if font != 0 {
                let _ = unsafe { DeleteObject(HGDIOBJ(font as *mut c_void)) };
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}
