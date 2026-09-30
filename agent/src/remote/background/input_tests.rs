//! Session 0 checks that the workspace's real input drives menus, double-clicks,
//! scrollbars and the wheel the way a local mouse does.
use super::tests::*;
use super::*;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use windows::Win32::UI::Controls::SetScrollInfo;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

fn display() -> meshrmm_protocol::DisplayId {
    meshrmm_protocol::DisplayId(background::DISPLAY_ID)
}

/// The screen rectangle of item `index` of the open popup menu `menu`. The
/// menu window has to be named: only the thread tracking a menu finds it.
fn popup_item(menu: HMENU, index: u32) -> anyhow::Result<RECT> {
    let (window, _) = open_menu()?.context("no menu is open")?;
    let mut item = RECT::default();
    unsafe { GetMenuItemRect(Some(window), menu, index, &mut item)? };
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(window, &mut bounds)? };
    anyhow::ensure!(
        item.left >= bounds.left
            && item.right <= bounds.right
            && item.top >= bounds.top
            && item.bottom <= bounds.bottom,
        "menu item {index} at {item:?} is outside its menu at {bounds:?}"
    );
    Ok(item)
}

fn normalized(x: i32, y: i32) -> (u16, u16) {
    (
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )
}

fn press(workspace: &mut Workspace, x: i32, y: i32, pressed: bool) -> anyhow::Result<()> {
    let (x, y) = normalized(x, y);
    workspace.apply(RemoteInput::PointerButtonAt {
        display_id: display(),
        x,
        y,
        button: PointerButton::Left,
        pressed,
    })
}

fn double_click(workspace: &mut Workspace, x: i32, y: i32) -> anyhow::Result<()> {
    for _ in 0..2 {
        press(workspace, x, y, true)?;
        press(workspace, x, y, false)?;
    }
    Ok(())
}

fn drag(workspace: &mut Workspace, from: POINT, to: POINT) -> anyhow::Result<()> {
    press(workspace, from.x, from.y, true)?;
    settle(workspace, 100);
    let (x, y) = normalized(to.x, to.y);
    workspace.apply(RemoteInput::PointerMove {
        display_id: display(),
        x,
        y,
    })?;
    settle(workspace, 100);
    press(workspace, to.x, to.y, false)?;
    settle(workspace, 200);
    Ok(())
}

fn center(window: HWND) -> anyhow::Result<POINT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(window, &mut rect)? };
    Ok(POINT {
        x: (rect.left + rect.right) / 2,
        y: (rect.top + rect.bottom) / 2,
    })
}

fn client_to_screen(window: HWND, x: i32, y: i32) -> POINT {
    let mut point = POINT { x, y };
    let _ = unsafe { ClientToScreen(window, &mut point) };
    point
}

/// Runs `test` on a thread bound to a fresh background desktop, with a workspace.
fn in_workspace(
    test: impl FnOnce(&mut Workspace) -> anyhow::Result<()> + Send + 'static,
) -> anyhow::Result<()> {
    std::thread::spawn(move || -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let mut workspace = Workspace::new()?;
        test(&mut workspace)
    })
    .join()
    .expect("background input test panicked")
}

fn register(name: PCWSTR, procedure: WNDPROC, style: WNDCLASS_STYLES) -> anyhow::Result<()> {
    unsafe {
        let class = WNDCLASSW {
            style,
            lpfnWndProc: procedure,
            lpszClassName: name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: GetSysColorBrush(COLOR_WINDOW),
            ..Default::default()
        };
        anyhow::ensure!(
            RegisterClassW(&class) != 0,
            "could not register a test class"
        );
    }
    Ok(())
}

static COMMAND: AtomicUsize = AtomicUsize::new(0);
/// The menu messages the test window received, for failure reports.
static MENU_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static CLIENT_PRESSES: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_POINT: AtomicIsize = AtomicIsize::new(0);

