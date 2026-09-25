//! The Task Manager window: creation, window procedures, menus and scrollbars.
use super::*;

pub(super) unsafe fn captured_scrollbars(list: HWND, dc: HDC) {
    unsafe {
        if !IsWindowVisible(list).as_bool() {
            return;
        }
        let mut frame = RECT::default();
        if GetWindowRect(list, &mut frame).is_err() {
            return;
        }
        let saved = SaveDC(dc);
        let _ = SelectClipRgn(dc, None);
        let _ = SetViewportOrgEx(dc, 0, 0, None);
        for (id, vertical) in [(OBJID_VSCROLL, true), (OBJID_HSCROLL, false)] {
            let mut info = SCROLLBARINFO {
                cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
                ..Default::default()
            };
            if GetScrollBarInfo(list, id, &mut info).is_err() || info.rgstate[0] & 0x8000 != 0
            // STATE_SYSTEM_INVISIBLE
            {
                continue;
            }
            let mut bar = info.rcScrollBar;
            let _ = OffsetRect(&mut bar, -frame.left, -frame.top);
            fill(dc, &bar, COLORREF(0xf0f0f0));
            let mut first = bar;
            let mut last = bar;
            let mut thumb = bar;
            if vertical {
                first.bottom = first.top + info.dxyLineButton;
                last.top = last.bottom - info.dxyLineButton;
                thumb.top = bar.top + info.xyThumbTop;
                thumb.bottom = bar.top + info.xyThumbBottom;
                thumb.left += 2;
                thumb.right -= 2;
            } else {
                first.right = first.left + info.dxyLineButton;
                last.left = last.right - info.dxyLineButton;
                thumb.left = bar.left + info.xyThumbTop;
                thumb.right = bar.left + info.xyThumbBottom;
                thumb.top += 2;
                thumb.bottom -= 2;
            }
            for (rect, arrow, part) in [
                (
                    &mut first,
                    if vertical {
                        DFCS_SCROLLUP
                    } else {
                        DFCS_SCROLLLEFT
                    },
                    1,
                ),
                (
                    &mut last,
                    if vertical {
                        DFCS_SCROLLDOWN
                    } else {
                        DFCS_SCROLLRIGHT
                    },
                    5,
                ),
            ] {
                let flags = arrow
                    | DFCS_FLAT
                    | if info.rgstate[part] & 1 != 0 {
                        DFCS_INACTIVE
                    } else {
                        DFCS_STATE(0)
                    }
                    | if info.rgstate[part] & 8 != 0 {
                        DFCS_PUSHED
                    } else {
                        DFCS_STATE(0)
                    };
                let _ = DrawFrameControl(dc, rect, DFC_SCROLL, flags);
            }
            if info.xyThumbBottom > info.xyThumbTop && info.rgstate[3] & 0x8000 == 0 {
                fill(dc, &thumb, COLORREF(0xc8c8c8));
            }
        }
        let _ = RestoreDC(dc, saved);
    }
}

#[derive(Clone, Copy)]
pub(super) enum ScrollInput {
    Drag {
        vertical: bool,
        start: i32,
        position: i32,
        travel: i32,
        minimum: i32,
        maximum: i32,
    },
    Repeat {
        vertical: bool,
        command: SCROLLBAR_COMMAND,
    },
}

pub(super) const SCROLL_REPEAT: usize = 0x4d524d54;

pub(super) unsafe fn scroll_command(
    hwnd: HWND,
    vertical: bool,
    command: SCROLLBAR_COMMAND,
    position: i32,
) {
    unsafe {
        if command == SB_THUMBPOSITION {
            // Report list views consult native tracking state for thumb messages;
            // posted background input never enters that modal tracking loop.
            // Their vertical range is in rows, while LVM_SCROLL takes pixels.
            let delta = position - GetScrollPos(hwnd, if vertical { SB_VERT } else { SB_HORZ });
            let (x, y) = if vertical {
                let top = SendMessageW(hwnd, LVM_GETTOPINDEX, None, None).0;
                let mut rect = RECT::default();
                SendMessageW(
                    hwnd,
                    LVM_GETITEMRECT,
                    Some(WPARAM(top.max(0) as usize)),
                    Some(LPARAM((&mut rect as *mut RECT) as isize)),
                );
                (0, delta.saturating_mul((rect.bottom - rect.top).max(1)))
            } else {
                (delta, 0)
            };
            SendMessageW(
                hwnd,
                LVM_SCROLL,
                Some(WPARAM(x as usize)),
                Some(LPARAM(y as isize)),
            );
            return;
        }
        SendMessageW(
            hwnd,
            if vertical { WM_VSCROLL } else { WM_HSCROLL },
            Some(WPARAM(
                command.0 as usize | ((position.clamp(0, 65535) as usize) << 16),
            )),
            None,
        );
    }
}

