//! An interactive check of the reconnect overlay on a real window: the
//! reason, the live elapsed time and countdown, and that "Retry now" in the
//! owned reconnect panel reaches the window both from `BM_CLICK` and from a
//! real mouse click. A synthetic frame stands in for the video. It needs an
//! interactive desktop:
//!
//! ```text
//! cargo test -p meshrmm-remote -- --ignored --nocapture reconnect_probe
//! ```
//!
//! With `MESHRMM_PROBE_SCREENSHOTS` set to a directory, it saves bitmaps of
//! the window there.

use std::path::{Path, PathBuf};
use std::time::Instant;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
    CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ,
    ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, IsWindowEnabled, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEINPUT, SendInput,
};

use super::reset_probe::{present_synthetic, pump};
use super::window::probe_reconnect_panel;
use super::*;
use crate::reconnect::{ReconnectPhase, ReconnectReason, ReconnectStatus, retry_generation};
use meshrmm_protocol::{DesktopSession, DisplayId, PixelFormat};

unsafe fn text(window: HWND) -> String {
    let mut text = vec![0_u16; unsafe { GetWindowTextLengthW(window) }.max(0) as usize + 1];
    let length = unsafe { GetWindowTextW(window, &mut text) }.max(0) as usize;
    String::from_utf16_lossy(&text[..length])
}

/// Runs the window's messages and the overlay refresh, as the worker loop does.
unsafe fn run(window: HWND, shared: &Shared, overlay: &mut ReconnectOverlay, duration: Duration) {
    let started = Instant::now();
    loop {
        unsafe { overlay.refresh(window, shared) };
        unsafe { pump(window, Duration::from_millis(20)) };
        if started.elapsed() >= duration {
            break;
        }
    }
}

/// Saves the screen under `window` and its popups as a 32-bit bitmap.
unsafe fn save_screenshot(window: HWND, name: &str) {
    let Some(directory) = std::env::var_os("MESHRMM_PROBE_SCREENSHOTS").map(PathBuf::from) else {
        return;
    };
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(window, &mut bounds) }.unwrap();
    let path = directory.join(format!("{name}.bmp"));
    unsafe { capture(bounds, &path) }.unwrap();
    println!("saved {}", path.display());
}

unsafe fn capture(bounds: RECT, path: &Path) -> anyhow::Result<()> {
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    let mut header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        // Top-down rows.
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut pixels = vec![0_u8; (width * height * 4) as usize];
    unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            Some(screen),
            bounds.left,
            bounds.top,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        SelectObject(memory, previous);
        let mut info = BITMAPINFO {
            bmiHeader: header,
            ..Default::default()
        };
        let rows = GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory);
        ReleaseDC(None, screen);
        copied.context("BitBlt from the screen failed")?;
        anyhow::ensure!(rows == height, "GetDIBits copied {rows} of {height} rows");
    }
    header.biSizeImage = pixels.len() as u32;
    let offset = 14 + header.biSize;
    let mut file = Vec::with_capacity(offset as usize + pixels.len());
    file.extend_from_slice(b"BM");
    file.extend_from_slice(&(offset + header.biSizeImage).to_le_bytes());
    file.extend_from_slice(&0_u32.to_le_bytes());
    file.extend_from_slice(&offset.to_le_bytes());
    // Safety: BITMAPINFOHEADER is plain data.
    file.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            (&header as *const BITMAPINFOHEADER).cast::<u8>(),
            header.biSize as usize,
        )
    });
    file.extend_from_slice(&pixels);
    std::fs::write(path, file)?;
    Ok(())
}

/// Clicks the middle of `target` with the real mouse.
unsafe fn click(target: HWND) -> bool {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(target, &mut bounds) }.unwrap();
    let (x, y) = (
        (bounds.left + bounds.right) / 2,
        (bounds.top + bounds.bottom) / 2,
    );
    if unsafe { SetCursorPos(x, y) }.is_err() {
        return false;
    }
    let button = |flags| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs = [button(MOUSEEVENTF_LEFTDOWN), button(MOUSEEVENTF_LEFTUP)];
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    sent == 2
}

