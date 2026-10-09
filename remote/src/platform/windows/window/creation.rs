use super::*;

/// Where the last session window was. A window opened for another display,
/// codec or connection takes its place instead of jumping to a new one.
static LAST_PLACEMENT: std::sync::Mutex<Option<WINDOWPLACEMENT>> = std::sync::Mutex::new(None);

const WINDOW_CLASS: PCWSTR = w!("MeshRmmRemoteDesktopWindow");
const VIDEO_CLASS: PCWSTR = w!("MeshRmmRemoteVideo");

pub(in crate::platform::windows) unsafe fn create_window(
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    debug: DebugInfo,
) -> anyhow::Result<HWND> {
    let module =
        unsafe { GetModuleHandleW(None) }.context("application module handle unavailable")?;
    let instance = HINSTANCE(module.0);
    unsafe { register_window_classes(instance) }?;
    let window_style = WINDOW_STYLE((WS_OVERLAPPEDWINDOW.0 & !WS_CAPTION.0) | WS_CLIPCHILDREN.0);
    // Read before creating: the new window's own size messages update it.
    let placement = *LAST_PLACEMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let title = window_title(&active_display);
    let context = Rc::new(WindowContext::new(
        format,
        active_display,
        displays,
        control,
        debug,
        title.clone(),
    ));
    let window = unsafe { create_top_level_window(&context, instance, &title, window_style) }?;
    let dpi = unsafe { window_dpi(window) };
    let font = unsafe { message_font(dpi) };
    context.window.set(window);
    context.dpi.set(dpi);
    context.font.set(font);
    context
        .toolbar_font
        .set(unsafe { toolbar::toolbar_font(dpi) });
    match placement {
        Some(placement) => unsafe { restore_placement(window, placement) },
        None => unsafe { place_initial_window(window, window_style, format, dpi) },
    }
    let video_window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            VIDEO_CLASS,
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
    unsafe { context.install_settings_window(window, instance, &mut controls) }?;
    let chat_popup = unsafe { meshrmm_chat::ChatPopup::new(window, context.control.chat()) }?;
    let _ = context.chat_popup.set(chat_popup);
    if !context.control.supports_chroma(ChromaMode::Yuv444) {
        let _ = unsafe { EnableWindow(controls.chroma_buttons[1].0, false) };
    }
    context.layout_toolbar(window);
    context.set_quality(context.control.quality_preset());
    context.set_chroma(context.control.chroma_mode());
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    close_launch_status();
    Ok(window)
}

unsafe fn register_window_classes(instance: HINSTANCE) -> anyhow::Result<()> {
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(messages::window_proc),
        hInstance: instance,
        lpszClassName: WINDOW_CLASS,
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
    let video_window_class = WNDCLASSW {
        lpfnWndProc: Some(messages::video_proc),
        hInstance: instance,
        lpszClassName: VIDEO_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&video_window_class) } == 0 {
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("remote video window class registration failed");
        }
    }
    Ok(())
}

/// Creates the viewer window, which takes its own reference to `context`.
unsafe fn create_top_level_window(
    context: &Rc<WindowContext>,
    instance: HINSTANCE,
    title: &HSTRING,
    window_style: WINDOW_STYLE,
) -> anyhow::Result<HWND> {
    // The window's own reference, released on WM_NCDESTROY.
    let owned = Rc::into_raw(Rc::clone(context));
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WINDOW_CLASS,
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
    match window {
        Ok(window) => Ok(window),
        Err(error) => {
            // A window that got as far as WM_NCCREATE released its reference
            // on WM_NCDESTROY.
            if Rc::strong_count(context) > 1 {
                drop(unsafe { Rc::from_raw(owned) });
            }
            Err(error).context("native remote desktop window creation failed")
        }
    }
}

unsafe fn restore_placement(window: HWND, mut placement: WINDOWPLACEMENT) {
    if placement.showCmd == SW_SHOWMINIMIZED.0 as u32 {
        placement.showCmd = SW_SHOWMINNOACTIVE.0 as u32;
    } else if placement.showCmd != SW_SHOWMAXIMIZED.0 as u32 {
        // Stay hidden until the controls exist; shown at the end.
        placement.showCmd = SW_HIDE.0 as u32;
    }
    let _ = unsafe { SetWindowPlacement(window, &placement) };
}

impl WindowContext {
    fn new(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
        title: HSTRING,
    ) -> Self {
        Self {
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
            title: RefCell::new(title),
            debug_visible: Cell::new(false),
            debug_refreshed: Cell::new(std::time::Instant::now()),
            recording_visible: Cell::new(false),
            controls: Cell::new(Controls::default()),
            toolbar: RefCell::new(toolbar::ToolbarModel::default()),
            chat_popup: OnceCell::new(),
        }
    }

    /// Creates the settings window at the DPI of its monitor and records it
    /// in `controls`, which become the window's controls.
    unsafe fn install_settings_window(
        &self,
        window: HWND,
        instance: HINSTANCE,
        controls: &mut Controls,
    ) -> anyhow::Result<()> {
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
        self.controls.set(*controls);
        self.settings_dpi.set(settings_dpi);
        self.settings_font.set(settings_font);
        Ok(())
    }
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

pub(super) unsafe fn remember_placement(window: HWND) {
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
