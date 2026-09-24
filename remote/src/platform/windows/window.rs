use super::*;
use crate::video_layout::{self, VideoRect};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetMonitorInfoW, HFONT, HGDIOBJ, MONITOR_DEFAULTTONEAREST,
    MONITORINFO, MapWindowPoints, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi,
};

/// The minimum outer window size, in 96-DPI pixels, that fits the toolbar.
const MINIMUM_WINDOW_WIDTH: i32 = 1176;
const MINIMUM_WINDOW_HEIGHT: i32 = 300;
/// The settings window's outer size, in 96-DPI pixels.
const SETTINGS_WINDOW_WIDTH: i32 = 560;
const SETTINGS_WINDOW_HEIGHT: i32 = 626;

/// The client area and the letterboxed video inside it, in physical pixels.
pub(super) struct ClientLayout {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) video: VideoRect,
}

struct WindowContext {
    video_width: u32,
    video_height: u32,
    dpi: u32,
    font: HFONT,
    settings_dpi: u32,
    settings_font: HFONT,
    resize_pending: bool,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
    cursor_shape: CursorShape,
    debug: DebugInfo,
    debug_overlay: HWND,
    debug_visible: bool,
    debug_refreshed: std::time::Instant,
    toolbar: HWND,
    user_combo: HWND,
    display_combo: HWND,
    quality_combo: HWND,
    chroma_combo: HWND,
    diagnostics_button: HWND,
    settings_button: HWND,
    recording_visible: std::cell::Cell<bool>,
    file_button: HWND,
    chat_button: HWND,
    secure_attention_button: HWND,
    type_clipboard_button: HWND,
    credential_buttons: [HWND; 3],
    credential_label: HWND,
    chat_popup: Option<meshrmm_chat::ChatPopup>,
    minimize_button: HWND,
    maximize_button: HWND,
    close_button: HWND,
    settings_window: HWND,
    quality_buttons: [(HWND, QualityPreset); 4],
    chroma_buttons: [(HWND, ChromaMode); 2],
}

/// Settings controls are created at 96 DPI; the caller scales them.
struct SettingsControls {
    window: HWND,
    dpi: u32,
    quality_buttons: [(HWND, QualityPreset); 4],
    chroma_buttons: [(HWND, ChromaMode); 2],
}

const USER_COMBO_ID: usize = 4013;
const DISPLAY_COMBO_ID: usize = 4001;
const QUALITY_COMBO_ID: usize = 4002;
const CHROMA_COMBO_ID: usize = 4008;
const DIAGNOSTICS_BUTTON_ID: usize = 4003;
const SETTINGS_BUTTON_ID: usize = 4004;
const FILE_BUTTON_ID: usize = 4010;
const CHAT_BUTTON_ID: usize = 4009;
const SECURE_ATTENTION_BUTTON_ID: usize = 4011;
const TYPE_CLIPBOARD_BUTTON_ID: usize = 4012;
const CREDENTIAL_BUTTON_ID: usize = 4020;
const MINIMIZE_BUTTON_ID: usize = 4005;
const MAXIMIZE_BUTTON_ID: usize = 4006;
const CLOSE_BUTTON_ID: usize = 4007;
const QUALITY_ULTRA_DATA_SAVER_ID: usize = 4104;
const QUALITY_DATA_SAVER_ID: usize = 4101;
const QUALITY_BALANCED_ID: usize = 4102;
const QUALITY_BEST_ID: usize = 4103;
const CHROMA_420_ID: usize = 4111;
const CHROMA_444_ID: usize = 4112;
const SETTINGS_DISPLAY_TAB_ID: usize = 4201;
const SETTINGS_ADVANCED_TAB_ID: usize = 4202;
const SETTINGS_DISPLAY_TITLE_ID: i32 = 4211;
const SETTINGS_QUALITY_TITLE_ID: i32 = 4212;
const SETTINGS_CHROMA_TITLE_ID: i32 = 4213;
const SETTINGS_ADVANCED_TITLE_ID: i32 = 4221;
const SETTINGS_DIAGNOSTICS_ID: usize = 4222;
const SETTINGS_TECHNICIAN_INPUT_ID: usize = 4223;
const SETTINGS_AGENT_INPUT_ID: usize = 4224;
const SETTINGS_BLACKOUT_ID: usize = 4225;
const SETTINGS_AUDIO_ID: usize = 4226;
const SETTINGS_RECORDING_ID: usize = 4228;
const SETTINGS_DISCONNECT_ID: usize = 4232;
const SETTINGS_CLIPBOARD_ID: usize = 4233;
const SETTINGS_IDLE_ID: usize = 4231;
const SETTINGS_DISPLAY_BORDER_ID: usize = 4230;
const SETTINGS_WALLPAPER_ID: usize = 4229;
const SETTINGS_REMOTE_CURSOR_ID: usize = 4227;
const SETTINGS_CLOSE_TITLE_ID: i32 = 4234;
const SETTINGS_CLEAR_CLIPBOARD_ID: usize = 4238;
const SETTINGS_CLOSE_ACTION_IDS: [(usize, SessionCloseAction); 3] = [
    (4235, SessionCloseAction::NoAction),
    (4236, SessionCloseAction::Lock),
    (4237, SessionCloseAction::Logout),
];

impl Drop for WindowContext {
    fn drop(&mut self) {
        for font in [self.font, self.settings_font] {
            if !font.is_invalid() {
                let _ = unsafe { DeleteObject(HGDIOBJ(font.0)) };
            }
        }
    }
}

impl WindowContext {
    fn send(&self, message: SessionMessage) {
        self.control.send(message);
    }

    /// Converts 96-DPI layout pixels to this window's physical pixels.
    fn px(&self, value: i32) -> i32 {
        scale(value, self.dpi)
    }

    fn video_rect_for(&self, width: u32, height: u32) -> VideoRect {
        let toolbar = toolbar_height(self.dpi);
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let height = i32::try_from(height).unwrap_or(i32::MAX);
        video_layout::letterbox(
            VideoRect {
                left: 0,
                top: toolbar,
                width,
                height: height.saturating_sub(toolbar),
            },
            self.video_width,
            self.video_height,
        )
    }

    fn video_rect(&self, window: HWND) -> Option<VideoRect> {
        let (width, height) = unsafe { client_size(window) }?;
        Some(self.video_rect_for(width, height))
    }

    fn pointer_position(&self, window: HWND, lparam: LPARAM) -> Option<(u16, u16)> {
        self.normalized_client_position(
            window,
            signed_low_word(lparam.0),
            signed_high_word(lparam.0),
        )
    }

    fn normalized_client_position(&self, window: HWND, x: i32, y: i32) -> Option<(u16, u16)> {
        video_layout::normalize(self.video_rect(window)?, x, y)
    }

