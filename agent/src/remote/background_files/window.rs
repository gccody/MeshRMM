//! The file browser window: creation, window procedures, context menu and preview.
use super::*;

pub(super) fn control(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    id: usize,
    style: WINDOW_STYLE,
    rect: [i32; 4],
    font: HFONT,
) -> anyhow::Result<HWND> {
    unsafe {
        let h = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR(wide(text).as_ptr()),
            WS_CHILD | WS_VISIBLE | style,
            rect[0],
            rect[1],
            rect[2],
            rect[3],
            Some(parent),
            Some(HMENU(id as *mut _)),
            None,
            None,
        )?;
        SendMessageW(
            h,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
        Ok(h)
    }
}

pub(super) fn context_menu(hwnd: HWND, state: &State, point: POINT) {
    unsafe {
        if let Ok(menu) = CreatePopupMenu() {
            let selected = !state.selection().is_empty();
            for (id, label, enabled) in [
                (OPEN, "Open\tEnter", selected),
                (CUT, "Cut\tCtrl+X", selected),
                (COPY, "Copy\tCtrl+C", selected),
                (
                    PASTE,
                    "Paste\tCtrl+V",
                    meshrmm_file_transfer::clipboard_has_files(),
                ),
                (RENAME, "Rename\tF2", selected),
                (DELETE, "Delete\tDel", selected),
                (NEW_FOLDER, "New folder\tCtrl+Shift+N", true),
                (REFRESH, "Refresh\tF5", true),
                (PROPERTIES, "Properties\tAlt+Enter", selected),
            ] {
                let _ = AppendMenuW(
                    menu,
                    if enabled {
                        MF_STRING
                    } else {
                        MF_STRING | MF_GRAYED
                    },
                    id,
                    PCWSTR(wide(label).as_ptr()),
                );
            }
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            if command.0 != 0 {
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_COMMAND,
                    WPARAM(command.0 as usize),
                    LPARAM(0),
                );
            }
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
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        // List notifications are synchronous, including during render(). Never
        // borrow State on those paths; defer selection/sort/navigation to the queue.
        if message == WM_NOTIFY && !pointer.is_null() {
            let header = &*(lparam.0 as *const NMHDR);
            if header.idFrom == LIST {
                if header.code == LVN_ITEMCHANGED {
                    if !GetPropW(hwnd, w!("MeshRMMReplacingRows")).is_invalid() {
                        return LRESULT(0);
                    }
                    let _ = PostMessageW(Some(hwnd), UPDATE_SELECTION, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                if header.code == LVN_COLUMNCLICK {
                    let event = &*(lparam.0 as *const NMLISTVIEW);
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_COMMAND,
                        WPARAM(SORT_NAME + event.iSubItem as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                if header.code == LVN_BEGINLABELEDITW {
                    return LRESULT(
                        (*pointer)
                            .try_borrow()
                            .is_ok_and(|state| state.receiver.is_some() || state.searching)
                            as isize,
                    );
                }
                if header.code == LVN_ENDLABELEDITW {
                    let event = &*(lparam.0 as *const NMLVDISPINFOW);
                    if !event.item.pszText.is_null()
                        && let Ok(mut state) = (*pointer).try_borrow_mut()
                        && let Some(entry) = state
                            .visible
                            .get(event.item.iItem as usize)
                            .and_then(|i| state.rows.get(*i))
                    {
                        state.pending_rename = Some((
                            entry.path.clone(),
                            event.item.pszText.to_string().unwrap_or_default(),
                        ));
                        let _ = PostMessageW(Some(hwnd), NAVIGATE, WPARAM(1), LPARAM(0));
                    }
                    return LRESULT(0);
                }
                if header.code == LVN_BEGINDRAG {
                    if let Ok(mut state) = (*pointer).try_borrow_mut()
                        && state.receiver.is_none()
                    {
                        state.dragging = state
                            .selection()
                            .into_iter()
                            .map(|entry| entry.path)
                            .collect();
                        set_text(
                            state.status,
                            "Drag to a folder to move. Hold Ctrl to copy; press Esc to cancel.",
                        );
                    }
                    return LRESULT(0);
                }
                if header.code == NM_CLICK {
                    let event = &*(lparam.0 as *const NMITEMACTIVATE);
                    let Ok(mut state) = (*pointer).try_borrow_mut() else {
                        return LRESULT(0);
                    };
                    let now = std::time::Instant::now();
                    let double = state.last_click.take().is_some_and(|(then, item)| {
                        item == event.iItem
                            && now.duration_since(then).as_millis()
                                <= u128::from(GetDoubleClickTime())
                    });
                    if event.iItem >= 0 {
                        if double {
                            let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(OPEN), LPARAM(0));
                        } else {
                            state.last_click = Some((now, event.iItem));
                        }
                    }
                    return LRESULT(0);
                }
            }
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_NCCALCSIZE {
            let rect = if wparam.0 != 0 {
                &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
            } else {
                &mut *(lparam.0 as *mut RECT)
            };
            rect.left += 1;
            rect.right -= 1;
            rect.top += 31;
            rect.bottom -= 1;
            return LRESULT(0);
        }
        if message == WM_NCHITTEST {
            return LRESULT(frame::hit(
                hwnd,
                POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                },
            ) as isize);
        }
        if !pointer.is_null() {
            if matches!(message, WM_NCPAINT | WM_NCACTIVATE | WM_PRINT) {
                let result = DefWindowProcW(hwnd, message, wparam, lparam);
                let dc = if message == WM_PRINT {
                    HDC(wparam.0 as *mut _)
                } else {
                    GetWindowDC(Some(hwnd))
                };
                if !dc.is_invalid() {
                    if let Ok(state) = (*pointer).try_borrow() {
                        frame::paint(hwnd, dc, state.font);
                    }
                    if message != WM_PRINT {
                        ReleaseDC(Some(hwnd), dc);
                    }
                }
                return result;
            }
            if message == WM_LBUTTONDOWN {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut point);
                let edge = frame::hit(hwnd, point);
                if edge == HTMINBUTTON {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_SYSCOMMAND,
                        WPARAM(SC_MINIMIZE as usize),
                        LPARAM(0),
                    );
                    return LRESULT(0);
                }
                if matches!(
                    edge,
                    HTLEFT
                        | HTRIGHT
                        | HTTOP
                        | HTBOTTOM
                        | HTTOPLEFT
                        | HTTOPRIGHT
                        | HTBOTTOMLEFT
                        | HTBOTTOMRIGHT
                ) {
                    let mut bounds = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut bounds);
                    if let Ok(mut state) = (*pointer).try_borrow_mut() {
                        state.resizing = Some((edge, point, bounds));
                    }
                    return LRESULT(0);
                }
            }
            if message == WM_MOUSEMOVE
                && let Some((edge, start, bounds)) = (*pointer)
                    .try_borrow()
                    .ok()
                    .and_then(|state| state.resizing)
            {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                let _ = ClientToScreen(hwnd, &mut point);
                let bounds = frame::resize(edge, start, bounds, point);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    bounds.left,
                    bounds.top,
                    bounds.right - bounds.left,
                    bounds.bottom - bounds.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                return LRESULT(0);
            }
            if matches!(message, WM_LBUTTONUP | WM_CANCELMODE)
                && let Ok(mut state) = (*pointer).try_borrow_mut()
            {
                state.resizing = None;
            }

            if message == WM_PAINT {
                if let Ok(state) = (*pointer).try_borrow() {
                    paint(hwnd, &state);
                    return LRESULT(0);
                }
                return DefWindowProcW(hwnd, message, wparam, lparam);
            }
            if message == WM_DRAWITEM {
                let Ok(state) = (*pointer).try_borrow() else {
                    return LRESULT(0);
                };
                let draw = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if draw.CtlID as usize == NAV {
                    draw_navigation(draw, &state);
                    return LRESULT(1);
                }
                draw_button(&*(lparam.0 as *const DRAWITEMSTRUCT), &state);
                return LRESULT(1);
            }
            if message == WM_CONTEXTMENU {
                let mut point = POINT {
                    x: lparam.0 as i16 as i32,
                    y: (lparam.0 >> 16) as i16 as i32,
                };
                if point.x == -1 {
                    let _ = GetCursorPos(&mut point);
                }
                if let Ok(state) = (*pointer).try_borrow() {
                    context_menu(hwnd, &state, point);
                }
                return LRESULT(0);
            }
            if message == WM_COMMAND && wparam.0 & 0xffff == FILE_MENU {
                if let Ok(menu) = CreatePopupMenu() {
                    for (id, label) in [
                        (NEW_WINDOW, "Open new window"),
                        (COPY_PATH, "Copy path"),
                        (UNDO, "Undo\tCtrl+Z"),
                        (PROPERTIES, "Properties"),
                        (CLOSE, "Close"),
                    ] {
                        let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(wide(label).as_ptr()));
                    }
                    let mut point = POINT { x: 0, y: 27 };
                    let _ = ClientToScreen(hwnd, &mut point);
                    let command =
                        TrackPopupMenu(menu, TPM_RETURNCMD, point.x, point.y, None, hwnd, None);
                    let _ = DestroyMenu(menu);
                    if command.0 != 0 {
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_COMMAND,
                            WPARAM(command.0 as usize),
                            LPARAM(0),
                        );
                    }
                }
                return LRESULT(0);
            }
            let Ok(mut state) = (*pointer).try_borrow_mut() else {
                return DefWindowProcW(hwnd, message, wparam, lparam);
            };
            let result = match message {
                WM_TIMER => state.poll(),
                WM_SIZE => {
                    state.layout();
                    Ok(())
                }
                UPDATE_SELECTION => {
                    state.selection_status();
                    Ok(())
                }
                DROP_FILES => {
                    let sources = std::mem::take(&mut state.dragging);
                    if sources.is_empty() {
                        Ok(())
                    } else {
                        let point = POINT {
                            x: lparam.0 as i16 as i32,
                            y: (lparam.0 >> 16) as i16 as i32,
                        };
                        let mut list_bounds = RECT::default();
                        let mut nav_bounds = RECT::default();
                        let _ = GetWindowRect(state.list, &mut list_bounds);
                        let _ = GetWindowRect(state.nav, &mut nav_bounds);
                        let contains = |r: RECT| {
                            point.x >= r.left
                                && point.x < r.right
                                && point.y >= r.top
                                && point.y < r.bottom
                        };
                        let target = if contains(list_bounds) {
                            let mut hit = LVHITTESTINFO {
                                pt: POINT {
                                    x: point.x - list_bounds.left,
                                    y: point.y - list_bounds.top,
                                },
                                ..Default::default()
                            };
                            let row = SendMessageW(
                                state.list,
                                LVM_HITTEST,
                                None,
                                Some(LPARAM((&mut hit as *mut LVHITTESTINFO) as isize)),
                            )
                            .0;
                            state
                                .visible
                                .get(row as usize)
                                .and_then(|index| state.rows.get(*index))
                                .filter(|entry| entry.directory)
                                .map(|entry| entry.path.clone())
                                .or_else(|| Some(state.path.clone()))
                        } else if contains(nav_bounds) {
                            let index = SendMessageW(
                                state.nav,
                                LB_ITEMFROMPOINT,
                                None,
                                Some(LPARAM(
                                    ((point.x - nav_bounds.left) as u16 as u32
                                        | ((point.y - nav_bounds.top) as u16 as u32) << 16)
                                        as isize,
                                )),
                            )
                            .0;
                            state.nav_paths.get((index & 0xffff) as usize).cloned()
                        } else {
                            None
                        };
                        if let Some(target) = target.filter(|path| !virtual_location(path)) {
                            let cut = !state.control_down
                                && sources.iter().all(|source| {
                                    source.components().next() == target.components().next()
                                });
                            if sources
                                .iter()
                                .all(|source| source.parent() == Some(target.as_path()))
                                && cut
                            {
                                state.selection_status();
                                Ok(())
                            } else {
                                {
                                    let path = state.path.clone();
                                    state.start(Work::Transfer(path, target, sources, cut, false))
                                }
                            }
                        } else {
                            state.selection_status();
                            Ok(())
                        }
                    }
                }
                NAVIGATE => {
                    if wparam.0 == 1 {
                        if let Some((source, mut name)) = state.pending_rename.take() {
                            if !state.extensions
                                && source.is_file()
                                && let Some(extension) = source.extension()
                            {
                                name.push('.');
                                name.push_str(&extension.to_string_lossy());
                            }
                            child_path(source.parent().unwrap_or(&state.path), &name).and_then(
                                |target| {
                                    if target == source {
                                        Ok(())
                                    } else {
                                        let directory = state.path.clone();
                                        state.start(Work::Rename(directory, source, target))
                                    }
                                },
                            )
                        } else {
                            Ok(())
                        }
                    } else {
                        let index = SendMessageW(state.nav, LB_GETCURSEL, None, None).0;
                        if let Some(path) = state.nav_paths.get(index as usize).cloned() {
                            state.start(Work::List(path))
                        } else {
                            Ok(())
                        }
                    }
                }
                WM_COMMAND
                    if wparam.0 & 0xffff == NAV && wparam.0 >> 16 == LBN_SELCHANGE as usize =>
                {
                    let _ = PostMessageW(Some(hwnd), NAVIGATE, WPARAM(0), LPARAM(0));
                    Ok(())
                }
                WM_COMMAND if wparam.0 >> 16 == 0 => state.command(wparam.0 & 0xffff),
                _ => Ok(()),
            };
            if let Err(error) = result {
                set_text(state.status, &format!("{error:#}"));
            }
        }
        if message == WM_CTLCOLORSTATIC {
            SetBkMode(HDC(wparam.0 as *mut _), TRANSPARENT);
            return LRESULT(GetStockObject(WHITE_BRUSH).0 as isize);
        }
        if message == WM_MEASUREITEM {
            let item = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
            if item.CtlID as usize == NAV {
                item.itemHeight = 28;
                return LRESULT(1);
            }
        }
        if message == WM_GETMINMAXINFO {
            let info = &mut *(lparam.0 as *mut MINMAXINFO);
            info.ptMinTrackSize = POINT { x: 940, y: 400 };
            info.ptMaxPosition = POINT { x: 0, y: 0 };
            info.ptMaxSize = POINT {
                x: meshrmm_remote_screen::background::WIDTH as i32,
                y: meshrmm_remote_screen::background::HEIGHT as i32
                    - crate::remote::background::TASKBAR_HEIGHT,
            };
            info.ptMaxTrackSize = info.ptMaxSize;
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) unsafe extern "system" fn preview_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if message == WM_SIZE
            && let Ok(edit) = GetDlgItem(Some(hwnd), 1)
        {
            let _ = MoveWindow(
                edit,
                0,
                0,
                (lparam.0 & 0xffff) as i32,
                ((lparam.0 >> 16) & 0xffff) as i32,
                true,
            );
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

pub(super) fn show_preview(path: &Path, text: &str) -> anyhow::Result<()> {
    unsafe {
        let title = wide(format!("{} — read-only", path.display()));
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            w!("MeshRMMBackgroundPreview"),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            80,
            48,
            1000,
            640,
            None,
            None,
            None,
            None,
        )?;
        let edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_CHILD
                | WS_VISIBLE
                | WS_VSCROLL
                | WS_HSCROLL
                | WINDOW_STYLE(
                    ES_MULTILINE as u32
                        | ES_READONLY as u32
                        | ES_AUTOVSCROLL as u32
                        | ES_AUTOHSCROLL as u32,
                ),
            0,
            0,
            980,
            600,
            Some(hwnd),
            Some(HMENU(std::ptr::without_provenance_mut(1))),
            None,
            None,
        )?;
        SendMessageW(
            edit,
            EM_SETLIMITTEXT,
            Some(WPARAM(MAX_PREVIEW as usize * 2)),
            None,
        );
        set_text(edit, text);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    Ok(())
}

