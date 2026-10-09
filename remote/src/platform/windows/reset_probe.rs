//! An interactive check of the in-place stream reset on a real window and
//! D3D11 device. Synthetic frames stand in for a decoder, so it runs
//! on GPUs without decoder MFTs too. It needs an interactive desktop:
//!
//! ```text
//! cargo test -p meshrmm-remote -- --ignored --nocapture reset_probe
//! ```

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HDC, HGDIOBJ, ReleaseDC,
    SelectObject,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;

use super::pipeline::{Decoder, Presentation};
use super::window::{ProbeState, probe_state, probe_toggle_chat};
use super::*;
use meshrmm_protocol::{DesktopSession, DisplayId, PixelFormat};

#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(window: HWND, dc: HDC, flags: u32) -> windows::core::BOOL;
}

/// Includes DirectComposition and flip-model swap-chain content.
const PW_RENDERFULLCONTENT: u32 = 2;

fn display(session: DesktopSession, id: u32, width: u32, height: u32) -> Display {
    Display {
        session,
        id: DisplayId(id),
        name: format!(r"\\.\DISPLAY{id}"),
        x: 0,
        y: 0,
        width,
        height,
        primary: id == 1,
    }
}

fn format(width: u32, height: u32, codec: Codec, pixel_format: PixelFormat) -> VideoFormat {
    VideoFormat {
        width,
        height,
        frames_per_second: 60,
        codec,
        pixel_format,
        bitrate_bits_per_second: 12_000_000,
    }
}

/// Runs the window's messages for a while, as the worker loop does.
pub(super) unsafe fn pump(window: HWND, duration: Duration) {
    let started = std::time::Instant::now();
    while started.elapsed() < duration {
        unsafe { pump_window_messages(window) };
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A frame of one shade, as a decoder would output it.
unsafe fn synthetic_frame(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
    bind_flags: D3D11_BIND_FLAG,
) -> anyhow::Result<ID3D11Texture2D> {
    let (format, pitch, data) = match pixel_format {
        PixelFormat::Nv12 => {
            let luma = vec![0xa0_u8; (width * height) as usize];
            let chroma = vec![0x60_u8; (width * height / 2) as usize];
            (DXGI_FORMAT_NV12, width, [luma, chroma].concat())
        }
        PixelFormat::Ayuv => (
            DXGI_FORMAT_AYUV,
            width * 4,
            [0x60_u8, 0xa0, 0xa0, 0xff].repeat((width * height) as usize),
        ),
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr().cast(),
        SysMemPitch: pitch,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&initial), Some(&mut texture)) }
        .with_context(|| format!("synthetic {pixel_format:?} texture creation failed"))?;
    texture.context("D3D11 returned no synthetic texture")
}

/// Presents a synthetic frame. Drivers differ in which surfaces the video
/// processor takes as input, so this tries the bindings a decoder uses.
pub(super) unsafe fn present_synthetic(
    presentation: &mut Presentation,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
) -> anyhow::Result<ID3D11Texture2D> {
    let device = presentation.device().clone();
    let mut last_error = None;
    for bind_flags in [
        D3D11_BIND_DECODER,
        D3D11_BIND_SHADER_RESOURCE,
        D3D11_BIND_RENDER_TARGET,
        D3D11_BIND_FLAG(D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0),
    ] {
        let presented =
            unsafe { synthetic_frame(&device, width, height, pixel_format, bind_flags) }.and_then(
                |frame| {
                    unsafe { presentation.renderer().present(&frame, 0) }?;
                    Ok(frame)
                },
            );
        match presented {
            Ok(frame) => {
                println!("presented a {width}x{height} {pixel_format:?} frame ({bind_flags:?})");
                return Ok(frame);
            }
            Err(error) => last_error = Some(error.context(format!("{bind_flags:?}"))),
        }
    }
    Err(last_error.unwrap())
}

unsafe extern "system" fn collect_window(window: HWND, windows: LPARAM) -> windows::core::BOOL {
    unsafe { (*(windows.0 as *mut Vec<HWND>)).push(window) };
    true.into()
}

