use super::*;
use std::path::Path;
use windows::Win32::UI::Controls::*;

fn click(workspace: &mut Workspace, x: i32, y: i32) -> anyhow::Result<()> {
    workspace.move_pointer(
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    workspace.button(PointerButton::Left, false)?;
    settle(workspace, 60);
    Ok(())
}

fn select_tab(workspace: &mut Workspace, tabs: HWND, index: usize) -> anyhow::Result<()> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(tabs, &mut rect)?;
    }
    // Exercise actual routed mouse input. The native tab widths depend on
    // the Windows font/theme, so scan instead of guessing fixed widths.
    for x in (rect.left + 5..rect.right).step_by(6) {
        click(workspace, x, rect.top + 10)?;
        if unsafe { SendMessageW(tabs, TCM_GETCURSEL, None, None).0 } == index as isize {
            return Ok(());
        }
    }
    anyhow::bail!("Tab {index} did not respond to background mouse input")
}

pub(super) fn interactions(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
    let proof = std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&proof)?;
    let tabs = unsafe { GetDlgItem(Some(window), 104)? };
    assert_eq!(
        unsafe { SendMessageW(tabs, TCM_GETITEMCOUNT, None, None).0 },
        7
    );
    settle(workspace, 2400);
    std::fs::write(
        proof.join("task-manager-processes.bmp"),
        background::snapshot_bmp()?,
    )?;
    every_tab_renders(workspace, window, tabs, &proof)?;
    caption_buttons_work(workspace, window)?;
    select_tab(workspace, tabs, 0)?;
    command(window, 213)?; // pause; manual refresh must still work
    command(window, 102)?;
    settle(workspace, 200);
    command(window, 105)?;
    settle(workspace, 100);
    assert!(!unsafe { IsWindowVisible(tabs) }.as_bool());
    std::fs::write(
        proof.join("task-manager-compact.bmp"),
        background::snapshot_bmp()?,
    )?;
    command(window, 105)?;
    command(window, 211)?;
    settle(workspace, 100);
    assert!(unsafe { IsWindowVisible(tabs) }.as_bool());
    runs_new_task(workspace, window, &proof)?;
    ends_task_after_confirmation(workspace, window, tabs, &proof)?;
    select_tab(workspace, tabs, 0)?;
    command(window, 106)?;
    settle(workspace, 300);
    Ok(())
}

fn every_tab_renders(
    workspace: &mut Workspace,
    window: HWND,
    tabs: HWND,
    proof: &Path,
) -> anyhow::Result<()> {
    for (index, name) in [
        "processes",
        "performance",
        "history",
        "startup",
        "users",
        "details",
        "services",
    ]
    .iter()
    .enumerate()
    {
        select_tab(workspace, tabs, index)?;
        settle(workspace, 250);
        if index == 6 {
            services_scrollbars_scroll(workspace, window)?;
            settle(workspace, 100);
        }
        std::fs::write(
            proof.join(format!("task-manager-{name}.bmp")),
            background::snapshot_bmp()?,
        )?;
    }
    Ok(())
}

