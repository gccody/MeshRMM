//! Session 0 checks that regedit and the MMC snap-ins take real pointer input.
use super::*;
use windows::Win32::UI::Controls::LVM_GETITEMCOUNT;

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
        let regedit = regedit_edit_menu_runs_find(workspace)?;
        let row = regedit_value_opens_editor(workspace, regedit)?;
        regedit_value_menu_opens_at_pointer(workspace, row)?;
        services_take_clicks(workspace)?;

        // Disk Management's graphical pane scrolls by its arrows and thumb.
        workspace.launch(pin("mmc.exe", "diskmgmt.msc"))?;
        let disks = find(workspace, "Disk Management", &|| unsafe {
            FindWindowW(w!("MMCMainFrame"), w!("Disk Management")).ok()
        })?;
        settle(workspace, 3000);
        disk_pane_scrolls(workspace, disks)?;
        device_manager_takes_double_clicks(workspace)?;

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

/// regedit: the Edit menu opens on click without moving the splitter,
/// and Find... runs from a click.
fn regedit_edit_menu_runs_find(workspace: &mut Workspace) -> anyhow::Result<HWND> {
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
            let length =
                unsafe { GetMenuStringW(popup, *position as u32, Some(&mut label), MF_BYPOSITION) };
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
    Ok(regedit)
}

/// Double-clicking a value opens its editor. Returns the value's row.
fn regedit_value_opens_editor(workspace: &mut Workspace, regedit: HWND) -> anyhow::Result<POINT> {
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
    Ok(row)
}

/// Right-clicking a value opens the value's menu at the pointer. regedit
/// reads the real cursor, so a stale one gets its empty-area New menu
/// at Session 0's screen centre.
fn regedit_value_menu_opens_at_pointer(
    workspace: &mut Workspace,
    row: POINT,
) -> anyhow::Result<()> {
    let (x, y) = normalized(row.x, row.y);
    for pressed in [true, false] {
        workspace.apply(RemoteInput::PointerButtonAt {
            display_id: display(),
            x,
            y,
            button: PointerButton::Right,
            pressed,
        })?;
    }
    wait_until(workspace, "regedit's value menu", 5, &|_| {
        open_menu().ok().flatten().is_some()
    })
    .inspect_err(|_| {
        let _ = proof("background-regedit-value-menu-failure.bmp");
    })?;
    let (menu, items) = open_menu()?.context("regedit's value menu closed")?;
    anyhow::ensure!(
        ["Modify", "Delete", "Rename"]
            .iter()
            .all(|name| items.iter().any(|item| item.starts_with(name))),
        "right-clicking a value opened {items:?}"
    );
    let mut rect = RECT::default();
    unsafe { GetWindowRect(menu, &mut rect)? };
    let near = |edge: i32, pointer: i32| (edge - pointer).abs() <= 2;
    anyhow::ensure!(
        (near(rect.left, row.x) || near(rect.right, row.x))
            && (near(rect.top, row.y) || near(rect.bottom, row.y)),
        "regedit's value menu is at {rect:?}, not at the pointer {row:?}"
    );
    proof("background-regedit-value-menu.bmp")?;
    key(workspace, 0x01, false)?;
    wait_until(workspace, "Escape to close the value menu", 5, &|_| {
        open_menu().ok().flatten().is_none()
    })?;
    Ok(())
}

/// Services: the Action menu opens on click, double-clicking a service
/// opens its properties, and double-clicking the caption maximizes.
fn services_take_clicks(workspace: &mut Workspace) -> anyhow::Result<()> {
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
            SendMessageW(list, LVM_GETITEMCOUNT, None, None).0 > 0
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
    Ok(())
}

/// Device Manager: double-clicking a category expands it, and
/// double-clicking a device opens its properties.
fn device_manager_takes_double_clicks(workspace: &mut Workspace) -> anyhow::Result<()> {
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
    Ok(())
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
