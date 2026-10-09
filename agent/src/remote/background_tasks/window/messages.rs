//! The Task Manager window procedure and its message handlers.
use super::*;

pub(super) unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let cell = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if message == WM_NCCALCSIZE {
            return client_area(hwnd, message, wparam, lparam);
        }
        if message == WM_GETMINMAXINFO && lparam.0 != 0 {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            let area = crate::remote::background::work_area();
            info.ptMaxTrackSize = POINT {
                x: area.right - area.left,
                y: area.bottom - area.top,
            };
            let compact = !cell.is_null() && (*cell).try_borrow().map_or(true, |s| s.compact);
            info.ptMinTrackSize = if compact {
                POINT { x: 280, y: 200 }
            } else {
                POINT { x: 650, y: 390 }
            };
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        if !cell.is_null()
            && let Some(result) = state_message(hwnd, &*cell, message, wparam, lparam)
        {
            return result;
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

unsafe fn client_area(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let rect = if wparam.0 != 0 {
            &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
        } else {
            &mut *(lparam.0 as *mut RECT)
        };
        let outer = crate::remote::background::frame_rect(hwnd, *rect);
        let border = crate::remote::background::frame_border(hwnd);
        let result = DefWindowProcW(hwnd, message, wparam, lparam);
        rect.left = outer.left + border;
        rect.right = outer.right - border;
        rect.bottom = outer.bottom - border;
        result
    }
}

/// Handles messages that need the window state. `None` falls through to
/// DefWindowProc.
unsafe fn state_message(
    hwnd: HWND,
    cell: &RefCell<State>,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    unsafe {
        if message == WM_DRAWITEM
            && lparam.0 != 0
            && let Ok(state) = cell.try_borrow()
        {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            if item.CtlID == COMPACT as u32 {
                draw_compact_button(&state, item);
                return Some(LRESULT(1));
            }
        }
        if message == WM_NCHITTEST {
            return Some(hit_test(hwnd, message, wparam, lparam));
        }
        if message == WM_NCLBUTTONDOWN
            && matches!(wparam.0 as u32, HTCLOSE | HTMAXBUTTON | HTMINBUTTON)
        {
            let command = match wparam.0 as u32 {
                HTCLOSE => SC_CLOSE,
                HTMINBUTTON => SC_MINIMIZE,
                _ => {
                    if IsZoomed(hwnd).as_bool() {
                        SC_RESTORE
                    } else {
                        SC_MAXIMIZE
                    }
                }
            };
            let _ = PostMessageW(
                Some(hwnd),
                WM_SYSCOMMAND,
                WPARAM(command as usize),
                LPARAM(0),
            );
            return Some(LRESULT(0));
        }
        if matches!(message, WM_NCPAINT | WM_NCACTIVATE | WM_PRINT) {
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            if let Ok(state) = cell.try_borrow() {
                let dc = if message == WM_PRINT {
                    HDC(wparam.0 as *mut _)
                } else {
                    GetWindowDC(Some(hwnd))
                };
                if !dc.is_invalid() {
                    caption(&state, dc);
                    if message != WM_PRINT {
                        ReleaseDC(Some(hwnd), dc);
                    }
                }
            }
            return Some(result);
        }
        if message == WM_NOTIFY && lparam.0 != 0 {
            let notification = &*(lparam.0 as *const NMHDR);
            if notification.code == NM_CUSTOMDRAW
                && let Ok(state) = cell.try_borrow()
                && notification.hwndFrom == state.list
                && let Some(result) =
                    list_custom_draw(&state, &mut *(lparam.0 as *mut NMLVCUSTOMDRAW))
            {
                return Some(result);
            }
        }
        let Ok(mut state) = cell.try_borrow_mut() else {
            return None;
        };
        let old_notice = state.notice.clone();
        let result = match message {
            WM_TIMER => {
                if wparam.0 == 1 {
                    state.refresh();
                }
                state.poll()
            }
            WM_SIZE => {
                state.layout();
                Ok(())
            }
            WM_COMMAND => state.command(wparam.0 & 0xffff),
            WM_NOTIFY if lparam.0 != 0 => {
                notification(hwnd, &mut state, lparam);
                Ok(())
            }
            WM_CONTEXTMENU => {
                let point = if lparam.0 == -1 {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    p
                } else {
                    POINT {
                        x: lparam.0 as i16 as i32,
                        y: (lparam.0 >> 16) as i16 as i32,
                    }
                };
                context_menu(&state, point);
                Ok(())
            }
            _ => return None,
        };
        if let Err(e) = result {
            state.notice = format!("{e:#}");
        }
        state.footer();
        if message == WM_COMMAND || state.notice != old_notice {
            state.layout();
        }
        Some(LRESULT(0))
    }
}

unsafe fn draw_compact_button(state: &State, item: &DRAWITEMSTRUCT) {
    unsafe {
        fill(item.hDC, &item.rcItem, WHITE);
        SelectObject(item.hDC, state.font.into());
        let pen = CreatePen(PS_SOLID, 1, COLORREF(0x999999));
        let old = SelectObject(item.hDC, pen.into());
        let brush = SelectObject(item.hDC, GetStockObject(WHITE_BRUSH));
        let _ = Ellipse(item.hDC, 1, 3, 19, 21);
        SelectObject(item.hDC, brush);
        SelectObject(item.hDC, old);
        let _ = DeleteObject(pen.into());
        draw_text(
            item.hDC,
            if state.compact { "⌄" } else { "⌃" },
            RECT {
                left: 1,
                top: 2,
                right: 19,
                bottom: 20,
            },
            COLORREF(0x555555),
            DT_CENTER,
        );
        draw_text(
            item.hDC,
            if state.compact {
                "More details"
            } else {
                "Fewer details"
            },
            RECT {
                left: 25,
                ..item.rcItem
            },
            COLORREF(0x222222),
            DT_LEFT,
        );
        if item.itemState.0 & ODS_FOCUS.0 != 0 {
            let _ = DrawFocusRect(item.hDC, &item.rcItem);
        }
    }
}

/// Adds resize edges and the custom caption buttons to the default hit test.
unsafe fn hit_test(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let hit = DefWindowProcW(hwnd, message, wparam, lparam);
        let mut window = RECT::default();
        let _ = GetWindowRect(hwnd, &mut window);
        let bounds = crate::remote::background::frame_rect(hwnd, window);
        let x = lparam.0 as i16 as i32 - bounds.left;
        let y = (lparam.0 >> 16) as i16 as i32 - bounds.top;
        if !IsZoomed(hwnd).as_bool() {
            let border = crate::remote::background::RESIZE_BORDER;
            let left = x < border;
            let right = x >= bounds.right - bounds.left - border;
            let top = y < border;
            let bottom = y >= bounds.bottom - bounds.top - border;
            let edge = match (left, right, top, bottom) {
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
                return LRESULT(edge as isize);
            }
        }
        if (4..30).contains(&y) && x >= bounds.right - bounds.left - 139 {
            return LRESULT(if x >= bounds.right - bounds.left - 47 {
                HTCLOSE
            } else if x >= bounds.right - bounds.left - 93 {
                HTMAXBUTTON
            } else {
                HTMINBUTTON
            } as isize);
        }
        hit
    }
}

