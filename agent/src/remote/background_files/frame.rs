//! Flat Explorer frame, including the posted-input resize/minimize adapter.
use super::*;

pub(super) fn hit(hwnd: HWND, point: POINT) -> u32 {
    unsafe {
        let mut bounds = RECT::default();
        let _ = GetWindowRect(hwnd, &mut bounds);
        let x = point.x - bounds.left;
        let y = point.y - bounds.top;
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        if !IsZoomed(hwnd).as_bool() {
            let edge = match (x < 4, x >= width - 4, y < 4, y >= height - 4) {
                (true, _, true, _) => HTTOPLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, _, _, _) => HTLEFT,
                (_, true, _, _) => HTRIGHT,
                (_, _, true, _) => HTTOP,
                (_, _, _, true) => HTBOTTOM,
                _ => HTNOWHERE,
            };
            if edge != HTNOWHERE {
                return edge;
            }
        }
        if y < 31 {
            if x >= width - 47 {
                HTCLOSE
            } else if x >= width - 93 {
                HTMAXBUTTON
            } else if x >= width - 139 {
                HTMINBUTTON
            } else {
                HTCAPTION
            }
        } else {
            HTCLIENT
        }
    }
}
pub(super) fn paint(hwnd: HWND, dc: HDC, font: HFONT) {
    unsafe {
        let mut bounds = RECT::default();
        let _ = GetWindowRect(hwnd, &mut bounds);
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        fill(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: 31,
            },
            0xffffff,
        );
        let pen = CreateSolidBrush(COLORREF(0xb4b4b4));
        FrameRect(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            pen,
        );
        let _ = DeleteObject(pen.into());
        // The folder mark uses the same gold as Explorer's standard folder icon.
        fill(
            dc,
            &RECT {
                left: 10,
                top: 10,
                right: 18,
                bottom: 12,
            },
            0x24c6f5,
        );
        fill(
            dc,
            &RECT {
                left: 10,
                top: 12,
                right: 26,
                bottom: 23,
            },
            0x24c6f5,
        );
        draw_text(
            dc,
            "File Explorer",
            RECT {
                left: 34,
                top: 1,
                right: width - 140,
                bottom: 30,
            },
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
            0x242424,
            font,
        );
        let line = CreatePen(PS_SOLID, 1, COLORREF(0x242424));
        let old = SelectObject(dc, line.into());
        for index in 0..3 {
            let x = width - 139 + index * 46 + 18;
            if index == 0 {
                let _ = MoveToEx(dc, x, 16, None);
                let _ = LineTo(dc, x + 10, 16);
            } else if index == 1 {
                let brush = SelectObject(dc, GetStockObject(HOLLOW_BRUSH));
                let _ = Rectangle(dc, x, 11, x + 10, 21);
                SelectObject(dc, brush);
            } else {
                let _ = MoveToEx(dc, x, 11, None);
                let _ = LineTo(dc, x + 10, 21);
                let _ = MoveToEx(dc, x + 9, 11, None);
                let _ = LineTo(dc, x - 1, 21);
            }
        }
        SelectObject(dc, old);
        let _ = DeleteObject(line.into());
    }
}
pub(super) fn resize(edge: u32, start: POINT, mut bounds: RECT, point: POINT) -> RECT {
    let dx = point.x - start.x;
    let dy = point.y - start.y;
    if matches!(edge, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
        bounds.left = (bounds.left + dx).min(bounds.right - 940);
    }
    if matches!(edge, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT) {
        bounds.right = (bounds.right + dx).max(bounds.left + 940);
    }
    if matches!(edge, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
        bounds.top = (bounds.top + dy).min(bounds.bottom - 400);
    }
    if matches!(edge, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT) {
        bounds.bottom = (bounds.bottom + dy).max(bounds.top + 400);
    }
    bounds
}
