//! The viewer settings window: display, advanced and keyboard pages, and the
//! maintenance state they and the toolbar show.

mod pages;
#[cfg(test)]
mod tests;
mod window_proc;

use super::*;
use meshrmm_protocol::HeadlessResolution;
use pages::SettingsPageBuilder;
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

/// What the settings window's controls show of the session. It is read
/// without touching a control, so a refresh can leave alone every control
/// whose value has not changed.
#[derive(Clone, PartialEq, Eq)]
struct SettingsView {
    /// Each shortcut key's combo box and the choice it selects.
    shortcut_keys: [(usize, Option<usize>); 2],
    idle_disconnect_allowed: bool,
    /// The selected time; one outside the offered choices selects nothing.
    idle_disconnect: usize,
    /// The selected virtual monitor size, likewise.
    headless: usize,
    headless_shown: bool,
    recording: bool,
    /// Each check box and radio button: whether it is checked and enabled.
    buttons: [(usize, bool, bool); 15],
}

/// What the settings window shows, kept by its owner's context.
#[derive(Default)]
pub(super) struct SettingsShown {
    /// The page the settings window shows.
    category: Cell<SettingsCategory>,
    /// What its controls were last given, while they still show it.
    view: RefCell<Option<SettingsView>>,
    /// How many controls refreshes have changed.
    #[cfg(test)]
    writes: Cell<usize>,
}

impl SettingsShown {
    /// Whether `part` of `view` differs from what the controls show.
    fn changed<T: PartialEq>(
        &self,
        shown: Option<&SettingsView>,
        view: &SettingsView,
        part: impl Fn(&SettingsView) -> T,
    ) -> bool {
        let changed = shown.is_none_or(|shown| part(shown) != part(view));
        #[cfg(test)]
        if changed {
            self.writes.set(self.writes.get() + 1);
        }
        changed
    }
}

impl WindowContext {
    /// Brings the toolbar and, while it is shown, the settings window up to
    /// date with the session. The window loop calls this on every pass, so
    /// it touches only the controls whose value changed; a hidden settings
    /// window is brought up to date when it is shown again.
    pub(super) fn refresh_maintenance_controls(&self) {
        self.refresh_toolbar();
        if unsafe { IsWindowVisible(self.controls().settings_window) }.as_bool() {
            self.refresh_settings();
        }
    }

    /// Has the next refresh give every settings control its value again.
    /// Check boxes, radio buttons and combo boxes change by themselves when
    /// used, whether or not the session takes the change.
    pub(super) fn forget_settings_view(&self) {
        self.settings_shown.view.take();
    }

    fn settings_view(&self) -> SettingsView {
        let control = &self.control;
        let maintenance = control.maintenance_state();
        let border_available = control.display_border_available();
        let close_action = control.session_close_action();
        let [no_action, lock, logout] =
            SETTINGS_CLOSE_ACTION_IDS.map(|(id, action)| (id, action == close_action, true));
        let shortcut = |id, shortcut| {
            let key = control.shortcut_key(shortcut);
            (
                id,
                ShortcutKey::ALL.iter().position(|choice| *choice == key),
            )
        };
        let minutes = control.idle_disconnect_minutes();
        let resolution = crate::preferences::headless_resolution();
        SettingsView {
            shortcut_keys: [
                shortcut(SETTINGS_DIAGNOSTICS_KEY_ID, ViewerShortcut::Diagnostics),
                shortcut(SETTINGS_DISPLAY_KEY_ID, ViewerShortcut::NextDisplay),
            ],
            idle_disconnect_allowed: control.allow_idle_disconnect_override(),
            idle_disconnect: crate::idle_disconnect::choices()
                .position(|choice| choice == minutes)
                .unwrap_or(usize::MAX),
            headless: HeadlessResolution::PRESETS
                .iter()
                .position(|choice| *choice == resolution)
                .unwrap_or(usize::MAX),
            // On the display page, and only for a device that may be on
            // Windows, which alone adds a virtual monitor.
            headless_shown: self.settings_shown.category.get() == SettingsCategory::Display
                && control.windows_device(),
            recording: control.recording().active(),
            buttons: [
                (
                    SETTINGS_DISCONNECT_ID,
                    control.disconnect_confirmation(),
                    true,
                ),
                (
                    SETTINGS_IDLE_ID,
                    control.prevent_idle_lock(),
                    control.allow_idle_override(),
                ),
                (
                    SETTINGS_DISPLAY_BORDER_ID,
                    border_available && control.display_border(),
                    border_available,
                ),
                (SETTINGS_WALLPAPER_ID, control.wallpaper_hidden(), true),
                (SETTINGS_AUDIO_ID, control.audio_muted(), true),
                (SETTINGS_CLIPBOARD_ID, control.clipboard_sync(), true),
                (
                    SETTINGS_CLEAR_CLIPBOARD_ID,
                    control.clear_clipboard_on_close(),
                    control.allow_clear_clipboard_override(),
                ),
                (
                    SETTINGS_REMOTE_CURSOR_ID,
                    control.show_remote_cursor(),
                    true,
                ),
                (
                    SETTINGS_TECHNICIAN_INPUT_ID,
                    control.technician_blocked(),
                    true,
                ),
                (
                    SETTINGS_WINDOWS_SHORTCUTS_ID,
                    control.send_windows_shortcuts(),
                    true,
                ),
                (
                    SETTINGS_BLACKOUT_ID,
                    maintenance.blacked_out,
                    maintenance.available,
                ),
                (
                    SETTINGS_AGENT_INPUT_ID,
                    control.agent_blocked(),
                    maintenance.available && !maintenance.blacked_out,
                ),
                no_action,
                lock,
                logout,
            ],
        }
    }