/// The window's children and the popups it owns, apart from the input
/// method windows Windows moves to whichever window has focus.
unsafe fn window_family(window: HWND) -> Vec<HWND> {
    let mut children = Vec::new();
    let mut thread_windows = Vec::new();
    unsafe {
        let _ = EnumChildWindows(
            Some(window),
            Some(collect_window),
            LPARAM(&mut children as *mut Vec<HWND> as isize),
        );
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(collect_window),
            LPARAM(&mut thread_windows as *mut Vec<HWND> as isize),
        );
    }
    let mut family: Vec<HWND> = children
        .into_iter()
        .chain(
            thread_windows
                .into_iter()
                .filter(|popup| unsafe { GetWindow(*popup, GW_OWNER) }.ok() == Some(window)),
        )
        .filter(|member| {
            !matches!(
                unsafe { class_name(*member) }.as_str(),
                "IME" | "MSCTFIME UI"
            )
        })
        .collect();
    family.sort_by_key(|hwnd| hwnd.0 as usize);
    family
}

unsafe fn class_name(window: HWND) -> String {
    let mut name = [0_u16; 64];
    let length = unsafe { GetClassNameW(window, &mut name) }.max(0) as usize;
    String::from_utf16_lossy(&name[..length])
}

unsafe fn text(window: HWND) -> String {
    let mut text = vec![0_u16; unsafe { GetWindowTextLengthW(window) }.max(0) as usize + 1];
    let length = unsafe { GetWindowTextW(window, &mut text) }.max(0) as usize;
    String::from_utf16_lossy(&text[..length])
}

/// The chat popup and the text of its transcript and entry.
unsafe fn chat_contents(family: &[HWND]) -> Option<(HWND, Vec<String>)> {
    let popup = *family
        .iter()
        .find(|window| unsafe { class_name(**window) } == "MeshRMMChat")?;
    let mut children = Vec::new();
    let _ = unsafe {
        EnumChildWindows(
            Some(popup),
            Some(collect_window),
            LPARAM(&mut children as *mut Vec<HWND> as isize),
        )
    };
    Some((
        popup,
        children
            .into_iter()
            .map(|child| unsafe { text(child) })
            .collect(),
    ))
}

/// Captures `window` with `PrintWindow` and counts the distinct colors in
/// its top `rows` rows (all rows when zero): a plain background means the
/// controls were not drawn.
unsafe fn distinct_colors(window: HWND, rows: i32) -> anyhow::Result<usize> {
    let mut bounds = RECT::default();
    unsafe { GetWindowRect(window, &mut bounds) }?;
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    anyhow::ensure!(width > 0 && height > 0, "window has no area");
    let pixels = unsafe {
        let screen = GetDC(None);
        let memory = CreateCompatibleDC(Some(screen));
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let previous = SelectObject(memory, HGDIOBJ(bitmap.0));
        let printed = PrintWindow(window, memory, PW_RENDERFULLCONTENT).as_bool();
        SelectObject(memory, previous);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Top-down rows.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0_u32; (width * height) as usize];
        let copied = GetDIBits(
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
        anyhow::ensure!(printed, "PrintWindow failed");
        anyhow::ensure!(
            copied == height,
            "GetDIBits copied {copied} of {height} rows"
        );
        pixels
    };
    let rows = if rows > 0 { rows.min(height) } else { height };
    let mut colors: Vec<u32> = pixels[..(rows * width) as usize]
        .iter()
        .map(|pixel| pixel & 0x00ff_ffff)
        .collect();
    colors.sort_unstable();
    colors.dedup();
    Ok(colors.len())
}

/// Posts a mouse move in client coordinates and returns the position the
/// window sent to the device.
unsafe fn map_pointer(
    window: HWND,
    sent: &Mutex<Vec<SessionMessage>>,
    x: i32,
    y: i32,
) -> Option<(DisplayId, u16, u16)> {
    sent.lock().unwrap().clear();
    let position = (x as u16 as usize) | ((y as u16 as usize) << 16);
    unsafe {
        PostMessageW(
            Some(window),
            WM_MOUSEMOVE,
            WPARAM(0),
            LPARAM(position as isize),
        )
    }
    .ok()?;
    unsafe { pump(window, Duration::from_millis(50)) };
    sent.lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|message| match message {
            SessionMessage::Input(RemoteInput::PointerMove { display_id, x, y }) => {
                Some((*display_id, *x, *y))
            }
            _ => None,
        })
}