    /// Applies a new monitor DPI: fonts, toolbar layout and video placement.
    fn set_dpi(&mut self, window: HWND, dpi: u32) {
        if dpi == self.dpi {
            return;
        }
        self.dpi = dpi;
        let font = unsafe { message_font(dpi) };
        for control in self.toolbar_controls() {
            unsafe { set_font(control, font) };
        }
        let old = std::mem::replace(&mut self.font, font);
        if !old.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(old.0)) };
        }
        self.layout_toolbar(window);
        self.resize_pending = true;
    }

    fn toolbar_controls(&self) -> [HWND; 18] {
        [
            self.user_combo,
            self.display_combo,
            self.quality_combo,
            self.chroma_combo,
            self.diagnostics_button,
            self.settings_button,
            self.file_button,
            self.chat_button,
            self.secure_attention_button,
            self.type_clipboard_button,
            self.credential_buttons[0],
            self.credential_buttons[1],
            self.credential_buttons[2],
            self.credential_label,
            self.minimize_button,
            self.maximize_button,
            self.close_button,
            self.debug_overlay,
        ]
    }

    fn set_quality(&self, preset: QualityPreset) {
        let selected = quality_index(preset);
        unsafe {
            SendMessageW(
                self.quality_combo,
                CB_SETCURSEL,
                Some(WPARAM(selected)),
                None,
            )
        };
        for (button, candidate) in self.quality_buttons {
            let state = usize::from(candidate == preset);
            unsafe { SendMessageW(button, BM_SETCHECK, Some(WPARAM(state)), None) };
        }
        self.send(SessionMessage::SetQuality { preset });
    }

    fn set_chroma(&self, mode: ChromaMode) {
        if !self.control.supports_chroma(mode) {
            unsafe {
                SendMessageW(
                    self.chroma_combo,
                    CB_SETCURSEL,
                    Some(WPARAM(chroma_index(self.control.chroma_mode()))),
                    None,
                )
            };
            return;
        }
        let selected = chroma_index(mode);
        unsafe {
            SendMessageW(
                self.chroma_combo,
                CB_SETCURSEL,
                Some(WPARAM(selected)),
                None,
            )
        };
        for (button, candidate) in self.chroma_buttons {
            let state = usize::from(candidate == mode);
            unsafe { SendMessageW(button, BM_SETCHECK, Some(WPARAM(state)), None) };
        }
        self.send(SessionMessage::SetChroma { mode });
    }

    fn layout_toolbar(&self, window: HWND) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(window, &mut rect) }.is_err() {
            return;
        }
        let width = rect.right.saturating_sub(rect.left);
        let px = |value| self.px(value);
        let place = |control: HWND, x: i32, y: i32, width: i32, height: i32| {
            let _ = unsafe { MoveWindow(control, x, y, width, height, true) };
        };
        place(self.toolbar, 0, 0, width, toolbar_height(self.dpi));
        place(self.user_combo, px(8), px(5), px(158), px(300));
        place(self.display_combo, px(172), px(5), px(110), px(300));
        place(self.quality_combo, px(288), px(5), px(154), px(300));
        place(self.chroma_combo, px(448), px(5), px(124), px(300));
        for (i, button) in self.credential_buttons.iter().enumerate() {
            place(*button, px(8 + i as i32 * 184), px(38), px(180), px(24));
        }
        place(
            self.credential_label,
            px(566),
            px(42),
            (width - px(574)).max(1),
            px(20),
        );
        let caption_x = width.saturating_sub(px(138));
        place(self.minimize_button, caption_x, 0, px(46), px(34));
        place(self.maximize_button, caption_x + px(46), 0, px(46), px(34));
        place(self.close_button, caption_x + px(92), 0, px(46), px(34));
        place(
            self.diagnostics_button,
            caption_x.saturating_sub(px(78)),
            px(5),
            px(34),
            px(24),
        );
        place(
            self.settings_button,
            caption_x.saturating_sub(px(40)),
            px(5),
            px(34),
            px(24),
        );
        place(
            self.chat_button,
            caption_x.saturating_sub(px(138)),
            px(5),
            px(54),
            px(24),
        );
        place(
            self.file_button,
            caption_x.saturating_sub(px(180)),
            px(5),
            px(38),
            px(24),
        );
        place(
            self.secure_attention_button,
            caption_x.saturating_sub(px(296)),
            px(5),
            px(110),
            px(24),
        );
        place(
            self.type_clipboard_button,
            caption_x.saturating_sub(px(422)),
            px(5),
            px(120),
            px(24),
        );
        place(
            self.debug_overlay,
            px(12),
            toolbar_height(self.dpi) + px(12),
            px(640),
            px(300),
        );
        if let Some(chat) = &self.chat_popup {
            chat.layout();
        }
        let maximize_title = if unsafe { IsZoomed(window) }.as_bool() {
            w!("❐")
        } else {
            w!("□")
        };
        let _ = unsafe { SetWindowTextW(self.maximize_button, maximize_title) };
    }

    fn select_display(&self, index: usize) {
        if let Some(display) = self
            .active_display
            .session_displays(&self.displays)
            .get(index)
            && display.id != self.active_display.id
        {
            self.send(SessionMessage::SelectDisplay {
                display_id: display.id,
            });
        }
    }

    fn select_user(&self, index: usize) {
        let sessions = Display::sessions(&self.displays);
        if let Some(session) = sessions.get(index)
            && *session != self.active_display.session
            && let Some(display) = self
                .displays
                .iter()
                .find(|d| &d.session == session && d.primary)
                .or_else(|| self.displays.iter().find(|d| &d.session == session))
        {
            self.send(SessionMessage::SelectDisplay {
                display_id: display.id,
            });
        }
        let current = sessions
            .iter()
            .position(|s| *s == self.active_display.session)
            .unwrap_or(0);
        unsafe {
            SendMessageW(self.user_combo, CB_SETCURSEL, Some(WPARAM(current)), None);
        }
    }

    fn refresh_maintenance_controls(&self) {
        let state = self.control.credential_state();
        for (i, button) in self.credential_buttons.iter().enumerate() {
            unsafe {
                let _ = EnableWindow(
                    *button,
                    !self.control.technician_blocked()
                        && match i {
                            0 => state.available && !state.prompt_active,
                            1 => state.can_autofill,
                            _ => state.saved && !state.prompt_active,
                        },
                );
                if i == 1 {
                    let _ = ShowWindow(*button, if state.can_autofill { SW_SHOW } else { SW_HIDE });
                }
            }
        }
        let text: Vec<u16> = state.message.encode_utf16().chain(Some(0)).collect();
        let _ = unsafe { SetWindowTextW(self.credential_label, PCWSTR(text.as_ptr())) };
        let close_action = self.control.session_close_action();
        let recording = self.control.recording().active();
        if self.recording_visible.replace(recording) != recording {
            unsafe {
                let _ = SetWindowTextW(
                    self.settings_button,
                    if recording { w!("REC") } else { w!("⚙") },
                );
                if let Ok(button) =
                    GetDlgItem(Some(self.settings_window), SETTINGS_RECORDING_ID as i32)
                {
                    let _ = SetWindowTextW(
                        button,
                        if recording {
                            w!("Stop recording and save")
                        } else {
                            w!("Record video to Downloads")
                        },
                    );
                }
            }
        }
        for (id, checked, enabled) in [
            (
                SETTINGS_DISCONNECT_ID,
                self.control.disconnect_confirmation(),
                true,
            ),
            (
                SETTINGS_IDLE_ID,
                self.control.prevent_idle_lock(),
                self.control.allow_idle_override(),
            ),
            (
                SETTINGS_DISPLAY_BORDER_ID,
                self.control.display_border(),
                true,
            ),
            (SETTINGS_WALLPAPER_ID, self.control.wallpaper_hidden(), true),
            (SETTINGS_AUDIO_ID, self.control.audio_muted(), true),
            (SETTINGS_CLIPBOARD_ID, self.control.clipboard_sync(), true),
            (
                SETTINGS_CLEAR_CLIPBOARD_ID,
                self.control.clear_clipboard_on_close(),
                true,
            ),
            (
                SETTINGS_REMOTE_CURSOR_ID,
                self.control.show_remote_cursor(),
                true,
            ),
            (
                SETTINGS_TECHNICIAN_INPUT_ID,
                self.control.technician_blocked(),
                true,
            ),
            (
                SETTINGS_BLACKOUT_ID,
                self.control.maintenance_state().blacked_out,
                self.control.maintenance_state().available,
            ),
            (
                SETTINGS_AGENT_INPUT_ID,
                self.control.agent_blocked(),
                self.control.maintenance_state().available
                    && !self.control.maintenance_state().blacked_out,
            ),
        ]
        .into_iter()
        .chain(SETTINGS_CLOSE_ACTION_IDS.map(|(id, action)| (id, action == close_action, true)))
        {
            if let Ok(button) = unsafe { GetDlgItem(Some(self.settings_window), id as i32) } {
                unsafe {
                    if id == SETTINGS_IDLE_ID {
                        let _ = SetWindowTextW(
                            button,
                            if enabled {
                                w!("Prevent idle lock")
                            } else {
                                w!("Prevent idle lock (company managed)")
                            },
                        );
                    }
                    let _ = EnableWindow(button, enabled);
                    SendMessageW(
                        button,
                        BM_SETCHECK,
                        Some(WPARAM(usize::from(checked))),
                        None,
                    );
                }
            }
        }
    }
    fn show_settings(&self) {
        self.refresh_maintenance_controls();
        let _ = unsafe { ShowWindow(self.settings_window, SW_SHOW) };
        let _ = unsafe { SetForegroundWindow(self.settings_window) };
    }

    fn move_pointer(&self, window: HWND, lparam: LPARAM) {
        let position = if self.pressed_buttons.is_empty() {
            self.pointer_position(window, lparam)
        } else {
            // A drag that started on the video keeps mouse capture; pin the
            // remote pointer to the nearest edge instead of dropping motion.
            self.video_rect(window).map(|video| {
                video_layout::normalize_clamped(
                    video,
                    signed_low_word(lparam.0),
                    signed_high_word(lparam.0),
                )
            })
        };
        if let Some((x, y)) = position {
            self.send(SessionMessage::Input(RemoteInput::PointerMove {
                display_id: self.active_display.id,
                x,
                y,
            }));
        }
    }

    fn button(&mut self, window: HWND, lparam: LPARAM, button: PointerButton, pressed: bool) {
        let position = self.pointer_position(window, lparam);
        match position {
            Some((x, y)) => self.send(SessionMessage::Input(RemoteInput::PointerButtonAt {
                display_id: self.active_display.id,
                x,
                y,
                button,
                pressed,
            })),
            None if !pressed && self.pressed_buttons.contains(&button) => {
                // Finish a drag that began over the video without moving the
                // remote pointer to an out-of-bounds/clamped position.
                self.send(SessionMessage::Input(RemoteInput::PointerButton {
                    display_id: self.active_display.id,
                    button,
                    pressed: false,
                }));
            }
            None => return,
        }
        if pressed {
            self.pressed_buttons.insert(button);
            let _ = unsafe { SetCapture(window) };
        } else {
            self.pressed_buttons.remove(&button);
            if self.pressed_buttons.is_empty() {
                let _ = unsafe { ReleaseCapture() };
            }
        }
    }

    fn release_input(&mut self) {
        for (scan_code, extended) in self.pressed_keys.drain().collect::<Vec<_>>() {
            self.send(SessionMessage::Input(RemoteInput::Key {
                display_id: self.active_display.id,
                scan_code,
                extended,
                pressed: false,
            }));
        }
        for button in self.pressed_buttons.drain().collect::<Vec<_>>() {
            self.send(SessionMessage::Input(RemoteInput::PointerButton {
                display_id: self.active_display.id,
                button,
                pressed: false,
            }));
        }
    }

    fn select_next_display(&self) {
        let displays = self.active_display.session_displays(&self.displays);
        if displays.len() < 2 {
            return;
        }
        let current = displays
            .iter()
            .position(|d| d.id == self.active_display.id)
            .unwrap_or(0);
        self.send(SessionMessage::SelectDisplay {
            display_id: displays[(current + 1) % displays.len()].id,
        });
    }

    fn toggle_debug(&mut self) {
        self.debug_visible = !self.debug_visible;
        let command = if self.debug_visible { SW_SHOW } else { SW_HIDE };
        let _ = unsafe { ShowWindow(self.debug_overlay, command) };
        unsafe {
            SendMessageW(
                self.diagnostics_button,
                BM_SETCHECK,
                Some(WPARAM(usize::from(self.debug_visible))),
                None,
            )
        };
        if let Ok(button) =
            unsafe { GetDlgItem(Some(self.settings_window), SETTINGS_DIAGNOSTICS_ID as i32) }
        {
            unsafe {
                SendMessageW(
                    button,
                    BM_SETCHECK,
                    Some(WPARAM(usize::from(self.debug_visible))),
                    None,
                )
            };
        }
        if self.debug_visible {
            self.refresh_debug(true);
        }
    }

    fn refresh_debug(&mut self, force: bool) {
        if !self.debug_visible
            || (!force && self.debug_refreshed.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.debug_refreshed = std::time::Instant::now();
        let text = HSTRING::from(self.debug.render().replace('\n', "\r\n"));
        let _ = unsafe { SetWindowTextW(self.debug_overlay, PCWSTR(text.as_ptr())) };
    }
}