fn list_custom_draw(state: &State, draw: &mut NMLVCUSTOMDRAW) -> Option<LRESULT> {
    match draw.nmcd.dwDrawStage {
        CDDS_PREPAINT => Some(LRESULT(CDRF_NOTIFYITEMDRAW as isize)),
        CDDS_ITEMPREPAINT => Some(LRESULT(CDRF_NOTIFYSUBITEMDRAW as isize)),
        // Keep the native cell renderer. Geometry/selection
        // queries re-entering the list from this paint callback
        // caused a user32 callback crash on the Session 0 desktop.
        stage if stage.0 == CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0 => {
            let row = state.rows.get(draw.nmcd.dwItemSpec)?;
            draw.clrText = if row.section {
                BLUE
            } else {
                COLORREF(0x222222)
            };
            draw.clrTextBk = if !row.section
                && state.tab == Tab::Processes
                && draw.iSubItem >= 2
                && !state.compact
            {
                let heat = row
                    .heat
                    .get(draw.iSubItem as usize)
                    .copied()
                    .unwrap_or(0.0)
                    .sqrt()
                    .clamp(0.0, 1.0);
                COLORREF(
                    255 | ((249.0 - 65.0 * heat) as u32) << 8
                        | ((215.0 - 170.0 * heat) as u32) << 16,
                )
            } else {
                WHITE
            };
            unsafe {
                SelectObject(
                    draw.nmcd.hdc,
                    if row.section {
                        state.heading_font
                    } else {
                        state.font
                    }
                    .into(),
                );
            }
            Some(LRESULT(CDRF_NEWFONT as isize))
        }
        _ => None,
    }
}

unsafe fn notification(hwnd: HWND, state: &mut State, lparam: LPARAM) {
    unsafe {
        let notification = &*(lparam.0 as *const NMHDR);
        if notification.hwndFrom == state.tabs && notification.code == TCN_SELCHANGE {
            let index = SendMessageW(state.tabs, TCM_GETCURSEL, None, None).0;
            state.change_tab(Tab::from_index(index as usize));
        } else if notification.hwndFrom == state.list {
            match notification.code {
                LVN_COLUMNCLICK => {
                    let info = &*(lparam.0 as *const NMLISTVIEW);
                    let column = info.iSubItem as usize;
                    if state.sort == column {
                        state.descending = !state.descending;
                    } else {
                        state.sort = column;
                        state.descending = false;
                    }
                    state.rebuild();
                }
                NM_DBLCLK => state.expand(),
                NM_CLICK => {
                    let info = &*(lparam.0 as *const NMITEMACTIVATE);
                    if info.ptAction.x < 28 {
                        state.expand();
                    }
                }
                LVN_KEYDOWN => {
                    let info = &*(lparam.0 as *const NMLVKEYDOWN);
                    match info.wVKey {
                        0x2e => {
                            let _ =
                                PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(END_TASK), LPARAM(0));
                        }
                        0x25 | 0x27 | 0x0d => state.expand(),
                        0x74 => state.refresh(),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}
