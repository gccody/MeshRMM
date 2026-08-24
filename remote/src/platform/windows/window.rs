use super::*;

struct WindowContext {
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
    display_combo: HWND,
    quality_combo: HWND,
    chroma_combo: HWND,
    diagnostics_button: HWND,
    settings_button: HWND,
    minimize_button: HWND,
    maximize_button: HWND,
    close_button: HWND,
    settings_window: HWND,
    quality_buttons: [(HWND, QualityPreset); 3],
    chroma_buttons: [(HWND, ChromaMode); 2],
}

struct SettingsControls {
    window: HWND,
    quality_buttons: [(HWND, QualityPreset); 3],
    chroma_buttons: [(HWND, ChromaMode); 2],
}

const DISPLAY_COMBO_ID: usize = 4001;
const QUALITY_COMBO_ID: usize = 4002;
const CHROMA_COMBO_ID: usize = 4008;
const DIAGNOSTICS_BUTTON_ID: usize = 4003;
const SETTINGS_BUTTON_ID: usize = 4004;
const MINIMIZE_BUTTON_ID: usize = 4005;
const MAXIMIZE_BUTTON_ID: usize = 4006;
const CLOSE_BUTTON_ID: usize = 4007;
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

impl WindowContext {
    fn send(&self, message: SessionMessage) {
        self.control.send(message);
    }

    fn pointer_position(&self, window: HWND, lparam: LPARAM) -> Option<(u16, u16)> {
        self.normalized_client_position(
            window,
            signed_low_word(lparam.0),
            signed_high_word(lparam.0),
        )
    }

