//! Session 0 checks that the workspace's real input drives menus, double-clicks,
//! scrollbars and the wheel the way a local mouse does.
use super::tests::*;
use super::*;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use windows::Win32::UI::Controls::SetScrollInfo;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

mod automation;
mod native_apps;

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

/// The launcher index of the pin that runs `program` with `arguments`.
fn pin(program: &str, arguments: &str) -> usize {
    PINS.iter()
        .position(|pin| pin.program.ends_with(program) && pin.arguments == arguments)
        .unwrap()
        + 1
}

fn find(
    workspace: &mut Workspace,
    what: &str,
    found: &dyn Fn() -> Option<HWND>,
) -> anyhow::Result<HWND> {
    wait_until(workspace, what, 20, &|_| found().is_some())?;
    found().context("window went away")
}

unsafe extern "system" fn menu_window_proc(
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

/// A window with a File and an Edit menu and a focused edit control, as
/// integers that can cross back to the test's thread.
fn menu_test_window() -> anyhow::Result<(isize, isize, isize)> {
    unsafe {
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
    }
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn menus_open_and_run_from_clicks() -> anyhow::Result<()> {
    in_workspace(|workspace| {
        register(
            w!("MeshRMMMenuTest"),
            Some(menu_window_proc),
            WNDCLASS_STYLES(0),
        )?;
        let (ui, (window, menu, edit)) = UiThread::start(menu_test_window)?;
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

        hover_switches_menus(workspace, window, menu)?;

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

/// Moving across the menu bar with a menu open switches menus.
fn hover_switches_menus(
    workspace: &mut Workspace,
    window: HWND,
    menu: HMENU,
) -> anyhow::Result<()> {
    let mut item = RECT::default();
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
    Ok(())
}

static DOUBLE_CLICKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "system" fn double_click_proc(
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
unsafe extern "system" fn scroll_pane_proc(
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

/// A window with a scrolling pane, a long list and a focused edit control,
/// as integers that can cross back to the test's thread.
fn pointer_test_windows() -> anyhow::Result<(isize, isize, isize)> {
    unsafe {
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
        // The edit keeps the focus: the test's clicks don't take it.
        SetFocus(Some(edit))?;
        Ok((window.0 as isize, pane.0 as isize, list.0 as isize))
    }
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn double_clicks_scrollbars_and_wheel() -> anyhow::Result<()> {
    in_workspace(|workspace| {
        register(
            w!("MeshRMMPointerTest"),
            Some(double_click_proc),
            CS_DBLCLKS,
        )?;
        register(
            w!("MeshRMMScrollPane"),
            Some(scroll_pane_proc),
            WNDCLASS_STYLES(0),
        )?;
        let (ui, (window, pane, list)) = UiThread::start(pointer_test_windows)?;
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
