//! The toolbar above the video: user, display, quality and color selectors,
//! session actions, and the caption buttons of the borderless window.

use super::*;

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

impl WindowContext {
    pub(super) fn toolbar_controls(&self) -> [HWND; 18] {
        let controls = self.controls();
        [
            controls.user_combo,
            controls.display_combo,
            controls.quality_combo,
            controls.chroma_combo,
            controls.diagnostics_button,
            controls.settings_button,
            controls.file_button,
            controls.chat_button,
            controls.secure_attention_button,
            controls.type_clipboard_button,
            controls.credential_buttons[0],
            controls.credential_buttons[1],
            controls.credential_buttons[2],
            controls.credential_label,
            controls.minimize_button,
            controls.maximize_button,
            controls.close_button,
            controls.debug_overlay,
        ]
    }

    pub(super) fn set_quality(&self, preset: QualityPreset) {
        let controls = self.controls();
        let selected = quality_index(preset);
        unsafe {
            SendMessageW(
                controls.quality_combo,
                CB_SETCURSEL,
                Some(WPARAM(selected)),
                None,
            )
        };
        for (button, candidate) in controls.quality_buttons {
            let state = usize::from(candidate == preset);
            unsafe { SendMessageW(button, BM_SETCHECK, Some(WPARAM(state)), None) };
        }
        self.send(SessionMessage::SetQuality { preset });
    }

    pub(super) fn set_chroma(&self, mode: ChromaMode) {
        let controls = self.controls();
        if !self.control.supports_chroma(mode) {
            unsafe {
                SendMessageW(
                    controls.chroma_combo,
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
                controls.chroma_combo,
                CB_SETCURSEL,
                Some(WPARAM(selected)),
                None,
            )
        };
        for (button, candidate) in controls.chroma_buttons {
            let state = usize::from(candidate == mode);
            unsafe { SendMessageW(button, BM_SETCHECK, Some(WPARAM(state)), None) };
        }
        self.send(SessionMessage::SetChroma { mode });
    }

    pub(super) fn layout_toolbar(&self, window: HWND) {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(window, &mut rect) }.is_err() {
            return;
        }
        let controls = self.controls();
        let dpi = self.dpi.get();
        let width = rect.right.saturating_sub(rect.left);
        let px = |value| self.px(value);
        let place = |control: HWND, x: i32, y: i32, width: i32, height: i32| {
            let _ = unsafe { MoveWindow(control, x, y, width, height, true) };
        };
        place(controls.toolbar, 0, 0, width, toolbar_height(dpi));
        place(controls.user_combo, px(8), px(5), px(158), px(300));
        place(controls.display_combo, px(172), px(5), px(110), px(300));
        place(controls.quality_combo, px(288), px(5), px(154), px(300));
        place(controls.chroma_combo, px(448), px(5), px(124), px(300));
        for (i, button) in controls.credential_buttons.iter().enumerate() {
            place(*button, px(8 + i as i32 * 184), px(38), px(180), px(24));
        }
        place(
            controls.credential_label,
            px(566),
            px(42),
            (width - px(574)).max(1),
            px(20),
        );
        let caption_x = width.saturating_sub(px(138));
        place(controls.minimize_button, caption_x, 0, px(46), px(34));
        place(
            controls.maximize_button,
            caption_x + px(46),
            0,
            px(46),
            px(34),
        );
        place(controls.close_button, caption_x + px(92), 0, px(46), px(34));
        place(
            controls.diagnostics_button,
            caption_x.saturating_sub(px(78)),
            px(5),
            px(34),
            px(24),
        );
        place(
            controls.settings_button,
            caption_x.saturating_sub(px(40)),
            px(5),
            px(34),
            px(24),
        );
        place(
            controls.chat_button,
            caption_x.saturating_sub(px(138)),
            px(5),
            px(54),
            px(24),
        );
        place(
            controls.file_button,
            caption_x.saturating_sub(px(180)),
            px(5),
            px(38),
            px(24),
        );
        place(
            controls.secure_attention_button,
            caption_x.saturating_sub(px(296)),
            px(5),
            px(110),
            px(24),
        );
        place(
            controls.type_clipboard_button,
            caption_x.saturating_sub(px(422)),
            px(5),
            px(120),
            px(24),
        );
        place(
            controls.video_window,
            0,
            toolbar_height(dpi),
            width,
            (rect.bottom - rect.top - toolbar_height(dpi)).max(0),
        );
        self.place_popups(window);
        let maximize_title = if unsafe { IsZoomed(window) }.as_bool() {
            w!("❐")
        } else {
            w!("□")
        };
        let _ = unsafe { SetWindowTextW(controls.maximize_button, maximize_title) };
    }

