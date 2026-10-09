use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsCategory {
    Display,
    Advanced,
    Keyboard,
}

pub(super) unsafe fn show_settings_category(window: HWND, category: SettingsCategory) {
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
        SETTINGS_HEADLESS_TITLE_ID,
        SETTINGS_HEADLESS_ID as i32,
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
        SETTINGS_IDLE_DISCONNECT_TITLE_ID,
        SETTINGS_IDLE_DISCONNECT_ID as i32,
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

pub(super) unsafe extern "system" fn settings_window_proc(
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
            unsafe { apply_cursor(self.video_cursor()) };
            return true;
        }
        if control_id == SETTINGS_DIAGNOSTICS_ID {
            self.toggle_debug();
            return true;
        }
        if control_id == SETTINGS_HEADLESS_ID {
            if notification == CBN_SELCHANGE as usize {
                let selected = unsafe {
                    SendMessageW(HWND(lparam.0 as *mut c_void), CB_GETCURSEL, None, None).0
                };
                if let Some(resolution) = usize::try_from(selected)
                    .ok()
                    .and_then(|index| HeadlessResolution::PRESETS.get(index))
                {
                    self.control.set_headless_resolution(*resolution);
                }
                self.refresh_maintenance_controls();
            }
            return true;
        }
        if control_id == SETTINGS_IDLE_DISCONNECT_ID {
            if notification == CBN_SELCHANGE as usize {
                let selected = unsafe {
                    SendMessageW(HWND(lparam.0 as *mut c_void), CB_GETCURSEL, None, None).0
                };
                if let Some(minutes) = usize::try_from(selected)
                    .ok()
                    .and_then(|index| crate::idle_disconnect::choices().nth(index))
                {
                    self.control.set_idle_disconnect_minutes(minutes);
                }
                self.refresh_maintenance_controls();
            }
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
                let title = window_title(&self.active_display());
                self.title.replace(title.clone());
                let _ = unsafe { SetWindowTextW(owner, PCWSTR(title.as_ptr())) };
            }
            self.refresh_maintenance_controls();
            return true;
        }
        false
    }
}