unsafe fn window_context(window: HWND) -> Option<&'static mut WindowContext> {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowContext;
    unsafe { pointer.as_mut() }
}

/// Scales a 96-DPI length to `dpi`, rounding to the nearest pixel.
fn scale(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi) + 48).div_euclid(96)) as i32
}

fn toolbar_height(dpi: u32) -> i32 {
    scale(VIEWER_TOOLBAR_HEIGHT as i32, dpi)
}

unsafe fn window_dpi(window: HWND) -> u32 {
    match unsafe { GetDpiForWindow(window) } {
        0 => 96,
        dpi => dpi,
    }
}

unsafe fn client_size(window: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(window, &mut rect) }.ok()?;
    Some((
        u32::try_from(rect.right.saturating_sub(rect.left)).unwrap_or(0),
        u32::try_from(rect.bottom.saturating_sub(rect.top)).unwrap_or(0),
    ))
}

/// The Windows message font at `dpi`. The caller owns the returned font.
unsafe fn message_font(dpi: u32) -> HFONT {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let loaded = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some((&mut metrics as *mut NONCLIENTMETRICSW).cast()),
            0,
            dpi,
        )
    };
    let font = if loaded.is_ok() {
        unsafe { CreateFontIndirectW(&metrics.lfMessageFont) }
    } else {
        HFONT::default()
    };
    if font.is_invalid() {
        // Stock objects ignore DeleteObject, so the fallback is safe to own.
        HFONT(unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0)
    } else {
        font
    }
}

unsafe fn set_font(control: HWND, font: HFONT) {
    unsafe {
        SendMessageW(
            control,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        )
    };
}

struct Rescale {
    parent: HWND,
    from: u32,
    to: u32,
    font: HFONT,
}

/// Moves and resizes a window's direct children from one DPI to another and
/// gives them `font`.
unsafe fn rescale_children(parent: HWND, from: u32, to: u32, font: HFONT) {
    unsafe extern "system" fn rescale_child(child: HWND, data: LPARAM) -> windows::core::BOOL {
        let rescale = unsafe { &*(data.0 as *const Rescale) };
        if unsafe { GetParent(child) }.ok() != Some(rescale.parent) {
            return true.into();
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(child, &mut rect) }.is_ok() {
            let mut points = [
                windows::Win32::Foundation::POINT {
                    x: rect.left,
                    y: rect.top,
                },
                windows::Win32::Foundation::POINT {
                    x: rect.right,
                    y: rect.bottom,
                },
            ];
            unsafe { MapWindowPoints(None, Some(rescale.parent), &mut points) };
            let convert = |value: i32| {
                (i64::from(value) * i64::from(rescale.to) / i64::from(rescale.from.max(1))) as i32
            };
            let _ = unsafe {
                MoveWindow(
                    child,
                    convert(points[0].x),
                    convert(points[0].y),
                    convert(points[1].x - points[0].x),
                    convert(points[1].y - points[0].y),
                    true,
                )
            };
        }
        unsafe { set_font(child, rescale.font) };
        true.into()
    }
    let rescale = Rescale {
        parent,
        from,
        to,
        font,
    };
    let _ = unsafe {
        EnumChildWindows(
            Some(parent),
            Some(rescale_child),
            LPARAM(&rescale as *const Rescale as isize),
        )
    };
}

/// Sizes a new window to show the video at 1:1 when it fits in 90% of the
/// monitor's work area, and scales it down otherwise, then centers it.
unsafe fn place_initial_window(window: HWND, style: WINDOW_STYLE, format: VideoFormat, dpi: u32) {
    let monitor = unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return;
    }
    let work = info.rcWork;
    let work_width = work.right - work.left;
    let work_height = work.bottom - work.top;
    let mut frame = RECT::default();
    if unsafe {
        AdjustWindowRectExForDpi(&mut frame, style, false, WINDOW_EX_STYLE::default(), dpi)
    }
    .is_err()
    {
        return;
    }
    let frame_width = frame.right - frame.left;
    let frame_height = frame.bottom - frame.top;
    let toolbar = toolbar_height(dpi);
    let (video_width, video_height) = video_layout::fit_within(
        format.width,
        format.height,
        work_width * 9 / 10 - frame_width,
        work_height * 9 / 10 - frame_height - toolbar,
    );
    let width = (video_width + frame_width)
        .max(scale(MINIMUM_WINDOW_WIDTH, dpi))
        .min(work_width);
    let height = (video_height + toolbar + frame_height)
        .max(scale(MINIMUM_WINDOW_HEIGHT, dpi))
        .min(work_height);
    let _ = unsafe {
        SetWindowPos(
            window,
            None,
            work.left + (work_width - width) / 2,
            work.top + (work_height - height) / 2,
            width,
            height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        )
    };
}

