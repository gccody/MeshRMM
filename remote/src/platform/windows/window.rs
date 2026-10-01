//! The viewer window: a toolbar above a child window that hosts the video
//! swap chain. [`messages`] is the window procedure, [`toolbar`] and
//! [`settings`] hold the controls, and [`input`] forwards the keyboard and
//! mouse to the device.
//!
//! The window owns its [`WindowContext`] through `GWLP_USERDATA`. Window
//! procedures are re-entered: a message box, a popup menu, `SetFocus` or
//! `SendMessageW` runs messages for the same window before it returns, and
//! the settings window calls into its owner's context. So the context is
//! reference counted and only ever shared. Every caller holds its own `Rc`,
//! which keeps the context alive even if a nested message destroys the
//! window, and the state that changes is in `Cell`s and `RefCell`s whose
//! borrows end before any call that can run window messages.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use super::keyboard_hook;
use super::*;
use crate::input::HeldInput;
use crate::shortcuts::{ShortcutKey, ViewerShortcut};
use crate::stream_reset::ResetPlan;
use crate::video_layout::{self, VideoRect};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateFontIndirectW, DeleteObject, GetMonitorInfoW, HFONT, HGDIOBJ,
    MONITOR_DEFAULTTONEAREST, MONITORINFO, MapWindowPoints, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, SystemParametersInfoForDpi,
};

mod input;
mod messages;
mod settings;
mod toolbar;

pub(super) use toolbar::set_agent_pointer_display;

/// Where the last session window was. A window opened for another display,
/// codec or connection takes its place instead of jumping to a new one.
static LAST_PLACEMENT: std::sync::Mutex<Option<WINDOWPLACEMENT>> = std::sync::Mutex::new(None);

/// SS_CENTER, which lives in an otherwise unused Windows feature.
pub(super) const STATIC_CENTER: u32 = 0x0001;

/// The minimum outer window size, in 96-DPI pixels, that fits the toolbar.
const MINIMUM_WINDOW_WIDTH: i32 = 760;
const MINIMUM_WINDOW_HEIGHT: i32 = 300;

/// The video window's size and the letterboxed video inside it, in physical pixels.
pub(super) struct ClientLayout {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) video: VideoRect,
}

/// The window's children and owned popups. They are created after the
/// window itself, so they are null until then.
#[derive(Clone, Copy)]
struct Controls {
    /// Hosts the swap chain below the toolbar. A flip-model swap chain covers
    /// every GDI child of its own window, so the video cannot be drawn on the
    /// top-level window without hiding the toolbar. The child is disabled, so
    /// mouse input and dropped files go to the top-level window.
    video_window: HWND,
    /// Owned popup: a child window over the video would be hidden by it.
    debug_overlay: HWND,
    /// Owned popup shown while the connection is being restored. It holds
    /// the two children below and forwards their notifications here.
    reconnect_panel: HWND,
    /// Why, for how long, and when the next attempt starts.
    reconnecting_label: HWND,
    /// "Retry now", which ends the wait before the next attempt.
    retry_button: HWND,
    /// The toolbar the viewer draws; see [`toolbar`].
    toolbar: HWND,
    /// The toolbar's tooltips, if the common controls are available.
    tooltip: HWND,
    settings_window: HWND,
    quality_buttons: [(HWND, QualityPreset); 4],
    chroma_buttons: [(HWND, ChromaMode); 2],
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            video_window: HWND::default(),
            debug_overlay: HWND::default(),
            reconnect_panel: HWND::default(),
            reconnecting_label: HWND::default(),
            retry_button: HWND::default(),
            toolbar: HWND::default(),
            tooltip: HWND::default(),
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
        }
    }
}

