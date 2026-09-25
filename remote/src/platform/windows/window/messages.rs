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
        WM_GETMINMAXINFO => {
            let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
            // Leave room for display/quality controls, session actions and caption buttons.
            let dpi = unsafe { window_dpi(window) };
            info.ptMinTrackSize.x = scale(MINIMUM_WINDOW_WIDTH, dpi);
            info.ptMinTrackSize.y = scale(MINIMUM_WINDOW_HEIGHT, dpi);
            LRESULT(0)
        }
        WM_DPICHANGED => {
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
        WM_NCHITTEST => {
            let default_hit = unsafe { DefWindowProcW(window, message, wparam, lparam) };
            if default_hit.0 != HTCLIENT as isize {
                return default_hit;
            }
            let mut point = windows::Win32::Foundation::POINT {
                x: signed_low_word(lparam.0),
                y: signed_high_word(lparam.0),
            };
            let mut bounds = RECT::default();
            if unsafe { ScreenToClient(window, &mut point) }.as_bool()
                && unsafe { GetClientRect(window, &mut bounds) }.is_ok()
            {
                let width = bounds.right.saturating_sub(bounds.left);
                let dpi = unsafe { window_dpi(window) };
                if point.y >= 0
                    && point.y < toolbar_height(dpi)
                    && point.x >= scale(302, dpi)
                    && point.x < width.saturating_sub(scale(220, dpi))
                {
                    return LRESULT(HTCAPTION as isize);
                }
            }
            default_hit
        }
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
        WM_SETCURSOR => {
            if let Some(context) = context
                && (lparam.0 as u32 & 0xffff) == HTCLIENT
            {
                unsafe {
                    apply_cursor(
                        context
                            .control
                            .effective_cursor_shape(context.cursor_shape.get()),
                    )
                };
                return LRESULT(1);
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
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
                context.control.set_input_enabled(true);
                if context.control.send_windows_shortcuts() {
                    keyboard_hook::install(window);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe { dark_control_colors(wparam) },
        WM_KILLFOCUS => {
            keyboard_hook::remove();
            if let Some(context) = context {
                context.release_input();
                context.control.set_input_enabled(false);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
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
        WM_DESTROY => {
            keyboard_hook::remove();
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let pointer =
                unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const WindowContext;
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
            if !pointer.is_null() {
                // Releases the window's reference. Calls still running for
                // this window hold their own, so the context outlives them.
                drop(unsafe { Rc::from_raw(pointer) });
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
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