/// Checks that the service list's scrollbar arrows and thumbs take clicks and
/// drags, then scrolls the list back.
fn services_scrollbars_scroll(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
    let list = unsafe { GetDlgItem(Some(window), 101)? };
    for (id, bar) in [(OBJID_VSCROLL, SB_VERT), (OBJID_HSCROLL, SB_HORZ)] {
        let mut info = SCROLLBARINFO {
            cbSize: std::mem::size_of::<SCROLLBARINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetScrollBarInfo(list, id, &mut info)? };
        assert_eq!(info.rgstate[0] & 0x8000, 0, "Scrollbar must be visible");
        let before = unsafe { GetScrollPos(list, bar) };
        let x = if bar == SB_VERT {
            (info.rcScrollBar.left + info.rcScrollBar.right) / 2
        } else {
            info.rcScrollBar.right - info.dxyLineButton / 2
        };
        let y = if bar == SB_VERT {
            info.rcScrollBar.bottom - info.dxyLineButton / 2
        } else {
            (info.rcScrollBar.top + info.rcScrollBar.bottom) / 2
        };
        click(workspace, x, y)?;
        assert!(
            unsafe { GetScrollPos(list, bar) } > before,
            "Scrollbar arrow did not scroll"
        );
        unsafe { GetScrollBarInfo(list, id, &mut info)? };
        let before_drag = unsafe { GetScrollPos(list, bar) };
        let thumb = (info.xyThumbTop + info.xyThumbBottom) / 2;
        let (x, y) = if bar == SB_VERT {
            (x, info.rcScrollBar.top + thumb)
        } else {
            (info.rcScrollBar.left + thumb, y)
        };
        workspace.move_pointer(
            (x as u32 * 65535 / (WIDTH - 1)) as u16,
            (y as u32 * 65535 / (HEIGHT - 1)) as u16,
        )?;
        workspace.button(PointerButton::Left, true)?;
        settle(workspace, 60);
        let (x, y) = if bar == SB_VERT {
            (x, y + 30)
        } else {
            (x + 30, y)
        };
        workspace.apply(RemoteInput::PointerMove {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            x: (x as u32 * 65535 / (WIDTH - 1)) as u16,
            y: (y as u32 * 65535 / (HEIGHT - 1)) as u16,
        })?;
        settle(workspace, 60);
        workspace.button(PointerButton::Left, false)?;
        settle(workspace, 60);
        assert!(
            unsafe { GetScrollPos(list, bar) } > before_drag,
            "Scrollbar {bar:?} thumb did not drag from {before_drag} (now {})",
            unsafe { GetScrollPos(list, bar) }
        );
        unsafe {
            SendMessageW(
                list,
                if bar == SB_VERT {
                    WM_VSCROLL
                } else {
                    WM_HSCROLL
                },
                Some(WPARAM(SB_TOP.0 as usize)),
                None,
            );
        }
    }
    Ok(())
}

