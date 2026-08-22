use super::*;

struct AppDelegateIvars {
    deep_link_tx: Sender<String>,
}

define_class!(
    // Safety: NSObject has no subclassing requirements and AppDelegate does
    // not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = AppDelegateIvars]
    struct AppDelegate;

    // Safety: these protocols have no additional safety requirements.
    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, application: &NSApplication, urls: &NSArray<NSURL>) {
            let Some(url) = urls.firstObject() else {
                return;
            };
            let Some(value) = url.absoluteString() else {
                return;
            };
            let value = value.to_string();
            if let Err(error) = self.ivars().deep_link_tx.send(value) {
                // The launch receiver is intentionally consumed by the first
                // session. A later dashboard handoff means the user is
                // replacing a stale/broken session, so start a fresh process
                // with that single-use URL before terminating this one.
                let replacement = std::env::current_exe()
                    .context("could not locate the macOS viewer executable")
                    .and_then(|executable| {
                        std::process::Command::new(executable)
                            .arg(error.0)
                            .spawn()
                            .context("could not launch the replacement macOS viewer")
                    });
                match replacement {
                    Ok(_) => application.terminate(None),
                    Err(error) => {
                        tracing::error!(error = %error, "failed to restart the macOS viewer")
                    }
                }
            }
        }
    }
);

impl AppDelegate {
    fn new(mtm: MainThreadMarker, deep_link_tx: Sender<String>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppDelegateIvars { deep_link_tx });
        // Safety: this invokes NSObject's parameterless initializer.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) struct RemoteViewIvars {
    active_display: Display,
    displays: Vec<Display>,
    video_width: u32,
    video_height: u32,
    control: ControlSink,
    pressed_keys: RefCell<Vec<(u16, bool)>>,
    pressed_buttons: RefCell<Vec<PointerButton>>,
}

define_class!(
    // Safety: NSView is designed for subclassing; all instances remain on the
    // AppKit main thread.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = RemoteViewIvars]
    pub(super) struct RemoteView;

    unsafe impl NSObjectProtocol for RemoteView {}

    unsafe impl NSWindowDelegate for RemoteView {
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _notification: &NSNotification) {
            if let Some(window) = self.window()
                && !window.makeFirstResponder(Some(self))
            {
                tracing::warn!("macOS viewer could not restore remote-input focus");
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.release_input();
        }
    }

    impl RemoteView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.send_pointer(event);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Left, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Left, false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Right, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.send_button(event, PointerButton::Right, false);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.send_button(event, mac_button(event), true);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.send_button(event, mac_button(event), false);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (x, y) = self.pointer_position(event);
            let horizontal = (event.scrollingDeltaX() * 120.0)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            let vertical = (event.scrollingDeltaY() * 120.0)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            self.send(SessionMessage::Input(RemoteInput::WheelAt {
                display_id: self.ivars().active_display.id,
                x,
                y,
                horizontal,
                vertical,
            }));
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let modifiers = event.modifierFlags();
            if modifiers.contains(NSEventModifierFlags::Control)
                && modifiers.contains(NSEventModifierFlags::Option)
                && matches!(event.keyCode(), 123 | 124)
            {
                self.select_adjacent(event.keyCode() == 124);
                return;
            }
            self.send_key(event.keyCode(), true);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.send_key(event.keyCode(), false);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let code = event.keyCode();
            if code == 57 {
                self.send_key(code, true);
                self.send_key(code, false);
                return;
            }
            let flag = match code {
                54 | 55 => NSEventModifierFlags::Command,
                56 | 60 => NSEventModifierFlags::Shift,
                58 | 61 => NSEventModifierFlags::Option,
                59 | 62 => NSEventModifierFlags::Control,
                _ => return,
            };
            self.send_key(code, event.modifierFlags().contains(flag));
        }
    }
);

impl RemoteView {
    pub(super) fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        active_display: Display,
        displays: Vec<Display>,
        video_width: u32,
        video_height: u32,
        control: ControlSink,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RemoteViewIvars {
            active_display,
            displays,
            video_width,
            video_height,
            control,
            pressed_keys: RefCell::new(Vec::new()),
            pressed_buttons: RefCell::new(Vec::new()),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn send(&self, message: SessionMessage) {
        (self.ivars().control)(message);
    }

    fn send_pointer(&self, event: &NSEvent) {
        let (x, y) = self.pointer_position(event);
        self.send(SessionMessage::Input(RemoteInput::PointerMove {
            display_id: self.ivars().active_display.id,
            x,
            y,
        }));
    }

    fn pointer_position(&self, event: &NSEvent) -> (u16, u16) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        normalized_video_position(
            point,
            self.bounds(),
            self.ivars().video_width,
            self.ivars().video_height,
        )
    }

    fn send_button(&self, event: &NSEvent, button: PointerButton, pressed: bool) {
        let (x, y) = self.pointer_position(event);
        self.send(SessionMessage::Input(RemoteInput::PointerButtonAt {
            display_id: self.ivars().active_display.id,
            x,
            y,
            button,
            pressed,
        }));
        let mut buttons = self.ivars().pressed_buttons.borrow_mut();
        if pressed {
            if !buttons.contains(&button) {
                buttons.push(button);
            }
        } else {
            buttons.retain(|candidate| *candidate != button);
        }
    }

