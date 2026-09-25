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

/// SS_CENTER and SS_CENTERIMAGE, which live in an otherwise unused Windows feature.
pub(super) const STATIC_CENTER: u32 = 0x0001;
const STATIC_CENTER_VERTICALLY: u32 = 0x0200;

/// The minimum outer window size, in 96-DPI pixels, that fits the toolbar.
const MINIMUM_WINDOW_WIDTH: i32 = 1176;
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
    /// Owned popup shown while the connection is being restored.
    reconnecting_label: HWND,
    toolbar: HWND,
    user_combo: HWND,
    display_combo: HWND,
    quality_combo: HWND,
    chroma_combo: HWND,
    diagnostics_button: HWND,
    settings_button: HWND,
    file_button: HWND,
    chat_button: HWND,
    secure_attention_button: HWND,
    type_clipboard_button: HWND,
    credential_buttons: [HWND; 3],
    credential_label: HWND,
    minimize_button: HWND,
    maximize_button: HWND,
    close_button: HWND,
    settings_window: HWND,
    quality_buttons: [(HWND, QualityPreset); 4],
    chroma_buttons: [(HWND, ChromaMode); 2],
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            video_window: HWND::default(),
            debug_overlay: HWND::default(),
            reconnecting_label: HWND::default(),
            toolbar: HWND::default(),
            user_combo: HWND::default(),
            display_combo: HWND::default(),
            quality_combo: HWND::default(),
            chroma_combo: HWND::default(),
            diagnostics_button: HWND::default(),
            settings_button: HWND::default(),
            file_button: HWND::default(),
            chat_button: HWND::default(),
            secure_attention_button: HWND::default(),
            type_clipboard_button: HWND::default(),
            credential_buttons: [HWND::default(); 3],
            credential_label: HWND::default(),
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
        }
    }
}

struct WindowContext {
    video_width: u32,
    video_height: u32,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
    dpi: Cell<u32>,
    font: Cell<HFONT>,
    settings_dpi: Cell<u32>,
    settings_font: Cell<HFONT>,
    resize_pending: Cell<bool>,
    held: RefCell<HeldInput>,
    cursor_shape: Cell<CursorShape>,
    title: RefCell<HSTRING>,
    debug_visible: Cell<bool>,
    debug_refreshed: Cell<std::time::Instant>,
    recording_visible: Cell<bool>,
    controls: Cell<Controls>,
    chat_popup: OnceCell<meshrmm_chat::ChatPopup>,
}

impl Drop for WindowContext {
    fn drop(&mut self) {
        for font in [self.font.get(), self.settings_font.get()] {
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
    fn set_dpi(&self, window: HWND, dpi: u32) {
        if dpi == self.dpi.get() {
            return;
        }
        self.dpi.set(dpi);
        let font = unsafe { message_font(dpi) };
        for control in self.toolbar_controls() {
            unsafe { set_font(control, font) };
        }
        let old = self.font.replace(font);
        if !old.is_invalid() {
            let _ = unsafe { DeleteObject(HGDIOBJ(old.0)) };
        }
        self.layout_toolbar(window);
        self.resize_pending.set(true);
    }

    fn set_reconnecting(&self, window: HWND, reconnecting: bool) {
        let command = if reconnecting {
            SW_SHOWNOACTIVATE
        } else {
            SW_HIDE
        };
        let _ = unsafe { ShowWindow(self.controls().reconnecting_label, command) };
        let title = if reconnecting {
            HSTRING::from(format!("{} — Reconnecting…", self.title.borrow()))
        } else {
            self.title.borrow().clone()
        };
        let _ = unsafe { SetWindowTextW(window, PCWSTR(title.as_ptr())) };
    }

    fn toggle_debug(&self) {
        let visible = !self.debug_visible.get();
        self.debug_visible.set(visible);
        let controls = self.controls();
        let command = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
        let _ = unsafe { ShowWindow(controls.debug_overlay, command) };
        unsafe {
            SendMessageW(
                controls.diagnostics_button,
                BM_SETCHECK,
                Some(WPARAM(usize::from(visible))),
                None,
            )
        };
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
        video_width: format.width,
        video_height: format.height,
        active_display,
        displays,
        control,
        debug,
        dpi: Cell::new(96),
        font: Cell::new(HFONT::default()),
        settings_dpi: Cell::new(96),
        settings_font: Cell::new(HFONT::default()),
        resize_pending: Cell::new(false),
        held: RefCell::new(HeldInput::default()),
        cursor_shape: Cell::new(CursorShape::Default),
        title: RefCell::new(title.clone()),
        debug_visible: Cell::new(false),
        debug_refreshed: Cell::new(std::time::Instant::now()),
        recording_visible: Cell::new(false),
        controls: Cell::new(Controls::default()),
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
    context.dpi.set(dpi);
    context.font.set(font);
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
    let chat_popup = unsafe {
        meshrmm_chat::ChatPopup::new(window, controls.chat_button, context.control.chat())
    }?;
    let _ = context.chat_popup.set(chat_popup);
    if !context.control.supports_chroma(ChromaMode::Yuv444) {
        let _ = unsafe { EnableWindow(controls.chroma_buttons[1].0, false) };
    }
    context.layout_toolbar(window);
    context.set_quality(context.control.quality_preset());
    context.set_chroma(context.control.chroma_mode());
    // New children are added below their siblings. Put the strip under
    // every control created after it, or its background paints over them.
    let _ = unsafe {
        SetWindowPos(
            controls.toolbar,
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
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

pub(super) unsafe fn set_reconnecting(window: HWND, reconnecting: bool) {
    if let Some(context) = unsafe { window_context(window) } {
        context.set_reconnecting(window, reconnecting);
    }
}

pub(super) unsafe fn set_window_cursor(window: HWND, shape: CursorShape) {
    if let Some(context) = unsafe { window_context(window) } {
        context.cursor_shape.set(shape);
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