/// The video's corners must map to the edges of the remote display.
unsafe fn assert_pointer_corners(
    window: HWND,
    sent: &Mutex<Vec<SessionMessage>>,
    state: &ProbeState,
) {
    let video = state.video.expect("video rectangle");
    let top_left = unsafe { map_pointer(window, sent, video.left, video.top) };
    let bottom_right = unsafe { map_pointer(window, sent, video.right() - 1, video.bottom() - 1) };
    println!("pointer corners of {video:?}: {top_left:?} {bottom_right:?}");
    assert_eq!(top_left, Some((state.active_display, 0, 0)));
    assert_eq!(bottom_right, Some((state.active_display, 65_535, 65_535)));
    // The toolbar is not part of the video.
    assert_eq!(
        unsafe { map_pointer(window, sent, video.left, state.toolbar_height - 1) },
        None
    );
}

/// Every child and owned popup of the original window must survive a reset.
fn assert_same_family(window: HWND, before: &[HWND]) {
    let after = unsafe { window_family(window) };
    let classes = |windows: &[HWND]| -> Vec<String> {
        windows
            .iter()
            .map(|window| unsafe { class_name(*window) })
            .collect()
    };
    assert_eq!(
        after,
        before,
        "the window's children and popups changed: {:?} -> {:?}",
        classes(before),
        classes(&after)
    );
    assert!(unsafe { IsWindow(Some(window)) }.as_bool());
    for control in before {
        assert!(unsafe { IsWindow(Some(*control)) }.as_bool());
    }
}

fn console(id: u32, width: u32, height: u32) -> Display {
    display(DesktopSession::Console, id, width, height)
}

fn rdp() -> DesktopSession {
    DesktopSession::Rdp {
        id: 2,
        user: "probe".into(),
    }
}

/// The probe window and the state each reset must leave as it was.
struct ResetProbe {
    sent: Arc<Mutex<Vec<SessionMessage>>>,
    presentation: Presentation,
    window: HWND,
    device: ID3D11Device,
    /// The window's children and popups before the first reset.
    family: Vec<HWND>,
    chat_popup: HWND,
    chat_before: Vec<String>,
    chat_visible: bool,
    focus: HWND,
}

#[test]
#[ignore = "needs an interactive desktop and a D3D11 video device"]
fn reset_probe_keeps_the_window_and_follows_the_new_stream() {
    unsafe { run_probe() }
}

unsafe fn run_probe() {
    super::enable_dpi_awareness();
    let mut probe = unsafe { ResetProbe::open() };
    let initial = unsafe { probe.check_initial_state() };
    let pressed = probe.hold_key();
    unsafe { probe.switch_display(&initial, &pressed) };
    let portrait_state = unsafe { probe.rotate_to_portrait() };
    unsafe { probe.try_full_chroma(&portrait_state) };
    unsafe { probe.refuse_profiles_without_decoder() };
    let restored = unsafe { probe.reset_while_minimized() };
    unsafe { probe.check_drawn_after_resets(&restored) };
    println!("reset probe passed on HWND {:?}", probe.window.0);
    drop(probe.presentation);
}

