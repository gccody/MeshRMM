//! Session 0 end to end: typing, a pin launch, rendering, encoding and console input.
use super::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

#[test]
#[ignore = "Requires a dedicated Session 0 process; creates GUI applications"]
fn session_zero_gui() -> anyhow::Result<()> {
    // A dedicated thread can bind before any HWNDs or hooks are created.
    std::thread::spawn(|| -> anyhow::Result<()> {
        unsafe {
            use windows::Win32::System::StationsAndDesktops::*;
            let station = OpenWindowStationW(w!("WinSta0"), false, 0x000f037f)?;
            SetProcessWindowStation(station)?;
        }
        let _owner = background::Desktop::create().context("create background desktop")?;
        let _binding = background::Desktop::bind().context("bind background desktop")?;
        background::windows().context("enumerate empty background desktop")?;
        let empty = background::snapshot_bmp().context("render empty background desktop")?;
        assert!(
            empty[54..]
                .chunks_exact(4)
                .all(|pixel| pixel[..3] == [0, 0, 0]),
            "background must be black"
        );
        let mut workspace = Workspace::new().context("create background workspace")?;
        typed_input_stays_ordered(&mut workspace)?;
        let processes = taskbar_click_renders_regedit(&mut workspace)?;
        renderer_encodes_keyframe(&mut workspace)?;
        powershell_runs_typed_command(&mut workspace)?;
        drop(workspace);
        for process in processes {
            if let Ok(handle) = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process) } {
                let status = unsafe { WaitForSingleObject(handle, 5000) };
                unsafe {
                    CloseHandle(handle)?;
                }
                assert_eq!(
                    status, WAIT_OBJECT_0,
                    "Background application survived workspace cleanup"
                );
            }
        }
        Ok(())
    })
    .join()
    .expect("background test thread panicked")
}

fn typed_input_stays_ordered(workspace: &mut Workspace) -> anyhow::Result<()> {
    let edit = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            w!(""),
            WS_OVERLAPPED | WS_CAPTION | WS_VISIBLE | WS_TABSTOP,
            50,
            200,
            600,
            180,
            None,
            None,
            None,
            None,
        )?
    };
    anyhow::ensure!(
        unsafe { SetForegroundWindow(edit) }.as_bool(),
        "the edit window did not take the foreground"
    );
    workspace.apply(RemoteInput::TypeText {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        text: "Session 0 GUI input verified".into(),
    })?;
    wait_until(workspace, "typed text", 5, &|_| {
        window_text(edit) == "Session 0 GUI input verified"
    })?;
    // This EDIT belongs to this thread, so it can't handle any key until
    // every one was sent. They must still arrive in order.
    unsafe {
        SetWindowTextW(edit, w!("C:\\"))?;
    }
    for scan in [0x47, 0x53, 0x53, 0x53] {
        for pressed in [true, false] {
            workspace.apply(RemoteInput::Key {
                display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
                scan_code: scan,
                extended: true,
                pressed,
            })?;
        }
    }
    let expected = "C:\\Windows\\System32";
    for character in expected.encode_utf16() {
        let key = unsafe { VkKeyScanW(character) };
        assert!(key >= 0);
        if key & 0x100 != 0 {
            send_key(workspace, 0x2a, true)?;
        }
        let scan = unsafe { MapVirtualKeyW((key & 0xff) as u32, MAPVK_VK_TO_VSC) } as u16;
        send_key(workspace, scan, true)?;
        send_key(workspace, scan, false)?;
        if key & 0x100 != 0 {
            send_key(workspace, 0x2a, false)?;
        }
    }
    settle(workspace, 300);
    assert_eq!(
        window_text(edit),
        expected,
        "queued navigation and literal text must stay ordered"
    );
    let before = background::snapshot_bmp()?;
    assert!(before[54..].chunks_exact(4).any(|p| p[..3] != [0, 0, 0]));
    unsafe {
        DestroyWindow(edit)?;
    }
    Ok(())
}

/// Clicks Registry Editor's pin, checks that its windows belong to Session 0
/// and render, and returns their processes.
fn taskbar_click_renders_regedit(workspace: &mut Workspace) -> anyhow::Result<Vec<u32>> {
    let baseline = background::snapshot_bmp()?;
    workspace.apply(RemoteInput::PointerButtonAt {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        x: ((8 + 2 * PIN_WIDTH as u32 + 30) * 65535 / (WIDTH - 1)) as u16,
        y: ((HEIGHT - TASKBAR_HEIGHT as u32 + 30) * 65535 / (HEIGHT - 1)) as u16,
        button: PointerButton::Left,
        pressed: true,
    })?;
    workspace.apply(RemoteInput::PointerButton {
        display_id: meshrmm_protocol::DisplayId(background::DISPLAY_ID),
        button: PointerButton::Left,
        pressed: false,
    })?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut applications = Vec::new();
    while std::time::Instant::now() < deadline {
        workspace.pump();
        applications = background::windows()?
            .into_iter()
            .filter(|hwnd| *hwnd != workspace.shell && *hwnd != workspace.tooltip)
            .collect();
        if !applications.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        !applications.is_empty(),
        "Registry Editor did not create a background window"
    );
    let mut processes = Vec::new();
    for hwnd in applications {
        let mut process = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
        }
        let mut session = u32::MAX;
        unsafe {
            windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(process, &mut session)?;
        }
        assert_eq!(session, 0);
        processes.push(process);
    }
    let mut after = baseline.clone();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while after == baseline && std::time::Instant::now() < deadline {
        workspace.pump();
        std::thread::sleep(std::time::Duration::from_millis(100));
        after = background::snapshot_bmp()?;
    }
    for hwnd in background::windows()? {
        let mut title = [0_u16; 256];
        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(hwnd, &mut rect)?;
        }
        let count = unsafe { GetWindowTextW(hwnd, &mut title) };
        println!(
            "application window: {} {rect:?}",
            String::from_utf16_lossy(&title[..count as usize])
        );
    }
    let evidence = std::env::temp_dir().join("meshrmm-background-gui.bmp");
    std::fs::write(&evidence, &after)?;
    assert!(
        baseline != after,
        "Registry Editor did not render into the desktop"
    );
    println!("Session 0 rendering evidence: {}", evidence.display());
    Ok(processes)
}