/// The current client layout, for creating the swap chain.
pub(super) unsafe fn client_layout(window: HWND) -> Option<ClientLayout> {
    let context = unsafe { window_context(window) }?;
    let (width, height) = unsafe { client_size(window) }?;
    Some(ClientLayout {
        width,
        height,
        video: context.video_rect_for(width, height),
    })
}

/// The new client layout if the window was resized or changed DPI since the
/// last call.
pub(super) unsafe fn take_resize(window: HWND) -> Option<ClientLayout> {
    let context = unsafe { window_context(window) }?;
    if !std::mem::take(&mut context.resize_pending) {
        return None;
    }
    unsafe { client_layout(window) }
}

fn signed_low_word(value: isize) -> i32 {
    i32::from(value as u16 as i16)
}

fn signed_high_word(value: isize) -> i32 {
    i32::from(((value as usize >> 16) as u16) as i16)
}

fn quality_index(preset: QualityPreset) -> usize {
    match preset {
        QualityPreset::UltraDataSaver => 0,
        QualityPreset::DataSaver => 1,
        QualityPreset::Balanced => 2,
        QualityPreset::BestQuality => 3,
    }
}

fn chroma_index(mode: ChromaMode) -> usize {
    match mode {
        ChromaMode::Yuv420 => 0,
        ChromaMode::Yuv444 => 1,
    }
}

unsafe fn show_settings_category(window: HWND, display: bool) {
    let display_command = if display { SW_SHOW } else { SW_HIDE };
    let advanced_command = if display { SW_HIDE } else { SW_SHOW };
    for id in [
        SETTINGS_DISPLAY_TITLE_ID,
        SETTINGS_QUALITY_TITLE_ID,
        SETTINGS_CHROMA_TITLE_ID,
        QUALITY_ULTRA_DATA_SAVER_ID as i32,
        QUALITY_DATA_SAVER_ID as i32,
        QUALITY_BALANCED_ID as i32,
        QUALITY_BEST_ID as i32,
        CHROMA_420_ID as i32,
        CHROMA_444_ID as i32,
        SETTINGS_REMOTE_CURSOR_ID as i32,
        SETTINGS_WALLPAPER_ID as i32,
        SETTINGS_DISPLAY_BORDER_ID as i32,
        SETTINGS_IDLE_ID as i32,
        SETTINGS_DISCONNECT_ID as i32,
    ] {
        if let Ok(control) = unsafe { GetDlgItem(Some(window), id) } {
            let _ = unsafe { ShowWindow(control, display_command) };
        }
    }
    for id in [
        SETTINGS_ADVANCED_TITLE_ID,
        SETTINGS_DIAGNOSTICS_ID as i32,
        SETTINGS_TECHNICIAN_INPUT_ID as i32,
        SETTINGS_AGENT_INPUT_ID as i32,
        SETTINGS_BLACKOUT_ID as i32,
        SETTINGS_AUDIO_ID as i32,
        SETTINGS_RECORDING_ID as i32,
        SETTINGS_CLIPBOARD_ID as i32,
        SETTINGS_CLEAR_CLIPBOARD_ID as i32,
        SETTINGS_CLOSE_TITLE_ID,
    ]
    .into_iter()
    .chain(SETTINGS_CLOSE_ACTION_IDS.map(|(id, _)| id as i32))
    {
        if let Ok(control) = unsafe { GetDlgItem(Some(window), id) } {
            let _ = unsafe { ShowWindow(control, advanced_command) };
        }
    }
    for (id, selected) in [
        (SETTINGS_DISPLAY_TAB_ID, display),
        (SETTINGS_ADVANCED_TAB_ID, !display),
    ] {
        if let Ok(control) = unsafe { GetDlgItem(Some(window), id as i32) } {
            unsafe {
                SendMessageW(
                    control,
                    BM_SETCHECK,
                    Some(WPARAM(usize::from(selected))),
                    None,
                )
            };
        }
    }
}

