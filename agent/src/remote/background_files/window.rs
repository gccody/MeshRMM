//! The file browser window: creation, window procedures, context menu and preview.
mod keyboard;
mod messages;

use super::*;
use keyboard::message_loop;
use messages::window_proc;

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
    // Run opens a folder here. Without one, the browser lists the drives.
    let path = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_default();
    unsafe {
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        })
        .ok()?;
        register_classes()?;
        let font = create_font(-12, w!("Segoe UI"));
        let symbols = create_font(-28, w!("Segoe MDL2 Assets"));
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
        let state = Box::new(RefCell::new(create_state(hwnd, &path, font, symbols)?));
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*state as *const RefCell<State>) as isize,
        );
        state.borrow_mut().start(Work::List(path))?;
        state.borrow().layout();
        SetTimer(Some(hwnd), 1, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        message_loop(&state);
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = DestroyWindow(hwnd);
        }
        let _ = DeleteObject(font.into());
        let _ = DeleteObject(symbols.into());
    }
    Ok(())
}

fn register_classes() -> anyhow::Result<()> {
    for (name, proc) in [
        (
            w!("MeshRMMBackgroundFiles"),
            Some(window_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
        ),
        (
            w!("MeshRMMBackgroundPreview"),
            Some(preview_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
        ),
    ] {
        let class = WNDCLASSW {
            lpfnWndProc: proc,
            lpszClassName: name,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            ..Default::default()
        };
        ensure!(
            unsafe { RegisterClassW(&class) } != 0,
            "Could not register File Explorer window"
        );
    }
    Ok(())
}

fn create_font(height: i32, face: PCWSTR) -> HFONT {
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
            face,
        )
    }
}

/// Creates the child controls in tab order and the state that owns them.
fn create_state(hwnd: HWND, path: &Path, font: HFONT, symbols: HFONT) -> anyhow::Result<State> {
    create_ribbon(hwnd, font)?;
    let (location, search) = create_address_bar(hwnd, path, font)?;
    let (nav, nav_paths) = create_navigation(hwnd, font)?;
    let list = create_list(hwnd, font)?;
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
    Ok(State {
        hwnd,
        location,
        list,
        status,
        search,
        nav,
        crumbs: Vec::new(),
        address_edit: false,
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
                let _ = unsafe { SHGetStockIconInfo(id, SHGSI_ICON | SHGSI_SMALLICON, &mut info) };
                info.hIcon
            })
            .collect(),
        path: path.to_path_buf(),
        rows: Vec::new(),
        visible: Vec::new(),
        nav_paths,
        copied: Vec::new(),
        cut: false,
        clipboard_sequence: 0,
        history: History::default(),
        travel: None,
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
    })
}

fn create_ribbon(hwnd: HWND, font: HFONT) -> anyhow::Result<()> {
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
    Ok(())
}

/// Returns the location and search edits.
fn create_address_bar(hwnd: HWND, path: &Path, font: HFONT) -> anyhow::Result<(HWND, HWND)> {
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
        &location_label(path),
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
    unsafe {
        SendMessageW(
            search,
            EM_SETCUEBANNER,
            Some(WPARAM(1)),
            Some(LPARAM(w!("Search this folder").as_ptr() as isize)),
        );
    }
    Ok((location, search))
}

/// Returns the navigation pane and the path behind each of its entries.
fn create_navigation(hwnd: HWND, font: HFONT) -> anyhow::Result<(HWND, Vec<PathBuf>)> {
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
    unsafe { SendMessageW(nav, LB_SETITEMHEIGHT, None, Some(LPARAM(28))) };
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
    let drives = unsafe { GetLogicalDrives() };
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
        unsafe {
            SendMessageW(
                nav,
                LB_ADDSTRING,
                None,
                Some(LPARAM(wide(&label).as_ptr() as isize)),
            )
        };
        nav_paths.push(path);
    }
    Ok((nav, nav_paths))
}

fn create_list(hwnd: HWND, font: HFONT) -> anyhow::Result<HWND> {
    let list = control(
        hwnd,
        w!("SysListView32"),
        "",
        LIST,
        WS_TABSTOP
            | WINDOW_STYLE(LVS_REPORT | LVS_SHOWSELALWAYS | LVS_EDITLABELS | LVS_SHAREIMAGELISTS),
        [194, 162, 880, 450],
        font,
    )?;
    unsafe {
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
    }
    controls::install(list, font)?;
    Ok(list)
}