    fn normalized_client_position(&self, window: HWND, x: i32, y: i32) -> Option<(u16, u16)> {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(window, &mut rect) }.is_err() {
            return None;
        }
        let width = rect.right.saturating_sub(rect.left).max(1);
        let height = rect
            .bottom
            .saturating_sub(rect.top)
            .saturating_sub(VIEWER_TOOLBAR_HEIGHT as i32)
            .max(1);
        let video_y = y.saturating_sub(VIEWER_TOOLBAR_HEIGHT as i32);
        if x < 0 || x >= width || video_y < 0 || video_y >= height {
            return None;
        }
        Some((
            (i64::from(x) * 65_535 / i64::from((width - 1).max(1))) as u16,
            (i64::from(video_y) * 65_535 / i64::from((height - 1).max(1))) as u16,
        ))
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
        let _ = unsafe {
            MoveWindow(
                self.toolbar,
                0,
                0,
                width,
                VIEWER_TOOLBAR_HEIGHT as i32,
                true,
            )
        };
        let _ = unsafe { MoveWindow(self.display_combo, 8, 5, 158, 300, true) };
        let _ = unsafe { MoveWindow(self.quality_combo, 172, 5, 124, 300, true) };
        let _ = unsafe { MoveWindow(self.chroma_combo, 302, 5, 124, 300, true) };
        let caption_x = width.saturating_sub(138);
        let _ = unsafe { MoveWindow(self.minimize_button, caption_x, 0, 46, 34, true) };
        let _ = unsafe { MoveWindow(self.maximize_button, caption_x + 46, 0, 46, 34, true) };
        let _ = unsafe { MoveWindow(self.close_button, caption_x + 92, 0, 46, 34, true) };
        let _ = unsafe {
            MoveWindow(
                self.diagnostics_button,
                caption_x.saturating_sub(78),
                5,
                34,
                24,
                true,
            )
        };
        let _ = unsafe {
            MoveWindow(
                self.settings_button,
                caption_x.saturating_sub(40),
                5,
                34,
                24,
                true,
            )
        };
        let maximize_title = if unsafe { IsZoomed(window) }.as_bool() {
            w!("❐")
        } else {
            w!("□")
        };
        let _ = unsafe { SetWindowTextW(self.maximize_button, maximize_title) };
    }

    fn select_display(&self, index: usize) {
        if let Some(display) = self.displays.get(index)
            && display.id != self.active_display.id
        {
            self.send(SessionMessage::SelectDisplay {
                display_id: display.id,
            });
        }
    }

    fn show_settings(&self) {
        let _ = unsafe { ShowWindow(self.settings_window, SW_SHOW) };
        let _ = unsafe { SetForegroundWindow(self.settings_window) };
    }

    fn move_pointer(&self, window: HWND, lparam: LPARAM) {
        if let Some((x, y)) = self.pointer_position(window, lparam) {
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
        if self.displays.len() < 2 {
            return;
        }
        let current = self
            .displays
            .iter()
            .position(|display| display.id == self.active_display.id)
            .unwrap_or(0);
        let next = self.displays[(current + 1) % self.displays.len()].id;
        self.send(SessionMessage::SelectDisplay { display_id: next });
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

fn signed_low_word(value: isize) -> i32 {
    i32::from(value as u16 as i16)
}

fn signed_high_word(value: isize) -> i32 {
    i32::from(((value as usize >> 16) as u16) as i16)
}

fn quality_index(preset: QualityPreset) -> usize {
    match preset {
        QualityPreset::DataSaver => 0,
        QualityPreset::Balanced => 1,
        QualityPreset::BestQuality => 2,
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
        QUALITY_DATA_SAVER_ID as i32,
        QUALITY_BALANCED_ID as i32,
        QUALITY_BEST_ID as i32,
        CHROMA_420_ID as i32,
        CHROMA_444_ID as i32,
    ] {
        if let Ok(control) = unsafe { GetDlgItem(Some(window), id) } {
            let _ = unsafe { ShowWindow(control, display_command) };
        }
    }
    for id in [SETTINGS_ADVANCED_TITLE_ID, SETTINGS_DIAGNOSTICS_ID as i32] {
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
            560,
            390,
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
    let data_saver = make_control(
        w!("BUTTON"),
        w!("Data saver · 3 Mbps"),
        WINDOW_STYLE(radio_style.0 | WS_GROUP.0),
        162,
        104,
        340,
        28,
        QUALITY_DATA_SAVER_ID,
    )?;
    let balanced = make_control(
        w!("BUTTON"),
        w!("Balanced · 6 Mbps"),
        radio_style,
        162,
        144,
        340,
        28,
        QUALITY_BALANCED_ID,
    )?;
    let best = make_control(
        w!("BUTTON"),
        w!("Best quality · 12 Mbps maximum"),
        radio_style,
        162,
        184,
        340,
        28,
        QUALITY_BEST_ID,
    )?;
    let _ = make_control(
        w!("STATIC"),
        w!("Color detail"),
        static_style,
        162,
        224,
        340,
        24,
        SETTINGS_CHROMA_TITLE_ID as usize,
    )?;
    let chroma_420 = make_control(
        w!("BUTTON"),
        w!("4:2:0 · bandwidth efficient"),
        WINDOW_STYLE(radio_style.0 | WS_GROUP.0),
        162,
        254,
        340,
        28,
        CHROMA_420_ID,
    )?;
    let chroma_444 = make_control(
        w!("BUTTON"),
        w!("4:4:4 · crisp text and color"),
        radio_style,
        162,
        294,
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
    unsafe { show_settings_category(settings, true) };
    Ok(SettingsControls {
        window: settings,
        quality_buttons: [
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
                    if point.y >= 0
                        && point.y < VIEWER_TOOLBAR_HEIGHT as i32
                        && point.x >= 302
                        && point.x < width.saturating_sub(220)
                    {
                        return LRESULT(HTCAPTION as isize);
                    }
                }
                default_hit
            }
            WM_MOUSEMOVE => {
                if let Some(context) = context {
                    context.move_pointer(window, lparam);
                }
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(context) = context {
                    context.layout_toolbar(window);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                if let Some(context) = context {
                    let control_id = wparam.0 & 0xffff;
                    let notification = (wparam.0 >> 16) & 0xffff;
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
                            0 => QualityPreset::DataSaver,
                            2 => QualityPreset::BestQuality,
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
                    unsafe { apply_cursor(context.cursor_shape) };
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
                if let Some(context) = context {
                    context.release_input();
                    context.control.set_input_enabled(false);
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
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: format.width as i32,
        bottom: format.height.saturating_add(VIEWER_TOOLBAR_HEIGHT) as i32,
    };
    let window_style = WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 & !WS_CAPTION.0);
    unsafe { AdjustWindowRect(&mut rect, window_style, false) }
        .context("remote window bounds calculation failed")?;
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
        display_combo: HWND::default(),
        quality_combo: HWND::default(),
        chroma_combo: HWND::default(),
        diagnostics_button: HWND::default(),
        settings_button: HWND::default(),
        minimize_button: HWND::default(),
        maximize_button: HWND::default(),
        close_button: HWND::default(),
        settings_window: HWND::default(),
        quality_buttons: [
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
            window_style | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
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
    let overlay = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_BORDER,
            12,
            VIEWER_TOOLBAR_HEIGHT as i32 + 12,
            640,
            300,
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
            format.width as i32,
            VIEWER_TOOLBAR_HEIGHT as i32,
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
    for display in &unsafe { window_context(window) }
        .context("viewer context unavailable while building toolbar")?
        .displays
    {
        let title = HSTRING::from(display.name.as_str());
        unsafe {
            SendMessageW(
                display_combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(title.as_ptr() as isize)),
            )
        };
    }
    let active_index = unsafe { window_context(window) }
        .and_then(|context| {
            context
                .displays
                .iter()
                .position(|display| display.id == context.active_display.id)
        })
        .unwrap_or(0);
    unsafe {
        SendMessageW(
            display_combo,
            CB_SETCURSEL,
            Some(WPARAM(active_index)),
            None,
        )
    };
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
    for title in [w!("Data saver"), w!("Balanced"), w!("Best quality")] {
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
    let caption_button_style =
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_PUSHBUTTON as u32 | BS_FLAT as u32);
    let minimize_button = make_toolbar_button(MINIMIZE_BUTTON_ID, w!("─"), caption_button_style)?;
    let maximize_button = make_toolbar_button(MAXIMIZE_BUTTON_ID, w!("□"), caption_button_style)?;
    let close_button = make_toolbar_button(CLOSE_BUTTON_ID, w!("×"), caption_button_style)?;
    let header_font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    for control in [
        display_combo,
        quality_combo,
        chroma_combo,
        diagnostics_button,
        settings_button,
        minimize_button,
        maximize_button,
        close_button,
    ] {
        unsafe {
            SendMessageW(
                control,
                WM_SETFONT,
                Some(WPARAM(header_font.0 as usize)),
                Some(LPARAM(1)),
            )
        };
    }
    let settings = unsafe { create_settings_window(window, instance) }?;
    if let Some(context) = unsafe { window_context(window) } {
        context.debug_overlay = overlay;
        context.toolbar = toolbar;
        context.display_combo = display_combo;
        context.quality_combo = quality_combo;
        context.chroma_combo = chroma_combo;
        context.diagnostics_button = diagnostics_button;
        context.settings_button = settings_button;
        context.minimize_button = minimize_button;
        context.maximize_button = maximize_button;
        context.close_button = close_button;
        context.settings_window = settings.window;
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
        unsafe { apply_cursor(shape) };
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
        context.refresh_debug(false);
    }
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return true;
        }
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    false
}