pub(in crate::remote) fn install_list_scrollbars(list: HWND) -> anyhow::Result<()> {
    unsafe {
        let scroll_input = Box::into_raw(Box::new(Cell::new(None::<ScrollInput>)));
        if !SetWindowSubclass(list, Some(list_paint), 1, scroll_input as usize).as_bool() {
            drop(Box::from_raw(scroll_input));
            anyhow::bail!("Could not initialize captured list scrollbars");
        }
    }
    Ok(())
}

pub(super) unsafe extern "system" fn panel_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let parent = GetParent(hwnd).unwrap_or_default();
        let cell = GetWindowLongPtrW(parent, GWLP_USERDATA) as *const RefCell<State>;
        if !cell.is_null() {
            if message == WM_LBUTTONUP
                && let Ok(mut state) = (*cell).try_borrow_mut()
                && hwnd == state.graph
            {
                let x = lparam.0 as i16 as i32;
                let y = (lparam.0 >> 16) as i16 as i32;
                if (0..180).contains(&x) && (10..274).contains(&y) {
                    state.performance = ((y - 10) / 66) as usize;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            if matches!(message, WM_PAINT | WM_PRINTCLIENT)
                && let Ok(state) = (*cell).try_borrow()
            {
                let mut paint = PAINTSTRUCT::default();
                let dc = if message == WM_PAINT {
                    BeginPaint(hwnd, &mut paint)
                } else {
                    HDC(wparam.0 as *mut _)
                };
                if hwnd == state.header {
                    header_paint(&state, hwnd, dc);
                } else {
                    performance_paint(&state, hwnd, dc);
                }
                if message == WM_PAINT {
                    let _ = EndPaint(hwnd, &paint);
                }
                return LRESULT(0);
            }
            if message == WM_LBUTTONUP
                && let Ok(mut state) = (*cell).try_borrow_mut()
                && hwnd == state.header
            {
                let click = (lparam.0 as i16) as i32 + GetScrollPos(state.list, SB_HORZ);
                let mut x = 0;
                for i in 0..6 {
                    x += SendMessageW(state.list, LVM_GETCOLUMNWIDTH, Some(WPARAM(i)), None).0
                        as i32;
                    if click < x {
                        if state.sort == i {
                            state.descending = !state.descending;
                        } else {
                            state.sort = i;
                            state.descending = i >= 2;
                        }
                        state.rebuild();
                        break;
                    }
                }
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) unsafe fn context_menu(state: &State, point: POINT) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let items: Vec<(usize, &str)> = match state.tab {
            Tab::Processes | Tab::Details => vec![
                (END_TASK, "End task"),
                (END_TREE, "End process tree"),
                (GO_DETAILS, "Go to details"),
                (PRIORITY_LOW, "Set priority: Low"),
                (PRIORITY_NORMAL, "Set priority: Normal"),
                (PRIORITY_HIGH, "Set priority: High"),
                (COPY, "Show process information"),
            ],
            Tab::Services => vec![
                (SERVICE_START, "Start"),
                (SERVICE_STOP, "Stop"),
                (SERVICE_RESTART, "Restart"),
                (GO_DETAILS, "Go to details"),
            ],
            Tab::Startup => vec![(END_TASK, "Enable / Disable"), (COPY, "Show command line")],
            Tab::Users => vec![
                (END_TASK, "Disconnect / End task"),
                (GO_DETAILS, "Go to details"),
            ],
            Tab::History => vec![(RESET_HISTORY, "Delete usage history")],
            _ => vec![],
        };
        for (id, text) in items {
            let text = wide(text);
            let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(text.as_ptr()));
        }
        // TPM_RETURNCMD avoids synchronous command re-entry while borrowing state.
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            state.hwnd,
            None,
        )
        .0;
        let _ = DestroyMenu(menu);
        if command != 0 {
            let _ = PostMessageW(
                Some(state.hwnd),
                WM_COMMAND,
                WPARAM(command as usize),
                LPARAM(0),
            );
        }
    }
}