    /// Keeps the owned popups over the video when the window moves or resizes.
    pub(super) fn place_popups(&self, window: HWND) {
        let controls = self.controls();
        let mut origin = windows::Win32::Foundation::POINT {
            x: self.px(12),
            y: toolbar_height(self.dpi.get()) + self.px(12),
        };
        if unsafe { ClientToScreen(window, &mut origin) }.as_bool() {
            let _ = unsafe {
                SetWindowPos(
                    controls.debug_overlay,
                    None,
                    origin.x,
                    origin.y,
                    self.px(640),
                    self.px(300),
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        let (width, height) = unsafe { client_size(window) }.unwrap_or_default();
        let video = self.video_rect_for(width, height);
        let (label_width, label_height) = (self.px(360), self.px(48));
        let mut center = windows::Win32::Foundation::POINT {
            x: video.left + (video.width - label_width) / 2,
            y: video.top + (video.height - label_height) / 2,
        };
        if unsafe { ClientToScreen(window, &mut center) }.as_bool() {
            let _ = unsafe {
                SetWindowPos(
                    controls.reconnecting_label,
                    None,
                    center.x,
                    center.y,
                    label_width,
                    label_height,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        if let Some(chat) = self.chat_popup.get() {
            chat.layout();
        }
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
            SendMessageW(
                self.controls().user_combo,
                CB_SETCURSEL,
                Some(WPARAM(current)),
                None,
            );
        }
    }

    pub(super) fn select_next_display(&self) {
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

    /// Handles WM_COMMAND from a toolbar control. Returns whether it did.
    pub(super) fn command(&self, window: HWND, wparam: WPARAM) -> bool {
        let controls = self.controls();
        let control_id = wparam.0 & 0xffff;
        let notification = (wparam.0 >> 16) & 0xffff;
        if control_id == USER_COMBO_ID && notification == CBN_SELCHANGE as usize {
            let selected = unsafe { SendMessageW(controls.user_combo, CB_GETCURSEL, None, None).0 };
            if selected >= 0 {
                self.release_input();
                self.select_user(selected as usize);
            }
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if control_id == DISPLAY_COMBO_ID && notification == CBN_SELCHANGE as usize {
            let selected =
                unsafe { SendMessageW(controls.display_combo, CB_GETCURSEL, None, None).0 };
            if selected >= 0 {
                self.select_display(selected as usize);
            }
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if control_id == QUALITY_COMBO_ID && notification == CBN_SELCHANGE as usize {
            let selected =
                unsafe { SendMessageW(controls.quality_combo, CB_GETCURSEL, None, None).0 };
            let preset = match selected {
                0 => QualityPreset::UltraDataSaver,
                1 => QualityPreset::DataSaver,
                3 => QualityPreset::BestQuality,
                _ => QualityPreset::Balanced,
            };
            self.set_quality(preset);
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if control_id == CHROMA_COMBO_ID && notification == CBN_SELCHANGE as usize {
            let selected =
                unsafe { SendMessageW(controls.chroma_combo, CB_GETCURSEL, None, None).0 };
            let mode = if selected == 1 {
                ChromaMode::Yuv444
            } else {
                ChromaMode::Yuv420
            };
            self.set_chroma(mode);
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if control_id == DIAGNOSTICS_BUTTON_ID {
            self.toggle_debug();
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if (CREDENTIAL_BUTTON_ID..CREDENTIAL_BUTTON_ID + 3).contains(&control_id) {
            self.release_input();
            self.control.send(match control_id - CREDENTIAL_BUTTON_ID {
                0 => SessionMessage::PromptForCredentials,
                1 => SessionMessage::AutofillCredentials,
                _ => SessionMessage::ForgetCredentials,
            });
            return true;
        }
        if control_id == TYPE_CLIPBOARD_BUTTON_ID {
            self.release_input();
            self.control.type_clipboard(self.active_display.id);
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if control_id == SECURE_ATTENTION_BUTTON_ID {
            self.release_input();
            self.control.send_secure_attention();
            let _ = unsafe { SetFocus(Some(window)) };
            return true;
        }
        if control_id == FILE_BUTTON_ID {
            self.release_input();
            self.control.set_input_enabled(false);
            unsafe {
                if let Ok(menu) = CreatePopupMenu() {
                    let _ = AppendMenuW(menu, MF_STRING, 1, w!("Send"));
                    let _ = AppendMenuW(menu, MF_STRING, 2, w!("Receive"));
                    let status = self.control.files().status();
                    if !status.is_empty() {
                        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                        let _ =
                            AppendMenuW(menu, MF_STRING | MF_DISABLED, 3, &HSTRING::from(status));
                    }
                    let mut point = windows::Win32::Foundation::POINT::default();
                    let _ = GetCursorPos(&mut point);
                    let chosen =
                        TrackPopupMenu(menu, TPM_RETURNCMD, point.x, point.y, None, window, None).0;
                    let _ = DestroyMenu(menu);
                    if chosen == 1 {
                        self.control.files().pick();
                    }
                    if chosen == 2 {
                        self.control.files().request_peer_pick();
                    }
                }
            }
            self.control.set_input_enabled(true);
            return true;
        }
        if control_id == CHAT_BUTTON_ID {
            self.release_input();
            self.control.set_input_enabled(false);
            if let Some(chat) = self.chat_popup.get() {
                chat.toggle();
            }
            return true;
        }
        if control_id == SETTINGS_BUTTON_ID {
            self.show_settings();
            return true;
        }
        if control_id == MINIMIZE_BUTTON_ID {
            let _ = unsafe { ShowWindow(window, SW_MINIMIZE) };
            return true;
        }
        if control_id == MAXIMIZE_BUTTON_ID {
            let command = if unsafe { IsZoomed(window) }.as_bool() {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            };
            let _ = unsafe { ShowWindow(window, command) };
            self.layout_toolbar(window);
            return true;
        }
        if control_id == CLOSE_BUTTON_ID {
            unsafe { SendMessageW(window, WM_CLOSE, None, None) };
            return true;
        }
        false
    }
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

/// Creates the toolbar and the owned popups over the video. Returns the
/// window's controls with the new ones filled in.
pub(super) unsafe fn create_toolbar(
    window: HWND,
    instance: HINSTANCE,
    context: &WindowContext,
    font: HFONT,
) -> anyhow::Result<Controls> {
    let overlay = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!(""),
            WS_POPUP | WS_BORDER,
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
    let reconnecting_label = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!("Reconnecting to the remote computer…"),
            WINDOW_STYLE(WS_POPUP.0 | WS_BORDER.0 | STATIC_CENTER | STATIC_CENTER_VERTICALLY),
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
    .context("reconnecting label creation failed")?;
    let toolbar = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_CLIPSIBLINGS,
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
        reconnecting_label,
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
    Ok(Controls {
        debug_overlay: overlay,
        reconnecting_label,
        toolbar,
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
        credential_buttons,
        credential_label,
        minimize_button,
        maximize_button,
        close_button,
        ..context.controls()
    })
}

// Only monitor/ownership transitions reach this path, never individual mouse moves.
pub(in crate::platform::windows) unsafe fn set_agent_pointer_display(
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
