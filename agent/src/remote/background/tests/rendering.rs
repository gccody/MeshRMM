//! Session 0 checks that captured frames show complete, steady windows.
use super::*;

/// Captures the desktop on another thread while this one keeps pumping messages.
fn capture(workspace: &mut Workspace) -> anyhow::Result<Vec<u8>> {
    let worker = std::thread::spawn(|| -> anyhow::Result<Vec<u8>> {
        let _binding = background::Desktop::bind()?;
        Ok(background::snapshot_bmp()?)
    });
    while !worker.is_finished() {
        workspace.pump();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    worker.join().expect("capture worker panicked")
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn stable_taskbar_and_management_caption() -> anyhow::Result<()> {
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
        let baseline = background::snapshot_bmp()?;
        let taskbar_start = 54 + (WIDTH * (HEIGHT - TASKBAR_HEIGHT as u32) * 4) as usize;
        for _ in 0..60 {
            workspace.pump();
            let frame = capture(&mut workspace)?;
            if frame[taskbar_start..] != baseline[taskbar_start..] {
                std::fs::write(std::env::temp_dir().join("taskbar-baseline.bmp"), &baseline)?;
                std::fs::write(std::env::temp_dir().join("taskbar-worker.bmp"), &frame)?;
                anyhow::bail!("idle taskbar changed");
            }
        }
        workspace.launch(8)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let management = loop {
            workspace.pump();
            let window = background::windows()?.into_iter().find(|hwnd| {
                let mut title = [0_u16; 256];
                let count = unsafe { GetWindowTextW(*hwnd, &mut title) };
                String::from_utf16_lossy(&title[..count as usize]) == "Computer Management"
            });
            if let Some(window) = window {
                break window;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Computer Management did not open"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let ready = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < ready {
            workspace.pump();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(management, &mut rect)?;
        }
        let caption = |frame: &[u8]| -> Vec<u8> {
            let mut pixels = Vec::new();
            for y in rect.top.max(0)..(rect.top + 28).min(HEIGHT as i32) {
                let start = 54 + (y as usize * WIDTH as usize + rect.left.max(0) as usize) * 4;
                let end =
                    54 + (y as usize * WIDTH as usize + rect.right.min(WIDTH as i32) as usize) * 4;
                pixels.extend_from_slice(&frame[start..end]);
            }
            pixels
        };
        let baseline = background::snapshot_bmp()?;
        std::fs::write(
            std::env::temp_dir().join("meshrmm-compact-taskbar.bmp"),
            &baseline,
        )?;
        for _ in 0..60 {
            workspace.pump();
            let frame = capture(&mut workspace)?;
            anyhow::ensure!(
                caption(&frame) == caption(&baseline),
                "idle management caption changed"
            );
            if frame[taskbar_start..] != baseline[taskbar_start..] {
                std::fs::write(
                    std::env::temp_dir().join("management-taskbar-baseline.bmp"),
                    &baseline,
                )?;
                std::fs::write(
                    std::env::temp_dir().join("management-taskbar-worker.bmp"),
                    &frame,
                )?;
                anyhow::bail!("taskbar changed with management open");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        mmc_menu_survives_moves(&mut workspace, management, rect, &baseline)?;
        Ok(())
    })
    .join()
    .expect("capture stability test thread panicked")
}

fn mmc_menu_survives_moves(
    workspace: &mut Workspace,
    management: HWND,
    rect: RECT,
    baseline: &[u8],
) -> anyhow::Result<()> {
    // MMC's File/Action/View/Help toolbar can print only its background
    // after a move on this off-screen desktop. Compare the actual menu
    // pixels after dragging, without hovering or activating the menu.
    let menu_pixels = |frame: &[u8], rect: RECT| -> Vec<u8> {
        let mut pixels = Vec::new();
        for y in rect.top + 30..rect.top + 50 {
            let start = 54 + (y as usize * WIDTH as usize + rect.left as usize + 8) * 4;
            pixels.extend_from_slice(&frame[start..start + 170 * 4]);
        }
        pixels
    };
    let expected_menu = menu_pixels(baseline, rect);
    anyhow::ensure!(
        expected_menu
            .chunks_exact(4)
            .filter(|pixel| pixel[..3].iter().all(|value| *value < 100))
            .count()
            > 40,
        "baseline MMC menu must contain visible text"
    );
    for (dx, dy) in [(150, 80), (60, 20), (200, 60), (0, 0)] {
        unsafe {
            SetWindowPos(
                management,
                None,
                rect.left + dx,
                rect.top + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
            )?;
        }
        let mut moved = RECT::default();
        unsafe { GetWindowRect(management, &mut moved)? };
        for _ in 0..10 {
            let frame = capture(workspace)?;
            if menu_pixels(&frame, moved) != expected_menu {
                std::fs::write(std::env::temp_dir().join("mmc-menu-before.bmp"), baseline)?;
                std::fs::write(std::env::temp_dir().join("mmc-menu-after.bmp"), &frame)?;
            }
            anyhow::ensure!(
                menu_pixels(&frame, moved) == expected_menu,
                "MMC menu labels changed after moving from {rect:?} to {moved:?}"
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn dialog_buttons_render_in_every_frame() -> anyhow::Result<()> {
    unsafe extern "system" fn push_buttons(window: HWND, parameter: LPARAM) -> windows::core::BOOL {
        unsafe {
            let mut class = [0_u16; 16];
            let count = GetClassNameW(window, &mut class);
            let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
            if String::from_utf16_lossy(&class[..count as usize]) == "Button"
                && style & WS_VISIBLE.0 != 0
                && matches!(style & 0xf, 0 | 1)
            {
                (*(parameter.0 as *mut Vec<HWND>)).push(window);
            }
        }
        windows::core::BOOL(1)
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
        let rundll32 = crate::win32::windows_directory()?
            .join("System32")
            .join("rundll32.exe");
        let started = launch::launch(launch::Launch {
            executable: Some(&rundll32),
            command: &format!(
                "\"{}\" sysdm.cpl,EditEnvironmentVariables",
                rundll32.display()
            ),
            flags: CREATE_SUSPENDED,
            ..Default::default()
        })?;
        unsafe {
            AssignProcessToJobObject(workspace.job, started.process.0)?;
            anyhow::ensure!(
                ResumeThread(started.thread.0) != u32::MAX,
                "resume rundll32"
            );
        }
        // Environment Variables has eight push buttons. Without DWM, PrintWindow
        // usually copied it before Cancel painted, and sometimes before any
        // button's label or anything at all painted.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let buttons = loop {
            workspace.pump();
            let dialog = background::windows()?.into_iter().find(|hwnd| {
                let mut title = [0_u16; 64];
                let count = unsafe { GetWindowTextW(*hwnd, &mut title) };
                String::from_utf16_lossy(&title[..count as usize]) == "Environment Variables"
            });
            if let Some(dialog) = dialog {
                let mut buttons = Vec::<HWND>::new();
                unsafe {
                    let _ = EnumChildWindows(
                        Some(dialog),
                        Some(push_buttons),
                        LPARAM((&mut buttons as *mut Vec<HWND>) as isize),
                    );
                }
                if buttons.len() >= 8 {
                    break buttons;
                }
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "Environment Variables did not open"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // A dialog that is still initializing may legitimately be half drawn.
        let ready = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < ready {
            workspace.pump();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        for attempt in 0..40 {
            let frame = capture(&mut workspace)?;
            for button in &buttons {
                let mut rect = RECT::default();
                unsafe { GetWindowRect(*button, &mut rect)? };
                // Every label draws dark text inside its button's face.
                let mut text = 0;
                for y in rect.top.max(0)..rect.bottom.min(HEIGHT as i32) {
                    for x in rect.left.max(0)..rect.right.min(WIDTH as i32) {
                        let pixel = 54 + (y as usize * WIDTH as usize + x as usize) * 4;
                        if frame[pixel..pixel + 3].iter().all(|value| *value < 100) {
                            text += 1;
                        }
                    }
                }
                if text < 10 {
                    let mut label = [0_u16; 64];
                    let count = unsafe { GetWindowTextW(*button, &mut label) };
                    std::fs::write(
                        std::env::temp_dir().join("dialog-buttons-failure.bmp"),
                        &frame,
                    )?;
                    anyhow::bail!(
                        "frame {attempt} is missing the {:?} button at {rect:?}",
                        String::from_utf16_lossy(&label[..count as usize])
                    );
                }
            }
        }
        Ok(())
    })
    .join()
    .expect("dialog button test thread panicked")
}