pub(super) unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let cell = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if message == WM_NCCALCSIZE {
            let rect = if wparam.0 != 0 {
                &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
            } else {
                &mut *(lparam.0 as *mut RECT)
            };
            let outer = *rect;
            let result = DefWindowProcW(hwnd, message, wparam, lparam);
            rect.left = outer.left + 1;
            rect.right = outer.right - 1;
            rect.bottom = outer.bottom - 1;
            return result;
        }
        if message == WM_GETMINMAXINFO && lparam.0 != 0 {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMaxPosition = POINT { x: 0, y: 0 };
            info.ptMaxSize = POINT {
                x: meshrmm_remote_screen::background::WIDTH as i32,
                y: meshrmm_remote_screen::background::HEIGHT as i32
                    - crate::remote::background::TASKBAR_HEIGHT,
            };
            info.ptMaxTrackSize = info.ptMaxSize;
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
        if !cell.is_null() {
            // Background input routes caption clicks as client messages except
            // for the workspace's move/maximize/close handling.
            if message == WM_LBUTTONDOWN {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut point);
                let hit = SendMessageW(
                    hwnd,
                    WM_NCHITTEST,
                    None,
                    Some(LPARAM(
                        ((point.y as u32) << 16 | (point.x as u32 & 0xffff)) as isize,
                    )),
                );
                if hit.0 == HTMINBUTTON as isize {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(SC_MINIMIZE as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                if matches!(
                    hit.0 as u32,
                    HTLEFT
                        | HTRIGHT
                        | HTTOP
                        | HTTOPLEFT
                        | HTTOPRIGHT
                        | HTBOTTOM
                        | HTBOTTOMLEFT
                        | HTBOTTOMRIGHT
                ) && let Ok(mut state) = (*cell).try_borrow_mut()
                {
                    let mut bounds = RECT::default();
                    if GetWindowRect(hwnd, &mut bounds).is_ok() {
                        state.resizing = Some((hit.0 as u32, point, bounds));
                    }
                    return LRESULT(0);
                }
            }
            if message == WM_DRAWITEM
                && lparam.0 != 0
                && let Ok(state) = (*cell).try_borrow()
            {
                let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if item.CtlID == COMPACT as u32 {
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
                    return LRESULT(1);
                }
            }
            if message == WM_NCHITTEST {
                let hit = DefWindowProcW(hwnd, message, wparam, lparam);
                let mut bounds = RECT::default();
                let _ = GetWindowRect(hwnd, &mut bounds);
                let x = lparam.0 as i16 as i32 - bounds.left;
                let y = (lparam.0 >> 16) as i16 as i32 - bounds.top;
                if !IsZoomed(hwnd).as_bool() {
                    let left = x < 4;
                    let right = x >= bounds.right - bounds.left - 4;
                    let top = y < 4;
                    let bottom = y >= bounds.bottom - bounds.top - 4;
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
                return hit;
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
                return LRESULT(0);
            }
            if matches!(message, WM_NCPAINT | WM_NCACTIVATE | WM_PRINT) {
                let result = DefWindowProcW(hwnd, message, wparam, lparam);
                if let Ok(state) = (*cell).try_borrow() {
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
                return result;
            }
            if message == WM_NOTIFY && lparam.0 != 0 {
                let notification = &*(lparam.0 as *const NMHDR);
                if notification.code == NM_CUSTOMDRAW
                    && let Ok(state) = (*cell).try_borrow()
                    && notification.hwndFrom == state.list
                {
                    let draw = &mut *(lparam.0 as *mut NMLVCUSTOMDRAW);
                    match draw.nmcd.dwDrawStage {
                        CDDS_PREPAINT => return LRESULT(CDRF_NOTIFYITEMDRAW as isize),
                        CDDS_ITEMPREPAINT => {
                            return LRESULT(CDRF_NOTIFYSUBITEMDRAW as isize);
                        }
                        // Keep the native cell renderer. Geometry/selection
                        // queries re-entering the list from this paint callback
                        // caused a user32 callback crash on the Session 0 desktop.
                        stage if stage.0 == CDDS_ITEMPREPAINT.0 | CDDS_SUBITEM.0 => {
                            if let Some(row) = state.rows.get(draw.nmcd.dwItemSpec) {
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
                                SelectObject(
                                    draw.nmcd.hdc,
                                    if row.section {
                                        state.heading_font
                                    } else {
                                        state.font
                                    }
                                    .into(),
                                );
                                return LRESULT(CDRF_NEWFONT as isize);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Ok(mut state) = (*cell).try_borrow_mut() {
                let old_notice = state.notice.clone();
                let mut handled = true;
                let result = match message {
                    WM_MOUSEMOVE => {
                        if let Some((edge, origin, mut bounds)) = state.resizing {
                            let mut point = POINT {
                                x: lparam.0 as i16 as i32,
                                y: (lparam.0 >> 16) as i16 as i32,
                            };
                            let _ = ClientToScreen(hwnd, &mut point);
                            let (dx, dy) = (point.x - origin.x, point.y - origin.y);
                            let (min_width, min_height) = if state.compact {
                                (280, 200)
                            } else {
                                (650, 390)
                            };
                            if matches!(edge, HTLEFT | HTTOPLEFT | HTBOTTOMLEFT) {
                                bounds.left = (bounds.left + dx).min(bounds.right - min_width);
                            }
                            if matches!(edge, HTRIGHT | HTTOPRIGHT | HTBOTTOMRIGHT) {
                                bounds.right = (bounds.right + dx).max(bounds.left + min_width);
                            }
                            if matches!(edge, HTTOP | HTTOPLEFT | HTTOPRIGHT) {
                                bounds.top = (bounds.top + dy).min(bounds.bottom - min_height);
                            }
                            if matches!(edge, HTBOTTOM | HTBOTTOMLEFT | HTBOTTOMRIGHT) {
                                bounds.bottom = (bounds.bottom + dy).max(bounds.top + min_height);
                            }
                            let _ = SetWindowPos(
                                hwnd,
                                None,
                                bounds.left,
                                bounds.top,
                                bounds.right - bounds.left,
                                bounds.bottom - bounds.top,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                            state.layout();
                        }
                        Ok(())
                    }
                    WM_LBUTTONUP | WM_CANCELMODE => {
                        state.resizing = None;
                        Ok(())
                    }
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
                        let notification = &*(lparam.0 as *const NMHDR);
                        if notification.hwndFrom == state.tabs && notification.code == TCN_SELCHANGE
                        {
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
                                            let _ = PostMessageW(
                                                Some(hwnd),
                                                WM_COMMAND,
                                                WPARAM(END_TASK),
                                                LPARAM(0),
                                            );
                                        }
                                        0x25 | 0x27 | 0x0d => state.expand(),
                                        0x74 => state.refresh(),
                                        _ => {}
                                    }
                                }
                                _ => {}
                            }
                        }
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
                    _ => {
                        handled = false;
                        Ok(())
                    }
                };
                if let Err(e) = result {
                    state.notice = format!("{e:#}");
                }
                if handled {
                    state.footer();
                    if message == WM_COMMAND || state.notice != old_notice {
                        state.layout();
                    }
                    return LRESULT(0);
                }
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) unsafe fn menu() -> anyhow::Result<HMENU> {
    unsafe {
        let bar = CreateMenu()?;
        for (label, entries) in [
            ("&File", vec![(RUN_TASK, "Run &new task"), (EXIT, "E&xit")]),
            ("&Options", vec![(TOPMOST, "Always on &top")]),
            (
                "&View",
                vec![
                    (REFRESH, "&Refresh now\tF5"),
                    (SPEED_HIGH, "Update speed: High"),
                    (SPEED_NORMAL, "Update speed: Normal"),
                    (SPEED_LOW, "Update speed: Low"),
                    (SPEED_PAUSED, "Update speed: Paused"),
                    (GROUP, "Group by type"),
                ],
            ),
        ] {
            let submenu = CreatePopupMenu()?;
            for (id, label) in entries {
                let text = wide(label);
                AppendMenuW(submenu, MF_STRING, id, PCWSTR(text.as_ptr()))?;
            }
            let text = wide(label);
            AppendMenuW(bar, MF_POPUP, submenu.0 as usize, PCWSTR(text.as_ptr()))?;
        }
        Ok(bar)
    }
}

pub(super) unsafe fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    id: usize,
    style: WINDOW_STYLE,
    font: HFONT,
) -> anyhow::Result<HWND> {
    unsafe {
        let text = wide(text);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(text.as_ptr()),
            WS_CHILD | style,
            0,
            0,
            100,
            25,
            Some(parent),
            Some(HMENU(id as *mut _)),
            None,
            None,
        )?;
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        Ok(hwnd)
    }
}

pub(super) fn create_window() -> anyhow::Result<Box<RefCell<State>>> {
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_TAB_CLASSES,
        })
        .ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: w!("MeshRMMBackgroundTasks"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            ensure!(
                GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
                "Could not register Task Manager"
            );
        }
        let panel = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            lpszClassName: w!("MeshRMMTaskPanel"),
            hCursor: class.hCursor,
            ..Default::default()
        };
        if RegisterClassW(&panel) == 0 {
            ensure!(
                GetLastError() == ERROR_CLASS_ALREADY_EXISTS,
                "Could not register Task Manager panels"
            );
        }
        let font = CreateFontW(
            -12,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            class.lpszClassName,
            w!("Task Manager"),
            WS_OVERLAPPEDWINDOW,
            40,
            24,
            650,
            480,
            None,
            Some(menu()?),
            None,
            None,
        )?;
        let list = control(
            hwnd,
            w!("SysListView32"),
            "",
            LIST,
            WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
            font,
        )?;
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
            )),
        );
        SendMessageW(list, LVM_SETBKCOLOR, None, Some(LPARAM(WHITE.0 as isize)));
        SendMessageW(
            list,
            LVM_SETTEXTBKCOLOR,
            None,
            Some(LPARAM(WHITE.0 as isize)),
        );
        let images = ImageList_Create(16, 28, ILC_COLOR24 | ILC_MASK, 1, 16);
        padded_icon(images, LoadIconW(None, IDI_APPLICATION)?);
        SendMessageW(
            list,
            LVM_SETIMAGELIST,
            Some(WPARAM(LVSIL_SMALL as usize)),
            Some(LPARAM(images.0 as isize)),
        );
        let tabs = control(
            hwnd,
            w!("SysTabControl32"),
            "",
            TABS,
            WS_VISIBLE | WS_TABSTOP,
            font,
        )?;
        for (index, name) in TAB_NAMES.iter().enumerate() {
            let mut text = wide(name);
            let item = TCITEMW {
                mask: TCIF_TEXT,
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                tabs,
                TCM_INSERTITEMW,
                Some(WPARAM(index)),
                Some(LPARAM((&item as *const TCITEMW) as isize)),
            );
        }
        install_list_scrollbars(list)?;
        let status = control(
            hwnd,
            w!("STATIC"),
            "Loading processes…",
            109,
            WS_VISIBLE,
            font,
        )?;
        for (id, label) in [
            (COMPACT, "⌃  Fewer details"),
            (REFRESH, "Refresh"),
            (END_TASK, "End task"),
            (CANCEL, "Cancel"),
            (RUN, "Run as SYSTEM"),
        ] {
            control(
                hwnd,
                w!("BUTTON"),
                label,
                id,
                (if id == REFRESH {
                    WINDOW_STYLE(0)
                } else {
                    WS_VISIBLE
                }) | WS_TABSTOP
                    | if id == COMPACT {
                        WINDOW_STYLE(BS_OWNERDRAW as u32)
                    } else {
                        WINDOW_STYLE(0)
                    },
                font,
            )?;
        }
        control(
            hwnd,
            w!("EDIT"),
            "",
            RUN_EDIT,
            WS_TABSTOP | WS_BORDER | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            font,
        )?;
        let header = control(hwnd, panel.lpszClassName, "", HEADER, WS_VISIBLE, font)?;
        let graph = control(hwnd, panel.lpszClassName, "", GRAPH, WINDOW_STYLE(0), font)?;
        let state = Box::new(RefCell::new(State {
            hwnd,
            menu: GetMenu(hwnd),
            list,
            tabs,
            status,
            header,
            graph,
            font,
            images,
            icon_indices: HashMap::new(),
            heading_font: CreateFontW(
                -16,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                DEFAULT_PITCH.0 as u32,
                w!("Segoe UI"),
            ),
            rows: Vec::new(),
            snapshot: Snapshot::default(),
            receiver: None,
            action: None,
            pending: None,
            notice: String::new(),
            tab: Tab::Processes,
            compact: false,
            resizing: None,
            expanded_size: (650, 480),
            grouped: true,
            expanded: HashSet::new(),
            sort: 0,
            descending: false,
            interval: 1000,
            tick: 0,
            run_visible: false,
            topmost: false,
            samples: VecDeque::new(),
            performance: 0,
            history: BTreeMap::new(),
            history_baseline: HashMap::new(),
        }));
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*state as *const RefCell<State>) as isize,
        );
        state.borrow_mut().configure();
        Ok(state)
    }
}
