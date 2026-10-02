//! A window with a finished toolbox run's outcome and output, which the
//! technician can read, select and copy. Each run gets its own window,
//! owned by the viewer, so it stays above it and closes with it.

use super::*;
use windows::Win32::Graphics::Gdi::{
    CLEARTYPE_QUALITY, FF_MODERN, FIXED_PITCH, FW_NORMAL, LOGFONTW, OUT_DEFAULT_PRECIS,
};
use windows::Win32::UI::Controls::EM_SETLIMITTEXT;

const OUTPUT_ID: i32 = 1;
const WIDTH: i32 = 760;
const HEIGHT: i32 = 500;
const FONT_POINTS: i32 = 10;

/// Opens a window titled `title` showing `text`, whose lines end in CRLF.
pub(super) unsafe fn show(owner: HWND, title: &str, text: &str) -> anyhow::Result<()> {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let class = w!("MeshRmmScriptOutputWindow");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(output_window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        hbrBackground: HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("script output window class registration failed");
        }
    }
    let dpi = unsafe { GetDpiForWindow(owner) }.max(96) as i32;
    let scale = |value: i32| value * dpi / 96;
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            &HSTRING::from(title),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(WIDTH),
            scale(HEIGHT),
            Some(owner),
            None,
            Some(instance),
            None,
        )
    }
    .context("script output window creation failed")?;
    let output = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR::null(),
            WINDOW_STYLE(
                WS_CHILD.0
                    | WS_VISIBLE.0
                    | WS_VSCROLL.0
                    | WS_HSCROLL.0
                    | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL | ES_AUTOHSCROLL) as u32,
            ),
            0,
            0,
            0,
            0,
            Some(window),
            Some(HMENU(OUTPUT_ID as usize as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .context("script output text creation failed")?;
    let mut face = [0_u16; 32];
    for (slot, unit) in face.iter_mut().zip("Consolas".encode_utf16()) {
        *slot = unit;
    }
    let font = unsafe {
        CreateFontIndirectW(&LOGFONTW {
            lfHeight: -(FONT_POINTS * dpi / 72),
            lfWeight: FW_NORMAL.0 as i32,
            lfOutPrecision: OUT_DEFAULT_PRECIS,
            lfQuality: CLEARTYPE_QUALITY,
            lfPitchAndFamily: FIXED_PITCH.0 | FF_MODERN.0,
            lfFaceName: face,
            ..Default::default()
        })
    };
    // The window deletes its font when it is destroyed.
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, font.0 as isize) };
    unsafe {
        // Lifts the 32,767-character default; output can be up to 1 MiB.
        SendMessageW(output, EM_SETLIMITTEXT, Some(WPARAM(0)), None);
        SendMessageW(
            output,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        let _ = SetWindowTextW(output, &HSTRING::from(text));
        layout(window);
        let _ = ShowWindow(window, SW_SHOW);
        let _ = SetForegroundWindow(window);
        let _ = SetFocus(Some(output));
    }
    Ok(())
}

unsafe fn layout(window: HWND) {
    let mut client = RECT::default();
    if unsafe { GetClientRect(window, &mut client) }.is_ok()
        && let Ok(output) = unsafe { GetDlgItem(Some(window), OUTPUT_ID) }
    {
        let _ = unsafe { MoveWindow(output, 0, 0, client.right, client.bottom, true) };
    }
}

unsafe extern "system" fn output_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_SIZE => {
            unsafe { layout(window) };
            LRESULT(0)
        }
        WM_SETFOCUS => {
            if let Ok(output) = unsafe { GetDlgItem(Some(window), OUTPUT_ID) } {
                let _ = unsafe { SetFocus(Some(output)) };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let font = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) };
            if font != 0 {
                let _ = unsafe { DeleteObject(HGDIOBJ(font as *mut c_void)) };
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}