struct WindowContext {
    /// The viewer window, once created.
    window: Cell<HWND>,
    // The stream fields change when the device replaces the stream; see
    // `reset_stream`. Read them through the cloning accessors.
    video_width: Cell<u32>,
    video_height: Cell<u32>,
    active_display: RefCell<Display>,
    displays: RefCell<Vec<Display>>,
    /// The display the device's pointer is on, marked in the display list.
    agent_pointer_display: Cell<Option<meshrmm_protocol::DisplayId>>,
    reconnecting: Cell<bool>,
    control: ControlSink,
    debug: DebugInfo,
    dpi: Cell<u32>,
    font: Cell<HFONT>,
    toolbar_font: Cell<HFONT>,
    settings_dpi: Cell<u32>,
    settings_font: Cell<HFONT>,
    resize_pending: Cell<bool>,
    held: RefCell<HeldInput>,
    annotator: RefCell<crate::annotation::Annotator>,
    cursor_shape: Cell<CursorShape>,
    title: RefCell<HSTRING>,
    debug_visible: Cell<bool>,
    debug_refreshed: Cell<std::time::Instant>,
    recording_visible: Cell<bool>,
    controls: Cell<Controls>,
    toolbar: RefCell<toolbar::ToolbarModel>,
    chat_popup: OnceCell<meshrmm_chat::ChatPopup>,
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        for font in [
            self.font.get(),
            self.toolbar_font.get(),
            self.settings_font.get(),
        ] {
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

    fn controls(&self) -> Controls {
        self.controls.get()
    }

    fn active_display(&self) -> Display {
        self.active_display.borrow().clone()
    }

    fn active_display_id(&self) -> meshrmm_protocol::DisplayId {
        self.active_display.borrow().id
    }

    fn displays(&self) -> Vec<Display> {
        self.displays.borrow().clone()
    }

    /// Converts 96-DPI layout pixels to this window's physical pixels.
    fn px(&self, value: i32) -> i32 {
        scale(value, self.dpi.get())
    }

    fn video_rect_for(&self, width: u32, height: u32) -> VideoRect {
        let toolbar = toolbar_height(self.dpi.get());
        let width = i32::try_from(width).unwrap_or(i32::MAX);
        let height = i32::try_from(height).unwrap_or(i32::MAX);
        video_layout::letterbox(
            VideoRect {
                left: 0,
                top: toolbar,
                width,
                height: height.saturating_sub(toolbar),
            },
            self.video_width.get(),
            self.video_height.get(),
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
    fn set_dpi(&self, window: HWND, dpi: u32) {
        if dpi == self.dpi.get() {
            return;
        }
        self.dpi.set(dpi);
        let font = unsafe { message_font(dpi) };
        for control in self.toolbar_controls() {
            unsafe { set_font(control, font) };
        }
        let toolbar_font = unsafe { toolbar::toolbar_font(dpi) };
        for old in [
            self.font.replace(font),
            self.toolbar_font.replace(toolbar_font),
        ] {
            if !old.is_invalid() {
                let _ = unsafe { DeleteObject(HGDIOBJ(old.0)) };
            }
        }
        self.layout_toolbar(window);
        self.resize_pending.set(true);
    }

    /// Shows the reconnect overlay with `text`, or hides it (`None`).
    fn set_reconnect_text(&self, window: HWND, text: Option<&ReconnectText>) {
        let controls = self.controls();
        if let Some(text) = text {
            let label = HSTRING::from(format!("{}\r\n{}", text.title, text.detail));
            let _ = unsafe { SetWindowTextW(controls.reconnecting_label, PCWSTR(label.as_ptr())) };
            let _ = unsafe { EnableWindow(controls.retry_button, text.retry_enabled) };
        }
        let reconnecting = text.is_some();
        if self.reconnecting.replace(reconnecting) == reconnecting {
            return;
        }
        let command = if reconnecting {
            SW_SHOWNOACTIVATE
        } else {
            SW_HIDE
        };
        let _ = unsafe { ShowWindow(controls.reconnect_panel, command) };
        self.show_title(window);
    }

    /// Shows the title, marked while the connection is being restored.
    fn show_title(&self, window: HWND) {
        let title = if self.reconnecting.get() {
            HSTRING::from(format!("{} — Reconnecting…", self.title.borrow()))
        } else {
            self.title.borrow().clone()
        };
        let _ = unsafe { SetWindowTextW(window, PCWSTR(title.as_ptr())) };
    }

    /// Moves the window to a replacement stream. The window, its placement,
    /// DPI, settings, chat popup, diagnostics overlay and keyboard hook stay
    /// as they are; the pointer mapping, title and selectors follow the new
    /// display. The renderer places the video afterwards.
    fn reset_stream(
        &self,
        window: HWND,
        plan: &ResetPlan,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) {
        if plan.display_changed {
            // Key-ups and button-ups name the display they were pressed on.
            self.release_input();
            // A stroke belongs to the display it started on.
            self.annotator.borrow_mut().finish();
            // A drag on the old display has ended with its button-up.
            if unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() } == window {
                let _ = unsafe { ReleaseCapture() };
            }
        }
        if display.session == meshrmm_protocol::DesktopSession::Background {
            self.annotator.borrow_mut().disable();
            unsafe { apply_cursor(self.video_cursor()) };
        }
        self.video_width.set(format.width);
        self.video_height.set(format.height);
        self.title.replace(window_title(&display));
        self.active_display.replace(display);
        self.displays.replace(displays);
        self.show_title(window);
        // Only the controls: sending the choices again would make the device
        // echo its configuration and reset the stream again.
        self.show_quality(self.control.quality_preset());
        self.show_chroma(self.control.chroma_mode());
        self.place_popups(window);
    }

    /// The cursor over the video: a crosshair while annotating.
    fn video_cursor(&self) -> CursorShape {
        if self.annotating() {
            CursorShape::Crosshair
        } else {
            self.control.effective_cursor_shape(self.cursor_shape.get())
        }
    }

    fn toggle_debug(&self) {
        let visible = !self.debug_visible.get();
        self.debug_visible.set(visible);
        let controls = self.controls();
        let command = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
        let _ = unsafe { ShowWindow(controls.debug_overlay, command) };
        if let Ok(button) = unsafe {
            GetDlgItem(
                Some(controls.settings_window),
                settings::SETTINGS_DIAGNOSTICS_ID as i32,
            )
        } {
            unsafe {
                SendMessageW(
                    button,
                    BM_SETCHECK,
                    Some(WPARAM(usize::from(visible))),
                    None,
                )
            };
        }
        if visible {
            self.refresh_debug(true);
        }
        self.refresh_toolbar();
    }

    fn refresh_debug(&self, force: bool) {
        if !self.debug_visible.get()
            || (!force && self.debug_refreshed.get().elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.debug_refreshed.set(std::time::Instant::now());
        let text = HSTRING::from(self.debug.render().replace('\n', "\r\n"));
        let _ = unsafe { SetWindowTextW(self.controls().debug_overlay, PCWSTR(text.as_ptr())) };
    }
}

/// The context of a window created by [`create_window`], if it still has one.
unsafe fn window_context(window: HWND) -> Option<Rc<WindowContext>> {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const WindowContext;
    if pointer.is_null() {
        return None;
    }
    // The pointer is the window's own reference from `Rc::into_raw`, which
    // it keeps until WM_NCDESTROY clears GWLP_USERDATA. All of this happens on
    // the window's thread.
    unsafe {
        Rc::increment_strong_count(pointer);
        Some(Rc::from_raw(pointer))
    }
}

/// The window title: the display, and the viewer's shortcut keys.
fn window_title(display: &Display) -> HSTRING {
    let next_display = crate::preferences::shortcut_key(ViewerShortcut::NextDisplay);
    let diagnostics = crate::preferences::shortcut_key(ViewerShortcut::Diagnostics);
    let hint = crate::shortcuts::hint(
        (next_display != ShortcutKey::Off).then(|| next_display.label()),
        diagnostics,
    );
    let mut title = format!(
        "MeshRMM Remote Desktop — {} ({})",
        display.name,
        if display.primary {
            "primary"
        } else {
            "secondary"
        }
    );
    if !hint.is_empty() {
        title.push_str(" — ");
        title.push_str(&hint);
    }
    HSTRING::from(title)
}

/// Scales a 96-DPI length to `dpi`, rounding to the nearest pixel.
pub(super) fn scale(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi) + 48).div_euclid(96)) as i32
}

