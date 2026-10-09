//! The file browser window procedure and its message handlers.
use super::*;

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
            return list_notification(hwnd, &*pointer, message, wparam, lparam);
        }
        if message == WM_NCCALCSIZE {
            client_area(hwnd, wparam, lparam);
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
        // The caption buttons are drawn here, so Windows' own button tracking
        // would draw classic ones over them.
        if message == WM_NCLBUTTONDOWN
            && matches!(wparam.0 as u32, HTCLOSE | HTMAXBUTTON | HTMINBUTTON)
        {
            let command = match wparam.0 as u32 {
                HTCLOSE => SC_CLOSE,
                HTMINBUTTON => SC_MINIMIZE,
                _ if IsZoomed(hwnd).as_bool() => SC_RESTORE,
                _ => SC_MAXIMIZE,
            };
            let _ = PostMessageW(
                Some(hwnd),
                WM_SYSCOMMAND,
                WPARAM(command as usize),
                LPARAM(0),
            );
            return LRESULT(0);
        }
        if !pointer.is_null()
            && let Some(result) = state_message(hwnd, &*pointer, message, wparam, lparam)
        {
            return result;
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
            let area = crate::remote::background::work_area();
            info.ptMinTrackSize = POINT { x: 940, y: 400 };
            info.ptMaxTrackSize = POINT {
                x: area.right - area.left,
                y: area.bottom - area.top,
            };
            return LRESULT(0);
        }
        if message == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

unsafe fn list_notification(
    hwnd: HWND,
    cell: &RefCell<State>,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
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
                    cell.try_borrow()
                        .is_ok_and(|state| state.receiver.is_some() || state.searching)
                        as isize,
                );
            }
            if header.code == LVN_ENDLABELEDITW {
                end_label_edit(hwnd, cell, &*(lparam.0 as *const NMLVDISPINFOW));
                return LRESULT(0);
            }
            if header.code == LVN_BEGINDRAG {
                if let Ok(mut state) = cell.try_borrow_mut()
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
                    SetCapture(header.hwndFrom);
                }
                return LRESULT(0);
            }
            if header.code == NM_DBLCLK {
                let event = &*(lparam.0 as *const NMITEMACTIVATE);
                if event.iItem >= 0 {
                    let _ = PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(OPEN), LPARAM(0));
                }
                return LRESULT(0);
            }
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

unsafe fn end_label_edit(hwnd: HWND, cell: &RefCell<State>, event: &NMLVDISPINFOW) {
    unsafe {
        if !event.item.pszText.is_null()
            && let Ok(mut state) = cell.try_borrow_mut()
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
    }
}

