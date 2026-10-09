//! Session 0 checks of the taskbar's pins and task buttons, and of the screen it shares.
use super::*;

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn running_window_taskbar_restores_minimized_window() -> anyhow::Result<()> {
    unsafe extern "system" fn test_window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
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
        let window = unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(test_window_proc),
                lpszClassName: w!("MeshRMMTaskbarTest"),
                hIcon: LoadIconW(None, IDI_WARNING)?,
                ..Default::default()
            });
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MeshRMMTaskbarTest"),
                w!("Taskbar restore test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                40,
                50,
                400,
                250,
                None,
                None,
                None,
                None,
            )?
        };
        let button = task_button_renders(&mut workspace, window)?;
        // Clicking its minimize button would start Windows' button tracking,
        // which waits for input this thread sends.
        unsafe {
            let _ = ShowWindow(window, SW_MINIMIZE);
        }
        workspace.refresh_tasks()?;
        anyhow::ensure!(
            unsafe { IsIconic(window) }.as_bool(),
            "window did not minimize"
        );
        anyhow::ensure!(
            workspace.tasks.iter().any(|task| task.window == window),
            "minimized window disappeared from taskbar"
        );
        click_task(&mut workspace, button)?;
        anyhow::ensure!(
            !unsafe { IsIconic(window) }.as_bool(),
            "taskbar button did not restore the minimized window"
        );

        let (run, app_window) = owned_windows_follow_taskbar_rules(&mut workspace, window)?;

        unsafe { DestroyWindow(window)? };
        workspace.refresh_tasks()?;
        anyhow::ensure!(
            !workspace
                .tasks
                .iter()
                .any(|task| [window, run, app_window].contains(&task.window)),
            "closed window remained on taskbar"
        );
        Ok(())
    })
    .join()
    .expect("background taskbar test panicked")
}

/// Checks that `window` has a task button showing its icon and the running
/// indicator, and returns the button.
fn task_button_renders(workspace: &mut Workspace, window: HWND) -> anyhow::Result<HWND> {
    workspace.refresh_tasks()?;
    let task = workspace
        .tasks
        .iter()
        .find(|task| task.window == window)
        .context("running window is missing from taskbar")?;
    anyhow::ensure!(
        !task.icon.is_invalid(),
        "running window has no taskbar icon"
    );
    let button = task.button;
    workspace.pump();
    let mut rect = RECT::default();
    unsafe { GetWindowRect(button, &mut rect)? };
    let frame = background::snapshot_bmp()?;
    let pixel = |x: i32, y: i32| -> &[u8] {
        let start = 54 + (y as usize * WIDTH as usize + x as usize) * 4;
        &frame[start..start + 3]
    };
    let background = pixel(rect.left + 1, rect.top + 1);
    anyhow::ensure!(
        (rect.top + 5..rect.bottom - 5)
            .any(|y| { (rect.left + 8..rect.right - 8).any(|x| pixel(x, y) != background) }),
        "running-window icon did not render"
    );
    anyhow::ensure!(
        pixel((rect.left + rect.right) / 2, rect.bottom - 2) != background,
        "running-window indicator did not render"
    );
    Ok(button)
}

/// Checks which of `window`'s owned windows get task buttons, and that one
/// restores a covered dialog. Returns the windows that get buttons.
fn owned_windows_follow_taskbar_rules(
    workspace: &mut Workspace,
    window: HWND,
) -> anyhow::Result<(HWND, HWND)> {
    // Run and System Properties are owned by hidden windows; Find and
    // Properties dialogs are owned by the visible window they belong to.
    let create = |title: PCWSTR, ex_style: WINDOW_EX_STYLE, owner: Option<HWND>, visible: bool| unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME | ex_style,
            w!("MeshRMMTaskbarTest"),
            title,
            WS_POPUP | WS_CAPTION | WS_SYSMENU | if visible { WS_VISIBLE } else { WINDOW_STYLE(0) },
            80,
            90,
            300,
            180,
            owner,
            None,
            None,
            None,
        )
    };
    let hidden_owner = create(w!("Hidden owner"), WINDOW_EX_STYLE(0), None, false)?;
    let run = create(w!("Run"), WINDOW_EX_STYLE(0), Some(hidden_owner), true)?;
    let hidden_tool = create(
        w!("Hidden tool"),
        WS_EX_TOOLWINDOW,
        Some(hidden_owner),
        true,
    )?;
    let find = create(w!("Find"), WINDOW_EX_STYLE(0), Some(window), true)?;
    let app_window = create(w!("App window"), WS_EX_APPWINDOW, Some(window), true)?;
    workspace.refresh_tasks()?;
    let task = |target: HWND| workspace.tasks.iter().find(|task| task.window == target);
    anyhow::ensure!(
        task(run).is_some(),
        "dialog owned by a hidden window has no taskbar button"
    );
    anyhow::ensure!(
        task(app_window).is_some(),
        "owned WS_EX_APPWINDOW window has no taskbar button"
    );
    anyhow::ensure!(
        task(find).is_none(),
        "dialog owned by a visible window got a taskbar button"
    );
    anyhow::ensure!(
        task(hidden_owner).is_none() && task(hidden_tool).is_none(),
        "hidden or tool window got a taskbar button"
    );
    let run_button = task(run).context("Run task disappeared")?.button;
    // A foreground window from another process can't be covered with
    // HWND_TOP, so restoring has to activate the dialog.
    anyhow::ensure!(
        unsafe { SetForegroundWindow(window) }.as_bool(),
        "test window did not take the foreground"
    );
    anyhow::ensure!(above(window, run), "test window did not cover the dialog");
    click_task(workspace, run_button)?;
    anyhow::ensure!(
        above(run, window) && unsafe { GetForegroundWindow() } == run,
        "taskbar button did not bring back the covered dialog"
    );
    unsafe { DestroyWindow(hidden_owner)? };
    Ok((run, app_window))
}