    /// Gives the settings controls whose value changed their new value.
    fn refresh_settings(&self) {
        let view = self.settings_view();
        // Recorded first, so that a control used meanwhile is shown again.
        let shown = self.settings_shown.view.replace(Some(view.clone()));
        let shown = shown.as_ref();
        let seen = &self.settings_shown;
        let settings_window = self.controls().settings_window;
        let control = |id: usize| unsafe { GetDlgItem(Some(settings_window), id as i32) }.ok();
        for (index, (id, selected)) in view.shortcut_keys.into_iter().enumerate() {
            if let Some(selected) = selected
                && seen.changed(shown, &view, |view| view.shortcut_keys[index])
                && let Some(combo) = control(id)
            {
                unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(selected)), None) };
            }
        }
        let allowed = view.idle_disconnect_allowed;
        if seen.changed(shown, &view, |view| view.idle_disconnect_allowed)
            && let Some(title) = control(SETTINGS_IDLE_DISCONNECT_TITLE_ID as usize)
        {
            let text = if allowed {
                w!("Disconnect when idle")
            } else {
                w!("Disconnect when idle (company managed)")
            };
            let _ = unsafe { SetWindowTextW(title, text) };
        }
        if seen.changed(shown, &view, |view| {
            (view.idle_disconnect, view.idle_disconnect_allowed)
        }) && let Some(combo) = control(SETTINGS_IDLE_DISCONNECT_ID)
        {
            let index = WPARAM(view.idle_disconnect);
            unsafe { SendMessageW(combo, CB_SETCURSEL, Some(index), None) };
            let _ = unsafe { EnableWindow(combo, allowed) };
        }
        if seen.changed(shown, &view, |view| view.headless)
            && let Some(combo) = control(SETTINGS_HEADLESS_ID)
        {
            unsafe { SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(view.headless)), None) };
        }
        if seen.changed(shown, &view, |view| view.headless_shown) {
            let command = if view.headless_shown {
                SW_SHOW
            } else {
                SW_HIDE
            };
            for id in [SETTINGS_HEADLESS_TITLE_ID as usize, SETTINGS_HEADLESS_ID] {
                if let Some(control) = control(id) {
                    let _ = unsafe { ShowWindow(control, command) };
                }
            }
        }
        if seen.changed(shown, &view, |view| view.recording)
            && let Some(button) = control(SETTINGS_RECORDING_ID)
        {
            let text = if view.recording {
                w!("Stop recording and save")
            } else {
                w!("Record video to Downloads")
            };
            let _ = unsafe { SetWindowTextW(button, text) };
        }
        for (index, (id, checked, enabled)) in view.buttons.into_iter().enumerate() {
            if !seen.changed(shown, &view, |view| view.buttons[index]) {
                continue;
            }
            let Some(button) = control(id) else {
                continue;
            };
            let label = match (id, enabled) {
                (SETTINGS_IDLE_ID, true) => Some(w!("Prevent idle lock")),
                (SETTINGS_IDLE_ID, false) => Some(w!("Prevent idle lock (company managed)")),
                (SETTINGS_CLEAR_CLIPBOARD_ID, true) => Some(w!("Clear clipboard on session close")),
                (SETTINGS_CLEAR_CLIPBOARD_ID, false) => {
                    Some(w!("Clear clipboard on close (company managed)"))
                }
                _ => None,
            };
            unsafe {
                if let Some(label) = label {
                    let _ = SetWindowTextW(button, label);
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

    pub(super) fn show_settings(&self) {
        self.refresh_toolbar();
        self.refresh_settings();
        let settings_window = self.controls().settings_window;
        let _ = unsafe { ShowWindow(settings_window, SW_SHOW) };
        let _ = unsafe { SetForegroundWindow(settings_window) };
    }

    /// Shows the settings page for `category`.
    fn show_settings_page(&self, category: SettingsCategory) {
        unsafe { show_settings_category(self.controls().settings_window, category) };
        self.settings_shown.category.set(category);
        self.refresh_maintenance_controls();
    }

    /// How many settings controls refreshes have changed.
    #[cfg(test)]
    pub(super) fn settings_writes(&self) -> usize {
        self.settings_shown.writes.get()
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

    let pages = SettingsPageBuilder {
        window: settings,
        instance,
    };
    pages.create_category_tabs()?;
    let quality_buttons = pages.create_quality_choices()?;
    let chroma_buttons = pages.create_chroma_choices()?;
    pages.create_advanced_page()?;
    pages.create_keyboard_page()?;
    unsafe { show_settings_category(settings, SettingsCategory::Display) };
    Ok(SettingsControls {
        window: settings,
        dpi: 96,
        quality_buttons,
        chroma_buttons,
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
