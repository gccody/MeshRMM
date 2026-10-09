//! Session 0 checks of the built-in Task Manager and File Explorer.
use super::*;

mod file_browser;
mod task_manager;

fn command(window: HWND, id: usize) -> anyhow::Result<()> {
    unsafe {
        PostMessageW(Some(window), WM_COMMAND, WPARAM(id), LPARAM(0))?;
    }
    Ok(())
}

/// Checks that a maximized built-in tool shows its whole frame and client area
/// on the work area, above the taskbar, and returns the frame.
fn maximized_frame(window: HWND) -> anyhow::Result<RECT> {
    let mut outer = RECT::default();
    let mut client = RECT::default();
    let mut origin = POINT::default();
    unsafe {
        GetWindowRect(window, &mut outer)?;
        GetClientRect(window, &mut client)?;
        ClientToScreen(window, &mut origin).ok()?;
    }
    let client = RECT {
        left: client.left + origin.x,
        top: client.top + origin.y,
        right: client.right + origin.x,
        bottom: client.bottom + origin.y,
    };
    let area = RECT {
        left: 0,
        top: 0,
        right: WIDTH as i32,
        bottom: HEIGHT as i32 - TASKBAR_HEIGHT,
    };
    let frame = frame_rect(window, outer);
    anyhow::ensure!(
        frame == area,
        "maximized frame {frame:?} (window {outer:?}) must fill the work area"
    );
    anyhow::ensure!(
        client.left == area.left + 1
            && client.right == area.right - 1
            && client.bottom == area.bottom - 1
            && client.top > area.top,
        "maximized client area {client:?} must fill the frame"
    );
    Ok(frame)
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn task_manager_opens_in_background() -> anyhow::Result<()> {
    tool_opens_in_background(7, "Task Manager", "background-task-manager.bmp")
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn file_browser_opens_in_background() -> anyhow::Result<()> {
    tool_opens_in_background(11, "File Explorer", "background-file-browser.bmp")
}

fn tool_opens_in_background(
    index: usize,
    expected_title: &'static str,
    screenshot: &'static str,
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
        workspace.launch(index)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            workspace.pump();
            for window in background::windows()? {
                let mut title = [0_u16; 256];
                let count = unsafe { GetWindowTextW(window, &mut title) };
                if String::from_utf16_lossy(&title[..count as usize]) == expected_title {
                    let list = unsafe { GetDlgItem(Some(window), 101)? };
                    let mut rows = 0;
                    unsafe {
                        SendMessageTimeoutW(
                            list,
                            windows::Win32::UI::Controls::LVM_GETITEMCOUNT,
                            WPARAM(0),
                            LPARAM(0),
                            SMTO_ABORTIFHUNG,
                            100,
                            Some(&mut rows),
                        );
                    }
                    if rows == 0 {
                        continue;
                    }
                    let mut pid = 0;
                    unsafe {
                        GetWindowThreadProcessId(window, Some(&mut pid));
                    }
                    let process = unsafe {
                        OpenProcess(
                            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                            false,
                            pid,
                        )?
                    };
                    let mut in_job = windows::core::BOOL(0);
                    unsafe {
                        IsProcessInJob(process, Some(workspace.job), &mut in_job)?;
                    }
                    assert!(in_job.as_bool(), "process manager escaped session cleanup");
                    if index == 7 {
                        task_manager::interactions(&mut workspace, window)?;
                    }
                    if index == 11 {
                        file_browser::interactions(&mut workspace, window)?;
                    }
                    std::fs::write(
                        std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
                            .map(std::path::PathBuf::from)
                            .unwrap_or_else(std::env::temp_dir)
                            .join(screenshot),
                        background::snapshot_bmp()?,
                    )?;
                    drop(workspace);
                    let exited = unsafe { WaitForSingleObject(process, 5000) };
                    let _ = unsafe { CloseHandle(process) };
                    assert_eq!(
                        exited, WAIT_OBJECT_0,
                        "background tool survived workspace close"
                    );
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                for window in background::windows()? {
                    let mut title = [0u16; 512];
                    let count = unsafe { GetWindowTextW(window, &mut title) };
                    eprintln!(
                        "Window: {}",
                        String::from_utf16_lossy(&title[..count as usize])
                    );
                }
                for (pid, process) in workspace
                    .task_managers
                    .iter()
                    .chain(&workspace.file_browsers)
                {
                    let mut code = 0;
                    let _ = unsafe { GetExitCodeProcess(*process, &mut code) };
                    eprintln!("Background tool {pid} exit={code}");
                }
                std::fs::write(
                    std::env::temp_dir().join("task-manager-failure.bmp"),
                    background::snapshot_bmp()?,
                )?;
                anyhow::bail!("{expected_title} did not open on the background desktop");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    })
    .join()
    .expect("background test panicked")
}