    fn send_key(&self, key_code: u16, pressed: bool) {
        let Some((scan_code, extended)) = mac_key_to_windows_scan_code(key_code) else {
            return;
        };
        self.send(SessionMessage::Input(RemoteInput::Key {
            display_id: self.ivars().active_display.id,
            scan_code,
            extended,
            pressed,
        }));
        let mut keys = self.ivars().pressed_keys.borrow_mut();
        if pressed {
            if !keys.contains(&(scan_code, extended)) {
                keys.push((scan_code, extended));
            }
        } else {
            keys.retain(|candidate| *candidate != (scan_code, extended));
        }
    }

    fn select_adjacent(&self, next: bool) {
        let displays = &self.ivars().displays;
        if displays.len() < 2 {
            return;
        }
        let current = displays
            .iter()
            .position(|display| display.id == self.ivars().active_display.id)
            .unwrap_or(0);
        let selected = if next {
            (current + 1) % displays.len()
        } else {
            (current + displays.len() - 1) % displays.len()
        };
        self.send(SessionMessage::SelectDisplay {
            display_id: displays[selected].id,
        });
    }

    pub(super) fn release_input(&self) {
        for (scan_code, extended) in self.ivars().pressed_keys.take() {
            self.send(SessionMessage::Input(RemoteInput::Key {
                display_id: self.ivars().active_display.id,
                scan_code,
                extended,
                pressed: false,
            }));
        }
        for button in self.ivars().pressed_buttons.take() {
            self.send(SessionMessage::Input(RemoteInput::PointerButton {
                display_id: self.ivars().active_display.id,
                button,
                pressed: false,
            }));
        }
    }
}

pub(super) fn normalized_video_position(
    point: NSPoint,
    bounds: NSRect,
    video_width: u32,
    video_height: u32,
) -> (u16, u16) {
    let bounds_width = bounds.size.width.max(1.0);
    let bounds_height = bounds.size.height.max(1.0);
    let video_width = f64::from(video_width.max(1));
    let video_height = f64::from(video_height.max(1));
    let scale = (bounds_width / video_width).min(bounds_height / video_height);
    let presented_width = (video_width * scale).max(1.0);
    let presented_height = (video_height * scale).max(1.0);
    let presented_x = bounds.origin.x + (bounds_width - presented_width) / 2.0;
    let presented_y = bounds.origin.y + (bounds_height - presented_height) / 2.0;
    let normalized_x = ((point.x - presented_x) / presented_width).clamp(0.0, 1.0);
    let normalized_y = ((point.y - presented_y) / presented_height).clamp(0.0, 1.0);
    (
        (normalized_x * 65_535.0).round() as u16,
        ((1.0 - normalized_y) * 65_535.0).round() as u16,
    )
}

fn mac_button(event: &NSEvent) -> PointerButton {
    match event.buttonNumber() {
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        _ => PointerButton::Forward,
    }
}

fn mac_key_to_windows_scan_code(code: u16) -> Option<(u16, bool)> {
    Some(match code {
        0 => (0x1e, false),
        1 => (0x1f, false),
        2 => (0x20, false),
        3 => (0x21, false),
        4 => (0x23, false),
        5 => (0x22, false),
        6 => (0x2c, false),
        7 => (0x2d, false),
        8 => (0x2e, false),
        9 => (0x2f, false),
        11 => (0x30, false),
        12 => (0x10, false),
        13 => (0x11, false),
        14 => (0x12, false),
        15 => (0x13, false),
        16 => (0x15, false),
        17 => (0x14, false),
        18 => (0x02, false),
        19 => (0x03, false),
        20 => (0x04, false),
        21 => (0x05, false),
        22 => (0x07, false),
        23 => (0x06, false),
        24 => (0x0d, false),
        25 => (0x0a, false),
        26 => (0x08, false),
        27 => (0x0c, false),
        28 => (0x09, false),
        29 => (0x0b, false),
        30 => (0x1b, false),
        31 => (0x18, false),
        32 => (0x16, false),
        33 => (0x1a, false),
        34 => (0x17, false),
        35 => (0x19, false),
        36 => (0x1c, false),
        37 => (0x26, false),
        38 => (0x24, false),
        39 => (0x28, false),
        40 => (0x25, false),
        41 => (0x27, false),
        42 => (0x2b, false),
        43 => (0x33, false),
        44 => (0x35, false),
        45 => (0x31, false),
        46 => (0x32, false),
        47 => (0x34, false),
        48 => (0x0f, false),
        49 => (0x39, false),
        50 => (0x29, false),
        51 => (0x0e, false),
        53 => (0x01, false),
        54 => (0x5c, true),
        55 => (0x5b, true),
        56 => (0x2a, false),
        57 => (0x3a, false),
        58 => (0x38, false),
        59 => (0x1d, false),
        60 => (0x36, false),
        61 => (0x38, true),
        62 => (0x1d, true),
        65 => (0x53, false),
        67 => (0x37, false),
        69 => (0x4e, false),
        71 => (0x45, false),
        75 => (0x35, true),
        76 => (0x1c, true),
        78 => (0x4a, false),
        81 => (0x0d, false),
        82 => (0x52, false),
        83 => (0x4f, false),
        84 => (0x50, false),
        85 => (0x51, false),
        86 => (0x4b, false),
        87 => (0x4c, false),
        88 => (0x4d, false),
        89 => (0x47, false),
        91 => (0x48, false),
        92 => (0x49, false),
        96 => (0x3f, false),
        97 => (0x40, false),
        98 => (0x41, false),
        99 => (0x3d, false),
        100 => (0x42, false),
        101 => (0x43, false),
        103 => (0x57, false),
        109 => (0x44, false),
        111 => (0x58, false),
        114 => (0x52, true),
        115 => (0x47, true),
        116 => (0x49, true),
        117 => (0x53, true),
        118 => (0x3e, false),
        119 => (0x4f, true),
        120 => (0x3c, false),
        121 => (0x51, true),
        122 => (0x3b, false),
        123 => (0x4b, true),
        124 => (0x4d, true),
        125 => (0x50, true),
        126 => (0x48, true),
        _ => return None,
    })
}