const FIND: usize = 101;
const COPY: usize = 201;

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn menus_open_and_run_from_clicks() -> anyhow::Result<()> {
    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Controls send WM_COMMAND too; a menu's has no control.
        let menu_command = message == WM_COMMAND && lparam.0 == 0;
        let name = match message {
            WM_ENTERMENULOOP => Some("enter"),
            WM_EXITMENULOOP => Some("exit"),
            WM_INITMENUPOPUP => Some("init"),
            WM_UNINITMENUPOPUP => Some("uninit"),
            WM_MENUSELECT => Some("select"),
            WM_CANCELMODE => Some("cancel"),
            _ if menu_command => Some("command"),
            _ => None,
        };
        if let Some(name) = name {
            MENU_LOG
                .lock()
                .unwrap()
                .push(format!("{name} {:#x} {:#x}", wparam.0, lparam.0));
        }
        match message {
            _ if menu_command => COMMAND.store(wparam.0 & 0xffff, Ordering::SeqCst),
            WM_LBUTTONDOWN => {
                CLIENT_PRESSES.fetch_add(1, Ordering::SeqCst);
            }
            WM_CONTEXTMENU => CONTEXT_POINT.store(lparam.0, Ordering::SeqCst),
            _ => return unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
        LRESULT(0)
    }
    in_workspace(|workspace| {
        register(w!("MeshRMMMenuTest"), Some(procedure), WNDCLASS_STYLES(0))?;
        let (ui, (window, menu, edit)) = UiThread::start(|| unsafe {
            let menu = CreateMenu()?;
            let file = CreatePopupMenu()?;
            AppendMenuW(file, MF_STRING, FIND, w!("&Find..."))?;
            AppendMenuW(file, MF_STRING, 102, w!("E&xit"))?;
            AppendMenuW(menu, MF_POPUP, file.0 as usize, w!("&File"))?;
            let edit_menu = CreatePopupMenu()?;
            AppendMenuW(edit_menu, MF_STRING, COPY, w!("&Copy"))?;
            AppendMenuW(menu, MF_POPUP, edit_menu.0 as usize, w!("&Edit"))?;
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MeshRMMMenuTest"),
                w!("Menu test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                500,
                320,
                None,
                Some(menu),
                None,
                None,
            )?;
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                10,
                10,
                240,
                24,
                Some(window),
                None,
                None,
                None,
            )?;
            anyhow::ensure!(
                SetForegroundWindow(window).as_bool(),
                "the test window did not take the foreground"
            );
            SetFocus(Some(edit))?;
            Ok((window.0 as isize, menu.0 as isize, edit.0 as isize))
        })?;
        let (window, menu, edit) = (hwnd(window), HMENU(menu as *mut _), hwnd(edit));
        settle(workspace, 200);

        // A menu-bar click opens its menu and never reaches the client area.
        let mut item = RECT::default();
        unsafe { GetMenuItemRect(Some(window), menu, 0, &mut item)? };
        click_at(
            workspace,
            (item.left + item.right) / 2,
            (item.top + item.bottom) / 2,
        )?;
        wait_until(workspace, "the File menu", 5, &|_| {
            open_menu()
                .ok()
                .flatten()
                .is_some_and(|(_, items)| items.first().is_some_and(|item| item == "Find..."))
        })?;
        anyhow::ensure!(
            CLIENT_PRESSES.load(Ordering::SeqCst) == 0,
            "the menu-bar click reached the client area"
        );
        proof("background-menu-open.bmp")?;

        // Clicking an item runs its command, and typing straight after the
        // menu closes reaches the control that had the focus.
        let popup = unsafe { GetSubMenu(menu, 0) };
        let find = popup_item(popup, 0)?;
        click_at(
            workspace,
            (find.left + find.right) / 2,
            (find.top + find.bottom) / 2,
        )?;
        workspace.apply(RemoteInput::TypeText {
            display_id: display(),
            text: "after menu".into(),
        })?;
        wait_until(workspace, "Find... to run and typing to arrive", 5, &|_| {
            COMMAND.load(Ordering::SeqCst) == FIND && window_text(edit) == "after menu"
        })
        .with_context(|| {
            format!(
                "command {}, edit {:?}, menu messages {:?}",
                COMMAND.load(Ordering::SeqCst),
                window_text(edit),
                MENU_LOG.lock().unwrap()
            )
        })?;
        anyhow::ensure!(open_menu()?.is_none(), "the menu stayed open");

        // Moving across the menu bar with a menu open switches menus.
        unsafe { GetMenuItemRect(Some(window), menu, 0, &mut item)? };
        click_at(
            workspace,
            (item.left + item.right) / 2,
            (item.top + item.bottom) / 2,
        )?;
        wait_until(workspace, "the File menu", 5, &|_| {
            open_menu().ok().flatten().is_some()
        })?;
        unsafe { GetMenuItemRect(Some(window), menu, 1, &mut item)? };
        let (x, y) = normalized((item.left + item.right) / 2, (item.top + item.bottom) / 2);
        workspace.apply(RemoteInput::PointerMove {
            display_id: display(),
            x,
            y,
        })?;
        wait_until(workspace, "the Edit menu on hover", 5, &|_| {
            open_menu()
                .ok()
                .flatten()
                .is_some_and(|(_, items)| items == ["Copy"])
        })?;
        key(workspace, 0x01, false)?;
        key(workspace, 0x01, false)?;
        wait_until(workspace, "Escape to close the menu", 5, &|_| {
            open_menu().ok().flatten().is_none()
        })?;

        // A right-click opens the context menu at the pointer.
        let point = client_to_screen(window, 300, 150);
        let (x, y) = normalized(point.x, point.y);
        for pressed in [true, false] {
            workspace.apply(RemoteInput::PointerButtonAt {
                display_id: display(),
                x,
                y,
                button: PointerButton::Right,
                pressed,
            })?;
        }
        wait_until(workspace, "the context menu request", 5, &|_| {
            CONTEXT_POINT.load(Ordering::SeqCst) != 0
        })?;
        anyhow::ensure!(
            CONTEXT_POINT.load(Ordering::SeqCst) == pack(point),
            "the context menu was requested at {:#x}, not at {point:?}",
            CONTEXT_POINT.load(Ordering::SeqCst)
        );
        drop(ui);
        println!("Session 0 menu-bar clicks, item clicks, typing and context menus passed");
        Ok(())
    })
}

