//! The viewer settings window: display, advanced and keyboard pages, and the
//! maintenance state they and the toolbar show.

use super::*;

/// The settings window's outer size, in 96-DPI pixels.
pub(super) const SETTINGS_WINDOW_WIDTH: i32 = 560;
pub(super) const SETTINGS_WINDOW_HEIGHT: i32 = 626;

const QUALITY_ULTRA_DATA_SAVER_ID: usize = 4104;
const QUALITY_DATA_SAVER_ID: usize = 4101;
const QUALITY_BALANCED_ID: usize = 4102;
const QUALITY_BEST_ID: usize = 4103;
const CHROMA_420_ID: usize = 4111;
const CHROMA_444_ID: usize = 4112;
const SETTINGS_DISPLAY_TAB_ID: usize = 4201;
const SETTINGS_ADVANCED_TAB_ID: usize = 4202;
const SETTINGS_KEYBOARD_TAB_ID: usize = 4203;
const SETTINGS_KEYBOARD_TITLE_ID: i32 = 4241;
const SETTINGS_WINDOWS_SHORTCUTS_ID: usize = 4242;
const SETTINGS_DIAGNOSTICS_KEY_TITLE_ID: i32 = 4243;
const SETTINGS_DIAGNOSTICS_KEY_ID: usize = 4244;
const SETTINGS_DISPLAY_KEY_TITLE_ID: i32 = 4245;
const SETTINGS_DISPLAY_KEY_ID: usize = 4246;
const SETTINGS_KEYBOARD_NOTE_ID: i32 = 4247;
const SETTINGS_DISPLAY_TITLE_ID: i32 = 4211;
const SETTINGS_QUALITY_TITLE_ID: i32 = 4212;
const SETTINGS_CHROMA_TITLE_ID: i32 = 4213;
const SETTINGS_ADVANCED_TITLE_ID: i32 = 4221;
pub(super) const SETTINGS_DIAGNOSTICS_ID: usize = 4222;
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

/// Settings controls are created at 96 DPI; the caller scales them.
pub(super) struct SettingsControls {
    pub(super) window: HWND,
    pub(super) dpi: u32,
    pub(super) quality_buttons: [(HWND, QualityPreset); 4],
    pub(super) chroma_buttons: [(HWND, ChromaMode); 2],
}