fn renderer_encodes_keyframe(workspace: &mut Workspace) -> anyhow::Result<()> {
    let (frames, received) = std::sync::mpsc::sync_channel(8);
    let mut streamer = meshrmm_remote_screen::WindowsDesktopDuplicationStreamer::new();
    let format = streamer.start(
        meshrmm_remote_screen::StreamConfig {
            frames_per_second: 10,
            ..Default::default()
        },
        background::DISPLAY_ID,
        std::sync::Arc::new(move |frame| {
            let _ = frames.try_send(frame);
        }),
    )?;
    assert_eq!((format.width, format.height), (WIDTH, HEIGHT));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut keyframe = false;
    while !keyframe && std::time::Instant::now() < deadline {
        workspace.pump();
        keyframe = received
            .try_iter()
            .any(|frame| frame.keyframe && !frame.data.is_empty());
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let stop = std::thread::spawn(move || streamer.stop());
    while !stop.is_finished() {
        workspace.pump();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    stop.join().expect("capture stop panicked")?;
    assert!(
        keyframe,
        "Session 0 renderer did not produce an encoded keyframe"
    );
    println!("Session 0 H.264 keyframe verified");
    Ok(())
}

fn powershell_runs_typed_command(workspace: &mut Workspace) -> anyhow::Result<()> {
    workspace.launch(2)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let console = loop {
        workspace.pump();
        let console = background::windows()?.into_iter().find(|hwnd| {
            let mut class = [0_u16; 64];
            let count = unsafe { GetClassNameW(*hwnd, &mut class) };
            String::from_utf16_lossy(&class[..count as usize]) == "ConsoleWindowClass"
        });
        if let Some(console) = console {
            break console;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "PowerShell console did not open"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    // The console HWND precedes PowerShell/PSReadLine initialization;
    // let startup finish before exercising its interactive input mode.
    let ready = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < ready {
        workspace.pump();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(console, &mut rect)?;
    }
    workspace.move_pointer(
        ((rect.left + 100) as u32 * 65535 / (WIDTH - 1)) as u16,
        ((rect.top + 80) as u32 * 65535 / (HEIGHT - 1)) as u16,
    )?;
    workspace.button(PointerButton::Left, true)?;
    workspace.button(PointerButton::Left, false)?;
    let evidence = std::env::temp_dir().join(format!(
        "meshrmm-console-keyboard-{}.txt",
        std::process::id()
    ));
    wait_until(
        workspace,
        "the console to take the foreground",
        5,
        &|_| unsafe { GetForegroundWindow() == console },
    )?;
    let _ = std::fs::remove_file(&evidence);
    let command = format!(
        "Set-Content -LiteralPath '{}' -Value 'AbC_123'",
        evidence.display()
    );
    for character in command.encode_utf16() {
        let key = unsafe { VkKeyScanW(character) };
        assert!(key >= 0, "test character has no keyboard mapping");
        let shift = key & 0x100 != 0;
        if shift {
            send_key(workspace, 0x2a, true)?;
        }
        let scan = unsafe { MapVirtualKeyW((key & 0xff) as u32, MAPVK_VK_TO_VSC) } as u16;
        send_key(workspace, scan, true)?;
        send_key(workspace, scan, false)?;
        if shift {
            send_key(workspace, 0x2a, false)?;
        }
        workspace.pump();
    }
    send_key(workspace, 0x1c, true)?;
    send_key(workspace, 0x1c, false)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !evidence.exists() && std::time::Instant::now() < deadline {
        workspace.pump();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::fs::write(
        std::env::temp_dir().join("meshrmm-background-console.bmp"),
        background::snapshot_bmp()?,
    )?;
    let output = std::fs::read_to_string(&evidence)
        .context("PowerShell did not execute the typed command")?;
    assert_eq!(
        output.trim(),
        "AbC_123",
        "Console duplicated or changed keyboard characters"
    );
    std::fs::remove_file(evidence)?;
    println!("Session 0 PowerShell mixed-case keyboard input verified");
    Ok(())
}
