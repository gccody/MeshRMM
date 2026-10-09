use super::*;
use std::path::Path;
use windows::Win32::UI::Controls::*;

/// The File Explorer controls the checks drive.
#[derive(Clone, Copy)]
struct Browser {
    window: HWND,
    location: HWND,
    list: HWND,
    status: HWND,
    search: HWND,
}

impl Browser {
    fn navigate(&self, path: &Path) -> anyhow::Result<()> {
        unsafe {
            SendMessageW(
                self.location,
                WM_SETTEXT,
                None,
                Some(LPARAM(wide(&*path.to_string_lossy()).as_ptr() as isize)),
            );
        }
        command(self.window, 202)
    }
}

fn text(window: HWND) -> String {
    let mut value = [0u16; 8192];
    let length = unsafe {
        SendMessageW(
            window,
            WM_GETTEXT,
            Some(WPARAM(value.len())),
            Some(LPARAM(value.as_mut_ptr() as isize)),
        )
        .0
    };
    String::from_utf16_lossy(&value[..length as usize])
}

fn wait(workspace: &mut Workspace, status: HWND) -> anyhow::Result<()> {
    settle(workspace, 300);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Browser operation timed out: {}",
            text(status)
        );
        let before = text(status);
        settle(workspace, 200);
        let after = text(status);
        if before == after && !after.starts_with("Working") && !after.starts_with("Cancelling") {
            println!("Browser status: {after}");
            return Ok(());
        }
    }
}

fn select(list: HWND, name: &str) {
    unsafe {
        SendMessageW(list, WM_KEYDOWN, Some(WPARAM(0x24)), None);
    }
    for c in name.encode_utf16() {
        unsafe {
            SendMessageW(list, WM_CHAR, Some(WPARAM(c as usize)), None);
        }
    }
}

pub(super) fn interactions(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
    let root = std::env::var_os("MESHRMM_BACKGROUND_PROOF_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let fixture = root.join(format!("explorer-fixture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(fixture.join("Folder"))?;
    std::fs::write(fixture.join("alpha.txt"), b"Explorer fixture\r\n")?;
    std::fs::write(fixture.join("beta.txt"), b"Second fixture\r\n")?;
    std::fs::write(
        fixture.join("Folder").join("nested.txt"),
        b"Recursive copy fixture",
    )?;
    let browser = Browser {
        window,
        location: unsafe { GetDlgItem(Some(window), 201)? },
        list: unsafe { GetDlgItem(Some(window), 101)? },
        status: unsafe { GetDlgItem(Some(window), 231)? },
        search: unsafe { GetDlgItem(Some(window), 227)? },
    };
    copies_renames_and_pastes(workspace, browser, &fixture, &root)?;
    cuts_and_confirms_deletes(workspace, browser, &fixture, &root)?;
    searches_selects_and_switches_views(workspace, browser, &fixture)?;
    drags_and_undoes(workspace, browser, &fixture)?;
    frame_resizes_and_minimizes(workspace, window)?;
    command(window, 221)?;
    settle(workspace, 150);
    std::fs::write(root.join("explorer-view.bmp"), background::snapshot_bmp()?)?;
    command(window, 220)?;
    settle(workspace, 150);
    std::fs::write(root.join("explorer-home.bmp"), background::snapshot_bmp()?)?;
    browser.navigate(&root)?;
    wait(workspace, browser.status)?;
    std::fs::remove_dir_all(fixture)?;
    println!(
        "Explorer navigation, inline rename, file/folder copy, move, deletion confirmation, search, selection and view modes passed"
    );
    Ok(())
}

/// Exercises toolbar commands and verifies their effects only inside the fixture.
fn copies_renames_and_pastes(
    workspace: &mut Workspace,
    browser: Browser,
    fixture: &Path,
    root: &Path,
) -> anyhow::Result<()> {
    let Browser {
        window,
        location,
        list,
        status,
        ..
    } = browser;
    browser.navigate(fixture)?;
    wait(workspace, status)?;
    std::fs::write(
        root.join("explorer-initial.bmp"),
        background::snapshot_bmp()?,
    )?;
    assert_eq!(
        unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 },
        3
    );
    select(list, "alpha");
    command(window, 209)?;
    settle(workspace, 100);
    command(window, 210)?;
    wait(workspace, status)?;
    assert_eq!(
        std::fs::read(fixture.join("alpha - Copy.txt"))?,
        b"Explorer fixture\r\n"
    );
    command(window, 207)?;
    wait(workspace, status)?;
    let edit = HWND(unsafe { SendMessageW(list, LVM_GETEDITCONTROL, None, None).0 } as *mut _);
    anyhow::ensure!(!edit.is_invalid(), "New folder did not begin inline rename");
    unsafe {
        SendMessageW(
            edit,
            WM_SETTEXT,
            None,
            Some(LPARAM(w!("Renamed folder").as_ptr() as isize)),
        );
        PostMessageW(Some(edit), WM_KEYDOWN, WPARAM(13), LPARAM(0))?;
    }
    wait(workspace, status)?;
    anyhow::ensure!(
        fixture.join("Renamed folder").is_dir(),
        "Inline rename failed"
    );
    select(list, "Folder");
    command(window, 209)?;
    settle(workspace, 100);
    browser.navigate(&fixture.join("Renamed folder"))?;
    wait(workspace, status)?;
    command(window, 210)?;
    wait(workspace, status)?;
    assert_eq!(
        std::fs::read(fixture.join("Renamed folder/Folder/nested.txt"))?,
        b"Recursive copy fixture"
    );
    command(window, 212)?;
    wait(workspace, status)?;
    assert_eq!(text(location), fixture.to_string_lossy());
    command(window, 213)?;
    wait(workspace, status)?;
    assert_eq!(
        text(location),
        fixture.join("Renamed folder").to_string_lossy()
    );
    Ok(())
}