impl ResetProbe {
    /// Opens the window on the first display and puts state in the popups
    /// that a new window would lose.
    unsafe fn open() -> Self {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let chat = meshrmm_chat::ChatSession::default();
        chat.set_available(true);
        chat.receive("Transcript line from before the reset".into());
        let first = console(1, 1280, 720);
        let first_displays = vec![
            first.clone(),
            console(2, 1920, 1080),
            display(rdp(), 7, 1600, 900),
        ];
        let first_format = format(1280, 720, Codec::H264, PixelFormat::Nv12);

        let mut presentation = unsafe {
            Presentation::new(
                first_format,
                first,
                first_displays,
                test_sink(Arc::clone(&sent), chat.clone()),
                DebugInfo::new("reset-probe"),
            )
        }
        .expect("the probe window and renderer");
        let window = presentation.window();
        let device = presentation.device().clone();
        unsafe { pump(window, Duration::from_millis(300)) };
        unsafe { present_synthetic(&mut presentation, 1280, 720, PixelFormat::Nv12) }.unwrap();

        // Put state in the popups that a new window would lose.
        unsafe { probe_toggle_chat(window) };
        unsafe { pump(window, Duration::from_millis(400)) };
        let family = unsafe { window_family(window) };
        let (chat_popup, _) = unsafe { chat_contents(&family) }.expect("chat popup");
        let mut chat_children = Vec::new();
        let _ = unsafe {
            EnumChildWindows(
                Some(chat_popup),
                Some(collect_window),
                LPARAM(&mut chat_children as *mut Vec<HWND> as isize),
            )
        };
        let entry = chat_children
            .iter()
            .copied()
            .find(|child| {
                unsafe { class_name(*child) }.eq_ignore_ascii_case("Edit")
                    && unsafe { GetWindowLongW(*child, GWL_STYLE) } & ES_MULTILINE == 0
            })
            .expect("chat entry");
        unsafe { SetWindowTextW(entry, w!("Draft typed before the reset")) }.unwrap();
        let chat_before = unsafe { chat_contents(&family) }.unwrap().1;
        println!("chat before: {chat_before:?}");
        let chat_visible = unsafe { IsWindowVisible(chat_popup) }.as_bool();
        let _ = unsafe { SetFocus(Some(window)) };
        let focus = unsafe { GetFocus() };
        Self {
            sent,
            presentation,
            window,
            device,
            family,
            chat_popup,
            chat_before,
            chat_visible,
            focus,
        }
    }

    unsafe fn check_initial_state(&self) -> ProbeState {
        let window = self.window;
        let initial = unsafe { probe_state(window) }.unwrap();
        println!("initial: {initial:#?}");
        assert_eq!(initial.displays, ["Display 1", "Display 2"]);
        assert_eq!(initial.users, ["Console", "probe (RDP 2)"]);
        assert!(initial.display_combo_enabled);
        unsafe { assert_pointer_corners(window, &self.sent, &initial) };
        let toolbar_colors = unsafe { distinct_colors(window, initial.toolbar_height) }.unwrap();
        println!("toolbar colors before: {toolbar_colors}");
        assert!(toolbar_colors > 2, "the toolbar was not drawn");

        // The agent pointer marker survives a repopulated display list.
        unsafe { window::set_agent_pointer_display(window, Some(DisplayId(2))) };
        initial
    }

    /// Holds a key on display 1 and returns what the window sent for it.
    fn hold_key(&self) -> Vec<SessionMessage> {
        self.sent.lock().unwrap().clear();
        let key_down = LPARAM(1 | (0x1e << 16));
        unsafe { PostMessageW(Some(self.window), WM_KEYDOWN, WPARAM(0x41), key_down) }.unwrap();
        unsafe { pump(self.window, Duration::from_millis(50)) };
        let pressed = self.sent.lock().unwrap().clone();
        println!("sent for the held key: {pressed:?}");
        pressed
    }

