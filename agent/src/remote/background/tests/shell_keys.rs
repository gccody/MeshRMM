//! Session 0 checks of the Windows and Apps keys.
use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn windows_and_apps_keys() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
    static CONTEXT_MENUS: AtomicUsize = AtomicUsize::new(0);
    static CONTEXT_POINT: AtomicIsize = AtomicIsize::new(0);
    unsafe extern "system" fn test_window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_CONTEXTMENU {
            CONTEXT_MENUS.fetch_add(1, Ordering::SeqCst);
            CONTEXT_POINT.store(lparam.0, Ordering::SeqCst);
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let mut workspace = Workspace::new()?;
        let (window, edit) = unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(test_window_proc),
                lpszClassName: w!("MeshRMMKeyboardTest"),
                ..Default::default()
            });
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MeshRMMKeyboardTest"),
                w!("Keyboard test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                40,
                50,
                400,
                250,
                None,
                None,
                None,
                None,
            )?;
            let edit = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("EDIT"),
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_BORDER,
                10,
                10,
                300,
                24,
                Some(window),
                None,
                None,
                None,
            )?;
            (window, edit)
        };
        unsafe {
            anyhow::ensure!(
                SetForegroundWindow(window).as_bool(),
                "the test window did not take the foreground"
            );
            SetFocus(Some(edit))?;
        }
        press_win_r(&mut workspace)?;
        close_run(&mut workspace, edit)?;
        unsafe { SetFocus(Some(edit))? };
        key(&mut workspace, 0x13, false)?;
        settle(&mut workspace, 300);
        assert_eq!(
            window_text(edit),
            "r",
            "Win+R must type nothing, and R alone must still type"
        );
        let run = workspace.run.as_ref().context("Win+R opened Run")?;
        assert_eq!(window_text(run.edit), "", "Win+R typed into Run");

        unsafe { SetFocus(Some(window))? };
        key(&mut workspace, 0x5d, true)?;
        settle(&mut workspace, 300);
        assert_eq!(
            CONTEXT_MENUS.load(Ordering::SeqCst),
            1,
            "the Apps key must open exactly one context menu"
        );
        assert_eq!(
            CONTEXT_POINT.load(Ordering::SeqCst),
            -1,
            "a keyboard context menu has no pointer position"
        );
        Ok(())
    })
    .join()
    .expect("keyboard test panicked")
}

/// Selects the first row of `list` with a click, presses the Apps key, and
/// returns the context menu's items. Escape then closes it for good.
fn apps_key_menu(workspace: &mut Workspace, list: HWND) -> anyhow::Result<Vec<String>> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(list, &mut rect)? };
    let header = child(list, "SysHeader32", &|_| true)
        .map(|header| {
            let mut header_rect = RECT::default();
            let _ = unsafe { GetWindowRect(header, &mut header_rect) };
            header_rect.bottom
        })
        .unwrap_or(rect.top);
    // The list fills after the app shows it; a click before then selects nothing.
    let count = |message| unsafe { SendMessageW(list, message, None, None).0 };
    wait_until(workspace, "the list to fill", 10, &|_| {
        count(windows::Win32::UI::Controls::LVM_GETITEMCOUNT) > 0
    })?;
    settle(workspace, 500);
    click_at(workspace, rect.left + 40, header + 8)?;
    wait_until(workspace, "the click to select a row", 5, &|_| {
        count(windows::Win32::UI::Controls::LVM_GETSELECTEDCOUNT) > 0
    })?;
    settle(workspace, 300);
    key(workspace, 0x5d, true)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let (menu, items) = loop {
        if let Some(open) = open_menu()? {
            break open;
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "the Apps key opened no context menu"
        );
        workspace.pump();
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut position = RECT::default();
    unsafe { GetWindowRect(menu, &mut position)? };
    anyhow::ensure!(
        position.left >= rect.left
            && position.left < rect.right
            && position.top >= rect.top
            && position.top < rect.bottom,
        "context menu at {position:?} is not at the selected item in {rect:?}"
    );
    key(workspace, 0x01, false)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while open_menu()?.is_some() {
        anyhow::ensure!(Instant::now() < deadline, "Escape did not close the menu");
        workspace.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    // A second menu would mean the app also acted on the Apps key itself.
    let quiet = Instant::now() + Duration::from_secs(1);
    while Instant::now() < quiet {
        anyhow::ensure!(open_menu()?.is_none(), "the Apps key opened a second menu");
        workspace.pump();
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(items)
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn apps_key_opens_native_context_menus() -> anyhow::Result<()> {
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let mut workspace = Workspace::new()?;
        let wait = |workspace: &mut Workspace,
                    what: &str,
                    found: &dyn Fn() -> Option<HWND>|
         -> anyhow::Result<HWND> {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(window) = found() {
                    return Ok(window);
                }
                anyhow::ensure!(Instant::now() < deadline, "timed out waiting for {what}");
                workspace.pump();
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let pin = |program: &str, arguments: &str| {
            PINS.iter()
                .position(|pin| pin.program.ends_with(program) && pin.arguments == arguments)
                .unwrap()
                + 1
        };

        workspace.launch(pin("regedit.exe", "/m"))?;
        let regedit = wait(&mut workspace, "Registry Editor", &|| unsafe {
            FindWindowW(w!("RegEdit_RegEdit"), None).ok()
        })?;
        let address = wait(&mut workspace, "the address bar", &|| {
            child(regedit, "Edit", &|edit| {
                window_text(edit).starts_with("Computer")
            })
        })?;
        let mut rect = RECT::default();
        unsafe { GetWindowRect(address, &mut rect)? };
        // Type straight after the click, with no time for regedit to handle it.
        // Real input goes through one ordered queue, so no key is lost.
        click_at(
            &mut workspace,
            (rect.left + rect.right) / 2,
            (rect.top + rect.bottom) / 2,
        )?;
        key(&mut workspace, 0x47, true)?;
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code: 0x2a,
            extended: false,
            pressed: true,
        })?;
        key(&mut workspace, 0x4f, true)?;
        workspace.apply(RemoteInput::Key {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            scan_code: 0x2a,
            extended: false,
            pressed: false,
        })?;
        let path = "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        workspace.apply(RemoteInput::TypeText {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            text: path.into(),
        })?;
        key(&mut workspace, 0x1c, false)?;
        let expected = format!("Computer\\{path}");
        wait(&mut workspace, "regedit to open the typed key", &|| {
            (window_text(address) == expected).then_some(address)
        })
        .with_context(|| format!("address bar shows {:?}", window_text(address)))?;
        let list = child(regedit, "SysListView32", &|_| true).context("regedit value list")?;
        let items = apps_key_menu(&mut workspace, list)?;
        anyhow::ensure!(
            items.iter().any(|item| item.starts_with("Modify")),
            "regedit opened {items:?}, not the value's menu"
        );

        workspace.launch(pin("mmc.exe", "services.msc"))?;
        let services = wait(&mut workspace, "Services", &|| unsafe {
            FindWindowW(w!("MMCMainFrame"), w!("Services")).ok()
        })?;
        let list = wait(&mut workspace, "the service list", &|| {
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
        let items = apps_key_menu(&mut workspace, list)?;
        anyhow::ensure!(
            items.iter().any(|item| item.starts_with("Properties")),
            "Services opened {items:?}, not the service's menu"
        );
        Ok(())
    })
    .join()
    .expect("context menu test panicked")
}
