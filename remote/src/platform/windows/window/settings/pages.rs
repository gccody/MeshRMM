use super::*;

const RADIO_STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTORADIOBUTTON as u32);
const CHECKBOX_STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32);
const COMBO_STYLE: WINDOW_STYLE =
    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32);
/// Page controls start right of the category buttons.
const PAGE_X: i32 = 162;

const fn grouped(style: WINDOW_STYLE) -> WINDOW_STYLE {
    WINDOW_STYLE(style.0 | WS_GROUP.0)
}

/// Creates the settings window's controls at 96 DPI. Creation order is the
/// tab order.
pub(super) struct SettingsPageBuilder {
    pub(super) window: HWND,
    pub(super) instance: HINSTANCE,
}

impl SettingsPageBuilder {
    fn control(
        &self,
        class_name: PCWSTR,
        text: PCWSTR,
        style: WINDOW_STYLE,
        (x, y, width, height): (i32, i32, i32, i32),
        id: usize,
    ) -> anyhow::Result<HWND> {
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
                Some(self.window),
                Some(HMENU(id as *mut c_void)),
                Some(self.instance),
                None,
            )
        }
        .context("viewer settings control creation failed")
    }

    fn label(&self, text: PCWSTR, y: i32, width: i32, height: i32, id: i32) -> anyhow::Result<()> {
        self.control(
            w!("STATIC"),
            text,
            WS_CHILD | WS_VISIBLE,
            (PAGE_X, y, width, height),
            id as usize,
        )?;
        Ok(())
    }

    fn checkbox(&self, text: PCWSTR, y: i32, id: usize) -> anyhow::Result<()> {
        self.control(w!("BUTTON"), text, CHECKBOX_STYLE, (PAGE_X, y, 340, 28), id)?;
        Ok(())
    }

    fn combo(
        &self,
        y: i32,
        id: usize,
        labels: impl Iterator<Item = HSTRING>,
    ) -> anyhow::Result<()> {
        let combo = self.control(
            w!("COMBOBOX"),
            w!(""),
            COMBO_STYLE,
            (PAGE_X, y, 280, 240),
            id,
        )?;
        for label in labels {
            unsafe {
                SendMessageW(
                    combo,
                    CB_ADDSTRING,
                    None,
                    Some(LPARAM(label.as_ptr() as isize)),
                );
            }
        }
        Ok(())
    }

    pub(super) fn create_category_tabs(&self) -> anyhow::Result<()> {
        for (text, style, y, id) in [
            (
                w!("Display"),
                grouped(RADIO_STYLE),
                20,
                SETTINGS_DISPLAY_TAB_ID,
            ),
            (w!("Advanced"), RADIO_STYLE, 60, SETTINGS_ADVANCED_TAB_ID),
            (w!("Keyboard"), RADIO_STYLE, 100, SETTINGS_KEYBOARD_TAB_ID),
        ] {
            self.control(w!("BUTTON"), text, style, (16, y, 118, 34), id)?;
        }
        Ok(())
    }

    fn radio(
        &self,
        text: PCWSTR,
        style: WINDOW_STYLE,
        y: i32,
        width: i32,
        id: usize,
    ) -> anyhow::Result<HWND> {
        self.control(w!("BUTTON"), text, style, (PAGE_X, y, width, 28), id)
    }

    /// The display page's quality choices, which come first on the page.
    pub(super) fn create_quality_choices(&self) -> anyhow::Result<[(HWND, QualityPreset); 4]> {
        self.label(w!("Image quality"), 24, 340, 28, SETTINGS_DISPLAY_TITLE_ID)?;
        self.label(
            w!("Choose the bandwidth used by the remote desktop."),
            58,
            350,
            24,
            SETTINGS_QUALITY_TITLE_ID,
        )?;
        Ok([
            (
                self.radio(
                    w!("Ultra data saver · 1 Mbps · grayscale · 24 FPS"),
                    grouped(RADIO_STYLE),
                    104,
                    370,
                    QUALITY_ULTRA_DATA_SAVER_ID,
                )?,
                QualityPreset::UltraDataSaver,
            ),
            (
                self.radio(
                    w!("Data saver · 3 Mbps"),
                    RADIO_STYLE,
                    144,
                    340,
                    QUALITY_DATA_SAVER_ID,
                )?,
                QualityPreset::DataSaver,
            ),
            (
                self.radio(
                    w!("Balanced · 6 Mbps"),
                    RADIO_STYLE,
                    184,
                    340,
                    QUALITY_BALANCED_ID,
                )?,
                QualityPreset::Balanced,
            ),
            (
                self.radio(
                    w!("Best quality · 12 Mbps maximum"),
                    RADIO_STYLE,
                    224,
                    340,
                    QUALITY_BEST_ID,
                )?,
                QualityPreset::BestQuality,
            ),
        ])
    }

    pub(super) fn create_chroma_choices(&self) -> anyhow::Result<[(HWND, ChromaMode); 2]> {
        self.label(w!("Color detail"), 264, 340, 24, SETTINGS_CHROMA_TITLE_ID)?;
        Ok([
            (
                self.radio(
                    w!("4:2:0 · bandwidth efficient"),
                    grouped(RADIO_STYLE),
                    294,
                    340,
                    CHROMA_420_ID,
                )?,
                ChromaMode::Yuv420,
            ),
            (
                self.radio(
                    w!("4:4:4 · crisp text and color"),
                    RADIO_STYLE,
                    334,
                    340,
                    CHROMA_444_ID,
                )?,
                ChromaMode::Yuv444,
            ),
        ])
    }

    pub(super) fn create_advanced_page(&self) -> anyhow::Result<()> {
        self.label(
            w!("Troubleshooting"),
            24,
            340,
            28,
            SETTINGS_ADVANCED_TITLE_ID,
        )?;
        for (text, y, id) in [
            (w!("Show diagnostics overlay"), 70, SETTINGS_DIAGNOSTICS_ID),
            (
                w!("Block technician input"),
                110,
                SETTINGS_TECHNICIAN_INPUT_ID,
            ),
            (
                w!("Block agent keyboard and mouse"),
                150,
                SETTINGS_AGENT_INPUT_ID,
            ),
            (
                w!("Black out all agent monitors"),
                190,
                SETTINGS_BLACKOUT_ID,
            ),
            (w!("Mute audio"), 230, SETTINGS_AUDIO_ID),
        ] {
            self.checkbox(text, y, id)?;
        }
        self.control(
            w!("BUTTON"),
            w!("Show remote cursor"),
            grouped(CHECKBOX_STYLE),
            (PAGE_X, 374, 340, 28),
            SETTINGS_REMOTE_CURSOR_ID,
        )?;
        self.control(
            w!("BUTTON"),
            w!("Record video to Downloads"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            (PAGE_X, 270, 340, 28),
            SETTINGS_RECORDING_ID,
        )?;
        self.checkbox(w!("Sync clipboard"), 310, SETTINGS_CLIPBOARD_ID)?;
        self.checkbox(
            w!("Clear clipboard on session close"),
            350,
            SETTINGS_CLEAR_CLIPBOARD_ID,
        )?;
        self.create_session_end_controls()?;
        for (text, y, id) in [
            (w!("Hide remote wallpaper"), 414, SETTINGS_WALLPAPER_ID),
            (
                w!("Highlight viewed monitor on agent"),
                450,
                SETTINGS_DISPLAY_BORDER_ID,
            ),
            (w!("Prevent idle lock"), 486, SETTINGS_IDLE_ID),
            (w!("Disconnect confirmation"), 522, SETTINGS_DISCONNECT_ID),
        ] {
            self.checkbox(text, y, id)?;
        }
        self.label(
            w!("Display size when the computer has no monitor"),
            562,
            370,
            24,
            SETTINGS_HEADLESS_TITLE_ID,
        )?;
        self.combo(
            590,
            SETTINGS_HEADLESS_ID,
            HeadlessResolution::PRESETS
                .iter()
                .map(|resolution| HSTRING::from(resolution.label())),
        )
    }

    /// The close action radios and the idle disconnect choice.
    fn create_session_end_controls(&self) -> anyhow::Result<()> {
        self.label(
            w!("On session close"),
            398,
            340,
            24,
            SETTINGS_CLOSE_TITLE_ID,
        )?;
        for (index, (id, action)) in SETTINGS_CLOSE_ACTION_IDS.into_iter().enumerate() {
            let label = HSTRING::from(action.label());
            self.control(
                w!("BUTTON"),
                PCWSTR(label.as_ptr()),
                if index == 0 {
                    grouped(RADIO_STYLE)
                } else {
                    RADIO_STYLE
                },
                (PAGE_X, 428 + 36 * index as i32, 340, 28),
                id,
            )?;
        }
        self.label(
            w!("Disconnect when idle"),
            542,
            340,
            24,
            SETTINGS_IDLE_DISCONNECT_TITLE_ID,
        )?;
        self.combo(
            570,
            SETTINGS_IDLE_DISCONNECT_ID,
            crate::idle_disconnect::choices()
                .map(|choice| HSTRING::from(crate::idle_disconnect::label(choice))),
        )
    }

    pub(super) fn create_keyboard_page(&self) -> anyhow::Result<()> {
        self.label(w!("Keyboard"), 24, 340, 28, SETTINGS_KEYBOARD_TITLE_ID)?;
        self.control(
            w!("BUTTON"),
            w!("Send Windows shortcuts to the device (Windows key, Alt+Tab, Alt+Esc, Ctrl+Esc)"),
            WINDOW_STYLE(CHECKBOX_STYLE.0 | BS_MULTILINE as u32),
            (PAGE_X, 66, 370, 48),
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
            self.label(title, y, 340, 24, title_id)?;
            self.combo(
                y + 28,
                combo_id,
                ShortcutKey::ALL
                    .iter()
                    .map(|key| HSTRING::from(key.label())),
            )?;
        }
        self.label(
            w!(
                "A key that is off goes to the device. Ctrl+Alt+Del and Windows+L always stay with this computer."
            ),
            280,
            370,
            48,
            SETTINGS_KEYBOARD_NOTE_ID,
        )
    }
}