    /// F8 or the display combo: a larger display, a new display list, and a
    /// new active display. Switching displays must release the held key on
    /// the display it was pressed on.
    unsafe fn switch_display(&mut self, initial: &ProbeState, pressed: &[SessionMessage]) {
        let window = self.window;
        let second = console(2, 1920, 1080);
        let second_displays = vec![
            console(1, 1280, 720),
            second.clone(),
            console(3, 1080, 1920),
            display(rdp(), 7, 1600, 900),
        ];
        let second_format = format(1920, 1080, Codec::H264, PixelFormat::Nv12);
        self.sent.lock().unwrap().clear();
        unsafe {
            self.presentation
                .reset_presentation(second_format, second, second_displays)
        }
        .unwrap();
        let during_reset = self.sent.lock().unwrap().clone();
        println!("sent during the display switch: {during_reset:?}");
        if pressed.iter().any(|message| {
            matches!(
                message,
                SessionMessage::Input(RemoteInput::Key { pressed: true, .. })
            )
        }) {
            assert_eq!(
                during_reset,
                [SessionMessage::Input(RemoteInput::Key {
                    display_id: DisplayId(1),
                    scan_code: 0x1e,
                    extended: false,
                    pressed: false,
                })],
                "a held key must be released on the display it was pressed on"
            );
        } else {
            // The key only reaches the device while the window has focus.
            println!("the posted key was not forwarded; skipping the release check");
            assert!(during_reset.iter().all(|message| matches!(
                message,
                SessionMessage::Input(RemoteInput::Key { pressed: false, .. })
            )));
        }
        unsafe { pump(window, Duration::from_millis(100)) };
        assert_same_family(window, &self.family);
        let switched = unsafe { probe_state(window) }.unwrap();
        println!("after the display switch: {switched:#?}");
        assert_eq!(switched.video_size, (1920, 1080));
        assert_eq!(switched.active_display, DisplayId(2));
        assert_ne!(switched.title, initial.title);
        assert!(
            switched.title.contains(r"\\.\DISPLAY2"),
            "{}",
            switched.title
        );
        assert_eq!(switched.displays, ["Display 1", "➤ Display 2", "Display 3"]);
        assert_eq!(switched.selected_display, 1);
        assert_eq!(switched.users, initial.users);
        assert_eq!(switched.selected_user, 0);
        assert_eq!(switched.quality, initial.quality);
        assert_eq!(switched.chroma, initial.chroma);
        assert_eq!(
            unsafe { GetFocus() },
            self.focus,
            "the reset moved keyboard focus"
        );
        assert_eq!(
            unsafe { chat_contents(&self.family) }.unwrap().1,
            self.chat_before
        );
        assert_eq!(
            unsafe { IsWindowVisible(self.chat_popup) }.as_bool(),
            self.chat_visible
        );
        unsafe { assert_pointer_corners(window, &self.sent, &switched) };
        unsafe { present_synthetic(&mut self.presentation, 1920, 1080, PixelFormat::Nv12) }
            .unwrap();
    }

    /// Portrait, and the same display rotated: only the size changes.
    unsafe fn rotate_to_portrait(&mut self) -> ProbeState {
        let window = self.window;
        let portrait = console(3, 1080, 1920);
        unsafe {
            self.presentation.reset_presentation(
                format(1080, 1920, Codec::H264, PixelFormat::Nv12),
                portrait,
                vec![
                    console(1, 1280, 720),
                    console(2, 1920, 1080),
                    console(3, 1080, 1920),
                ],
            )
        }
        .unwrap();
        unsafe { pump(window, Duration::from_millis(100)) };
        let portrait_state = unsafe { probe_state(window) }.unwrap();
        println!("portrait: {portrait_state:#?}");
        assert_eq!(portrait_state.video_size, (1080, 1920));
        assert_eq!(portrait_state.users, ["Console"]);
        assert_eq!(portrait_state.selected_display, 2);
        let video = portrait_state.video.unwrap();
        assert!(video.height > video.width, "{video:?}");
        unsafe { assert_pointer_corners(window, &self.sent, &portrait_state) };
        unsafe { present_synthetic(&mut self.presentation, 1080, 1920, PixelFormat::Nv12) }
            .unwrap();
        assert_same_family(window, &self.family);
        portrait_state
    }

    /// 4:4:4: the processor takes AYUV surfaces, if the GPU converts them.
    unsafe fn try_full_chroma(&mut self, portrait_state: &ProbeState) {
        let crisp = format(1080, 1920, Codec::H264, PixelFormat::Ayuv);
        self.sent.lock().unwrap().clear();
        match unsafe {
            self.presentation.reset_presentation(
                crisp,
                console(3, 1080, 1920),
                vec![console(3, 1080, 1920)],
            )
        } {
            Ok(()) => {
                println!("4:4:4 reset applied");
                if let Err(error) = unsafe {
                    present_synthetic(&mut self.presentation, 1080, 1920, PixelFormat::Ayuv)
                } {
                    // Synthetic AYUV surfaces are a probe limitation; decoders
                    // allocate their own.
                    println!("no synthetic 4:4:4 frame: {error:#}");
                }
            }
            Err(error) => {
                // Nothing may change when the new processor cannot be created.
                println!("4:4:4 reset refused: {error:#}");
                assert_eq!(
                    unsafe { probe_state(self.window) }.unwrap(),
                    *portrait_state
                );
            }
        }
        // Quality and chroma are shown, never sent again: the device would
        // answer with another configuration and another reset.
        let resent = self.sent.lock().unwrap().clone();
        assert!(
            !resent.iter().any(|message| matches!(
                message,
                SessionMessage::SetQuality { .. } | SessionMessage::SetChroma { .. }
            )),
            "{resent:?}"
        );
        assert_same_family(self.window, &self.family);
    }