pub(super) fn run_inner() -> anyhow::Result<()> {
    meshrmm_remote_screen::background::require_session_zero()?;
    let _desktop = meshrmm_remote_screen::background::Desktop::bind()?;
    let _styles = controls::VisualStyles::activate()?;
    let path = PathBuf::new();
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        })
        .ok()?;
        for (name, proc) in [
            (
                w!("MeshRMMBackgroundFiles"),
                Some(
                    window_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                ),
            ),
            (
                w!("MeshRMMBackgroundPreview"),
                Some(
                    preview_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                ),
            ),
        ] {
            let class = WNDCLASSW {
                lpfnWndProc: proc,
                lpszClassName: name,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
                ..Default::default()
            };
            ensure!(
                RegisterClassW(&class) != 0,
                "Could not register File Explorer window"
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
        let symbols = CreateFontW(
            -28,
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
            w!("Segoe MDL2 Assets"),
        );
        let hwnd = CreateWindowExW(
            WS_EX_COMPOSITED,
            w!("MeshRMMBackgroundFiles"),
            w!("File Explorer"),
            WS_OVERLAPPEDWINDOW,
            40,
            24,
            1100,
            680,
            None,
            None,
            None,
            None,
        )?;
        for (id, text, x, width) in [(HOME, "Home", 60, 60), (VIEW, "View", 120, 60)] {
            control(
                hwnd,
                w!("BUTTON"),
                text,
                id,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [x, 0, width, 27],
                font,
            )?;
        }
        control(
            hwnd,
            w!("BUTTON"),
            "File",
            FILE_MENU,
            WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            [0, 0, 56, 27],
            font,
        )?;
        for item in RIBBON {
            control(
                hwnd,
                w!("BUTTON"),
                item.label,
                item.id,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [item.x, 28, item.width, 72],
                font,
            )?;
        }
        for (id, label, x, width) in [
            (BACK, "←", 6, 30),
            (FORWARD, "→", 40, 30),
            (UP, "↑", 76, 30),
            (GO, "→", 814, 30),
            (SEARCH_GO, "⌕", 1030, 30),
        ] {
            control(
                hwnd,
                w!("BUTTON"),
                label,
                id,
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                [x, 129, width, 26],
                font,
            )?;
        }
        let location = control(
            hwnd,
            w!("EDIT"),
            &location_label(&path),
            LOCATION,
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            [116, 130, 682, 24],
            font,
        )?;
        control(
            hwnd,
            w!("BUTTON"),
            "",
            ADDRESS_EDIT,
            WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            [116, 129, 600, 26],
            font,
        )?;
        let search = control(
            hwnd,
            w!("EDIT"),
            "",
            SEARCH,
            WS_TABSTOP | WINDOW_STYLE(ES_AUTOHSCROLL as u32),
            [850, 130, 180, 24],
            font,
        )?;
        SendMessageW(
            search,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(w!("Search this folder").as_ptr() as isize)),
        );
        let nav = control(
            hwnd,
            w!("LISTBOX"),
            "",
            NAV,
            WS_TABSTOP
                | WS_VSCROLL
                | WINDOW_STYLE(
                    LBS_NOTIFY as u32
                        | LBS_NOINTEGRALHEIGHT as u32
                        | LBS_OWNERDRAWFIXED as u32
                        | LBS_HASSTRINGS as u32,
                ),
            [0, 162, 190, 450],
            font,
        )?;
        SendMessageW(nav, LB_SETITEMHEIGHT, None, Some(LPARAM(28)));
        let mut nav_paths = Vec::new();
        let public =
            PathBuf::from(std::env::var("PUBLIC").unwrap_or_else(|_| "C:\\Users\\Public".into()));
        let mut places = vec![
            ("Quick access".to_owned(), PathBuf::from("::QuickAccess")),
            ("Desktop".to_owned(), public.join("Desktop")),
            ("Downloads".to_owned(), public.join("Downloads")),
            ("Documents".to_owned(), public.join("Documents")),
            ("Pictures".to_owned(), public.join("Pictures")),
            ("This PC".to_owned(), PathBuf::new()),
        ];
        let drives = GetLogicalDrives();
        for bit in 0..26 {
            if drives & (1 << bit) != 0 {
                let drive = format!("{}:\\", (b'A' + bit) as char);
                places.push((
                    format!("Local Disk ({}:)", (b'A' + bit) as char),
                    PathBuf::from(drive),
                ));
            }
        }
        for (label, path) in places {
            SendMessageW(
                nav,
                LB_ADDSTRING,
                None,
                Some(LPARAM(wide(&label).as_ptr() as isize)),
            );
            nav_paths.push(path);
        }
        let list = control(
            hwnd,
            w!("SysListView32"),
            "",
            LIST,
            WS_TABSTOP
                | WINDOW_STYLE(
                    LVS_REPORT | LVS_SHOWSELALWAYS | LVS_EDITLABELS | LVS_SHAREIMAGELISTS,
                ),
            [194, 162, 880, 450],
            font,
        )?;
        let _ = SetWindowTheme(list, w!("Explorer"), PCWSTR::null());
        SendMessageW(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            None,
            Some(LPARAM(
                (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
            )),
        );
        for (flags, kind) in [
            (SHGFI_SMALLICON, LVSIL_SMALL),
            (SHGFI_LARGEICON, LVSIL_NORMAL),
        ] {
            let mut info = SHFILEINFOW::default();
            let images = SHGetFileInfoW(
                w!("C:\\"),
                FILE_ATTRIBUTE_DIRECTORY,
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_USEFILEATTRIBUTES | flags,
            );
            SendMessageW(
                list,
                LVM_SETIMAGELIST,
                Some(WPARAM(kind as usize)),
                Some(LPARAM(images as isize)),
            );
        }
        for (index, (name, width)) in [
            ("Name", 340),
            ("Date modified", 160),
            ("Type", 170),
            ("Size", 100),
        ]
        .iter()
        .enumerate()
        {
            let mut text = wide(name);
            let column = LVCOLUMNW {
                mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT,
                cx: *width,
                fmt: if index == 3 {
                    LVCFMT_RIGHT
                } else {
                    LVCFMT_LEFT
                },
                pszText: PWSTR(text.as_mut_ptr()),
                ..Default::default()
            };
            SendMessageW(
                list,
                LVM_INSERTCOLUMNW,
                Some(WPARAM(index)),
                Some(LPARAM((&column as *const LVCOLUMNW) as isize)),
            );
        }
        controls::install(list, font)?;
        let status = control(
            hwnd,
            w!("STATIC"),
            "Loading…",
            STATUS,
            WINDOW_STYLE(0),
            [12, 618, 1054, 22],
            font,
        )?;
        for (id, label) in [(CONFIRM, "Delete"), (CANCEL, "Cancel")] {
            control(
                hwnd,
                w!("BUTTON"),
                label,
                id,
                WS_TABSTOP,
                [0, 0, 94, 28],
                font,
            )?;
        }
        let state = Box::new(RefCell::new(State {
            hwnd,
            location,
            list,
            status,
            search,
            nav,
            crumbs: Vec::new(),
            address_edit: false,
            resizing: None,
            dragging: Vec::new(),
            control_down: false,
            undo_stack: Vec::new(),
            font,
            symbols,
            nav_icons: [SIID_FOLDER, SIID_DESKTOPPC, SIID_DRIVEFIXED]
                .into_iter()
                .map(|id| {
                    let mut info = SHSTOCKICONINFO {
                        cbSize: std::mem::size_of::<SHSTOCKICONINFO>() as u32,
                        ..Default::default()
                    };
                    let _ = SHGetStockIconInfo(id, SHGSI_ICON | SHGSI_SMALLICON, &mut info);
                    info.hIcon
                })
                .collect(),
            path: path.clone(),
            rows: Vec::new(),
            visible: Vec::new(),
            nav_paths,
            copied: Vec::new(),
            cut: false,
            clipboard_sequence: 0,
            history: History::default(),
            travel: None,
            last_click: None,
            receiver: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            sort: 0,
            descending: false,
            hidden: false,
            extensions: true,
            view_tab: false,
            searching: false,
            pending_delete: Vec::new(),
            pending_rename: None,
        }));
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*state as *const RefCell<State>) as isize,
        );
        state.borrow_mut().start(Work::List(path))?;
        state.borrow().layout();
        SetTimer(Some(hwnd), 1, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut message = MSG::default();
        let mut control_down = false;
        let mut shift_down = false;
        let mut alt_down = false;
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let key = message.wParam.0;
            if matches!(
                message.message,
                WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
            ) {
                let down = matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN);
                match key {
                    0x11 | 0xa2 | 0xa3 => {
                        control_down = down;
                        state.borrow_mut().control_down = down;
                    }
                    0x10 | 0xa0 | 0xa1 => shift_down = down,
                    0x12 | 0xa4 | 0xa5 => alt_down = down,
                    _ => {}
                }
            }
            let mut handled = false;
            if matches!(message.message, WM_KEYDOWN | WM_SYSKEYDOWN)
                && GetAncestor(message.hwnd, GA_ROOT) == hwnd
            {
                let editing = message.hwnd == location
                    || message.hwnd == search
                    || SendMessageW(list, LVM_GETEDITCONTROL, None, None).0
                        == message.hwnd.0 as isize;
                let command = match key {
                    0x5a if control_down && !editing => Some(UNDO),
                    0x41 if control_down && !editing => Some(SELECT_ALL),
                    0x43 if control_down && !editing => Some(COPY),
                    0x58 if control_down && !editing => Some(CUT),
                    0x56 if control_down && !editing => Some(PASTE),
                    0x4e if control_down && shift_down && !editing => Some(NEW_FOLDER),
                    0x71 if !editing => Some(RENAME),
                    0x74 => Some(REFRESH),
                    0x2e if !editing => Some(DELETE),
                    0x25 if alt_down => Some(BACK),
                    0x27 if alt_down => Some(FORWARD),
                    0x26 if alt_down => Some(UP),
                    0x08 if !editing => Some(UP),
                    13 if alt_down => Some(PROPERTIES),
                    13 if message.hwnd == location => Some(GO),
                    13 if message.hwnd == search => Some(SEARCH_GO),
                    13 if message.hwnd == list => Some(OPEN),
                    27 if message.hwnd == search => {
                        set_text(search, "");
                        Some(SEARCH_GO)
                    }
                    27 if !editing => Some(CANCEL),
                    _ => None,
                };
                if key == 0x41 && control_down && editing {
                    SendMessageW(message.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    handled = true;
                } else if (control_down && key == 0x4c) || (alt_down && key == 0x44) || key == 0x75
                {
                    state.borrow_mut().address_edit = true;
                    state.borrow().layout();
                    let _ = SetFocus(Some(location));
                    SendMessageW(location, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    handled = true;
                } else if (control_down && matches!(key, 0x46 | 0x45)) || key == 0x72 {
                    let _ = SetFocus(Some(search));
                    SendMessageW(search, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                    handled = true;
                } else if let Some(command) = command {
                    if let Err(e) = state.borrow_mut().command(command) {
                        set_text(status, &format!("{e:#}"));
                    }
                    handled = true;
                }
            }
            if !handled && !IsDialogMessageW(GetAncestor(message.hwnd, GA_ROOT), &message).as_bool()
            {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(symbols.into());
    }
    Ok(())
}