static DOUBLE_CLICKS: AtomicUsize = AtomicUsize::new(0);

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn double_clicks_scrollbars_and_wheel() -> anyhow::Result<()> {
    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_LBUTTONDBLCLK {
            DOUBLE_CLICKS.fetch_add(1, Ordering::SeqCst);
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }
    /// A pane with its own scrollbar, like Disk Management's graphical view.
    unsafe extern "system" fn pane(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_VSCROLL {
                let mut info = SCROLLINFO {
                    cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_ALL,
                    ..Default::default()
                };
                let _ = GetScrollInfo(window, SB_VERT, &mut info);
                let position = match SCROLLBAR_COMMAND((wparam.0 & 0xffff) as i32) {
                    SB_LINEUP => info.nPos - 1,
                    SB_LINEDOWN => info.nPos + 1,
                    SB_PAGEUP => info.nPos - info.nPage as i32,
                    SB_PAGEDOWN => info.nPos + info.nPage as i32,
                    SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos,
                    _ => info.nPos,
                };
                info.fMask = SIF_POS;
                info.nPos = position;
                SetScrollInfo(window, SB_VERT, &info, true);
                return LRESULT(0);
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
    }
    in_workspace(|workspace| {
        register(w!("MeshRMMPointerTest"), Some(procedure), CS_DBLCLKS)?;
        register(w!("MeshRMMScrollPane"), Some(pane), WNDCLASS_STYLES(0))?;
        let (ui, (window, pane, list)) = UiThread::start(|| unsafe {
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MeshRMMPointerTest"),
                w!("Pointer test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                80,
                600,
                400,
                None,
                None,
                None,
                None,
            )?;
            let pane = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("MeshRMMScrollPane"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL,
                10,
                10,
                200,
                240,
                Some(window),
                None,
                None,
                None,
            )?;
            let info = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: 109,
                nPage: 10,
                nPos: 0,
                nTrackPos: 0,
            };
            SetScrollInfo(pane, SB_VERT, &info, true);
            let list = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("LISTBOX"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_VSCROLL,
                240,
                10,
                200,
                200,
                Some(window),
                None,
                None,
                None,
            )?;
            for index in 0..100 {
                let label = wide(format!("Item {index}"));
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    None,
                    Some(LPARAM(label.as_ptr() as isize)),
                );
            }
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                240,
                230,
                200,
                24,
                Some(window),
                None,
                None,
                None,
            )?;
            anyhow::ensure!(
                SetForegroundWindow(window).as_bool(),
                "the test window did not take the foreground"
            );
            // The edit keeps the focus: the clicks below don't take it.
            SetFocus(Some(edit))?;
            Ok((window.0 as isize, pane.0 as isize, list.0 as isize))
        })?;
        let (window, pane, list) = (hwnd(window), hwnd(pane), hwnd(list));
        settle(workspace, 200);

        // Two quick clicks are a double-click.
        let point = client_to_screen(window, 500, 300);
        double_click(workspace, point.x, point.y)?;
        wait_until(workspace, "a double-click", 5, &|_| {
            DOUBLE_CLICKS.load(Ordering::SeqCst) == 1
        })?;

        // The pane's arrows scroll a line, its track a page, and its thumb drags.
        let position = || unsafe { GetScrollPos(pane, SB_VERT) };
        let mut bar = SCROLLBARINFO {
            cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetScrollBarInfo(pane, OBJID_VSCROLL, &mut bar)? };
        let x = (bar.rcScrollBar.left + bar.rcScrollBar.right) / 2;
        click_at(workspace, x, bar.rcScrollBar.bottom - bar.dxyLineButton / 2)?;
        wait_until(workspace, "the arrow to scroll a line", 5, &|_| {
            position() == 1
        })?;
        click_at(workspace, x, bar.rcScrollBar.bottom - bar.dxyLineButton - 4)?;
        wait_until(workspace, "the track to scroll a page", 5, &|_| {
            position() == 11
        })?;
        unsafe { GetScrollBarInfo(pane, OBJID_VSCROLL, &mut bar)? };
        let thumb = bar.rcScrollBar.top + (bar.xyThumbTop + bar.xyThumbBottom) / 2;
        drag(workspace, POINT { x, y: thumb }, POINT { x, y: thumb + 60 })?;
        anyhow::ensure!(
            position() > 30,
            "dragging the thumb scrolled to {}",
            position()
        );

        // The wheel scrolls the list under the pointer, not the focused edit.
        let point = center(list)?;
        let (x, y) = normalized(point.x, point.y);
        workspace.apply(RemoteInput::WheelAt {
            display_id: display(),
            x,
            y,
            horizontal: 0,
            vertical: -360,
        })?;
        wait_until(workspace, "the wheel to scroll the list", 5, &|_| unsafe {
            SendMessageW(list, LB_GETTOPINDEX, None, None).0 > 0
        })?;
        proof("background-pointer-test.bmp")?;

        // Double-clicking the caption maximizes the window.
        let mut rect = RECT::default();
        unsafe { GetWindowRect(window, &mut rect)? };
        double_click(workspace, (rect.left + rect.right) / 2, rect.top + 12)?;
        wait_until(
            workspace,
            "a caption double-click to maximize",
            5,
            &|_| unsafe { IsZoomed(window).as_bool() },
        )?;
        drop(ui);
        println!("Session 0 double-clicks, scrollbars, wheel and caption double-click passed");
        Ok(())
    })
}