fn cuts_and_confirms_deletes(
    workspace: &mut Workspace,
    browser: Browser,
    fixture: &Path,
    root: &Path,
) -> anyhow::Result<()> {
    let Browser {
        window,
        list,
        status,
        ..
    } = browser;
    command(window, 203)?;
    wait(workspace, status)?;
    select(list, "beta");
    command(window, 214)?;
    settle(workspace, 100);
    browser.navigate(&fixture.join("Renamed folder"))?;
    wait(workspace, status)?;
    command(window, 210)?;
    wait(workspace, status)?;
    anyhow::ensure!(
        !fixture.join("beta.txt").exists() && fixture.join("Renamed folder/beta.txt").exists(),
        "Cut/paste failed"
    );
    // Cancel and confirm must act on the captured selection, not another row.
    select(list, "beta");
    command(window, 215)?;
    settle(workspace, 150);
    anyhow::ensure!(
        fixture.join("Renamed folder/beta.txt").exists(),
        "Delete skipped confirmation"
    );
    std::fs::write(
        root.join("explorer-delete.bmp"),
        background::snapshot_bmp()?,
    )?;
    command(window, 230)?;
    settle(workspace, 100);
    anyhow::ensure!(
        fixture.join("Renamed folder/beta.txt").exists(),
        "Cancel deleted the fixture"
    );
    command(window, 215)?;
    settle(workspace, 100);
    command(window, 229)?;
    wait(workspace, status)?;
    anyhow::ensure!(
        !fixture.join("Renamed folder/beta.txt").exists(),
        "Confirmed delete failed"
    );
    Ok(())
}

fn searches_selects_and_switches_views(
    workspace: &mut Workspace,
    browser: Browser,
    fixture: &Path,
) -> anyhow::Result<()> {
    let Browser {
        window,
        list,
        status,
        search,
        ..
    } = browser;
    browser.navigate(fixture)?;
    wait(workspace, status)?;
    unsafe {
        SendMessageW(
            search,
            WM_SETTEXT,
            None,
            Some(LPARAM(w!("nested").as_ptr() as isize)),
        );
    }
    command(window, 232)?;
    wait(workspace, status)?;
    assert_eq!(
        unsafe { SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 },
        2
    );
    unsafe {
        SendMessageW(
            search,
            WM_SETTEXT,
            None,
            Some(LPARAM(w!("").as_ptr() as isize)),
        );
    }
    command(window, 232)?;
    wait(workspace, status)?;
    command(window, 217)?;
    wait(workspace, status)?;
    assert_eq!(
        unsafe { SendMessageW(list, LVM_GETSELECTEDCOUNT, None, None).0 },
        4
    );
    command(window, 219)?;
    wait(workspace, status)?;
    assert_eq!(
        unsafe { SendMessageW(list, LVM_GETSELECTEDCOUNT, None, None).0 },
        0
    );
    for (command_id, mode) in [
        (224, LV_VIEW_ICON),
        (223, LV_VIEW_SMALLICON),
        (222, LV_VIEW_DETAILS),
    ] {
        command(window, command_id)?;
        wait(workspace, status)?;
        assert_eq!(
            unsafe { SendMessageW(list, LVM_GETVIEW, None, None).0 },
            mode as isize
        );
    }
    Ok(())
}