fn status(since: Instant, phase: ReconnectPhase) -> Option<ReconnectStatus> {
    Some(ReconnectStatus {
        reason: ReconnectReason::NetworkLost,
        since,
        phase,
    })
}

/// The probe window, the presenter state that drives its overlay, and the
/// overlay's controls.
struct ReconnectProbe {
    overlay: ReconnectOverlay,
    shared: Shared,
    window: HWND,
    panel: HWND,
    label: HWND,
    button: HWND,
    title_before: String,
    _presentation: super::pipeline::Presentation,
}

#[test]
#[ignore = "needs an interactive desktop and a D3D11 video device"]
fn reconnect_probe_shows_the_status_and_retry_now_reaches_the_owner() {
    unsafe { run_probe() }
}

unsafe fn run_probe() {
    super::enable_dpi_awareness();
    let mut probe = unsafe { ReconnectProbe::open() };
    let since = Instant::now() - Duration::from_secs(65);
    unsafe { probe.check_waiting(since) };
    unsafe { probe.check_live_counts() };
    unsafe { probe.check_retry_from_bm_click() };
    unsafe { probe.check_retry_from_mouse_click(since) };
    unsafe { probe.check_attempting_ignores_clicks(since) };
    unsafe { probe.check_reason_change(since) };

    // The first frame of the next connection hides the overlay.
    probe.shared.set_reconnect_status(None);
    unsafe { probe.run(Duration::from_millis(100)) };
    assert!(!unsafe { IsWindowVisible(probe.panel) }.as_bool());
    assert_eq!(unsafe { text(probe.window) }, probe.title_before);
    println!("reconnect probe passed");
}

