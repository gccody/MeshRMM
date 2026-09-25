//! The small window shown from launch until the first remote desktop window
//! replaces it. Without it nothing appears on Windows until the video
//! starts, so a slow launch would not say what it is waiting on.
//!
//! The window runs on its own thread, so it keeps painting while the launch
//! blocks on the network or the previous viewer. Like the macOS connecting
//! window it cannot be closed by the user: the launch closes it.

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
/// The client area, in 96-DPI pixels. Two lines fit the longest status.
const CLIENT_WIDTH: i32 = 460;
const CLIENT_HEIGHT: i32 = 110;
const MARGIN: i32 = 24;

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
}

static LAUNCH_WINDOW: Mutex<LaunchWindow> = Mutex::new(LaunchWindow {
    state: State::Hidden,
    message: String::new(),
});

fn launch_window() -> MutexGuard<'static, LaunchWindow> {
    LAUNCH_WINDOW
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Shows `message`, opening the window the first time. Does nothing once the
/// launch has closed the window.
pub fn show_launch_status(message: String) {
    let mut launch = launch_window();
    launch.message = message;
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
    if let Err(error) = unsafe { open_window() } {
        tracing::warn!(error = ?error, "could not show the viewer launch status");
        launch_window().state = State::Closed;
        return;
    }
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe fn open_window() -> anyhow::Result<()> {
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
    let style = WS_OVERLAPPED | WS_CAPTION;
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
            scale(MARGIN + 12, dpi),
            scale(CLIENT_WIDTH - 2 * MARGIN, dpi),
            scale(CLIENT_HEIGHT - 2 * MARGIN - 12, dpi),
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
    unsafe { center_on_primary_monitor(window, style, dpi) };

    let message = {
        let mut launch = launch_window();
        if matches!(launch.state, State::Closed) {
            drop(launch);
            // Posts the quit message that ends the loop.
            let _ = unsafe { DestroyWindow(window) };
            return Ok(());
        }
        launch.state = State::Open(window.0 as usize);
        HSTRING::from(launch.message.as_str())
    };
    unsafe {
        let _ = SetWindowTextW(label, &message);
        let _ = ShowWindow(window, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(window);
    }
    Ok(())
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
            let text = HSTRING::from(launch_window().message.as_str());
            if let Ok(label) = unsafe { GetDlgItem(Some(window), STATUS_LABEL_ID) } {
                let _ = unsafe { SetWindowTextW(label, &text) };
            }
            LRESULT(0)
        }
        // Only the launch closes the window.
        WM_CLOSE => {
            if matches!(launch_window().state, State::Closed) {
                let _ = unsafe { DestroyWindow(window) };
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