/// A visible dialog (`#32770`) of the workspace's job whose title satisfies `accept`.
fn dialog(workspace: &Workspace, accept: &dyn Fn(&str) -> bool) -> Option<HWND> {
    background::windows().ok()?.into_iter().find(|window| {
        let mut class = [0_u16; 16];
        let length = unsafe { GetClassNameW(*window, &mut class) } as usize;
        String::from_utf16_lossy(&class[..length]) == "#32770"
            && accept(&window_text(*window))
            && workspace.owns_window(*window)
    })
}

/// Waits for a dialog, then closes it with Escape.
fn dialog_opens(
    workspace: &mut Workspace,
    what: &str,
    accept: &dyn Fn(&str) -> bool,
) -> anyhow::Result<()> {
    wait_until(workspace, what, 10, &|workspace| {
        dialog(workspace, accept).is_some()
    })
    .inspect_err(|_| {
        let _ = proof(&format!(
            "background-{}-failure.bmp",
            what.replace(' ', "-")
        ));
    })?;
    proof(&format!("background-{}.bmp", what.replace(' ', "-")))?;
    key(workspace, 0x01, false)?;
    wait_until(workspace, &format!("{what} to close"), 10, &|workspace| {
        dialog(workspace, accept).is_none()
    })
}

/// The first row of a report-view list, below its header.
fn first_row(list: HWND) -> anyhow::Result<POINT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(list, &mut rect)? };
    let top = child(list, "SysHeader32", &|_| true)
        .map(|header| {
            let mut header_rect = RECT::default();
            let _ = unsafe { GetWindowRect(header, &mut header_rect) };
            header_rect.bottom
        })
        .unwrap_or(rect.top);
    Ok(POINT {
        x: rect.left + 40,
        y: top + 8,
    })
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn native_apps_take_real_pointer_input() -> anyhow::Result<()> {
    in_workspace(|workspace| {
        let pin = |program: &str, arguments: &str| {
            PINS.iter()
                .position(|pin| pin.program.ends_with(program) && pin.arguments == arguments)
                .unwrap()
                + 1
        };
        let find = |workspace: &mut Workspace,
                    what: &str,
                    found: &dyn Fn() -> Option<HWND>|
         -> anyhow::Result<HWND> {
            wait_until(workspace, what, 20, &|_| found().is_some())?;
            found().context("window went away")
        };

        // regedit: the Edit menu opens on click without moving the splitter,
        // and Find... runs from a click.
        workspace.launch(pin("regedit.exe", "/m"))?;
        let regedit = find(workspace, "Registry Editor", &|| unsafe {
            FindWindowW(w!("RegEdit_RegEdit"), None).ok()
        })?;
        let tree = find(workspace, "the key tree", &|| {
            child(regedit, "SysTreeView32", &|_| true)
        })?;
        // regedit restores where it last was, which can be off the canvas.
        unsafe {
            let _ = ShowWindow(regedit, SW_RESTORE);
            SetWindowPos(regedit, None, 40, 24, 1000, 640, SWP_NOZORDER)?;
        }
        settle(workspace, 1000);
        let mut before = RECT::default();
        unsafe { GetWindowRect(tree, &mut before)? };
        let menu = unsafe { GetMenu(regedit) };
        let mut item = RECT::default();
        unsafe { GetMenuItemRect(Some(regedit), menu, 1, &mut item)? };
        println!("regedit's Edit menu is at {item:?}");
        click_at(
            workspace,
            (item.left + item.right) / 2,
            (item.top + item.bottom) / 2,
        )?;
        wait_until(workspace, "regedit's Edit menu", 5, &|_| {
            open_menu()
                .ok()
                .flatten()
                .is_some_and(|(_, items)| items.iter().any(|item| item.starts_with("Find")))
        })?;
        let mut after = RECT::default();
        unsafe { GetWindowRect(tree, &mut after)? };
        anyhow::ensure!(
            before == after,
            "the menu-bar click moved regedit's splitter from {before:?} to {after:?}"
        );
        let popup = unsafe { GetSubMenu(menu, 1) };
        let find_item = (0..unsafe { GetMenuItemCount(Some(popup)) })
            .find(|position| {
                let mut label = [0_u16; 64];
                let length = unsafe {
                    GetMenuStringW(popup, *position as u32, Some(&mut label), MF_BYPOSITION)
                };
                String::from_utf16_lossy(&label[..length.max(0) as usize])
                    .replace('&', "")
                    .starts_with("Find")
            })
            .context("regedit's Edit menu has no Find...")?;
        let target = popup_item(popup, find_item as u32)?;
        click_at(
            workspace,
            (target.left + target.right) / 2,
            (target.top + target.bottom) / 2,
        )?;
        dialog_opens(workspace, "regedit Find", &|title| title == "Find")?;

        // Double-clicking a value opens its editor.
        let address = find(workspace, "the address bar", &|| {
            child(regedit, "Edit", &|edit| {
                window_text(edit).starts_with("Computer")
            })
        })?;
        let point = center(address)?;
        click_at(workspace, point.x, point.y)?;
        let path = "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        // Home, then Shift+End, selects what the address bar shows.
        key(workspace, 0x47, true)?;
        for pressed in [true, false] {
            workspace.apply(RemoteInput::Key {
                display_id: display(),
                scan_code: 0x2a,
                extended: false,
                pressed,
            })?;
            if pressed {
                key(workspace, 0x4f, true)?;
            }
        }
        workspace.apply(RemoteInput::TypeText {
            display_id: display(),
            text: path.into(),
        })?;
        key(workspace, 0x1c, false)?;
        let expected = format!("Computer\\{path}");
        wait_until(workspace, "regedit to open the typed key", 10, &|_| {
            window_text(address) == expected
        })
        .with_context(|| format!("address bar shows {:?}", window_text(address)))?;
        let list = child(regedit, "SysListView32", &|_| true).context("regedit value list")?;
        settle(workspace, 500);
        let row = first_row(list)?;
        double_click(workspace, row.x, row.y)?;
        dialog_opens(workspace, "regedit value editor", &|title| {
            title.starts_with("Edit ")
        })?;

        // Services: the Action menu opens on click, double-clicking a service
        // opens its properties, and double-clicking the caption maximizes.
        workspace.launch(pin("mmc.exe", "services.msc"))?;
        let services = find(workspace, "Services", &|| unsafe {
            FindWindowW(w!("MMCMainFrame"), w!("Services")).ok()
        })?;
        wait_until(
            workspace,
            "Services to come to the front",
            10,
            &|_| unsafe { GetForegroundWindow() == services },
        )?;
        let list = find(workspace, "the service list", &|| {
            child(services, "SysListView32", &|list| unsafe {
                SendMessageW(
                    list,
                    windows::Win32::UI::Controls::LVM_GETITEMCOUNT,
                    None,
                    None,
                )
                .0 > 0
            })
        })?;
        settle(workspace, 1000);
        let row = first_row(list)?;
        println!("double-clicking the first service at {row:?}");
        double_click(workspace, row.x, row.y)?;
        dialog_opens(workspace, "service properties", &|title| {
            title.contains("Properties")
        })?;
        action_menu_opens(workspace, services)?;
        let mut rect = RECT::default();
        unsafe { GetWindowRect(services, &mut rect)? };
        double_click(workspace, rect.left + 200, rect.top + 12)?;
        wait_until(
            workspace,
            "a caption double-click to maximize",
            5,
            &|_| unsafe { IsZoomed(services).as_bool() },
        )?;

        // Disk Management's graphical pane scrolls by its arrows and thumb.
        workspace.launch(pin("mmc.exe", "diskmgmt.msc"))?;
        let disks = find(workspace, "Disk Management", &|| unsafe {
            FindWindowW(w!("MMCMainFrame"), w!("Disk Management")).ok()
        })?;
        settle(workspace, 3000);
        disk_pane_scrolls(workspace, disks)?;

        // Device Manager: double-clicking a category expands it, and
        // double-clicking a device opens its properties.
        workspace.launch(pin("mmc.exe", "devmgmt.msc"))?;
        let devices = find(workspace, "Device Manager", &|| unsafe {
            FindWindowW(w!("MMCMainFrame"), w!("Device Manager")).ok()
        })?;
        settle(workspace, 2000);
        let tree = child(devices, "SysTreeView32", &|_| true).context("the device tree")?;
        let row = |index: i32| -> anyhow::Result<POINT> {
            use windows::Win32::UI::Controls::TVM_GETITEMHEIGHT;
            let height = unsafe { SendMessageW(tree, TVM_GETITEMHEIGHT, None, None).0 } as i32;
            let mut rect = RECT::default();
            unsafe { GetWindowRect(tree, &mut rect)? };
            Ok(POINT {
                x: rect.left + 60,
                y: rect.top + 2 + index * height + height / 2,
            })
        };
        // Row 0 is the computer; row 1 its first category.
        let category = row(1)?;
        double_click(workspace, category.x, category.y)?;
        settle(workspace, 1000);
        let device = row(2)?;
        double_click(workspace, device.x + 20, device.y)?;
        dialog_opens(workspace, "device properties", &|title| {
            title.ends_with("Properties")
        })?;

        // Resource Monitor's menus open on click.
        workspace.launch(pin("resmon.exe", ""))?;
        let monitor = find(workspace, "Resource Monitor", &|| unsafe {
            FindWindowW(None, w!("Resource Monitor")).ok()
        })?;
        settle(workspace, 3000);
        let items = menu_bar_opens(workspace, monitor, 0)?;
        anyhow::ensure!(!items.is_empty(), "Resource Monitor's first menu is empty");
        println!("Resource Monitor menu: {items:?}");
        println!(
            "Session 0 regedit, Services, Disk Management and Resource Monitor pointer input passed"
        );
        Ok(())
    })
}