/// Whether `upper` comes before `lower` in the desktop's z-order.
fn above(upper: HWND, lower: HWND) -> bool {
    let mut next = unsafe { GetWindow(upper, GW_HWNDNEXT) };
    while let Ok(window) = next {
        if window == lower {
            return true;
        }
        next = unsafe { GetWindow(window, GW_HWNDNEXT) };
    }
    false
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; changes Session 0's display mode"]
fn workspace_sizes_session_zero_screen_to_canvas() -> anyhow::Result<()> {
    unsafe extern "system" fn test_window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }
    fn metrics() -> (i32, i32, RECT) {
        let mut area = RECT::default();
        unsafe {
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some((&mut area as *mut RECT).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            (
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
                area,
            )
        }
    }
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let before = metrics();
        let workspace = Workspace::new()?;
        let (width, height, area) = metrics();
        assert_eq!((width, height), (WIDTH as i32, HEIGHT as i32));
        assert_eq!(
            (area.left, area.top, area.right, area.bottom),
            (0, 0, WIDTH as i32, HEIGHT as i32 - TASKBAR_HEIGHT)
        );
        let window = unsafe {
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(test_window_proc),
                lpszClassName: w!("MeshRMMScreenTest"),
                ..Default::default()
            });
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("MeshRMMScreenTest"),
                w!("Screen size test"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                40,
                50,
                400,
                250,
                None,
                None,
                None,
                None,
            )?
        };
        let mut rect = RECT::default();
        unsafe {
            let _ = ShowWindow(window, SW_MAXIMIZE);
            GetWindowRect(window, &mut rect)?;
            DestroyWindow(window)?;
        }
        // A maximized frame overhangs the work area by its border on each side.
        assert_eq!(rect.left + rect.right, WIDTH as i32, "{rect:?}");
        assert_eq!(
            rect.top + rect.bottom,
            HEIGHT as i32 - TASKBAR_HEIGHT,
            "{rect:?}"
        );
        drop(workspace);
        let after = metrics();
        assert_eq!((after.0, after.1), (before.0, before.1));
        assert_eq!(
            (after.2.right, after.2.bottom),
            (before.2.right, before.2.bottom)
        );
        Ok(())
    })
    .join()
    .expect("screen size test panicked")
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn console_exit_closes_window_and_task() -> anyhow::Result<()> {
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let mut workspace = Workspace::new()?;
        let pin = PINS
            .iter()
            .position(|pin| pin.program == "cmd.exe")
            .unwrap()
            + 1;
        workspace.launch(pin)?;
        let console = wait_for_job_window(&mut workspace, "cmd", &|class, _| {
            class == "ConsoleWindowClass"
        })?;
        wait_until(&mut workspace, "the console task", 10, &|workspace| {
            workspace.tasks.iter().any(|task| task.window == console)
        })?;
        // Let cmd reach its prompt before typing.
        settle(&mut workspace, 2000);
        let mut rect = RECT::default();
        unsafe { GetWindowRect(console, &mut rect)? };
        click_at(&mut workspace, rect.left + 100, rect.top + 80)?;
        wait_until(
            &mut workspace,
            "the console to take the foreground",
            5,
            &|_| unsafe { GetForegroundWindow() == console },
        )?;
        // A leaked Win+R would turn the command into "rexit". Closing Run
        // must hand typing back to the console.
        press_win_r(&mut workspace)?;
        close_run(&mut workspace, console)?;
        workspace.apply(RemoteInput::TypeText {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            text: "exit".into(),
        })?;
        send_key(&mut workspace, 0x1c, true)?;
        send_key(&mut workspace, 0x1c, false)?;
        wait_until(&mut workspace, "the console window to close", 10, &|_| {
            !unsafe { IsWindow(Some(console)) }.as_bool()
        })?;
        wait_until(&mut workspace, "the console task to go", 10, &|workspace| {
            !workspace.tasks.iter().any(|task| task.window == console)
        })?;
        println!("Session 0 console exit closed its window and task");
        Ok(())
    })
    .join()
    .expect("console exit test thread panicked")
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn new_pins_open_in_workspace_job() -> anyhow::Result<()> {
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create()?;
        let _binding = background::Desktop::bind()?;
        let mut workspace = Workspace::new()?;
        workspace.pump();
        // Every pin has an icon.
        for (index, pin) in PINS.iter().enumerate() {
            let button = unsafe { GetDlgItem(Some(workspace.shell), index as i32 + 1)? };
            let icon = unsafe { GetWindowLongPtrW(button, GWLP_USERDATA) };
            anyhow::ensure!(icon != 0, "{} has no icon", pin.label);
        }
        proof("background-pins.bmp")?;
        let pin = |label: &str| PINS.iter().position(|pin| pin.label == label).unwrap() + 1;
        for (label, class, title) in [
            ("Disk Management", "MMCMainFrame", "Disk Management"),
            ("System Properties", "#32770", "System Properties"),
            ("Notepad", "Notepad", "Untitled - Notepad"),
        ] {
            workspace.launch(pin(label))?;
            let window = wait_for_job_window(&mut workspace, label, &|window_class, text| {
                window_class == class && text == title
            })?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !workspace.tasks.iter().any(|task| task.window == window) {
                anyhow::ensure!(Instant::now() < deadline, "{label} got no taskbar button");
                workspace.pump();
                std::thread::sleep(Duration::from_millis(50));
            }
            proof(&format!("background-pin-{}.bmp", label.replace(' ', "-")))?;
            close_job_window(&mut workspace, window)?;
        }
        Ok(())
    })
    .join()
    .expect("pin test panicked")
}