unsafe extern "system" fn settings_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    let owner = HWND(unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut c_void);
    match message {
        WM_COMMAND => {
            let control_id = wparam.0 & 0xffff;
            if control_id == SETTINGS_DISPLAY_TAB_ID {
                unsafe { show_settings_category(window, true) };
                return LRESULT(0);
            }
            if control_id == SETTINGS_ADVANCED_TAB_ID {
                unsafe { show_settings_category(window, false) };
                return LRESULT(0);
            }
            if let Some(context) = unsafe { window_context(owner) } {
                let preset = match control_id {
                    QUALITY_ULTRA_DATA_SAVER_ID => Some(QualityPreset::UltraDataSaver),
                    QUALITY_DATA_SAVER_ID => Some(QualityPreset::DataSaver),
                    QUALITY_BALANCED_ID => Some(QualityPreset::Balanced),
                    QUALITY_BEST_ID => Some(QualityPreset::BestQuality),
                    _ => None,
                };
                if let Some(preset) = preset {
                    context.set_quality(preset);
                    return LRESULT(0);
                }
                let chroma = match control_id {
                    CHROMA_420_ID => Some(ChromaMode::Yuv420),
                    CHROMA_444_ID => Some(ChromaMode::Yuv444),
                    _ => None,
                };
                if let Some(chroma) = chroma {
                    context.set_chroma(chroma);
                    return LRESULT(0);
                }
                if control_id == SETTINGS_DISCONNECT_ID {
                    context.control.toggle_disconnect_confirmation();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_IDLE_ID {
                    context.control.toggle_prevent_idle_lock();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_DISPLAY_BORDER_ID {
                    context.control.toggle_display_border();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_WALLPAPER_ID {
                    context.control.toggle_wallpaper();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_REMOTE_CURSOR_ID {
                    context.control.toggle_remote_cursor();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_RECORDING_ID {
                    context.control.toggle_recording();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_AUDIO_ID {
                    context.control.toggle_audio();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_CLIPBOARD_ID {
                    context.control.toggle_clipboard_sync();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_CLEAR_CLIPBOARD_ID {
                    context.control.toggle_clear_clipboard_on_close();
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if let Some((_, action)) = SETTINGS_CLOSE_ACTION_IDS
                    .iter()
                    .find(|(id, _)| *id == control_id)
                {
                    context.control.set_session_close_action(*action);
                    context.refresh_maintenance_controls();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_BLACKOUT_ID {
                    context.release_input();
                    context.control.toggle_blackout();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_AGENT_INPUT_ID {
                    context.release_input();
                    context.control.toggle_agent_input();
                    return LRESULT(0);
                }
                if control_id == SETTINGS_TECHNICIAN_INPUT_ID {
                    context.release_input();
                    context
                        .control
                        .set_technician_blocked(!context.control.technician_blocked());
                    unsafe {
                        apply_cursor(context.control.effective_cursor_shape(context.cursor_shape))
                    };
                    return LRESULT(0);
                }
                if control_id == SETTINGS_DIAGNOSTICS_ID {
                    context.toggle_debug();
                    return LRESULT(0);
                }
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe {
            SetTextColor(
                windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                windows::Win32::Foundation::COLORREF(0x00f4_f4f4),
            );
            SetBkColor(
                windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                windows::Win32::Foundation::COLORREF(0x0014_1414),
            );
            LRESULT(GetStockObject(BLACK_BRUSH).0 as isize)
        },
        WM_CLOSE => {
            let _ = unsafe { ShowWindow(window, SW_HIDE) };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xffff) as u32;
            if let Some(context) = unsafe { window_context(owner) } {
                let font = unsafe { message_font(dpi) };
                unsafe { rescale_children(window, context.settings_dpi, dpi, font) };
                let old = std::mem::replace(&mut context.settings_font, font);
                if !old.is_invalid() {
                    let _ = unsafe { DeleteObject(HGDIOBJ(old.0)) };
                }
                context.settings_dpi = dpi;
            }
            let suggested = unsafe { &*(lparam.0 as *const RECT) };
            let _ = unsafe {
                SetWindowPos(
                    window,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

unsafe fn create_settings_window(
    owner: HWND,
    instance: HINSTANCE,
) -> anyhow::Result<SettingsControls> {
    let class = w!("MeshRmmRemoteSettingsWindow");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(settings_window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        hbrBackground: HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("viewer settings window class registration failed");
        }
    }
    let settings = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("Viewer settings"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            SETTINGS_WINDOW_WIDTH,
            SETTINGS_WINDOW_HEIGHT,
            Some(owner),
            None,
            Some(instance),
            Some(owner.0),
        )
    }
    .context("viewer settings window creation failed")?;

    let make_control = |class_name: PCWSTR,
                        text: PCWSTR,
                        style: WINDOW_STYLE,
                        x: i32,
                        y: i32,
                        width: i32,
                        height: i32,
                        id: usize|
     -> anyhow::Result<HWND> {
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class_name,
                text,
                style,
                x,
                y,
                width,
                height,
                Some(settings),
                Some(HMENU(id as *mut c_void)),
                Some(instance),
                None,
            )
        }
        .context("viewer settings control creation failed")
    };
    let tab_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32 | WS_GROUP.0,
    );
    let _ = make_control(
        w!("BUTTON"),
        w!("Display"),
        tab_style,
        16,
        20,
        118,
        34,
        SETTINGS_DISPLAY_TAB_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Advanced"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32),
        16,
        60,
        118,
        34,
        SETTINGS_ADVANCED_TAB_ID,
    )?;
    let static_style = WS_CHILD | WS_VISIBLE;
    let _ = make_control(
        w!("STATIC"),
        w!("Image quality"),
        static_style,
        162,
        24,
        340,
        28,
        SETTINGS_DISPLAY_TITLE_ID as usize,
    )?;
    let _ = make_control(
        w!("STATIC"),
        w!("Choose the bandwidth used by the remote desktop."),
        static_style,
        162,
        58,
        350,
        24,
        SETTINGS_QUALITY_TITLE_ID as usize,
    )?;
    let radio_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32);
    let ultra_data_saver = make_control(
        w!("BUTTON"),
        w!("Ultra data saver · 1 Mbps · grayscale · 24 FPS"),
        WINDOW_STYLE(radio_style.0 | WS_GROUP.0),
        162,
        104,
        370,
        28,
        QUALITY_ULTRA_DATA_SAVER_ID,
    )?;
    let data_saver = make_control(
        w!("BUTTON"),
        w!("Data saver · 3 Mbps"),
        radio_style,
        162,
        144,
        340,
        28,
        QUALITY_DATA_SAVER_ID,
    )?;
    let balanced = make_control(
        w!("BUTTON"),
        w!("Balanced · 6 Mbps"),
        radio_style,
        162,
        184,
        340,
        28,
        QUALITY_BALANCED_ID,
    )?;
    let best = make_control(
        w!("BUTTON"),
        w!("Best quality · 12 Mbps maximum"),
        radio_style,
        162,
        224,
        340,
        28,
        QUALITY_BEST_ID,
    )?;
    let _ = make_control(
        w!("STATIC"),
        w!("Color detail"),
        static_style,
        162,
        264,
        340,
        24,
        SETTINGS_CHROMA_TITLE_ID as usize,
    )?;
    let chroma_420 = make_control(
        w!("BUTTON"),
        w!("4:2:0 · bandwidth efficient"),
        WINDOW_STYLE(radio_style.0 | WS_GROUP.0),
        162,
        294,
        340,
        28,
        CHROMA_420_ID,
    )?;
    let chroma_444 = make_control(
        w!("BUTTON"),
        w!("4:4:4 · crisp text and color"),
        radio_style,
        162,
        334,
        340,
        28,
        CHROMA_444_ID,
    )?;
    let _ = make_control(
        w!("STATIC"),
        w!("Troubleshooting"),
        static_style,
        162,
        24,
        340,
        28,
        SETTINGS_ADVANCED_TITLE_ID as usize,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Show diagnostics overlay"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        70,
        340,
        28,
        SETTINGS_DIAGNOSTICS_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Block technician input"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        110,
        340,
        28,
        SETTINGS_TECHNICIAN_INPUT_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Block agent keyboard and mouse"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        150,
        340,
        28,
        SETTINGS_AGENT_INPUT_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Black out all agent monitors"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        190,
        340,
        28,
        SETTINGS_BLACKOUT_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Mute audio"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        230,
        340,
        28,
        SETTINGS_AUDIO_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Show remote cursor"),
        WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_GROUP.0 | BS_AUTOCHECKBOX as u32,
        ),
        162,
        374,
        340,
        28,
        SETTINGS_REMOTE_CURSOR_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Record video to Downloads"),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        162,
        270,
        340,
        28,
        SETTINGS_RECORDING_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Sync clipboard"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        310,
        340,
        28,
        SETTINGS_CLIPBOARD_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Clear clipboard on session close"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        350,
        340,
        28,
        SETTINGS_CLEAR_CLIPBOARD_ID,
    )?;
    let _ = make_control(
        w!("STATIC"),
        w!("On session close"),
        static_style,
        162,
        398,
        340,
        24,
        SETTINGS_CLOSE_TITLE_ID as usize,
    )?;
    for (index, (id, action)) in SETTINGS_CLOSE_ACTION_IDS.into_iter().enumerate() {
        let label = HSTRING::from(action.label());
        let _ = make_control(
            w!("BUTTON"),
            PCWSTR(label.as_ptr()),
            WINDOW_STYLE(if index == 0 {
                radio_style.0 | WS_GROUP.0
            } else {
                radio_style.0
            }),
            162,
            428 + 36 * index as i32,
            340,
            28,
            id,
        )?;
    }
    let _ = make_control(
        w!("BUTTON"),
        w!("Hide remote wallpaper"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        414,
        340,
        28,
        SETTINGS_WALLPAPER_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Highlight viewed monitor on agent"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        450,
        340,
        28,
        SETTINGS_DISPLAY_BORDER_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Prevent idle lock"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        486,
        340,
        28,
        SETTINGS_IDLE_ID,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Disconnect confirmation"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32),
        162,
        522,
        340,
        28,
        SETTINGS_DISCONNECT_ID,
    )?;
    unsafe { show_settings_category(settings, true) };
    Ok(SettingsControls {
        window: settings,
        dpi: 96,
        quality_buttons: [
            (ultra_data_saver, QualityPreset::UltraDataSaver),
            (data_saver, QualityPreset::DataSaver),
            (balanced, QualityPreset::Balanced),
            (best, QualityPreset::BestQuality),
        ],
        chroma_buttons: [
            (chroma_420, ChromaMode::Yuv420),
            (chroma_444, ChromaMode::Yuv444),
        ],
    })
}

pub(super) unsafe fn create_window(
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
) -> anyhow::Result<HWND> {
    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe {
                SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            }
        }
        let context = unsafe { window_context(window) };
        match message {
            WM_GETMINMAXINFO => {
                let info = unsafe { &mut *(lparam.0 as *mut MINMAXINFO) };
                // Leave room for display/quality controls, session actions and caption buttons.
                let dpi = unsafe { window_dpi(window) };
                info.ptMinTrackSize.x = scale(MINIMUM_WINDOW_WIDTH, dpi);
                info.ptMinTrackSize.y = scale(MINIMUM_WINDOW_HEIGHT, dpi);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                if let Some(context) = context {
                    context.set_dpi(window, (wparam.0 & 0xffff) as u32);
                }
                let suggested = unsafe { &*(lparam.0 as *const RECT) };
                let _ = unsafe {
                    SetWindowPos(
                        window,
                        None,
                        suggested.left,
                        suggested.top,
                        suggested.right - suggested.left,
                        suggested.bottom - suggested.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                };
                LRESULT(0)
            }
            WM_NCHITTEST => {
                let default_hit = unsafe { DefWindowProcW(window, message, wparam, lparam) };
                if default_hit.0 != HTCLIENT as isize {
                    return default_hit;
                }
                let mut point = windows::Win32::Foundation::POINT {
                    x: signed_low_word(lparam.0),
                    y: signed_high_word(lparam.0),
                };
                let mut bounds = RECT::default();
                if unsafe { ScreenToClient(window, &mut point) }.as_bool()
                    && unsafe { GetClientRect(window, &mut bounds) }.is_ok()
                {
                    let width = bounds.right.saturating_sub(bounds.left);
                    let dpi = unsafe { window_dpi(window) };
                    if point.y >= 0
                        && point.y < toolbar_height(dpi)
                        && point.x >= scale(302, dpi)
                        && point.x < width.saturating_sub(scale(220, dpi))
                    {
                        return LRESULT(HTCAPTION as isize);
                    }
                }
                default_hit
            }
            WM_DROPFILES => {
                let Some(context) = context else {
                    return LRESULT(0);
                };
                let drop = windows::Win32::UI::Shell::HDROP(wparam.0 as *mut _);
                let paths = unsafe { meshrmm_file_transfer::windows::paths_from_drop(drop) };
                let mut point = windows::Win32::Foundation::POINT::default();
                unsafe {
                    let _ = windows::Win32::UI::Shell::DragQueryPoint(drop, &mut point);
                    windows::Win32::UI::Shell::DragFinish(drop);
                }
                let destination = context
                    .normalized_client_position(window, point.x, point.y)
                    .map(|(x, y)| meshrmm_protocol::FileDestination::Drop {
                        display_id: context.active_display.id,
                        x,
                        y,
                    })
                    .unwrap_or(meshrmm_protocol::FileDestination::Documents);
                context.release_input();
                context.control.files().send(paths, destination);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if let Some(context) = context {
                    context.move_pointer(window, lparam);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(context) = context {
                    // The worker resizes the swap chain after the message pump
                    // returns, which is after a border drag ends.
                    if wparam.0 != SIZE_MINIMIZED as usize {
                        context.resize_pending = true;
                    }
                    context.layout_toolbar(window);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if let Some(context) = context {
                    let control_id = wparam.0 & 0xffff;
                    let notification = (wparam.0 >> 16) & 0xffff;
                    if control_id == USER_COMBO_ID && notification == CBN_SELCHANGE as usize {
                        let selected =
                            unsafe { SendMessageW(context.user_combo, CB_GETCURSEL, None, None).0 };
                        if selected >= 0 {
                            context.release_input();
                            context.select_user(selected as usize);
                        }
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if control_id == DISPLAY_COMBO_ID && notification == CBN_SELCHANGE as usize {
                        let selected = unsafe {
                            SendMessageW(context.display_combo, CB_GETCURSEL, None, None).0
                        };
                        if selected >= 0 {
                            context.select_display(selected as usize);
                        }
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if control_id == QUALITY_COMBO_ID && notification == CBN_SELCHANGE as usize {
                        let selected = unsafe {
                            SendMessageW(context.quality_combo, CB_GETCURSEL, None, None).0
                        };
                        let preset = match selected {
                            0 => QualityPreset::UltraDataSaver,
                            1 => QualityPreset::DataSaver,
                            3 => QualityPreset::BestQuality,
                            _ => QualityPreset::Balanced,
                        };
                        context.set_quality(preset);
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if control_id == CHROMA_COMBO_ID && notification == CBN_SELCHANGE as usize {
                        let selected = unsafe {
                            SendMessageW(context.chroma_combo, CB_GETCURSEL, None, None).0
                        };
                        let mode = if selected == 1 {
                            ChromaMode::Yuv444
                        } else {
                            ChromaMode::Yuv420
                        };
                        context.set_chroma(mode);
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if control_id == DIAGNOSTICS_BUTTON_ID {
                        context.toggle_debug();
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if (CREDENTIAL_BUTTON_ID..CREDENTIAL_BUTTON_ID + 3).contains(&control_id) {
                        context.release_input();
                        context
                            .control
                            .send(match control_id - CREDENTIAL_BUTTON_ID {
                                0 => SessionMessage::PromptForCredentials,
                                1 => SessionMessage::AutofillCredentials,
                                _ => SessionMessage::ForgetCredentials,
                            });
                        return LRESULT(0);
                    }
                    if control_id == TYPE_CLIPBOARD_BUTTON_ID {
                        context.release_input();
                        context.control.type_clipboard(context.active_display.id);
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if control_id == SECURE_ATTENTION_BUTTON_ID {
                        context.release_input();
                        context.control.send_secure_attention();
                        let _ = unsafe { SetFocus(Some(window)) };
                        return LRESULT(0);
                    }
                    if control_id == FILE_BUTTON_ID {
                        context.release_input();
                        context.control.set_input_enabled(false);
                        unsafe {
                            if let Ok(menu) = CreatePopupMenu() {
                                let _ = AppendMenuW(menu, MF_STRING, 1, w!("Send"));
                                let _ = AppendMenuW(menu, MF_STRING, 2, w!("Receive"));
                                let status = context.control.files().status();
                                if !status.is_empty() {
                                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                                    let _ = AppendMenuW(
                                        menu,
                                        MF_STRING | MF_DISABLED,
                                        3,
                                        &HSTRING::from(status),
                                    );
                                }
                                let mut point = windows::Win32::Foundation::POINT::default();
                                let _ = GetCursorPos(&mut point);
                                let chosen = TrackPopupMenu(
                                    menu,
                                    TPM_RETURNCMD,
                                    point.x,
                                    point.y,
                                    None,
                                    window,
                                    None,
                                )
                                .0;
                                let _ = DestroyMenu(menu);
                                if chosen == 1 {
                                    context.control.files().pick();
                                }
                                if chosen == 2 {
                                    context.control.send(SessionMessage::FileTransfer(
                                        meshrmm_protocol::FileMessage::Pick,
                                    ));
                                }
                            }
                        }
                        context.control.set_input_enabled(true);
                        return LRESULT(0);
                    }
                    if control_id == CHAT_BUTTON_ID {
                        context.release_input();
                        context.control.set_input_enabled(false);
                        if let Some(chat) = &context.chat_popup {
                            chat.toggle();
                        }
                        return LRESULT(0);
                    }
                    if control_id == SETTINGS_BUTTON_ID {
                        context.show_settings();
                        return LRESULT(0);
                    }
                    if control_id == MINIMIZE_BUTTON_ID {
                        let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
                        return LRESULT(0);
                    }
                    if control_id == MAXIMIZE_BUTTON_ID {
                        let command = if unsafe { IsZoomed(window) }.as_bool() {
                            SW_RESTORE
                        } else {
                            SW_MAXIMIZE
                        };
                        let _ = unsafe { ShowWindow(window, command) };
                        context.layout_toolbar(window);
                        return LRESULT(0);
                    }
                    if control_id == CLOSE_BUTTON_ID {
                        unsafe { SendMessageW(window, WM_CLOSE, None, None) };
                        return LRESULT(0);
                    }
                }
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
            WM_SETCURSOR => {
                if let Some(context) = context
                    && (lparam.0 as u32 & 0xffff) == HTCLIENT
                {
                    unsafe {
                        apply_cursor(context.control.effective_cursor_shape(context.cursor_shape))
                    };
                    return LRESULT(1);
                }
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if let Some(context) = context {
                    let button = match message {
                        WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Left,
                        WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Right,
                        WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
                        _ if ((wparam.0 >> 16) as u16) == XBUTTON1 => PointerButton::Back,
                        _ => PointerButton::Forward,
                    };
                    let pressed = matches!(
                        message,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                    );
                    context.button(window, lparam, button, pressed);
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if let Some(context) = context {
                    let mut point = windows::Win32::Foundation::POINT {
                        x: signed_low_word(lparam.0),
                        y: signed_high_word(lparam.0),
                    };
                    if unsafe { ScreenToClient(window, &mut point) }.as_bool()
                        && let Some((x, y)) =
                            context.normalized_client_position(window, point.x, point.y)
                    {
                        let delta = ((wparam.0 >> 16) as u16) as i16;
                        context.send(SessionMessage::Input(RemoteInput::WheelAt {
                            display_id: context.active_display.id,
                            x,
                            y,
                            horizontal: if message == WM_MOUSEHWHEEL { delta } else { 0 },
                            vertical: if message == WM_MOUSEWHEEL { delta } else { 0 },
                        }));
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                if let Some(context) = context {
                    let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
                    if wparam.0 as u16 == VK_F8.0 && pressed {
                        if lparam.0 & (1 << 30) == 0 {
                            context.select_next_display();
                        }
                        return LRESULT(0);
                    }
                    if wparam.0 as u16 == VK_F8.0 {
                        return LRESULT(0);
                    }
                    if wparam.0 as u16 == VK_F12.0 {
                        if pressed && lparam.0 & (1 << 30) == 0 {
                            context.toggle_debug();
                        }
                        return LRESULT(0);
                    }
                    if pressed
                        && wparam.0 == 0x56
                        && unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x11) }
                            < 0
                        && context
                            .control
                            .files()
                            .paste_files(context.active_display.id)
                    {
                        context.release_input();
                        return LRESULT(0);
                    }
                    let scan_code = ((lparam.0 >> 16) & 0xff) as u16;
                    let extended = lparam.0 & (1 << 24) != 0;
                    if scan_code != 0 {
                        context.send(SessionMessage::Input(RemoteInput::Key {
                            display_id: context.active_display.id,
                            scan_code,
                            extended,
                            pressed,
                        }));
                        if pressed {
                            context.pressed_keys.insert((scan_code, extended));
                        } else {
                            context.pressed_keys.remove(&(scan_code, extended));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_SETFOCUS => {
                if let Some(context) = context {
                    context.control.set_input_enabled(true);
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe {
                SetTextColor(
                    windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                    windows::Win32::Foundation::COLORREF(0x00f4_f4f4),
                );
                SetBkColor(
                    windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                    windows::Win32::Foundation::COLORREF(0x0014_1414),
                );
                LRESULT(GetStockObject(BLACK_BRUSH).0 as isize)
            },
            WM_KILLFOCUS => {
                if let Some(context) = context {
                    context.release_input();
                    context.control.set_input_enabled(false);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let confirm = context
                    .map(|context| {
                        context.release_input();
                        context.control.set_input_enabled(false);
                        context.control.disconnect_confirmation()
                    })
                    .unwrap_or(false);
                if confirm
                    && unsafe {
                        MessageBoxW(
                            Some(window),
                            w!("Disconnect from this device?"),
                            w!("End remote session"),
                            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
                        )
                    } != IDYES
                {
                    if let Some(context) = unsafe { window_context(window) } {
                        context.control.set_input_enabled(true);
                    }
                    return LRESULT(0);
                }
                let _ = unsafe { DestroyWindow(window) };
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let pointer =
                    unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowContext;
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                if !pointer.is_null() {
                    drop(unsafe { Box::from_raw(pointer) });
                }
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    let module =
        unsafe { GetModuleHandleW(None) }.context("application module handle unavailable")?;
    let instance = HINSTANCE(module.0);
    let class = w!("MeshRmmRemoteDesktopWindow");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        // A second session in the same process may find the class registered.
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("remote desktop window class registration failed");
        }
    }
    let window_style = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 & !WS_CAPTION.0);
    let title = HSTRING::from(format!(
        "MeshRMM Remote Desktop — {} ({}) — F8 display · F12 diagnostics",
        active_display.name,
        if active_display.primary {
            "primary"
        } else {
            "secondary"
        }
    ));
    let context = Box::new(WindowContext {
        video_width: format.width,
        video_height: format.height,
        dpi: 96,
        font: HFONT::default(),
        settings_dpi: 96,
        settings_font: HFONT::default(),
        resize_pending: false,
        active_display,
        displays,
        control,
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
        cursor_shape: CursorShape::Default,
        debug,
        debug_overlay: HWND::default(),
        debug_visible: false,
        debug_refreshed: std::time::Instant::now(),
        toolbar: HWND::default(),
        user_combo: HWND::default(),
        display_combo: HWND::default(),
        quality_combo: HWND::default(),
        chroma_combo: HWND::default(),
        diagnostics_button: HWND::default(),
        settings_button: HWND::default(),
        recording_visible: std::cell::Cell::new(false),
        file_button: HWND::default(),
        chat_button: HWND::default(),
        secure_attention_button: HWND::default(),
        type_clipboard_button: HWND::default(),
        credential_buttons: [HWND::default(); 3],
        credential_label: HWND::default(),
        chat_popup: None,
        minimize_button: HWND::default(),
        maximize_button: HWND::default(),
        close_button: HWND::default(),
        settings_window: HWND::default(),
        quality_buttons: [
            (HWND::default(), QualityPreset::UltraDataSaver),
            (HWND::default(), QualityPreset::DataSaver),
            (HWND::default(), QualityPreset::Balanced),
            (HWND::default(), QualityPreset::BestQuality),
        ],
        chroma_buttons: [
            (HWND::default(), ChromaMode::Yuv420),
            (HWND::default(), ChromaMode::Yuv444),
        ],
    });
    let context = Box::into_raw(context);
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            PCWSTR(title.as_ptr()),
            window_style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(MINIMUM_WINDOW_WIDTH, 96),
            scale(MINIMUM_WINDOW_HEIGHT, 96),
            None,
            None,
            Some(instance),
            Some(context.cast()),
        )
    };
    let window = match window {
        Ok(window) => window,
        Err(error) => {
            drop(unsafe { Box::from_raw(context) });
            return Err(error).context("native remote desktop window creation failed");
        }
    };
    let dpi = unsafe { window_dpi(window) };
    let font = unsafe { message_font(dpi) };
    if let Some(context) = unsafe { window_context(window) } {
        context.dpi = dpi;
        context.font = font;
    }
    unsafe { place_initial_window(window, window_style, format, dpi) };
    let overlay = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_BORDER,
            0,
            0,
            1,
            1,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("debug overlay creation failed")?;
    let toolbar = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER,
            0,
            0,
            1,
            1,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("viewer toolbar creation failed")?;
    let display_combo = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("COMBOBOX"),
            w!(""),
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            12,
            10,
            200,
            300,
            Some(window),
            Some(HMENU(DISPLAY_COMBO_ID as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .context("display dropdown creation failed")?;
    let user_combo = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("COMBOBOX"),
            w!(""),
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            8,
            5,
            158,
            300,
            Some(window),
            Some(HMENU(USER_COMBO_ID as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .context("user dropdown creation failed")?;
    let context = unsafe { window_context(window) }.context("viewer context unavailable")?;
    let sessions = Display::sessions(&context.displays);
    for session in &sessions {
        let title = HSTRING::from(session.label());
        unsafe {
            SendMessageW(
                user_combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(title.as_ptr() as isize)),
            );
        }
    }
    let active_session = sessions
        .iter()
        .position(|s| *s == context.active_display.session)
        .unwrap_or(0);
    unsafe {
        SendMessageW(user_combo, CB_SETCURSEL, Some(WPARAM(active_session)), None);
    }
    let visible = context.active_display.session_displays(&context.displays);
    for (index, display) in visible.iter().enumerate() {
        let title = HSTRING::from(display.selection_label(index));
        unsafe {
            SendMessageW(
                display_combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(title.as_ptr() as isize)),
            );
        }
    }
    let active_index = visible
        .iter()
        .position(|d| d.id == context.active_display.id)
        .unwrap_or(0);
    unsafe {
        SendMessageW(
            display_combo,
            CB_SETCURSEL,
            Some(WPARAM(active_index)),
            None,
        );
        let _ = EnableWindow(display_combo, visible.len() > 1);
    }
    let quality_combo = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("COMBOBOX"),
            w!(""),
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            222,
            10,
            150,
            300,
            Some(window),
            Some(HMENU(QUALITY_COMBO_ID as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .context("quality dropdown creation failed")?;
    for title in [
        w!("Ultra data saver"),
        w!("Data saver"),
        w!("Balanced"),
        w!("Best quality"),
    ] {
        unsafe {
            SendMessageW(
                quality_combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(title.as_ptr() as isize)),
            )
        };
    }
    let chroma_combo = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("COMBOBOX"),
            w!(""),
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            378,
            10,
            150,
            300,
            Some(window),
            Some(HMENU(CHROMA_COMBO_ID as *mut c_void)),
            Some(instance),
            None,
        )
    }
    .context("chroma dropdown creation failed")?;
    for title in [w!("4:2:0 efficient"), w!("4:4:4 crisp")] {
        unsafe {
            SendMessageW(
                chroma_combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(title.as_ptr() as isize)),
            )
        };
    }
    let make_toolbar_button =
        |id: usize, text: PCWSTR, style: WINDOW_STYLE| -> anyhow::Result<HWND> {
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    text,
                    style,
                    0,
                    0,
                    1,
                    1,
                    Some(window),
                    Some(HMENU(id as *mut c_void)),
                    Some(instance),
                    None,
                )
            }
            .context("viewer toolbar button creation failed")
        };
    let toolbar_button_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP;
    let diagnostics_button = make_toolbar_button(
        DIAGNOSTICS_BUTTON_ID,
        w!("ⓘ"),
        WINDOW_STYLE(toolbar_button_style.0 | BS_AUTOCHECKBOX as u32),
    )?;
    let settings_button = make_toolbar_button(SETTINGS_BUTTON_ID, w!("⚙"), toolbar_button_style)?;
    unsafe {
        windows::Win32::UI::Shell::DragAcceptFiles(window, true);
    }
    let file_button = make_toolbar_button(FILE_BUTTON_ID, w!("📁"), toolbar_button_style)?;
    let chat_button = make_toolbar_button(CHAT_BUTTON_ID, w!("💬"), toolbar_button_style)?;
    let type_clipboard_button = make_toolbar_button(
        TYPE_CLIPBOARD_BUTTON_ID,
        w!("Type clipboard"),
        toolbar_button_style,
    )?;
    let credential_buttons = [
        make_toolbar_button(
            CREDENTIAL_BUTTON_ID,
            w!("Prompt for credentials"),
            toolbar_button_style,
        )?,
        make_toolbar_button(
            CREDENTIAL_BUTTON_ID + 1,
            w!("Autofill credentials?"),
            toolbar_button_style,
        )?,
        make_toolbar_button(
            CREDENTIAL_BUTTON_ID + 2,
            w!("Forget credentials"),
            toolbar_button_style,
        )?,
    ];
    let credential_label = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            1,
            1,
            Some(window),
            None,
            Some(instance),
            None,
        )
    }
    .context("credential status label creation failed")?;
    let secure_attention_button = make_toolbar_button(
        SECURE_ATTENTION_BUTTON_ID,
        w!("Ctrl+Alt+Del"),
        toolbar_button_style,
    )?;
    let caption_button_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32 | BS_FLAT as u32);
    let minimize_button = make_toolbar_button(MINIMIZE_BUTTON_ID, w!("─"), caption_button_style)?;
    let maximize_button = make_toolbar_button(MAXIMIZE_BUTTON_ID, w!("□"), caption_button_style)?;
    let close_button = make_toolbar_button(CLOSE_BUTTON_ID, w!("×"), caption_button_style)?;
    for control in [
        overlay,
        user_combo,
        display_combo,
        quality_combo,
        chroma_combo,
        diagnostics_button,
        settings_button,
        file_button,
        chat_button,
        secure_attention_button,
        type_clipboard_button,
        credential_buttons[0],
        credential_buttons[1],
        credential_buttons[2],
        credential_label,
        minimize_button,
        maximize_button,
        close_button,
    ] {
        unsafe { set_font(control, font) };
    }
    let settings = unsafe { create_settings_window(window, instance) }?;
    let settings_dpi = unsafe { window_dpi(settings.window) };
    let settings_font = unsafe { message_font(settings_dpi) };
    unsafe { rescale_children(settings.window, settings.dpi, settings_dpi, settings_font) };
    let _ = unsafe {
        SetWindowPos(
            settings.window,
            None,
            0,
            0,
            scale(SETTINGS_WINDOW_WIDTH, settings_dpi),
            scale(SETTINGS_WINDOW_HEIGHT, settings_dpi),
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
        )
    };
    if let Some(context) = unsafe { window_context(window) } {
        context.debug_overlay = overlay;
        context.toolbar = toolbar;
        context.user_combo = user_combo;
        context.display_combo = display_combo;
        context.quality_combo = quality_combo;
        context.chroma_combo = chroma_combo;
        context.diagnostics_button = diagnostics_button;
        context.settings_button = settings_button;
        context.file_button = file_button;
        context.chat_button = chat_button;
        context.secure_attention_button = secure_attention_button;
        context.type_clipboard_button = type_clipboard_button;
        context.credential_buttons = credential_buttons;
        context.credential_label = credential_label;
        context.chat_popup = Some(unsafe {
            meshrmm_chat::ChatPopup::new(window, chat_button, context.control.chat())
        }?);
        context.minimize_button = minimize_button;
        context.maximize_button = maximize_button;
        context.close_button = close_button;
        context.settings_window = settings.window;
        context.settings_dpi = settings_dpi;
        context.settings_font = settings_font;
        context.quality_buttons = settings.quality_buttons;
        context.chroma_buttons = settings.chroma_buttons;
        if !context.control.supports_chroma(ChromaMode::Yuv444) {
            let _ = unsafe { EnableWindow(context.chroma_buttons[1].0, false) };
        }
        context.layout_toolbar(window);
        context.set_quality(context.control.quality_preset());
        context.set_chroma(context.control.chroma_mode());
    }
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    Ok(window)
}

pub(super) unsafe fn set_window_cursor(window: HWND, shape: CursorShape) {
    if let Some(context) = unsafe { window_context(window) } {
        context.cursor_shape = shape;
        unsafe { apply_cursor(context.control.effective_cursor_shape(shape)) };
    }
}

unsafe fn apply_cursor(shape: CursorShape) {
    let resource = match shape {
        CursorShape::Default => IDC_ARROW,
        CursorShape::Text => IDC_IBEAM,
        CursorShape::Wait => IDC_WAIT,
        CursorShape::Crosshair => IDC_CROSS,
        CursorShape::UpArrow => IDC_UPARROW,
        CursorShape::ResizeNorthWestSouthEast => IDC_SIZENWSE,
        CursorShape::ResizeNorthEastSouthWest => IDC_SIZENESW,
        CursorShape::ResizeWestEast => IDC_SIZEWE,
        CursorShape::ResizeNorthSouth => IDC_SIZENS,
        CursorShape::Move => IDC_SIZEALL,
        CursorShape::NotAllowed => IDC_NO,
        CursorShape::Pointer => IDC_HAND,
        CursorShape::Progress => IDC_APPSTARTING,
        CursorShape::Help => IDC_HELP,
        CursorShape::Pin => IDC_PIN,
        CursorShape::Person => IDC_PERSON,
    };
    let cursor =
        unsafe { LoadCursorW(None, resource) }.or_else(|_| unsafe { LoadCursorW(None, IDC_ARROW) });
    if let Ok(cursor) = cursor {
        unsafe { SetCursor(Some(cursor)) };
    }
}

pub(super) unsafe fn pump_window_messages(window: HWND) -> bool {
    if let Some(context) = unsafe { window_context(window) } {
        context.refresh_maintenance_controls();
        if let Some(notice) = context.control.recording().take_notice() {
            let text = HSTRING::from(notice);
            unsafe {
                MessageBoxW(
                    Some(window),
                    PCWSTR(text.as_ptr()),
                    w!("Session recording"),
                    MB_OK,
                );
            }
        }
        if let Some(error) = context.control.take_maintenance_error() {
            let text: Vec<u16> = error.encode_utf16().chain(Some(0)).collect();
            unsafe {
                MessageBoxW(
                    Some(window),
                    PCWSTR(text.as_ptr()),
                    w!("Maintenance control failed"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        context.refresh_debug(false);
        if let Some(chat) = &context.chat_popup {
            chat.refresh();
        }
    }
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return true;
        }
        if let Some(context) = unsafe { window_context(window) }
            && let Some(chat) = &context.chat_popup
            && chat.handle_message(&message)
        {
            continue;
        }
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    false
}

// Only monitor/ownership transitions reach this path, never individual mouse moves.
pub(super) unsafe fn set_agent_pointer_display(
    window: HWND,
    display_id: Option<meshrmm_protocol::DisplayId>,
) {
    if let Some(context) = unsafe { window_context(window) } {
        unsafe {
            let combo = GetDlgItem(Some(window), DISPLAY_COMBO_ID as i32);
            if let Ok(combo) = combo {
                let selected = SendMessageW(combo, CB_GETCURSEL, None, None);
                SendMessageW(combo, CB_RESETCONTENT, None, None);
                for (index, display) in context
                    .active_display
                    .session_displays(&context.displays)
                    .iter()
                    .enumerate()
                {
                    let title = if display_id == Some(display.id) {
                        format!("➤ {}", display.selection_label(index))
                    } else {
                        display.selection_label(index)
                    };
                    let title = HSTRING::from(title);
                    SendMessageW(
                        combo,
                        CB_ADDSTRING,
                        None,
                        Some(LPARAM(title.as_ptr() as isize)),
                    );
                }
                SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(selected.0 as usize)), None);
            }
        }
    }
}
