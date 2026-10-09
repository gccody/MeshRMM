use super::{ICON_SIZE, PIN_WIDTH, PINS, TASKBAR_HEIGHT};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::WindowsAndMessaging::*;

pub(super) unsafe extern "system" fn tooltip_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if matches!(message, WM_PAINT | WM_PRINT | WM_PRINTCLIENT) {
            let mut paint = PAINTSTRUCT::default();
            let dc = if message == WM_PAINT {
                BeginPaint(hwnd, &mut paint)
            } else {
                HDC(wparam.0 as *mut _)
            };
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            FillRect(dc, &rect, HBRUSH(GetStockObject(WHITE_BRUSH).0));
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0));
            let font = SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
            let mut label = [0_u16; 64];
            let length = GetWindowTextW(hwnd, &mut label) as usize;
            rect.left += 5;
            DrawTextW(
                dc,
                &mut label[..length],
                &mut rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(dc, font);
            if message == WM_PAINT {
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) unsafe extern "system" fn launcher_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_ERASEBKGND {
            let dc = HDC(wparam.0 as *mut _);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let brush = CreateSolidBrush(COLORREF(0x3f3933));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush.into());
            return LRESULT(1);
        }
        if matches!(message, WM_PAINT | WM_PRINTCLIENT) {
            let mut paint = PAINTSTRUCT::default();
            let dc = if message == WM_PAINT {
                BeginPaint(hwnd, &mut paint)
            } else {
                HDC(wparam.0 as *mut _)
            };
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let brush = CreateSolidBrush(COLORREF(0x3f3933));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush.into());
            if message == WM_PAINT {
                let _ = EndPaint(hwnd, &paint);
            }
            return LRESULT(0);
        }
        if message == WM_DRAWITEM && lparam.0 != 0 {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            let brush = CreateSolidBrush(COLORREF(if item.itemState.0 & ODS_SELECTED.0 != 0 {
                0x655c53
            } else {
                0x3f3933
            }));
            FillRect(item.hDC, &item.rcItem, brush);
            let _ = DeleteObject(brush.into());
            let icon = HICON(GetWindowLongPtrW(item.hwndItem, GWLP_USERDATA) as *mut _);
            if !icon.is_invalid() {
                let x = if (item.CtlID as usize) <= PINS.len() {
                    (PIN_WIDTH - ICON_SIZE) / 2
                } else {
                    (item.rcItem.right - item.rcItem.left - ICON_SIZE) / 2
                };
                let _ = DrawIconEx(
                    item.hDC,
                    x,
                    (TASKBAR_HEIGHT - 8 - ICON_SIZE) / 2,
                    icon,
                    ICON_SIZE,
                    ICON_SIZE,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
            if (item.CtlID as usize) > PINS.len() || item.itemState.0 & ODS_SELECTED.0 != 0 {
                let brush = CreateSolidBrush(COLORREF(0xcbb54c));
                let mut line = item.rcItem;
                line.top = line.bottom - 3;
                FillRect(item.hDC, &line, brush);
                let _ = DeleteObject(brush.into());
            }
            return LRESULT(1);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}