/// Checks the minimize, maximize and restore buttons and the resize border,
/// then puts the window back where it was.
fn caption_buttons_work(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(window, &mut bounds)? };
    click(workspace, bounds.right - 116, bounds.top + 15)?;
    settle(workspace, 200);
    assert!(
        unsafe { IsIconic(window) }.as_bool(),
        "Minimize button did not minimize"
    );
    let helpers = workspace.task_managers.len();
    workspace.launch(7)?;
    settle(workspace, 200);
    let items = menu_bar_opens(workspace, window, 0)?;
    anyhow::ensure!(
        items.iter().any(|item| item.starts_with("Run new task")),
        "Task Manager's File menu opened {items:?}"
    );
    settle(workspace, 200);
    assert!(
        !unsafe { IsIconic(window) }.as_bool(),
        "Taskbar pin did not restore Task Manager"
    );
    assert_eq!(
        workspace.task_managers.len(),
        helpers,
        "Restoring created a duplicate helper"
    );
    unsafe { GetWindowRect(window, &mut bounds)? };
    click(workspace, bounds.right - 70, bounds.top + 15)?;
    settle(workspace, 200);
    assert!(
        unsafe { IsZoomed(window) }.as_bool(),
        "Maximize button did not maximize"
    );
    bounds = maximized_frame(window)?;
    click(workspace, bounds.right - 70, bounds.top + 15)?;
    settle(workspace, 200);
    assert!(!unsafe { IsZoomed(window) }.as_bool());
    unsafe { GetWindowRect(window, &mut bounds)? };
    let original = bounds;
    workspace.move_pointer(
        ((bounds.right - 1) as u32 * 65535 / (WIDTH - 1)) as u16,
        ((bounds.bottom - 1) as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    settle(workspace, 60);
    workspace.apply(RemoteInput::PointerMove {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        x: ((bounds.right + 89) as u32 * 65535 / (WIDTH - 1)) as u16,
        y: ((bounds.bottom + 59) as u32 * 65535 / (HEIGHT - 1)) as u16,
    })?;
    settle(workspace, 60);
    workspace.button(PointerButton::Left, false)?;
    settle(workspace, 100);
    unsafe { GetWindowRect(window, &mut bounds)? };
    assert!(
        bounds.right > original.right + 50 && bounds.bottom > original.bottom + 30,
        "Window border did not resize: {bounds:?}"
    );
    unsafe {
        SetWindowPos(
            window,
            None,
            original.left,
            original.top,
            original.right - original.left,
            original.bottom - original.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    settle(workspace, 100);
    Ok(())
}

/// Runs a benign command through the actual new-task form on this desktop.
fn runs_new_task(workspace: &mut Workspace, window: HWND, proof: &Path) -> anyhow::Result<()> {
    let output = proof.join("task-manager-run.txt");
    let _ = std::fs::remove_file(&output);
    command(window, 201)?;
    settle(workspace, 100);
    let edit = unsafe { GetDlgItem(Some(window), 107)? };
    let text = wide(format!(
        "cmd.exe /c echo MeshRMMTaskManagerFixture>\"{}\"",
        output.display()
    ));
    unsafe {
        SendMessageW(edit, WM_SETTEXT, None, Some(LPARAM(text.as_ptr() as isize)));
    }
    command(window, 108)?;
    settle(workspace, 600);
    anyhow::ensure!(
        std::fs::read_to_string(&output)?.contains("MeshRMMTaskManagerFixture"),
        "Run new task did not execute"
    );
    Ok(())
}

/// Termination is tested only against an executable copied into this test's
/// directory. Verify the selected PID before issuing any End Task command.
fn ends_task_after_confirmation(
    workspace: &mut Workspace,
    window: HWND,
    tabs: HWND,
    proof: &Path,
) -> anyhow::Result<()> {
    let exe = proof.join("MeshRMMTaskFixture.exe");
    std::fs::copy(
        crate::win32::windows_directory()?.join("System32\\cmd.exe"),
        &exe,
    )?;
    let mut child = std::process::Command::new(&exe)
        .args(["/c", "pause"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()?;
    let result = (|| -> anyhow::Result<()> {
        select_tab(workspace, tabs, 5)?;
        command(window, 102)?;
        settle(workspace, 1800);
        let list = unsafe { GetDlgItem(Some(window), 101)? };
        unsafe {
            SendMessageW(list, WM_KEYDOWN, Some(WPARAM(0x24)), None);
        }
        for c in "MeshRMMTaskFixture.exe".encode_utf16() {
            unsafe {
                SendMessageW(list, WM_CHAR, Some(WPARAM(c as usize)), None);
            }
        }
        command(window, 222)?;
        settle(workspace, 100);
        let status = unsafe { GetDlgItem(Some(window), 109)? };
        let mut text = [0u16; 4096];
        let len = unsafe { GetWindowTextW(status, &mut text) };
        let text = String::from_utf16_lossy(&text[..len as usize]);
        anyhow::ensure!(
            text.split("  |  ").nth(1) == Some(child.id().to_string().as_str()),
            "Refusing to end unexpected selection: {text}"
        );
        command(window, 103)?;
        settle(workspace, 100);
        anyhow::ensure!(child.try_wait()?.is_none(), "End task skipped confirmation");
        std::fs::write(
            proof.join("task-manager-confirmation.bmp"),
            background::snapshot_bmp()?,
        )?;
        command(window, 106)?;
        settle(workspace, 100);
        anyhow::ensure!(child.try_wait()?.is_none(), "Cancel terminated the process");
        command(window, 103)?;
        settle(workspace, 100);
        command(window, 103)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait()?.is_none() {
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Confirmed End Task did not terminate fixture"
            );
            settle(workspace, 50);
        }
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(exe);
    result
}