unsafe fn client_area(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    unsafe {
        let rect = if wparam.0 != 0 {
            &mut (*(lparam.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0]
        } else {
            &mut *(lparam.0 as *mut RECT)
        };
        *rect = crate::remote::background::frame_rect(hwnd, *rect);
        let border = crate::remote::background::frame_border(hwnd);
        rect.left += border;
        rect.right -= border;
        rect.top += 31;
        rect.bottom -= border;
    }
}

/// Handles messages that need the window state. `None` falls through to the
/// stateless handlers.
unsafe fn state_message(
    hwnd: HWND,
    cell: &RefCell<State>,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    unsafe {
        if matches!(message, WM_NCPAINT | WM_NCACTIVATE | WM_PRINT) {
            return Some(paint_frame(hwnd, cell, message, wparam, lparam));
        }
        if message == WM_PAINT {
            if let Ok(state) = cell.try_borrow() {
                paint(hwnd, &state);
                return Some(LRESULT(0));
            }
            return Some(DefWindowProcW(hwnd, message, wparam, lparam));
        }
        if message == WM_DRAWITEM {
            let Ok(state) = cell.try_borrow() else {
                return Some(LRESULT(0));
            };
            let draw = &*(lparam.0 as *const DRAWITEMSTRUCT);
            if draw.CtlID as usize == NAV {
                draw_navigation(draw, &state);
                return Some(LRESULT(1));
            }
            draw_button(&*(lparam.0 as *const DRAWITEMSTRUCT), &state);
            return Some(LRESULT(1));
        }
        if message == WM_CONTEXTMENU {
            let mut point = POINT {
                x: lparam.0 as i16 as i32,
                y: (lparam.0 >> 16) as i16 as i32,
            };
            if point.x == -1 {
                let _ = GetCursorPos(&mut point);
            }
            if let Ok(state) = cell.try_borrow() {
                context_menu(hwnd, &state, point);
            }
            return Some(LRESULT(0));
        }
        if message == WM_COMMAND && wparam.0 & 0xffff == FILE_MENU {
            file_menu(hwnd);
            return Some(LRESULT(0));
        }
        let Ok(mut state) = cell.try_borrow_mut() else {
            return Some(DefWindowProcW(hwnd, message, wparam, lparam));
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
            DROP_FILES => drop_files(&mut state, lparam),
            NAVIGATE => navigate(&mut state, wparam),
            WM_COMMAND if wparam.0 & 0xffff == NAV && wparam.0 >> 16 == LBN_SELCHANGE as usize => {
                let _ = PostMessageW(Some(hwnd), NAVIGATE, WPARAM(0), LPARAM(0));
                Ok(())
            }
            WM_COMMAND if wparam.0 >> 16 == 0 => state.command(wparam.0 & 0xffff),
            _ => Ok(()),
        };
        if let Err(error) = result {
            set_text(state.status, &format!("{error:#}"));
        }
        None
    }
}

unsafe fn paint_frame(
    hwnd: HWND,
    cell: &RefCell<State>,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let result = DefWindowProcW(hwnd, message, wparam, lparam);
        let dc = if message == WM_PRINT {
            HDC(wparam.0 as *mut _)
        } else {
            GetWindowDC(Some(hwnd))
        };
        if !dc.is_invalid() {
            if let Ok(state) = cell.try_borrow() {
                frame::paint(hwnd, dc, state.font);
            }
            if message != WM_PRINT {
                ReleaseDC(Some(hwnd), dc);
            }
        }
        result
    }
}

unsafe fn file_menu(hwnd: HWND) {
    unsafe {
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
            let command = TrackPopupMenu(menu, TPM_RETURNCMD, point.x, point.y, None, hwnd, None);
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

fn drop_files(state: &mut State, lparam: LPARAM) -> anyhow::Result<()> {
    let sources = std::mem::take(&mut state.dragging);
    if sources.is_empty() {
        return Ok(());
    }
    let point = POINT {
        x: lparam.0 as i16 as i32,
        y: (lparam.0 >> 16) as i16 as i32,
    };
    if let Some(target) = drop_target(state, point).filter(|path| !virtual_location(path)) {
        let cut = !state.control_down
            && sources
                .iter()
                .all(|source| source.components().next() == target.components().next());
        if sources
            .iter()
            .all(|source| source.parent() == Some(target.as_path()))
            && cut
        {
            state.selection_status();
            Ok(())
        } else {
            let path = state.path.clone();
            state.start(Work::Transfer(path, target, sources, cut, false))
        }
    } else {
        state.selection_status();
        Ok(())
    }
}

fn drop_target(state: &State, point: POINT) -> Option<PathBuf> {
    unsafe {
        let mut list_bounds = RECT::default();
        let mut nav_bounds = RECT::default();
        let _ = GetWindowRect(state.list, &mut list_bounds);
        let _ = GetWindowRect(state.nav, &mut nav_bounds);
        let contains = |r: RECT| {
            point.x >= r.left && point.x < r.right && point.y >= r.top && point.y < r.bottom
        };
        if contains(list_bounds) {
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
        }
    }
}

/// `wparam` 1 applies a finished label edit; otherwise the navigation pane
/// selection is opened.
fn navigate(state: &mut State, wparam: WPARAM) -> anyhow::Result<()> {
    if wparam.0 == 1 {
        let Some((source, mut name)) = state.pending_rename.take() else {
            return Ok(());
        };
        if !state.extensions
            && source.is_file()
            && let Some(extension) = source.extension()
        {
            name.push('.');
            name.push_str(&extension.to_string_lossy());
        }
        child_path(source.parent().unwrap_or(&state.path), &name).and_then(|target| {
            if target == source {
                Ok(())
            } else {
                let directory = state.path.clone();
                state.start(Work::Rename(directory, source, target))
            }
        })
    } else {
        let index = unsafe { SendMessageW(state.nav, LB_GETCURSEL, None, None).0 };
        if let Some(path) = state.nav_paths.get(index as usize).cloned() {
            state.start(Work::List(path))
        } else {
            Ok(())
        }
    }
}