/// Drags the list's second row onto its first with remote pointer input.
fn drag_item(workspace: &mut Workspace, browser: Browser) -> anyhow::Result<()> {
    let Browser { list, status, .. } = browser;
    let header = HWND(unsafe { SendMessageW(list, LVM_GETHEADER, None, None).0 } as *mut _);
    let mut bounds = RECT::default();
    let mut heading = RECT::default();
    unsafe {
        GetWindowRect(list, &mut bounds)?;
        GetWindowRect(header, &mut heading)?;
    }
    let x = bounds.left + 80;
    let from_y = heading.bottom + 27;
    let to_y = heading.bottom + 9;
    workspace.move_pointer(
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (from_y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    settle(workspace, 100);
    for (px, py) in [(x + 18, from_y), (x + 18, to_y)] {
        workspace.apply(RemoteInput::PointerMove {
            display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
            x: (px as u32 * 65535 / (WIDTH - 1)) as u16,
            y: (py as u32 * 65535 / (HEIGHT - 1)) as u16,
        })?;
        settle(workspace, 100);
    }
    workspace.button(PointerButton::Left, false)?;
    wait(workspace, status)
}

/// Exercises real list-view drag notifications, then undoes the drops.
fn drags_and_undoes(
    workspace: &mut Workspace,
    browser: Browser,
    fixture: &Path,
) -> anyhow::Result<()> {
    let Browser {
        window,
        location,
        status,
        ..
    } = browser;
    let drag_folder = fixture.join("Drag test");
    std::fs::create_dir_all(drag_folder.join("Target"))?;
    std::fs::write(drag_folder.join("drag.txt"), b"drag fixture")?;
    browser.navigate(&drag_folder)?;
    wait(workspace, status)?;
    drag_item(workspace, browser)?;
    anyhow::ensure!(
        !drag_folder.join("drag.txt").exists() && drag_folder.join("Target/drag.txt").exists(),
        "Dragging into a folder did not move the file"
    );
    assert_eq!(
        text(location),
        drag_folder.to_string_lossy(),
        "Drop unexpectedly navigated away from the source"
    );
    std::fs::write(drag_folder.join("copy.txt"), b"ctrl drag fixture")?;
    command(window, 204)?;
    wait(workspace, status)?;
    workspace.apply(RemoteInput::Key {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        scan_code: 0x1d,
        extended: false,
        pressed: true,
    })?;
    settle(workspace, 100);
    drag_item(workspace, browser)?;
    workspace.apply(RemoteInput::Key {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        scan_code: 0x1d,
        extended: false,
        pressed: false,
    })?;
    anyhow::ensure!(
        drag_folder.join("copy.txt").exists() && drag_folder.join("Target/copy.txt").exists(),
        "Ctrl-drag did not preserve and copy the source"
    );
    command(window, 236)?;
    wait(workspace, status)?;
    anyhow::ensure!(
        drag_folder.join("copy.txt").exists() && !drag_folder.join("Target/copy.txt").exists(),
        "Undo copy did not preserve the original and remove its unchanged copy"
    );
    command(window, 236)?;
    wait(workspace, status)?;
    anyhow::ensure!(
        drag_folder.join("drag.txt").exists() && !drag_folder.join("Target/drag.txt").exists(),
        "Undo move did not restore the original location"
    );
    browser.navigate(fixture)?;
    wait(workspace, status)
}

/// Posted background input must resize and minimize the custom frame.
fn frame_resizes_and_minimizes(workspace: &mut Workspace, window: HWND) -> anyhow::Result<()> {
    let mut original = RECT::default();
    unsafe {
        GetWindowRect(window, &mut original)?;
    }
    let x = original.right - 2;
    let y = original.top + 220;
    workspace.move_pointer(
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    settle(workspace, 100);
    workspace.apply(RemoteInput::PointerMove {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        x: ((x - 60) as u32 * 65535 / (WIDTH - 1)) as u16,
        y: (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    })?;
    settle(workspace, 100);
    workspace.button(PointerButton::Left, false)?;
    settle(workspace, 100);
    let mut resized = RECT::default();
    unsafe {
        GetWindowRect(window, &mut resized)?;
    }
    anyhow::ensure!(
        resized.right - resized.left < original.right - original.left,
        "Explorer border did not resize with background pointer input"
    );
    unsafe {
        PostMessageW(
            Some(window),
            WM_SYSCOMMAND,
            WPARAM(SC_MAXIMIZE as usize),
            LPARAM(0),
        )?;
    }
    settle(workspace, 200);
    maximized_frame(window)?;
    unsafe {
        PostMessageW(
            Some(window),
            WM_SYSCOMMAND,
            WPARAM(SC_RESTORE as usize),
            LPARAM(0),
        )?;
    }
    settle(workspace, 200);
    let helpers = workspace.file_browsers.len();
    unsafe {
        GetWindowRect(window, &mut resized)?;
    }
    let x = resized.right - 115;
    let y = resized.top + 16;
    workspace.move_pointer(
        (x as u32 * 65535 / (WIDTH - 1)) as u16,
        (y as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    workspace.button(PointerButton::Left, false)?;
    settle(workspace, 200);
    anyhow::ensure!(
        unsafe { IsIconic(window) }.as_bool(),
        "Explorer minimize button did not respond"
    );
    workspace.launch(11)?;
    settle(workspace, 200);
    anyhow::ensure!(
        !unsafe { IsIconic(window) }.as_bool(),
        "Explorer taskbar pin did not restore the window"
    );
    assert_eq!(
        workspace.file_browsers.len(),
        helpers,
        "Restoring Explorer launched a duplicate"
    );
    Ok(())
}