fn toolbar_height(dpi: u32) -> i32 {
    scale(VIEWER_TOOLBAR_HEIGHT as i32, dpi)
}

pub(super) unsafe fn window_dpi(window: HWND) -> u32 {
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
pub(super) unsafe fn message_font(dpi: u32) -> HFONT {
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

pub(super) unsafe fn set_font(control: HWND, font: HFONT) {
    unsafe {
        SendMessageW(
            control,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
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

/// The display the window's input goes to.
pub(super) unsafe fn active_display_id(window: HWND) -> Option<meshrmm_protocol::DisplayId> {
    let context = unsafe { window_context(window) }?;
    Some(context.active_display_id())
}

/// Moves the window to a replacement stream; see [`WindowContext::reset_stream`].
pub(super) unsafe fn reset_stream(
    window: HWND,
    plan: &ResetPlan,
    format: VideoFormat,
    display: Display,
    displays: Vec<Display>,
) {
    if let Some(context) = unsafe { window_context(window) } {
        context.reset_stream(window, plan, format, display, displays);
    }
}

/// The window that hosts the swap chain.
pub(super) unsafe fn video_window(window: HWND) -> Option<HWND> {
    let context = unsafe { window_context(window) }?;
    let video_window = context.controls().video_window;
    (!video_window.is_invalid()).then_some(video_window)
}

/// The video window's size and the video's place in it, for the swap chain.
pub(super) unsafe fn client_layout(window: HWND) -> Option<ClientLayout> {
    let context = unsafe { window_context(window) }?;
    let (width, height) = unsafe { client_size(window) }?;
    let toolbar = toolbar_height(context.dpi.get());
    let mut video = context.video_rect_for(width, height);
    video.top -= toolbar;
    Some(ClientLayout {
        width,
        height: height.saturating_sub(u32::try_from(toolbar).unwrap_or(0)),
        video,
    })
}

/// The new client layout if the window was resized or changed DPI since the
/// last call.
pub(super) unsafe fn take_resize(window: HWND) -> Option<ClientLayout> {
    let context = unsafe { window_context(window) }?;
    if !context.resize_pending.replace(false) {
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

pub(super) unsafe fn create_window(
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
) -> anyhow::Result<HWND> {
    let module =
        unsafe { GetModuleHandleW(None) }.context("application module handle unavailable")?;
    let instance = HINSTANCE(module.0);
    let class = w!("MeshRmmRemoteDesktopWindow");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(messages::window_proc),
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
    let video_class = w!("MeshRmmRemoteVideo");
    let video_window_class = WNDCLASSW {
        lpfnWndProc: Some(messages::video_proc),
        hInstance: instance,
        lpszClassName: video_class,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&video_window_class) } == 0 {
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("remote video window class registration failed");
        }
    }
    let window_style = WINDOW_STYLE((WS_OVERLAPPEDWINDOW.0 & !WS_CAPTION.0) | WS_CLIPCHILDREN.0);
    // Read before creating: the new window's own size messages update it.
    let placement = *LAST_PLACEMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let title = window_title(&active_display);
    let context = Rc::new(WindowContext {
        window: Cell::new(HWND::default()),
        video_width: Cell::new(format.width),
        video_height: Cell::new(format.height),
        active_display: RefCell::new(active_display),
        displays: RefCell::new(displays),
        agent_pointer_display: Cell::new(None),
        reconnecting: Cell::new(false),
        control,
        debug,
        dpi: Cell::new(96),
        font: Cell::new(HFONT::default()),
        toolbar_font: Cell::new(HFONT::default()),
        settings_dpi: Cell::new(96),
        settings_font: Cell::new(HFONT::default()),
        resize_pending: Cell::new(false),
        held: RefCell::new(HeldInput::default()),
        annotator: RefCell::new(crate::annotation::Annotator::default()),
        cursor_shape: Cell::new(CursorShape::Default),
        title: RefCell::new(title.clone()),
        debug_visible: Cell::new(false),
        debug_refreshed: Cell::new(std::time::Instant::now()),
        recording_visible: Cell::new(false),
        controls: Cell::new(Controls::default()),
        toolbar: RefCell::new(toolbar::ToolbarModel::default()),
        chat_popup: OnceCell::new(),
    });
    // The window's own reference, released on WM_NCDESTROY.
    let owned = Rc::into_raw(Rc::clone(&context));
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
            Some(owned.cast()),
        )
    };
    let window = match window {
        Ok(window) => window,
        Err(error) => {
            // A window that got as far as WM_NCCREATE released its reference
            // on WM_NCDESTROY.
            if Rc::strong_count(&context) > 1 {
                drop(unsafe { Rc::from_raw(owned) });
            }
            return Err(error).context("native remote desktop window creation failed");
        }
    };
    let dpi = unsafe { window_dpi(window) };
    let font = unsafe { message_font(dpi) };
    context.window.set(window);
    context.dpi.set(dpi);
    context.font.set(font);
    context
        .toolbar_font
        .set(unsafe { toolbar::toolbar_font(dpi) });
    match placement {
        Some(mut placement) => {
            if placement.showCmd == SW_SHOWMINIMIZED.0 as u32 {
                placement.showCmd = SW_SHOWMINNOACTIVE.0 as u32;
            } else if placement.showCmd != SW_SHOWMAXIMIZED.0 as u32 {
                // Stay hidden until the controls exist; shown at the end.
                placement.showCmd = SW_HIDE.0 as u32;
            }
            let _ = unsafe { SetWindowPlacement(window, &placement) };
        }
        None => unsafe { place_initial_window(window, window_style, format, dpi) },
    }
    let video_window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            video_class,
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_DISABLED | WS_CLIPSIBLINGS,
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
    .context("remote video window creation failed")?;
    context.controls.set(Controls {
        video_window,
        ..context.controls()
    });
    let mut controls = unsafe { toolbar::create_toolbar(window, instance, &context, font) }?;
    let settings = unsafe { settings::create_settings_window(window, instance) }?;
    let settings_dpi = unsafe { window_dpi(settings.window) };
    let settings_font = unsafe { message_font(settings_dpi) };
    unsafe {
        settings::rescale_children(settings.window, settings.dpi, settings_dpi, settings_font)
    };
    let _ = unsafe {
        SetWindowPos(
            settings.window,
            None,
            0,
            0,
            scale(settings::SETTINGS_WINDOW_WIDTH, settings_dpi),
            scale(settings::SETTINGS_WINDOW_HEIGHT, settings_dpi),
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
        )
    };
    controls.settings_window = settings.window;
    controls.quality_buttons = settings.quality_buttons;
    controls.chroma_buttons = settings.chroma_buttons;
    context.controls.set(controls);
    context.settings_dpi.set(settings_dpi);
    context.settings_font.set(settings_font);
    let chat_popup = unsafe { meshrmm_chat::ChatPopup::new(window, context.control.chat()) }?;
    let _ = context.chat_popup.set(chat_popup);
    if !context.control.supports_chroma(ChromaMode::Yuv444) {
        let _ = unsafe { EnableWindow(controls.chroma_buttons[1].0, false) };
    }
    context.layout_toolbar(window);
    context.set_quality(context.control.quality_preset());
    context.set_chroma(context.control.chroma_mode());
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    super::close_launch_status();
    Ok(window)
}

unsafe fn remember_placement(window: HWND) {
    if !unsafe { IsWindowVisible(window) }.as_bool() {
        return;
    }
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    if unsafe { GetWindowPlacement(window, &mut placement) }.is_ok() {
        *LAST_PLACEMENT
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(placement);
    }
}

pub(super) unsafe fn set_reconnect_text(window: HWND, text: Option<&ReconnectText>) {
    if let Some(context) = unsafe { window_context(window) } {
        context.set_reconnect_text(window, text);
    }
}

pub(super) unsafe fn set_window_cursor(window: HWND, shape: CursorShape) {
    if let Some(context) = unsafe { window_context(window) } {
        context.cursor_shape.set(shape);
        unsafe { apply_cursor(context.video_cursor()) };
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
        if let Some(chat) = context.chat_popup.get() {
            chat.refresh();
        }
    }
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return true;
        }
        if let Some(context) = unsafe { window_context(window) }
            && let Some(chat) = context.chat_popup.get()
            && chat.handle_message(&message)
        {
            continue;
        }
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    false
}

/// What the reset probe reads back from a live window.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProbeState {
    pub(super) title: String,
    pub(super) video_size: (u32, u32),
    pub(super) active_display: meshrmm_protocol::DisplayId,
    pub(super) users: Vec<String>,
    pub(super) selected_user: isize,
    pub(super) displays: Vec<String>,
    pub(super) selected_display: isize,
    pub(super) display_combo_enabled: bool,
    pub(super) quality: isize,
    pub(super) chroma: isize,
    pub(super) toolbar_height: i32,
    /// The letterboxed video in the window's client coordinates.
    pub(super) video: Option<VideoRect>,
}

#[cfg(test)]
pub(super) unsafe fn probe_state(window: HWND) -> Option<ProbeState> {
    let context = unsafe { window_context(window) }?;
    let title = unsafe {
        let mut text = vec![0_u16; GetWindowTextLengthW(window).max(0) as usize + 1];
        let length = GetWindowTextW(window, &mut text).max(0) as usize;
        String::from_utf16_lossy(&text[..length])
    };
    let state = context.toolbar_state();
    let displays = crate::toolbar::menu(crate::toolbar::Action::Display, &state)
        .into_iter()
        .filter_map(|entry| match entry {
            crate::toolbar::MenuEntry::Item { label, .. } => Some(label),
            crate::toolbar::MenuEntry::Separator => None,
        })
        .collect::<Vec<_>>();
    Some(ProbeState {
        title,
        video_size: (context.video_width.get(), context.video_height.get()),
        active_display: context.active_display_id(),
        users: state.sessions.clone(),
        selected_user: state.session as isize,
        display_combo_enabled: displays.len() > 1,
        displays,
        selected_display: state.display as isize,
        quality: [
            QualityPreset::UltraDataSaver,
            QualityPreset::DataSaver,
            QualityPreset::Balanced,
            QualityPreset::BestQuality,
        ]
        .iter()
        .position(|preset| *preset == state.quality)
        .map_or(-1, |index| index as isize),
        chroma: match state.chroma {
            Some((ChromaMode::Yuv444, _)) => 1,
            _ => 0,
        },
        toolbar_height: toolbar_height(context.dpi.get()),
        video: context.video_rect(window),
    })
}

/// The reconnect panel, its label and its "Retry now" button, for the
/// reconnect probe.
#[cfg(test)]
pub(super) unsafe fn probe_reconnect_panel(window: HWND) -> Option<(HWND, HWND, HWND)> {
    let controls = unsafe { window_context(window) }?.controls();
    Some((
        controls.reconnect_panel,
        controls.reconnecting_label,
        controls.retry_button,
    ))
}

#[cfg(test)]
pub(super) unsafe fn probe_toggle_chat(window: HWND) {
    if let Some(context) = unsafe { window_context(window) }
        && let Some(chat) = context.chat_popup.get()
    {
        chat.toggle();
    }
}

/// The toolbar's items with their rectangles in client pixels.
#[cfg(test)]
pub(super) type ProbeItems = Vec<(crate::toolbar::Item, RECT)>;

/// The toolbar window, its tooltip control, and its items, for the toolbar
/// probe.
#[cfg(test)]
pub(super) unsafe fn probe_toolbar(window: HWND) -> Option<(HWND, HWND, ProbeItems)> {
    let context = unsafe { window_context(window) }?;
    let controls = context.controls();
    let items = context.probe_toolbar_items();
    Some((controls.toolbar, controls.tooltip, items))
}

/// What the toolbar shows, for the toolbar probe.
#[cfg(test)]
pub(super) unsafe fn probe_toolbar_state(window: HWND) -> Option<crate::toolbar::State> {
    Some(unsafe { window_context(window) }?.toolbar_state())
}