impl ReconnectProbe {
    unsafe fn open() -> Self {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let display = Display {
            session: DesktopSession::Console,
            id: DisplayId(1),
            name: r"\\.\DISPLAY1".into(),
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
        let chat = meshrmm_chat::ChatSession::default();
        let mut presentation = unsafe {
            super::pipeline::Presentation::new(
                format,
                display.clone(),
                vec![display],
                test_sink(Arc::clone(&sent), chat.clone()),
                DebugInfo::new("reconnect-probe"),
            )
        }
        .expect("the probe window and renderer");
        let window = presentation.window();
        unsafe { pump(window, Duration::from_millis(300)) };
        unsafe { present_synthetic(&mut presentation, 1280, 720, PixelFormat::Nv12) }.unwrap();
        // Launched without foreground rights, the window may open behind others.
        let _ = unsafe {
            SetWindowPos(
                window,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
        let shared = Shared::new(test_sink(sent, chat), DebugInfo::new("reconnect-probe"));
        let overlay = ReconnectOverlay::new();
        let (panel, label, button) =
            unsafe { probe_reconnect_panel(window) }.expect("reconnect panel");
        let title_before = unsafe { text(window) };
        assert!(!unsafe { IsWindowVisible(panel) }.as_bool());
        Self {
            overlay,
            shared,
            window,
            panel,
            label,
            button,
            title_before,
            _presentation: presentation,
        }
    }

    unsafe fn run(&mut self, duration: Duration) {
        unsafe { run(self.window, &self.shared, &mut self.overlay, duration) };
    }

    /// Waiting out the backoff: reason, elapsed time, countdown, Retry now.
    unsafe fn check_waiting(&mut self, since: Instant) {
        let (window, panel, button) = (self.window, self.panel, self.button);
        let until = Instant::now() + Duration::from_millis(8_500);
        self.shared
            .set_reconnect_status(status(since, ReconnectPhase::Waiting { until }));
        unsafe { self.run(Duration::from_millis(200)) };
        let first = unsafe { text(self.label) };
        println!("label: {first:?}");
        println!("title: {:?}", unsafe { text(window) });
        assert!(unsafe { IsWindowVisible(panel) }.as_bool());
        assert!(unsafe { IsWindowVisible(button) }.as_bool());
        assert!(first.contains("Network connection lost"), "{first:?}");
        assert!(
            first.contains("Disconnected for 1:05 · retrying in 9 s"),
            "{first:?}"
        );
        assert_eq!(
            unsafe { text(window) },
            format!("{} — Reconnecting…", self.title_before)
        );
        assert!(unsafe { IsWindowEnabled(button) }.as_bool());
        assert_eq!(unsafe { text(button) }, "Retry now");
        // The panel is owned by the window and hosts the button, whose clicks
        // it forwards to the window.
        assert_eq!(unsafe { GetWindow(panel, GW_OWNER) }.ok(), Some(window));
        assert_eq!(unsafe { GetParent(button) }.ok(), Some(panel));
        assert_eq!(unsafe { GetDlgCtrlID(button) }, 4030);
        unsafe { save_screenshot(window, "1-waiting") };
    }

    /// The counts update live.
    unsafe fn check_live_counts(&mut self) {
        unsafe { self.run(Duration::from_millis(2_100)) };
        let later = unsafe { text(self.label) };
        println!("label 2.1 s later: {later:?}");
        assert!(
            later.contains("Disconnected for 1:07 · retrying in 7 s"),
            "{later:?}"
        );
        unsafe { save_screenshot(self.window, "2-waiting-later") };
    }

    /// BN_CLICKED from BM_CLICK reaches the owner.
    unsafe fn check_retry_from_bm_click(&mut self) {
        let generation = retry_generation();
        unsafe { SendMessageW(self.button, BM_CLICK, None, None) };
        unsafe { self.run(Duration::from_millis(100)) };
        println!(
            "BM_CLICK: retry generation {generation} -> {}",
            retry_generation()
        );
        assert_eq!(retry_generation(), generation + 1);
        // The click disables the button until the next wait.
        assert!(!unsafe { IsWindowEnabled(self.button) }.as_bool());
    }

    /// So does a real mouse click, in a panel that does not take activation.
    unsafe fn check_retry_from_mouse_click(&mut self, since: Instant) {
        let until = Instant::now() + Duration::from_secs(4);
        self.shared
            .set_reconnect_status(status(since, ReconnectPhase::Waiting { until }));
        unsafe { self.run(Duration::from_millis(100)) };
        assert!(unsafe { IsWindowEnabled(self.button) }.as_bool());
        let generation = retry_generation();
        let clicked = unsafe { click(self.button) };
        unsafe { self.run(Duration::from_millis(300)) };
        println!(
            "mouse click sent={clicked}: retry generation {generation} -> {}",
            retry_generation()
        );
        assert!(
            clicked,
            "SendInput failed; run the probe on an interactive desktop"
        );
        assert_eq!(retry_generation(), generation + 1);
    }

    /// While an attempt runs, the button is disabled and ignores clicks.
    unsafe fn check_attempting_ignores_clicks(&mut self, since: Instant) {
        self.shared
            .set_reconnect_status(status(since, ReconnectPhase::Attempting));
        unsafe { self.run(Duration::from_millis(100)) };
        let attempting = unsafe { text(self.label) };
        println!("label while attempting: {attempting:?}");
        assert!(attempting.contains("· reconnecting…"), "{attempting:?}");
        assert!(!unsafe { IsWindowEnabled(self.button) }.as_bool());
        let generation = retry_generation();
        assert!(unsafe { click(self.button) });
        unsafe { self.run(Duration::from_millis(300)) };
        assert_eq!(retry_generation(), generation);
        unsafe { save_screenshot(self.window, "3-attempting") };
    }

    /// Another reason replaces the text in place.
    unsafe fn check_reason_change(&mut self, since: Instant) {
        self.shared.set_reconnect_status(Some(ReconnectStatus {
            reason: ReconnectReason::RemoteUnavailable,
            since,
            phase: ReconnectPhase::Waiting {
                until: Instant::now() + Duration::from_secs(15),
            },
        }));
        unsafe { self.run(Duration::from_millis(200)) };
        let offline = unsafe { text(self.label) };
        println!("label for an offline Agent: {offline:?}");
        assert!(offline.contains("The remote computer is restarting or offline"));
        unsafe { save_screenshot(self.window, "4-remote-offline") };
    }
}
