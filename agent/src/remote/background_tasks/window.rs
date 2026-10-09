//! The Task Manager window: creation, window procedures, menus and scrollbars.
mod messages;

use super::*;
use messages::window_proc;

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

pub(in crate::remote) fn install_list_scrollbars(list: HWND) -> anyhow::Result<()> {
    if !unsafe { SetWindowSubclass(list, Some(list_paint), 1, 0) }.as_bool() {
        anyhow::bail!("Could not initialize captured list scrollbars");
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
        let (class, panel) = register_classes()?;
        let font = create_font(-12);
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            class,
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
        let (list, images) = create_list(hwnd, font)?;
        let tabs = create_tabs(hwnd, font)?;
        install_list_scrollbars(list)?;
        let status = control(
            hwnd,
            w!("STATIC"),
            "Loading processes…",
            109,
            WS_VISIBLE,
            font,
        )?;
        create_buttons(hwnd, font)?;
        let header = control(hwnd, panel, "", HEADER, WS_VISIBLE, font)?;
        let graph = control(hwnd, panel, "", GRAPH, WINDOW_STYLE(0), font)?;
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
            heading_font: create_font(-16),
            rows: Vec::new(),
            snapshot: Snapshot::default(),
            receiver: None,
            action: None,
            pending: None,
            notice: String::new(),
            tab: Tab::Processes,
            compact: false,
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

/// Returns the window and panel class names.
fn register_classes() -> anyhow::Result<(PCWSTR, PCWSTR)> {
    unsafe {
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
        Ok((class.lpszClassName, panel.lpszClassName))
    }
}

fn create_font(height: i32) -> HFONT {
    unsafe {
        CreateFontW(
            height,
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
        )
    }
}

/// Returns the list view and its small image list.
unsafe fn create_list(hwnd: HWND, font: HFONT) -> anyhow::Result<(HWND, HIMAGELIST)> {
    unsafe {
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
        Ok((list, images))
    }
}

unsafe fn create_tabs(hwnd: HWND, font: HFONT) -> anyhow::Result<HWND> {
    unsafe {
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
        Ok(tabs)
    }
}

/// Creates the footer buttons and the hidden Run edit.
unsafe fn create_buttons(hwnd: HWND, font: HFONT) -> anyhow::Result<()> {
    unsafe {
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
        Ok(())
    }
}
