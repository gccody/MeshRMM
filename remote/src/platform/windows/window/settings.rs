//! The viewer settings window: display, advanced and keyboard pages, and the
//! maintenance state they and the toolbar show.

mod window_proc;

use super::*;
use meshrmm_protocol::HeadlessResolution;
use window_proc::{SettingsCategory, settings_window_proc, show_settings_category};

/// The settings window's outer size, in 96-DPI pixels.
pub(super) const SETTINGS_WINDOW_WIDTH: i32 = 560;
pub(super) const SETTINGS_WINDOW_HEIGHT: i32 = 690;

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
const SETTINGS_HEADLESS_TITLE_ID: i32 = 4248;
const SETTINGS_HEADLESS_ID: usize = 4249;
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
const SETTINGS_IDLE_DISCONNECT_TITLE_ID: i32 = 4239;
const SETTINGS_IDLE_DISCONNECT_ID: usize = 4240;
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
        self.refresh_idle_disconnect();
        self.refresh_headless_resolution();
        self.refresh_toolbar();
        let controls = self.controls();
        let close_action = self.control.session_close_action();
        let recording = self.control.recording().active();
        if self.recording_visible.replace(recording) != recording {
            unsafe {
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
                self.control.allow_clear_clipboard_override(),
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
                    if id == SETTINGS_CLEAR_CLIPBOARD_ID {
                        let _ = SetWindowTextW(
                            button,
                            if enabled {
                                w!("Clear clipboard on session close")
                            } else {
                                w!("Clear clipboard on close (company managed)")
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

    fn refresh_idle_disconnect(&self) {
        let settings_window = self.controls().settings_window;
        let allowed = self.control.allow_idle_disconnect_override();
        let minutes = self.control.idle_disconnect_minutes();
        unsafe {
            if let Ok(title) = GetDlgItem(Some(settings_window), SETTINGS_IDLE_DISCONNECT_TITLE_ID)
            {
                let _ = SetWindowTextW(
                    title,
                    if allowed {
                        w!("Disconnect when idle")
                    } else {
                        w!("Disconnect when idle (company managed)")
                    },
                );
            }
            if let Ok(combo) = GetDlgItem(Some(settings_window), SETTINGS_IDLE_DISCONNECT_ID as i32)
            {
                // A time outside the offered choices shows no selection.
                let index = crate::idle_disconnect::choices()
                    .position(|choice| choice == minutes)
                    .unwrap_or(usize::MAX);
                SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None);
                let _ = EnableWindow(combo, allowed);
            }
        }
    }

    fn refresh_headless_resolution(&self) {
        let resolution = crate::preferences::headless_resolution();
        // A size outside the offered choices shows no selection.
        let index = HeadlessResolution::PRESETS
            .iter()
            .position(|choice| *choice == resolution)
            .unwrap_or(usize::MAX);
        if let Ok(combo) = unsafe {
            GetDlgItem(
                Some(self.controls().settings_window),
                SETTINGS_HEADLESS_ID as i32,
            )
        } {
            unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None) };
        }
    }

    pub(super) fn show_settings(&self) {
        self.refresh_maintenance_controls();
        let settings_window = self.controls().settings_window;
        let _ = unsafe { ShowWindow(settings_window, SW_SHOW) };
        let _ = unsafe { SetForegroundWindow(settings_window) };
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
        w!("STATIC"),
        w!("Disconnect when idle"),
        static_style,
        162,
        542,
        340,
        24,
        SETTINGS_IDLE_DISCONNECT_TITLE_ID as usize,
    )?;
    let idle_disconnect = make_control(
        w!("COMBOBOX"),
        w!(""),
        WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
        ),
        162,
        570,
        280,
        240,
        SETTINGS_IDLE_DISCONNECT_ID,
    )?;
    for choice in crate::idle_disconnect::choices() {
        let label = HSTRING::from(crate::idle_disconnect::label(choice));
        unsafe {
            SendMessageW(
                idle_disconnect,
                CB_ADDSTRING,
                None,
                Some(LPARAM(label.as_ptr() as isize)),
            );
        }
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
        w!("Display size when the computer has no monitor"),
        static_style,
        162,
        562,
        370,
        24,
        SETTINGS_HEADLESS_TITLE_ID as usize,
    )?;
    let headless = make_control(
        w!("COMBOBOX"),
        w!(""),
        WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32,
        ),
        162,
        590,
        280,
        240,
        SETTINGS_HEADLESS_ID,
    )?;
    for resolution in HeadlessResolution::PRESETS {
        let label = HSTRING::from(resolution.label());
        unsafe {
            SendMessageW(
                headless,
                CB_ADDSTRING,
                None,
                Some(LPARAM(label.as_ptr() as isize)),
            );
        }
    }
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