impl WindowContext {
    pub(super) fn refresh_maintenance_controls(&self) {
        self.refresh_shortcut_keys();
        let controls = self.controls();
        let state = self.control.credential_state();
        for (i, button) in controls.credential_buttons.iter().enumerate() {
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
        let _ = unsafe { SetWindowTextW(controls.credential_label, PCWSTR(text.as_ptr())) };
        let close_action = self.control.session_close_action();
        let recording = self.control.recording().active();
        if self.recording_visible.replace(recording) != recording {
            unsafe {
                let _ = SetWindowTextW(
                    controls.settings_button,
                    if recording { w!("REC") } else { w!("⚙") },
                );
                if let Ok(button) =
                    GetDlgItem(Some(controls.settings_window), SETTINGS_RECORDING_ID as i32)
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
                SETTINGS_WINDOWS_SHORTCUTS_ID,
                self.control.send_windows_shortcuts(),
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
            if let Ok(button) = unsafe { GetDlgItem(Some(controls.settings_window), id as i32) } {
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

    fn refresh_shortcut_keys(&self) {
        let settings_window = self.controls().settings_window;
        for (id, shortcut) in [
            (SETTINGS_DIAGNOSTICS_KEY_ID, ViewerShortcut::Diagnostics),
            (SETTINGS_DISPLAY_KEY_ID, ViewerShortcut::NextDisplay),
        ] {
            let key = self.control.shortcut_key(shortcut);
            let index = ShortcutKey::ALL.iter().position(|choice| *choice == key);
            if let (Ok(combo), Some(index)) = (
                unsafe { GetDlgItem(Some(settings_window), id as i32) },
                index,
            ) {
                unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None) };
            }
        }
    }

    pub(super) fn show_settings(&self) {
        self.refresh_maintenance_controls();
        let settings_window = self.controls().settings_window;
        let _ = unsafe { ShowWindow(settings_window, SW_SHOW) };
        let _ = unsafe { SetForegroundWindow(settings_window) };
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsCategory {
    Display,
    Advanced,
    Keyboard,
}

unsafe fn show_settings_category(window: HWND, category: SettingsCategory) {
    let display: &[i32] = &[
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
    ];
    let advanced: Vec<i32> = [
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
    .collect();
    let keyboard: &[i32] = &[
        SETTINGS_KEYBOARD_TITLE_ID,
        SETTINGS_WINDOWS_SHORTCUTS_ID as i32,
        SETTINGS_DIAGNOSTICS_KEY_TITLE_ID,
        SETTINGS_DIAGNOSTICS_KEY_ID as i32,
        SETTINGS_DISPLAY_KEY_TITLE_ID,
        SETTINGS_DISPLAY_KEY_ID as i32,
        SETTINGS_KEYBOARD_NOTE_ID,
    ];
    for (ids, shown) in [
        (display, category == SettingsCategory::Display),
        (advanced.as_slice(), category == SettingsCategory::Advanced),
        (keyboard, category == SettingsCategory::Keyboard),
    ] {
        for id in ids {
            if let Ok(control) = unsafe { GetDlgItem(Some(window), *id) } {
                let _ = unsafe { ShowWindow(control, if shown { SW_SHOW } else { SW_HIDE }) };
            }
        }
    }
    for (id, selected) in [
        (
            SETTINGS_DISPLAY_TAB_ID,
            category == SettingsCategory::Display,
        ),
        (
            SETTINGS_ADVANCED_TAB_ID,
            category == SettingsCategory::Advanced,
        ),
        (
            SETTINGS_KEYBOARD_TAB_ID,
            category == SettingsCategory::Keyboard,
        ),
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
            let notification = (wparam.0 >> 16) & 0xffff;
            for (tab, category) in [
                (SETTINGS_DISPLAY_TAB_ID, SettingsCategory::Display),
                (SETTINGS_ADVANCED_TAB_ID, SettingsCategory::Advanced),
                (SETTINGS_KEYBOARD_TAB_ID, SettingsCategory::Keyboard),
            ] {
                if control_id == tab {
                    unsafe { show_settings_category(window, category) };
                    return LRESULT(0);
                }
            }
            if let Some(context) = unsafe { window_context(owner) }
                && context.settings_command(owner, control_id, notification, lparam)
            {
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(window, message, wparam, lparam) }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe { messages::dark_control_colors(wparam) },
        WM_CLOSE => {
            let _ = unsafe { ShowWindow(window, SW_HIDE) };
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let dpi = (wparam.0 & 0xffff) as u32;
            if let Some(context) = unsafe { window_context(owner) } {
                let font = unsafe { message_font(dpi) };
                unsafe { rescale_children(window, context.settings_dpi.get(), dpi, font) };
                let old = context.settings_font.replace(font);
                if !old.is_invalid() {
                    let _ = unsafe { DeleteObject(HGDIOBJ(old.0)) };
                }
                context.settings_dpi.set(dpi);
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

impl WindowContext {
    /// Handles WM_COMMAND from a settings control. `owner` is the viewer
    /// window. Returns whether the command was handled.
    fn settings_command(
        &self,
        owner: HWND,
        control_id: usize,
        notification: usize,
        lparam: LPARAM,
    ) -> bool {
        let preset = match control_id {
            QUALITY_ULTRA_DATA_SAVER_ID => Some(QualityPreset::UltraDataSaver),
            QUALITY_DATA_SAVER_ID => Some(QualityPreset::DataSaver),
            QUALITY_BALANCED_ID => Some(QualityPreset::Balanced),
            QUALITY_BEST_ID => Some(QualityPreset::BestQuality),
            _ => None,
        };
        if let Some(preset) = preset {
            self.set_quality(preset);
            return true;
        }
        let chroma = match control_id {
            CHROMA_420_ID => Some(ChromaMode::Yuv420),
            CHROMA_444_ID => Some(ChromaMode::Yuv444),
            _ => None,
        };
        if let Some(chroma) = chroma {
            self.set_chroma(chroma);
            return true;
        }
        let toggle: Option<fn(&ControlSink)> = match control_id {
            SETTINGS_DISCONNECT_ID => Some(ControlSink::toggle_disconnect_confirmation),
            SETTINGS_IDLE_ID => Some(ControlSink::toggle_prevent_idle_lock),
            SETTINGS_DISPLAY_BORDER_ID => Some(ControlSink::toggle_display_border),
            SETTINGS_WALLPAPER_ID => Some(ControlSink::toggle_wallpaper),
            SETTINGS_REMOTE_CURSOR_ID => Some(ControlSink::toggle_remote_cursor),
            SETTINGS_RECORDING_ID => Some(ControlSink::toggle_recording),
            SETTINGS_AUDIO_ID => Some(ControlSink::toggle_audio),
            SETTINGS_CLIPBOARD_ID => Some(ControlSink::toggle_clipboard_sync),
            SETTINGS_CLEAR_CLIPBOARD_ID => Some(ControlSink::toggle_clear_clipboard_on_close),
            SETTINGS_WINDOWS_SHORTCUTS_ID => Some(ControlSink::toggle_send_windows_shortcuts),
            _ => None,
        };
        if let Some(toggle) = toggle {
            toggle(&self.control);
            self.refresh_maintenance_controls();
            return true;
        }
        if let Some((_, action)) = SETTINGS_CLOSE_ACTION_IDS
            .iter()
            .find(|(id, _)| *id == control_id)
        {
            self.control.set_session_close_action(*action);
            self.refresh_maintenance_controls();
            return true;
        }
        if control_id == SETTINGS_BLACKOUT_ID {
            self.release_input();
            self.control.toggle_blackout();
            return true;
        }
        if control_id == SETTINGS_AGENT_INPUT_ID {
            self.release_input();
            self.control.toggle_agent_input();
            return true;
        }
        if control_id == SETTINGS_TECHNICIAN_INPUT_ID {
            self.release_input();
            self.control
                .set_technician_blocked(!self.control.technician_blocked());
            unsafe { apply_cursor(self.control.effective_cursor_shape(self.cursor_shape.get())) };
            return true;
        }
        if control_id == SETTINGS_DIAGNOSTICS_ID {
            self.toggle_debug();
            return true;
        }
        let shortcut = match control_id {
            SETTINGS_DIAGNOSTICS_KEY_ID => Some(ViewerShortcut::Diagnostics),
            SETTINGS_DISPLAY_KEY_ID => Some(ViewerShortcut::NextDisplay),
            _ => None,
        };
        if let Some(shortcut) = shortcut
            && notification == CBN_SELCHANGE as usize
        {
            let selected =
                unsafe { SendMessageW(HWND(lparam.0 as *mut c_void), CB_GETCURSEL, None, None).0 };
            if let Some(key) = usize::try_from(selected)
                .ok()
                .and_then(|index| ShortcutKey::ALL.get(index))
            {
                self.control.set_shortcut_key(shortcut, *key);
                let title = window_title(&self.active_display);
                self.title.replace(title.clone());
                let _ = unsafe { SetWindowTextW(owner, PCWSTR(title.as_ptr())) };
            }
            self.refresh_maintenance_controls();
            return true;
        }
        false
    }
}

pub(super) unsafe fn create_settings_window(
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
    let _ = make_control(
        w!("BUTTON"),
        w!("Keyboard"),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32),
        16,
        100,
        118,
        34,
        SETTINGS_KEYBOARD_TAB_ID,
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
    let _ = make_control(
        w!("STATIC"),
        w!("Keyboard"),
        static_style,
        162,
        24,
        340,
        28,
        SETTINGS_KEYBOARD_TITLE_ID as usize,
    )?;
    let _ = make_control(
        w!("BUTTON"),
        w!("Send Windows shortcuts to the device (Windows key, Alt+Tab, Alt+Esc, Ctrl+Esc)"),
        WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32,
        ),
        162,
        66,
        370,
        48,
        SETTINGS_WINDOWS_SHORTCUTS_ID,
    )?;
    for (title_id, title, combo_id, y) in [
        (
            SETTINGS_DIAGNOSTICS_KEY_TITLE_ID,
            w!("Diagnostics overlay key"),
            SETTINGS_DIAGNOSTICS_KEY_ID,
            130,
        ),
        (
            SETTINGS_DISPLAY_KEY_TITLE_ID,
            w!("Next display key"),
            SETTINGS_DISPLAY_KEY_ID,
            204,
        ),
    ] {
        let _ = make_control(
            w!("STATIC"),
            title,
            static_style,
            162,
            y,
            340,
            24,
            title_id as usize,
        )?;
        let combo = make_control(
            w!("COMBOBOX"),
            w!(""),
            WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
            ),
            162,
            y + 28,
            280,
            240,
            combo_id,
        )?;
        for key in ShortcutKey::ALL {
            let label = HSTRING::from(key.label());
            unsafe {
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(label.as_ptr() as isize)),
                );
            }
        }
    }
    let _ = make_control(
        w!("STATIC"),
        w!(
            "A key that is off goes to the device. Ctrl+Alt+Del and Windows+L always stay with this computer."
        ),
        static_style,
        162,
        280,
        370,
        48,
        SETTINGS_KEYBOARD_NOTE_ID as usize,
    )?;
    unsafe { show_settings_category(settings, SettingsCategory::Display) };
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

struct Rescale {
    parent: HWND,
    from: u32,
    to: u32,
    font: HFONT,
}

/// Moves and resizes a window's direct children from one DPI to another and
/// gives them `font`.
pub(super) unsafe fn rescale_children(parent: HWND, from: u32, to: u32, font: HFONT) {
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