/// MMC draws its menu bar as a toolbar in its own process. Its Action button
/// is found by opening each button from the left until a menu shows the
/// snap-in's commands.
fn action_menu_opens(workspace: &mut Workspace, frame: HWND) -> anyhow::Result<()> {
    let mut frame_rect = RECT::default();
    unsafe { GetWindowRect(frame, &mut frame_rect)? };
    let bar = child(frame, "ToolbarWindow32", &|toolbar| {
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(toolbar, &mut rect) };
        rect.top - frame_rect.top < 60
    })
    .context("MMC's menu bar")?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(bar, &mut rect)? };
    let y = (rect.top + rect.bottom) / 2;
    for x in (rect.left + 4..rect.left + 200).step_by(8) {
        click_at(workspace, x, y)?;
        settle(workspace, 300);
        if let Some((_, items)) = open_menu()? {
            key(workspace, 0x01, false)?;
            key(workspace, 0x01, false)?;
            settle(workspace, 300);
            if items.iter().any(|item| item.starts_with("Refresh")) {
                println!("MMC Action menu: {items:?}");
                return Ok(());
            }
        }
    }
    anyhow::bail!("no click on MMC's menu bar opened the Action menu")
}

fn disk_pane_scrolls(workspace: &mut Workspace, frame: HWND) -> anyhow::Result<()> {
    let pane = child(frame, "AfxWnd42u", &|window| unsafe {
        let mut info = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_ALL,
            ..Default::default()
        };
        GetWindowLongW(window, GWL_STYLE) as u32 & WS_VSCROLL.0 != 0
            && GetScrollInfo(window, SB_VERT, &mut info).is_ok()
            && info.nMax - info.nMin + 1 > info.nPage as i32
    });
    let Some(pane) = pane else {
        println!("Disk Management's graphical pane has nothing to scroll on this machine");
        return Ok(());
    };
    let position = || unsafe { GetScrollPos(pane, SB_VERT) };
    let mut bar = SCROLLBARINFO {
        cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetScrollBarInfo(pane, OBJID_VSCROLL, &mut bar)? };
    let x = (bar.rcScrollBar.left + bar.rcScrollBar.right) / 2;
    let start = position();
    click_at(workspace, x, bar.rcScrollBar.bottom - bar.dxyLineButton / 2)?;
    wait_until(workspace, "Disk Management's arrow to scroll", 5, &|_| {
        position() > start
    })?;
    unsafe { GetScrollBarInfo(pane, OBJID_VSCROLL, &mut bar)? };
    let thumb = bar.rcScrollBar.top + (bar.xyThumbTop + bar.xyThumbBottom) / 2;
    let scrolled = position();
    drag(
        workspace,
        POINT { x, y: thumb },
        POINT {
            x,
            y: bar.rcScrollBar.top,
        },
    )?;
    anyhow::ensure!(
        position() < scrolled,
        "dragging Disk Management's thumb left it at {}",
        position()
    );
    proof("background-disk-management.bmp")?;
    Ok(())
}
