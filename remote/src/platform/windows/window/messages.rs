//! The window procedures of the viewer window and its video child.

use super::*;

pub(super) unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    let context = unsafe { window_context(window) };
    let context = context.as_deref();
    match message {
        WM_GETMINMAXINFO => unsafe { min_max_info(window, lparam) },
        WM_DPICHANGED => unsafe { dpi_changed(context, window, wparam, lparam) },
        // Windows gives every overlapped window a caption, whatever its
        // style. The toolbar replaces it: the client area starts at the top
        // of the window, and the side and bottom borders stay.
        WM_NCCALCSIZE if wparam.0 != 0 => unsafe { client_area_size(window, wparam, lparam) },
        WM_NCHITTEST => unsafe { hit_test(window, wparam, lparam) },
        WM_DROPFILES => {
            if let Some(context) = context {
                unsafe { context.drop_files(window, wparam) };
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(context) = context {
                context.move_pointer(window, lparam);
            }
            LRESULT(0)
        }
        WM_MOVE => {
            if let Some(context) = context {
                context.place_popups(window);
            }
            unsafe { remember_placement(window) };
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(context) = context {
                // The worker resizes the swap chain after the message pump
                // returns, which is after a border drag ends.
                if wparam.0 != SIZE_MINIMIZED as usize {
                    context.resize_pending.set(true);
                }
                context.layout_toolbar(window);
            }
            unsafe { remember_placement(window) };
            LRESULT(0)
        }
        WM_COMMAND => {
            if let Some(context) = context
                && context.command(window, wparam)
            {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_SETCURSOR => unsafe { set_cursor(context, window, wparam, lparam) },
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            if let Some(context) = context {
                context.mouse_button(window, message, wparam, lparam);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if let Some(context) = context {
                context.wheel(window, message, wparam, lparam);
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            if let Some(context) = context {
                context.key_message(message, wparam, lparam);
            }
            LRESULT(0)
        }
        keyboard_hook::WM_SYSTEM_SHORTCUT_KEY => {
            if let Some(context) = context {
                let (scan_code, extended, pressed) = keyboard_hook::unpack(wparam);
                context.send_key(scan_code, extended, pressed);
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Some(context) = context {
                gain_focus(context, window);
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe { dark_control_colors(wparam) },
        WM_KILLFOCUS => {
            keyboard_hook::remove();
            if let Some(context) = context {
                lose_focus(context);
            }
            LRESULT(0)
        }
        WM_CLOSE => unsafe { close(context, window) },
        WM_DESTROY => {
            keyboard_hook::remove();
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => unsafe { release_context(window, wparam, lparam) },
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn min_max_info(window: HWND, lparam: LPARAM) -> LRESULT {
    let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
    // Leave room for display/quality controls, session actions and caption buttons.
    let dpi = unsafe { window_dpi(window) };
    info.ptMinTrackSize.x = scale(MINIMUM_WINDOW_WIDTH, dpi);
    info.ptMinTrackSize.y = scale(MINIMUM_WINDOW_HEIGHT, dpi);
    LRESULT(0)
}

unsafe fn dpi_changed(
    context: Option<&WindowContext>,
    window: HWND,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(context) = context {
        context.set_dpi(window, (wparam.0 & 0xffff) as u32);
    }
    let suggested = unsafe { &*(lparam.0 as *const RECT) };
    let _ = unsafe {
        SetWindowPos(
            window,
            None,
            suggested.left,
            suggested.top,
            suggested.right - suggested.left,
            suggested.bottom - suggested.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
    LRESULT(0)
}

unsafe fn client_area_size(window: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
    let top = unsafe { (*params).rgrc[0].top };
    let result = unsafe { DefWindowProcW(window, WM_NCCALCSIZE, wparam, lparam) };
    // A maximized window extends past the monitor by its border.
    let inset = if unsafe { IsZoomed(window) }.as_bool() {
        unsafe { resize_border(window) }
    } else {
        0
    };
    unsafe { (*params).rgrc[0].top = top + inset };
    result
}

unsafe fn hit_test(window: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let default_hit = unsafe { DefWindowProcW(window, WM_NCHITTEST, wparam, lparam) };
    if default_hit.0 != HTCLIENT as isize {
        return default_hit;
    }
    let mut point = windows::Win32::Foundation::POINT {
        x: signed_low_word(lparam.0),
        y: signed_high_word(lparam.0),
    };
    // The toolbar passes the space between its items through. Its
    // top edge resizes the window, as the caption's did.
    if unsafe { ScreenToClient(window, &mut point) }.as_bool() && point.y >= 0 {
        if point.y < unsafe { resize_border(window) } && !unsafe { IsZoomed(window) }.as_bool() {
            return LRESULT(HTTOP as isize);
        }
        if point.y < toolbar_height(unsafe { window_dpi(window) }) {
            return LRESULT(HTCAPTION as isize);
        }
    }
    default_hit
}

unsafe fn set_cursor(
    context: Option<&WindowContext>,
    window: HWND,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // The toolbar keeps its own arrow cursor.
    if let Some(context) = context
        && (lparam.0 as u32 & 0xffff) == HTCLIENT
        && wparam.0 != context.controls().toolbar.0 as usize
    {
        unsafe { apply_cursor(context.video_cursor()) };
        return LRESULT(1);
    }
    unsafe { DefWindowProcW(window, WM_SETCURSOR, wparam, lparam) }
}

fn gain_focus(context: &WindowContext, window: HWND) {
    context.control.set_input_enabled(true);
    if context.control.send_windows_shortcuts() {
        keyboard_hook::install(window);
    }
}

fn lose_focus(context: &WindowContext) {
    context.annotator.borrow_mut().finish();
    context.release_input();
    context.control.set_input_enabled(false);
}

unsafe fn close(context: Option<&WindowContext>, window: HWND) -> LRESULT {
    let confirm = context
        .map(|context| {
            context.release_input();
            context.control.set_input_enabled(false);
            context.control.disconnect_confirmation()
        })
        .unwrap_or(false);
    if confirm
        && unsafe {
            MessageBoxW(
                Some(window),
                w!("Disconnect from this device?"),
                w!("End remote session"),
                MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
            )
        } != IDYES
    {
        // The message box ran nested messages; the window may have
        // lost its context meanwhile.
        if let Some(context) = unsafe { window_context(window) } {
            context.control.set_input_enabled(true);
        }
        return LRESULT(0);
    }
    // Ends the session even while it is reconnecting and no
    // transport is watching this window.
    crate::shutdown::request("the viewer window was closed");
    let _ = unsafe { DestroyWindow(window) };
    LRESULT(0)
}

unsafe fn release_context(window: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const WindowContext;
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
    if !pointer.is_null() {
        // Releases the window's reference. Calls still running for
        // this window hold their own, so the context outlives them.
        drop(unsafe { Rc::from_raw(pointer) });
    }
    unsafe { DefWindowProcW(window, WM_NCDESTROY, wparam, lparam) }
}

pub(super) unsafe extern "system" fn video_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        // The swap chain paints every pixel; skip the GDI erase.
        WM_ERASEBKGND => LRESULT(1),
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// The reconnect panel: an owned popup, so it shows over the swap chain,
/// that hosts the reconnect text and "Retry now" as children. A button that
/// is itself a popup reports its clicks to the desktop rather than to its
/// owner, so the panel passes its children's notifications to the owner.
pub(super) unsafe extern "system" fn reconnect_panel_proc(
    panel: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND => {
            if let Ok(owner) = unsafe { GetWindow(panel, GW_OWNER) } {
                return unsafe { SendMessageW(owner, message, Some(wparam), Some(lparam)) };
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe { dark_control_colors(wparam) },
        _ => unsafe { DefWindowProcW(panel, message, wparam, lparam) },
    }
}

/// Light text on the dark toolbar and settings backgrounds.
pub(super) unsafe fn dark_control_colors(wparam: WPARAM) -> LRESULT {
    unsafe {
        SetTextColor(
            windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
            windows::Win32::Foundation::COLORREF(0x00f4_f4f4),
        );
        SetBkColor(
            windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
            windows::Win32::Foundation::COLORREF(0x0014_1414),
        );
        LRESULT(GetStockObject(BLACK_BRUSH).0 as isize)
    }
}

/// The height of the window's sizing border.
unsafe fn resize_border(window: HWND) -> i32 {
    let dpi = unsafe { window_dpi(window) };
    unsafe {
        windows::Win32::UI::HiDpi::GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
            + windows::Win32::UI::HiDpi::GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}