    /// A profile without a decoder is refused before anything
    /// changes; the caller then reports it with VideoProfileRejected.
    unsafe fn refuse_profiles_without_decoder(&mut self) {
        let before_refusal = unsafe { probe_state(self.window) }.unwrap();
        let mut refused = 0;
        for (codec, pixel_format) in [
            (Codec::H264, PixelFormat::Nv12),
            (Codec::H265, PixelFormat::Nv12),
            (Codec::H264, PixelFormat::Ayuv),
            (Codec::H265, PixelFormat::Ayuv),
        ] {
            let candidate = format(2560, 1440, codec, pixel_format);
            match unsafe { Decoder::new(&self.device, candidate) } {
                Ok(_) => println!("{codec:?} {pixel_format:?}: decoder available"),
                Err(error) => {
                    println!("{codec:?} {pixel_format:?}: no decoder: {error:#}");
                    let result = unsafe {
                        self.presentation.reset_stream(
                            candidate,
                            console(1, 2560, 1440),
                            vec![console(1, 2560, 1440)],
                        )
                    };
                    assert!(result.is_err());
                    assert_eq!(unsafe { probe_state(self.window) }.unwrap(), before_refusal);
                    refused += 1;
                }
            }
        }
        println!("profiles refused without changes: {refused}");
        println!(
            "supported_video_profiles: {:?}",
            supported_video_profiles(format(1920, 1080, Codec::H264, PixelFormat::Nv12))
        );
    }

    /// A minimized window has no client area; the reset must still leave a
    /// usable output until it is restored.
    unsafe fn reset_while_minimized(&mut self) -> ProbeState {
        let window = self.window;
        let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
        unsafe { pump(window, Duration::from_millis(200)) };
        unsafe {
            self.presentation.reset_presentation(
                format(1920, 1080, Codec::H264, PixelFormat::Nv12),
                console(2, 1920, 1080),
                vec![console(1, 1280, 720), console(2, 1920, 1080)],
            )
        }
        .unwrap();
        let frame =
            unsafe { present_synthetic(&mut self.presentation, 1920, 1080, PixelFormat::Nv12) }
                .unwrap();
        let _ = unsafe { ShowWindow(window, SW_RESTORE) };
        unsafe { pump(window, Duration::from_millis(300)) };
        if let Some(layout) = unsafe { window::take_resize(window) } {
            unsafe { self.presentation.renderer().resize(&layout) }.unwrap();
        }
        unsafe { self.presentation.renderer().present(&frame, 0) }.unwrap();
        unsafe { pump(window, Duration::from_millis(200)) };
        let restored = unsafe { probe_state(window) }.unwrap();
        unsafe { assert_pointer_corners(window, &self.sent, &restored) };
        assert_same_family(window, &self.family);
        restored
    }

    unsafe fn check_drawn_after_resets(&self, restored: &ProbeState) {
        let toolbar_colors =
            unsafe { distinct_colors(self.window, restored.toolbar_height) }.unwrap();
        println!("toolbar colors after: {toolbar_colors}");
        assert!(
            toolbar_colors > 2,
            "the toolbar was not drawn after the resets"
        );
        if unsafe { IsWindowVisible(self.chat_popup) }.as_bool() {
            let chat_colors = unsafe { distinct_colors(self.chat_popup, 0) }.unwrap();
            println!("chat colors after: {chat_colors}");
            assert!(
                chat_colors > 2,
                "the chat popup was not drawn after the resets"
            );
        } else {
            println!("the chat popup is hidden (no foreground rights); its contents were compared");
        }
        assert_eq!(
            unsafe { chat_contents(&self.family) }.unwrap().1,
            self.chat_before
        );
    }
}