thread_local! {
    static CONNECTING_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
}

pub(super) fn activate_application(mtm: MainThreadMarker) {
    let application = NSApplication::sharedApplication(mtm);
    // `activate` is newer than the MVP's macOS 12 deployment target.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
}

fn show_connecting_window(mtm: MainThreadMarker) -> anyhow::Result<()> {
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 420.0,
            height: 150.0,
        },
    };
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("PulseRMM Remote"));
    let view = window
        .contentView()
        .context("AppKit connecting window has no content view")?;

    let spinner = NSProgressIndicator::new(mtm);
    spinner.setStyle(NSProgressIndicatorStyle::Spinning);
    spinner.setIndeterminate(true);
    spinner.setDisplayedWhenStopped(true);
    spinner.setFrame(NSRect {
        origin: NSPoint { x: 198.0, y: 82.0 },
        size: NSSize {
            width: 24.0,
            height: 24.0,
        },
    });
    unsafe { spinner.startAnimation(None) };
    view.addSubview(&spinner);

    let label = NSTextField::labelWithString(
        &NSString::from_str("Connecting to the remote computer…"),
        mtm,
    );
    label.setAlignment(NSTextAlignment::Center);
    label.setFrame(NSRect {
        origin: NSPoint { x: 30.0, y: 45.0 },
        size: NSSize {
            width: 360.0,
            height: 24.0,
        },
    });
    view.addSubview(&label);

    window.center();
    activate_application(mtm);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    CONNECTING_WINDOW.with(|state| {
        if let Some(old) = state.borrow_mut().replace(window) {
            old.orderOut(None);
        }
    });
    Ok(())
}

pub(super) fn close_connecting_window() {
    CONNECTING_WINDOW.with(|state| {
        if let Some(window) = state.borrow_mut().take() {
            window.orderOut(None);
        }
    });
}

fn show_connection_error(mtm: MainThreadMarker, error: &str) {
    close_connecting_window();
    activate_application(mtm);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Remote connection failed"));
    alert.setInformativeText(&NSString::from_str(error));
    alert.runModal();
}

pub fn monotonic_timestamp_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn run_application<F>(network: F) -> anyhow::Result<()>
where
    F: FnOnce(Option<String>) -> anyhow::Result<()> + Send + 'static,
{
    let mtm = MainThreadMarker::new().context("PulseRMM must start on the macOS main thread")?;
    let application = NSApplication::sharedApplication(mtm);
    let (deep_link_tx, deep_link_rx) = std::sync::mpsc::channel();
    let delegate = AppDelegate::new(mtm, deep_link_tx);
    application.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    application.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    application.finishLaunching();
    show_connecting_window(mtm)?;

    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let has_command_line_session = std::env::args_os()
        .skip(1)
        .any(|argument| !argument.to_string_lossy().starts_with("-psn_"))
        || std::env::var_os("PULSERMM_HANDOFF_TOKEN").is_some();
    std::thread::Builder::new()
        .name("pulsermm-network".into())
        .spawn(move || {
            let deep_link = if has_command_line_session {
                None
            } else {
                deep_link_rx.recv_timeout(Duration::from_secs(5)).ok()
            };
            let result = network(deep_link);
            let error = result.as_ref().err().map(|error| format!("{error:#}"));
            let _ = result_tx.send(result);
            DispatchQueue::main().exec_async(move || {
                if let Some(mtm) = MainThreadMarker::new() {
                    if let Some(error) = error.as_deref() {
                        show_connection_error(mtm, error);
                    } else {
                        close_connecting_window();
                    }
                    NSApplication::sharedApplication(mtm).stop(None);
                }
            });
        })
        .context("failed to start macOS network runtime")?;

    activate_application(mtm);
    application.run();
    drop(delegate);
    result_rx
        .recv()
        .context("macOS network runtime exited without a result")?
}
