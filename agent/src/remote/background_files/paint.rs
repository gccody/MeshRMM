//! Drawing of the file browser window: ribbon, navigation bar and buttons.
use super::*;

pub(super) fn fill(dc: HDC, rect: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color));
        FillRect(dc, rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

pub(super) fn draw_text(
    dc: HDC,
    text: &str,
    mut rect: RECT,
    flags: DRAW_TEXT_FORMAT,
    color: u32,
    font: HFONT,
) {
    if text.is_empty() {
        return;
    }
    unsafe {
        let old = SelectObject(dc, font.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(color));
        DrawTextW(
            dc,
            &mut text.encode_utf16().collect::<Vec<_>>(),
            &mut rect,
            flags,
        );
        SelectObject(dc, old);
    }
}

pub(super) fn paint(hwnd: HWND, state: &State) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        fill(dc, &r, 0xffffff);
        fill(
            dc,
            &RECT {
                top: 27,
                bottom: 122,
                ..r
            },
            0xf5f6f7,
        );
        fill(
            dc,
            &RECT {
                top: 121,
                bottom: 122,
                ..r
            },
            0xd9d9d9,
        );
        fill(
            dc,
            &RECT {
                top: 161,
                bottom: 162,
                ..r
            },
            0xe5e5e5,
        );
        fill(
            dc,
            &RECT {
                left: 190,
                right: 191,
                top: 162,
                bottom: r.bottom - 26,
            },
            0xe5e5e5,
        );
        fill(
            dc,
            &RECT {
                top: r.bottom - 27,
                bottom: r.bottom - 26,
                ..r
            },
            0xe5e5e5,
        );
        for (label, left, right) in if state.view_tab {
            vec![
                ("Layout", 10, 262),
                ("Sort by", 276, 568),
                ("Show/hide", 582, 820),
                ("Panes", 830, 912),
            ]
        } else {
            vec![
                ("Clipboard", 10, 182),
                ("Organize", 192, 324),
                ("New", 334, 482),
                ("Open", 492, 630),
                ("Select", 640, 908),
            ]
        } {
            fill(
                dc,
                &RECT {
                    left: right,
                    right: right + 1,
                    top: 36,
                    bottom: 111,
                },
                0xd9d9d9,
            );
            draw_text(
                dc,
                label,
                RECT {
                    left,
                    right,
                    top: 101,
                    bottom: 119,
                },
                DT_CENTER | DT_SINGLELINE,
                0x646464,
                state.font,
            );
        }
        for h in [state.location, state.search] {
            let mut box_rect = RECT::default();
            let _ = GetWindowRect(h, &mut box_rect);
            let mut point = POINT {
                x: box_rect.left,
                y: box_rect.top,
            };
            let _ = ScreenToClient(hwnd, &mut point);
            let edge = RECT {
                left: point.x - 3,
                top: point.y - 2,
                right: point.x + box_rect.right - box_rect.left + 2,
                bottom: point.y + box_rect.bottom - box_rect.top + 2,
            };
            let brush = CreateSolidBrush(COLORREF(0xc5c5c5));
            FrameRect(dc, &edge, brush);
            let _ = DeleteObject(brush.into());
        }
        let _ = EndPaint(hwnd, &ps);
    }
}

pub(super) fn draw_navigation(draw: &DRAWITEMSTRUCT, state: &State) {
    if draw.itemID == u32::MAX {
        return;
    }
    let index = draw.itemID as usize;
    let selected = draw.itemState.0 & ODS_SELECTED.0 != 0;
    fill(
        draw.hDC,
        &draw.rcItem,
        if selected { 0xf7e8d5 } else { 0xffffff },
    );
    let mut text = vec![0u16; 512];
    unsafe {
        SendMessageW(
            state.nav,
            LB_GETTEXT,
            Some(WPARAM(index)),
            Some(LPARAM(text.as_mut_ptr() as isize)),
        );
    }
    let text =
        String::from_utf16_lossy(&text[..text.iter().position(|c| *c == 0).unwrap_or(text.len())]);
    let heading = index == 0 || index == 5;
    let left = if heading { 20 } else { 36 };
    if index == 0 {
        draw_text(
            draw.hDC,
            "★",
            RECT {
                left,
                right: left + 20,
                ..draw.rcItem
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            0xdb8f22,
            state.font,
        );
    } else {
        let icon = state.nav_icons[if index == 5 {
            1
        } else if index > 5 {
            2
        } else {
            0
        }];
        if !icon.is_invalid() {
            unsafe {
                let _ = DrawIconEx(
                    draw.hDC,
                    left + 2,
                    draw.rcItem.top + 6,
                    icon,
                    16,
                    16,
                    0,
                    None,
                    DI_NORMAL,
                );
            }
        }
    }
    draw_text(
        draw.hDC,
        &text,
        RECT {
            left: left + 26,
            ..draw.rcItem
        },
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        0x242424,
        state.font,
    );
}

pub(super) fn draw_button(draw: &DRAWITEMSTRUCT, state: &State) {
    let id = draw.CtlID as usize;
    let pressed = draw.itemState.0 & ODS_SELECTED.0 != 0;
    let checked = (id == HOME && !state.view_tab)
        || (id == VIEW && state.view_tab)
        || (id == HIDDEN && state.hidden)
        || (id == EXTENSIONS && state.extensions);
    let disabled = draw.itemState.0 & ODS_DISABLED.0 != 0;
    fill(
        draw.hDC,
        &draw.rcItem,
        if pressed || checked {
            0xf7e8d5
        } else if RIBBON.iter().any(|i| i.id == id) {
            0xf5f6f7
        } else {
            0xffffff
        },
    );
    let color = if id == FILE_MENU {
        0xffffff
    } else if disabled {
        0xaaaaaa
    } else {
        0x303030
    };
    if id == FILE_MENU {
        fill(draw.hDC, &draw.rcItem, 0xc06700);
    }
    if let Some(item) = RIBBON.iter().find(|i| i.id == id) {
        if id == NEW_FOLDER {
            let x = (draw.rcItem.right - 32) / 2;
            fill(
                draw.hDC,
                &RECT {
                    left: x,
                    top: 13,
                    right: x + 13,
                    bottom: 20,
                },
                0x24c6f5,
            );
            fill(
                draw.hDC,
                &RECT {
                    left: x,
                    top: 18,
                    right: x + 32,
                    bottom: 39,
                },
                0x24c6f5,
            );
        } else {
            draw_text(
                draw.hDC,
                &String::from_utf16_lossy(&[item.glyph]),
                RECT {
                    top: 9,
                    bottom: 41,
                    ..draw.rcItem
                },
                DT_CENTER | DT_SINGLELINE,
                if id == NEW_FOLDER {
                    0x00b6ed
                } else if id == DELETE {
                    0x4444cb
                } else {
                    0x92652d
                },
                state.symbols,
            );
        }
        draw_text(
            draw.hDC,
            item.label,
            RECT {
                top: 48,
                bottom: 68,
                ..draw.rcItem
            },
            DT_CENTER | DT_SINGLELINE,
            color,
            state.font,
        );
    } else {
        let label = window_text(draw.hwndItem);
        draw_text(
            draw.hDC,
            &label,
            draw.rcItem,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
            color,
            state.font,
        );
    }
    if draw.itemState.0 & ODS_FOCUS.0 != 0 {
        unsafe {
            let _ = DrawFocusRect(draw.hDC, &draw.rcItem);
        }
    }
}
