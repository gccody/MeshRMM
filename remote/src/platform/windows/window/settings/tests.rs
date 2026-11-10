use meshrmm_protocol::{DesktopSession, DisplayId, PixelFormat};
use windows::Win32::UI::Controls::BST_CHECKED;

use super::*;

/// Runs `passes` passes of the window loop's message pump, which refreshes
/// the toolbar and settings on each one.
unsafe fn pump(window: HWND, passes: usize) {
    for _ in 0..passes {
        assert!(!unsafe { pump_window_messages(window) });
    }
}

fn button(context: &WindowContext, id: usize) -> HWND {
    unsafe { GetDlgItem(Some(context.controls().settings_window), id as i32) }
        .expect("the settings control")
}

fn checked(context: &WindowContext, id: usize) -> bool {
    let state = unsafe { SendMessageW(button(context, id), BM_GETCHECK, None, None) };
    state.0 == BST_CHECKED.0 as isize
}

#[test]
fn the_settings_window_changes_only_what_changed_and_only_while_shown() {
    let control = crate::platform::windows::test_sink(Default::default(), Default::default());
    let display = Display {
        session: DesktopSession::Console,
        id: DisplayId(1),
        name: r"\.\DISPLAY1".into(),
        x: 0,
        y: 0,
        width: 1280,
        height: 720,
        primary: true,
    };
    let format = VideoFormat {
        width: 1280,
        height: 720,
        frames_per_second: 60,
        codec: Codec::H264,
        pixel_format: PixelFormat::Nv12,
        bitrate_bits_per_second: 12_000_000,
    };
    let window = unsafe {
        create_window(
            format,
            display.clone(),
            vec![display],
            control.clone(),
            DebugInfo::new("settings-refresh-test"),
        )
    }
    .expect("the viewer window");
    let context = unsafe { window_context(window) }.expect("the window's context");
    let settings_window = context.controls().settings_window;

    // Closed, the settings window is left alone, even when a setting changes.
    unsafe { pump(window, 1000) };
    control.toggle_wallpaper();
    unsafe { pump(window, 100) };
    assert_eq!(context.settings_writes(), 0);

    // Opened, it shows the session at once, and then nothing changes it
    // while the session stays as it is.
    context.show_settings();
    let opened = context.settings_writes();
    assert!(opened > 0);
    assert!(checked(&context, SETTINGS_WALLPAPER_ID));
    unsafe { pump(window, 1000) };
    assert_eq!(context.settings_writes(), opened);

    // A change made elsewhere, here as by the toolbar, reaches its control
    // once.
    control.set_technician_blocked(true);
    unsafe { pump(window, 100) };
    assert_eq!(context.settings_writes(), opened + 1);
    assert!(checked(&context, SETTINGS_TECHNICIAN_INPUT_ID));

    // A change while it is closed again is shown when it opens.
    let _ = unsafe { ShowWindow(settings_window, SW_HIDE) };
    control.toggle_wallpaper();
    unsafe { pump(window, 100) };
    assert_eq!(context.settings_writes(), opened + 1);
    assert!(checked(&context, SETTINGS_WALLPAPER_ID));
    context.show_settings();
    assert_eq!(context.settings_writes(), opened + 2);
    assert!(!checked(&context, SETTINGS_WALLPAPER_ID));

    // A check box checks itself when clicked. One the session has not
    // taken, here a request the Agent has not answered, shows the session
    // again on the next pass.
    let agent_input = button(&context, SETTINGS_AGENT_INPUT_ID);
    unsafe {
        SendMessageW(agent_input, BM_SETCHECK, Some(WPARAM(1)), None);
        SendMessageW(
            settings_window,
            WM_COMMAND,
            Some(WPARAM(
                (BN_CLICKED as usize) << 16 | SETTINGS_AGENT_INPUT_ID,
            )),
            Some(LPARAM(agent_input.0 as isize)),
        );
        pump(window, 1);
    }
    assert!(!checked(&context, SETTINGS_AGENT_INPUT_ID));

    drop(context);
    let _ = unsafe { DestroyWindow(window) };
}
