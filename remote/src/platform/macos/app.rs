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
            tracing::info!(
                url_count = urls.len(),
                "macOS viewer received a dashboard handoff"
            );
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
                    Ok(_) => {
                        tracing::warn!(
                            "replacing the macOS viewer process for a new dashboard handoff"
                        );
                        application.terminate(None);
                    }
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

#[derive(Default)]
pub(super) struct VideoHostViewIvars;

define_class!(
    // Safety: NSView is designed for subclassing; this view remains on the
    // AppKit main thread and owns no resources requiring Drop.
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = VideoHostViewIvars]
    pub(super) struct VideoHostView;

    unsafe impl NSObjectProtocol for VideoHostView {}

    impl VideoHostView {
        /// Let the parent RemoteView receive pointer input over the video while
        /// the controls in the settings sidebar retain normal hit testing.
        #[unsafe(method_id(hitTest:))]
        #[unsafe(method_family = none)]
        fn hit_test(&self, _point: NSPoint) -> Option<Retained<Self>> {
            None
        }
    }
);

impl VideoHostView {
    pub(super) fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(VideoHostViewIvars);
        // Safety: NSView's frame initializer is the designated initializer for
        // a programmatically created view.
        unsafe { msg_send![super(this), initWithFrame: frame] }
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
    cursor_shape: RefCell<CursorShape>,
    debug: DebugInfo,
    debug_label: Retained<NSTextField>,
    debug_visible: RefCell<bool>,
    debug_refreshed: RefCell<Instant>,
    quality_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    chroma_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    settings_quality_buttons: RefCell<Vec<Retained<NSButton>>>,
    settings_chroma_buttons: RefCell<Vec<Retained<NSButton>>>,
    settings_debug_button: RefCell<Option<Retained<NSButton>>>,
    settings_window: RefCell<Option<Retained<NSWindow>>>,
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
            self.ivars().control.set_input_enabled(true);
            if let Some(window) = self.window()
                && !window.makeFirstResponder(Some(self))
            {
                tracing::warn!("macOS viewer could not restore remote-input focus");
            }
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.disable_input();
        }
    }

    impl RemoteView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            false
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let cursor = mac_cursor(*self.ivars().cursor_shape.borrow());
            let mut bounds = self.bounds();
            bounds.size.height = (bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
            self.addCursorRect_cursor(bounds, &cursor);
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
            let Some((x, y)) = self.pointer_position(event) else {
                return;
            };
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
            if event.keyCode() == 111 {
                if !event.isARepeat() {
                    self.toggle_debug();
                }
                return;
            }
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
            if event.keyCode() == 111 {
                return;
            }
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

        #[unsafe(method(selectDataSaver:))]
        fn select_data_saver(&self, _sender: &NSButton) {
            self.select_quality(QualityPreset::DataSaver);
        }

        #[unsafe(method(selectBalanced:))]
        fn select_balanced(&self, _sender: &NSButton) {
            self.select_quality(QualityPreset::Balanced);
        }

        #[unsafe(method(selectBestQuality:))]
        fn select_best_quality(&self, _sender: &NSButton) {
            self.select_quality(QualityPreset::BestQuality);
        }

        #[unsafe(method(selectChroma420:))]
        fn select_chroma_420(&self, _sender: &NSButton) {
            self.select_chroma(ChromaMode::Yuv420);
        }

        #[unsafe(method(selectChroma444:))]
        fn select_chroma_444(&self, _sender: &NSButton) {
            self.select_chroma(ChromaMode::Yuv444);
        }

        #[unsafe(method(selectDisplayFromToolbar:))]
        fn select_display_from_toolbar(&self, sender: &NSPopUpButton) {
            let index = sender.indexOfSelectedItem();
            if index >= 0
                && let Some(display) = self.ivars().displays.get(index as usize)
                && display.id != self.ivars().active_display.id
            {
                self.send(SessionMessage::SelectDisplay {
                    display_id: display.id,
                });
            }
        }

        #[unsafe(method(selectQualityFromToolbar:))]
        fn select_quality_from_toolbar(&self, sender: &NSPopUpButton) {
            let preset = match sender.indexOfSelectedItem() {
                0 => QualityPreset::DataSaver,
                2 => QualityPreset::BestQuality,
                _ => QualityPreset::Balanced,
            };
            self.select_quality(preset);
        }

        #[unsafe(method(selectChromaFromToolbar:))]
        fn select_chroma_from_toolbar(&self, sender: &NSPopUpButton) {
            let mode = if sender.indexOfSelectedItem() == 1 {
                ChromaMode::Yuv444
            } else {
                ChromaMode::Yuv420
            };
            self.select_chroma(mode);
        }

        #[unsafe(method(toggleDiagnostics:))]
        fn toggle_diagnostics_action(&self, _sender: &NSButton) {
            self.toggle_debug();
        }

        #[unsafe(method(toggleViewerFullscreen:))]
        fn toggle_viewer_fullscreen(&self, _sender: &NSButton) {
            if let Some(window) = self.window() {
                window.toggleFullScreen(None);
            }
        }

        #[unsafe(method(openViewerSettings:))]
        fn open_viewer_settings(&self, _sender: &NSButton) {
            self.open_settings();
        }
    }
);

impl RemoteView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        active_display: Display,
        displays: Vec<Display>,
        video_width: u32,
        video_height: u32,
        control: ControlSink,
        debug: DebugInfo,
    ) -> Retained<Self> {
        let debug_label =
            NSTextField::wrappingLabelWithString(&NSString::from_str("MeshRMM diagnostics"), mtm);
        debug_label.setFrame(NSRect {
            origin: NSPoint {
                x: 12.0,
                y: (frame.size.height - VIEWER_TOOLBAR_HEIGHT - 312.0).max(12.0),
            },
            size: NSSize {
                width: (frame.size.width - 24.0).clamp(300.0, 640.0),
                height: 300.0,
            },
        });
        debug_label.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        debug_label.setDrawsBackground(true);
        debug_label.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.04, 0.88)));
        debug_label.setTextColor(Some(&NSColor::whiteColor()));
        debug_label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
            12.0,
            unsafe { NSFontWeightRegular },
        )));
        debug_label.setHidden(true);
        let this = Self::alloc(mtm).set_ivars(RemoteViewIvars {
            active_display,
            displays,
            video_width,
            video_height,
            control,
            pressed_keys: RefCell::new(Vec::new()),
            pressed_buttons: RefCell::new(Vec::new()),
            cursor_shape: RefCell::new(CursorShape::Default),
            debug,
            debug_label,
            debug_visible: RefCell::new(false),
            debug_refreshed: RefCell::new(Instant::now()),
            quality_popup: RefCell::new(None),
            chroma_popup: RefCell::new(None),
            settings_quality_buttons: RefCell::new(Vec::new()),
            settings_chroma_buttons: RefCell::new(Vec::new()),
            settings_debug_button: RefCell::new(None),
            settings_window: RefCell::new(None),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        this.addSubview(&this.ivars().debug_label);
        this.install_toolbar(mtm, frame);
        this
    }

    fn install_toolbar(&self, mtm: MainThreadMarker, frame: NSRect) {
        let toolbar = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect {
                origin: NSPoint {
                    x: 0.0,
                    y: (frame.size.height - VIEWER_TOOLBAR_HEIGHT).max(0.0),
                },
                size: NSSize {
                    width: frame.size.width,
                    height: VIEWER_TOOLBAR_HEIGHT,
                },
            },
        );
        toolbar.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        toolbar.setWantsLayer(true);
        if let Some(layer) = toolbar.layer() {
            let background = NSColor::colorWithWhite_alpha(0.08, 0.96).CGColor();
            layer.setBackgroundColor(Some(&background));
        }

        let display_popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect {
                origin: NSPoint { x: 78.0, y: 6.0 },
                size: NSSize {
                    width: 160.0,
                    height: 24.0,
                },
            },
            false,
        );
        for display in &self.ivars().displays {
            display_popup.addItemWithTitle(&NSString::from_str(&display.name));
        }
        let active_index = self
            .ivars()
            .displays
            .iter()
            .position(|display| display.id == self.ivars().active_display.id)
            .unwrap_or(0);
        display_popup.selectItemAtIndex(active_index as isize);
        unsafe {
            display_popup.setTarget(Some(self));
            display_popup.setAction(Some(sel!(selectDisplayFromToolbar:)));
        }
        toolbar.addSubview(&display_popup);

        let quality_popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect {
                origin: NSPoint { x: 244.0, y: 6.0 },
                size: NSSize {
                    width: 124.0,
                    height: 24.0,
                },
            },
            false,
        );
        for title in ["Data saver", "Balanced", "Best quality"] {
            quality_popup.addItemWithTitle(&NSString::from_str(title));
        }
        quality_popup.selectItemAtIndex(quality_index(self.ivars().control.quality_preset()));
        unsafe {
            quality_popup.setTarget(Some(self));
            quality_popup.setAction(Some(sel!(selectQualityFromToolbar:)));
        }
        toolbar.addSubview(&quality_popup);
        *self.ivars().quality_popup.borrow_mut() = Some(quality_popup);

        let chroma_popup = NSPopUpButton::initWithFrame_pullsDown(
            NSPopUpButton::alloc(mtm),
            NSRect {
                origin: NSPoint { x: 374.0, y: 6.0 },
                size: NSSize {
                    width: 124.0,
                    height: 24.0,
                },
            },
            false,
        );
        for title in ["4:2:0 efficient", "4:4:4 crisp"] {
            chroma_popup.addItemWithTitle(&NSString::from_str(title));
        }
        chroma_popup.selectItemAtIndex(chroma_index(self.ivars().control.chroma_mode()));
        unsafe {
            chroma_popup.setTarget(Some(self));
            chroma_popup.setAction(Some(sel!(selectChromaFromToolbar:)));
        }
        toolbar.addSubview(&chroma_popup);
        *self.ivars().chroma_popup.borrow_mut() = Some(chroma_popup);

        let diagnostics = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Diagnostics"),
                Some(self),
                Some(sel!(toggleDiagnostics:)),
                mtm,
            )
        };
        diagnostics.setFrame(NSRect {
            origin: NSPoint { x: 504.0, y: 6.0 },
            size: NSSize {
                width: 88.0,
                height: 24.0,
            },
        });
        toolbar.addSubview(&diagnostics);

        let fullscreen = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Full screen"),
                Some(self),
                Some(sel!(toggleViewerFullscreen:)),
                mtm,
            )
        };
        fullscreen.setFrame(NSRect {
            origin: NSPoint { x: 598.0, y: 6.0 },
            size: NSSize {
                width: 84.0,
                height: 24.0,
            },
        });
        toolbar.addSubview(&fullscreen);

        let settings = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("⚙"),
                Some(self),
                Some(sel!(openViewerSettings:)),
                mtm,
            )
        };
        settings.setFrame(NSRect {
            origin: NSPoint { x: 688.0, y: 6.0 },
            size: NSSize {
                width: 34.0,
                height: 24.0,
            },
        });
        settings.setToolTip(Some(&NSString::from_str("Viewer settings")));
        toolbar.addSubview(&settings);
        self.addSubview(&toolbar);
    }

    fn open_settings(&self) {
        if let Some(window) = self.ivars().settings_window.borrow().as_ref() {
            window.makeKeyAndOrderFront(None);
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 520.0,
                height: 350.0,
            },
        };
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable;
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Viewer settings"));

        let tabs = NSTabView::initWithFrame(NSTabView::alloc(mtm), rect);

        let display_item =
            unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
        display_item.setLabel(&NSString::from_str("Display"));
        let display_pane = NSView::initWithFrame(NSView::alloc(mtm), rect);
        let display_heading =
            NSTextField::labelWithString(&NSString::from_str("Image quality"), mtm);
        display_heading.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
        display_heading.setFrame(NSRect {
            origin: NSPoint { x: 26.0, y: 248.0 },
            size: NSSize {
                width: 420.0,
                height: 26.0,
            },
        });
        display_pane.addSubview(&display_heading);
        let display_copy = NSTextField::wrappingLabelWithString(
            &NSString::from_str(
                "Choose how much bandwidth the remote desktop may use. Changes apply immediately.",
            ),
            mtm,
        );
        display_copy.setFrame(NSRect {
            origin: NSPoint { x: 26.0, y: 202.0 },
            size: NSSize {
                width: 450.0,
                height: 42.0,
            },
        });
        display_pane.addSubview(&display_copy);
        let quality_buttons = [
            unsafe {
                NSButton::radioButtonWithTitle_target_action(
                    &NSString::from_str("Data saver · 3 Mbps"),
                    Some(self),
                    Some(sel!(selectDataSaver:)),
                    mtm,
                )
            },
            unsafe {
                NSButton::radioButtonWithTitle_target_action(
                    &NSString::from_str("Balanced · 6 Mbps"),
                    Some(self),
                    Some(sel!(selectBalanced:)),
                    mtm,
                )
            },
            unsafe {
                NSButton::radioButtonWithTitle_target_action(
                    &NSString::from_str("Best quality · 12 Mbps maximum"),
                    Some(self),
                    Some(sel!(selectBestQuality:)),
                    mtm,
                )
            },
        ];
        for (index, button) in quality_buttons.iter().enumerate() {
            button.setFrame(NSRect {
                origin: NSPoint {
                    x: 26.0,
                    y: 158.0 - index as f64 * 42.0,
                },
                size: NSSize {
                    width: 430.0,
                    height: 26.0,
                },
            });
            display_pane.addSubview(button);
        }
        quality_buttons[quality_index(self.ivars().control.quality_preset()) as usize]
            .setState(NSControlStateValueOn);
        *self.ivars().settings_quality_buttons.borrow_mut() = quality_buttons.into_iter().collect();
        let chroma_heading = NSTextField::labelWithString(&NSString::from_str("Color detail"), mtm);
        chroma_heading.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        chroma_heading.setFrame(NSRect {
            origin: NSPoint { x: 270.0, y: 166.0 },
            size: NSSize {
                width: 200.0,
                height: 24.0,
            },
        });
        display_pane.addSubview(&chroma_heading);
        let chroma_buttons = [
            unsafe {
                NSButton::radioButtonWithTitle_target_action(
                    &NSString::from_str("4:2:0 · bandwidth efficient"),
                    Some(self),
                    Some(sel!(selectChroma420:)),
                    mtm,
                )
            },
            unsafe {
                NSButton::radioButtonWithTitle_target_action(
                    &NSString::from_str("4:4:4 · crisp text"),
                    Some(self),
                    Some(sel!(selectChroma444:)),
                    mtm,
                )
            },
        ];
        for (index, button) in chroma_buttons.iter().enumerate() {
            button.setFrame(NSRect {
                origin: NSPoint {
                    x: 270.0,
                    y: 126.0 - index as f64 * 42.0,
                },
                size: NSSize {
                    width: 220.0,
                    height: 26.0,
                },
            });
            display_pane.addSubview(button);
        }
        chroma_buttons[chroma_index(self.ivars().control.chroma_mode()) as usize]
            .setState(NSControlStateValueOn);
        chroma_buttons[1].setEnabled(self.ivars().control.supports_chroma(ChromaMode::Yuv444));
        *self.ivars().settings_chroma_buttons.borrow_mut() = chroma_buttons.into_iter().collect();
        display_item.setView(Some(&display_pane));
        tabs.addTabViewItem(&display_item);

        let input_item = unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
        input_item.setLabel(&NSString::from_str("Input"));
        let input_pane = NSView::initWithFrame(NSView::alloc(mtm), rect);
        let input_heading =
            NSTextField::labelWithString(&NSString::from_str("Keyboard and pointer"), mtm);
        input_heading.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
        input_heading.setFrame(NSRect {
            origin: NSPoint { x: 26.0, y: 248.0 },
            size: NSSize {
                width: 420.0,
                height: 26.0,
            },
        });
        input_pane.addSubview(&input_heading);
        let input_copy = NSTextField::wrappingLabelWithString(
            &NSString::from_str(
                "Remote input is active while the viewer is focused. Display shortcuts remain available from the top bar.",
            ),
            mtm,
        );
        input_copy.setFrame(NSRect {
            origin: NSPoint { x: 26.0, y: 186.0 },
            size: NSSize {
                width: 450.0,
                height: 56.0,
            },
        });
        input_pane.addSubview(&input_copy);
        input_item.setView(Some(&input_pane));
        tabs.addTabViewItem(&input_item);

        let advanced_item =
            unsafe { NSTabViewItem::initWithIdentifier(NSTabViewItem::alloc(), None) };
        advanced_item.setLabel(&NSString::from_str("Advanced"));
        let advanced_pane = NSView::initWithFrame(NSView::alloc(mtm), rect);
        let advanced_heading =
            NSTextField::labelWithString(&NSString::from_str("Troubleshooting"), mtm);
        advanced_heading.setFont(Some(&NSFont::boldSystemFontOfSize(17.0)));
        advanced_heading.setFrame(NSRect {
            origin: NSPoint { x: 26.0, y: 248.0 },
            size: NSSize {
                width: 420.0,
                height: 26.0,
            },
        });
        advanced_pane.addSubview(&advanced_heading);
        let diagnostics = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str("Show diagnostics overlay"),
                Some(self),
                Some(sel!(toggleDiagnostics:)),
                mtm,
            )
        };
        diagnostics.setFrame(NSRect {
            origin: NSPoint { x: 26.0, y: 196.0 },
            size: NSSize {
                width: 300.0,
                height: 28.0,
            },
        });
        diagnostics.setState(if *self.ivars().debug_visible.borrow() {
            NSControlStateValueOn
        } else {
            objc2_app_kit::NSControlStateValueOff
        });
        advanced_pane.addSubview(&diagnostics);
        *self.ivars().settings_debug_button.borrow_mut() = Some(diagnostics);
        advanced_item.setView(Some(&advanced_pane));
        tabs.addTabViewItem(&advanced_item);

        window.setContentView(Some(&tabs));
        window.center();
        window.makeKeyAndOrderFront(None);
        *self.ivars().settings_window.borrow_mut() = Some(window);
    }

    fn send(&self, message: SessionMessage) {
        self.ivars().control.send(message);
    }

    fn select_quality(&self, preset: QualityPreset) {
        let selected = quality_index(preset);
        if let Some(popup) = self.ivars().quality_popup.borrow().as_ref() {
            popup.selectItemAtIndex(selected);
        }
        for (index, button) in self
            .ivars()
            .settings_quality_buttons
            .borrow()
            .iter()
            .enumerate()
        {
            button.setState(if index as isize == selected {
                NSControlStateValueOn
            } else {
                objc2_app_kit::NSControlStateValueOff
            });
        }
        self.send(SessionMessage::SetQuality { preset });
        if let Some(window) = self.window()
            && !window.makeFirstResponder(Some(self))
        {
            tracing::warn!("macOS viewer could not restore input focus after changing quality");
        }
    }

    fn select_chroma(&self, mode: ChromaMode) {
        if !self.ivars().control.supports_chroma(mode) {
            if let Some(popup) = self.ivars().chroma_popup.borrow().as_ref() {
                popup.selectItemAtIndex(chroma_index(self.ivars().control.chroma_mode()));
            }
            return;
        }
        let selected = chroma_index(mode);
        if let Some(popup) = self.ivars().chroma_popup.borrow().as_ref() {
            popup.selectItemAtIndex(selected);
        }
        for (index, button) in self
            .ivars()
            .settings_chroma_buttons
            .borrow()
            .iter()
            .enumerate()
        {
            button.setState(if index as isize == selected {
                NSControlStateValueOn
            } else {
                objc2_app_kit::NSControlStateValueOff
            });
        }
        self.send(SessionMessage::SetChroma { mode });
    }

    pub(super) fn set_cursor_shape(&self, shape: CursorShape) {
        if *self.ivars().cursor_shape.borrow() == shape {
            return;
        }
        *self.ivars().cursor_shape.borrow_mut() = shape;
        if let Some(window) = self.window() {
            window.invalidateCursorRectsForView(self);
        }
        mac_cursor(shape).set();
    }

    fn send_pointer(&self, event: &NSEvent) {
        let Some((x, y)) = self.pointer_position(event) else {
            return;
        };
        self.send(SessionMessage::Input(RemoteInput::PointerMove {
            display_id: self.ivars().active_display.id,
            x,
            y,
        }));
    }

    fn pointer_position(&self, event: &NSEvent) -> Option<(u16, u16)> {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let mut bounds = self.bounds();
        bounds.size.height = (bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
        normalized_video_position(
            point,
            bounds,
            self.ivars().video_width,
            self.ivars().video_height,
        )
    }

    fn send_button(&self, event: &NSEvent, button: PointerButton, pressed: bool) {
        let mut buttons = self.ivars().pressed_buttons.borrow_mut();
        let position = self.pointer_position(event);
        match position {
            Some((x, y)) => self.send(SessionMessage::Input(RemoteInput::PointerButtonAt {
                display_id: self.ivars().active_display.id,
                x,
                y,
                button,
                pressed,
            })),
            None if !pressed && buttons.contains(&button) => {
                // Finish a drag that began over the video without moving the
                // remote pointer to an out-of-bounds/clamped position.
                self.send(SessionMessage::Input(RemoteInput::PointerButton {
                    display_id: self.ivars().active_display.id,
                    button,
                    pressed: false,
                }));
            }
            None => return,
        }
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

    fn toggle_debug(&self) {
        let visible = !*self.ivars().debug_visible.borrow();
        *self.ivars().debug_visible.borrow_mut() = visible;
        self.ivars().debug_label.setHidden(!visible);
        if let Some(button) = self.ivars().settings_debug_button.borrow().as_ref() {
            button.setState(if visible {
                NSControlStateValueOn
            } else {
                objc2_app_kit::NSControlStateValueOff
            });
        }
        if visible {
            self.refresh_debug(true);
        }
    }

    pub(super) fn refresh_debug(&self, force: bool) {
        if !*self.ivars().debug_visible.borrow()
            || (!force
                && self.ivars().debug_refreshed.borrow().elapsed() < Duration::from_millis(250))
        {
            return;
        }
        *self.ivars().debug_refreshed.borrow_mut() = Instant::now();
        self.ivars()
            .debug_label
            .setStringValue(&NSString::from_str(&self.ivars().debug.render()));
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

    pub(super) fn disable_input(&self) {
        self.release_input();
        self.ivars().control.set_input_enabled(false);
    }
}

fn quality_index(preset: QualityPreset) -> isize {
    match preset {
        QualityPreset::DataSaver => 0,
        QualityPreset::Balanced => 1,
        QualityPreset::BestQuality => 2,
    }
}

fn chroma_index(mode: ChromaMode) -> isize {
    match mode {
        ChromaMode::Yuv420 => 0,
        ChromaMode::Yuv444 => 1,
    }
}

pub(super) fn normalized_video_position(
    point: NSPoint,
    bounds: NSRect,
    video_width: u32,
    video_height: u32,
) -> Option<(u16, u16)> {
    let bounds_width = bounds.size.width.max(1.0);
    let bounds_height = bounds.size.height.max(1.0);
    let video_width = f64::from(video_width.max(1));
    let video_height = f64::from(video_height.max(1));
    let scale = (bounds_width / video_width).min(bounds_height / video_height);
    let presented_width = (video_width * scale).max(1.0);
    let presented_height = (video_height * scale).max(1.0);
    let presented_x = bounds.origin.x + (bounds_width - presented_width) / 2.0;
    let presented_y = bounds.origin.y + (bounds_height - presented_height) / 2.0;
    if point.x < presented_x
        || point.x > presented_x + presented_width
        || point.y < presented_y
        || point.y > presented_y + presented_height
    {
        return None;
    }
    let normalized_x = ((point.x - presented_x) / presented_width).clamp(0.0, 1.0);
    let normalized_y = ((point.y - presented_y) / presented_height).clamp(0.0, 1.0);
    Some((
        (normalized_x * 65_535.0).round() as u16,
        ((1.0 - normalized_y) * 65_535.0).round() as u16,
    ))
}

fn mac_button(event: &NSEvent) -> PointerButton {
    match event.buttonNumber() {
        2 => PointerButton::Middle,
        3 => PointerButton::Back,
        _ => PointerButton::Forward,
    }
}

#[allow(deprecated)]
fn mac_cursor(shape: CursorShape) -> Retained<NSCursor> {
    match shape {
        CursorShape::Text => NSCursor::IBeamCursor(),
        CursorShape::Crosshair => NSCursor::crosshairCursor(),
        CursorShape::ResizeWestEast => NSCursor::resizeLeftRightCursor(),
        CursorShape::ResizeNorthSouth => NSCursor::resizeUpDownCursor(),
        CursorShape::Move => NSCursor::openHandCursor(),
        CursorShape::NotAllowed => NSCursor::operationNotAllowedCursor(),
        CursorShape::Pointer => NSCursor::pointingHandCursor(),
        // AppKit has no public equivalent for these Windows system cursors.
        _ => NSCursor::arrowCursor(),
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
    tracing::info!("bringing the macOS viewer application to the foreground");
    let application = NSApplication::sharedApplication(mtm);
    // `activate` is newer than the MVP's macOS 12 deployment target.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
}

fn show_connecting_window(mtm: MainThreadMarker) -> anyhow::Result<()> {
    tracing::info!("showing macOS viewer connecting window");
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
    window.setTitle(&NSString::from_str("MeshRMM Remote"));
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
            tracing::info!("closing macOS viewer connecting window");
            window.orderOut(None);
        }
    });
}

fn show_connection_error(mtm: MainThreadMarker, error: &str) {
    tracing::error!(%error, "showing macOS viewer connection error");
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
    let mtm = MainThreadMarker::new().context("MeshRMM must start on the macOS main thread")?;
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
        || std::env::var_os("MESHRMM_HANDOFF_TOKEN").is_some();
    std::thread::Builder::new()
        .name("meshrmm-network".into())
        .spawn(move || {
            let deep_link = if has_command_line_session {
                None
            } else {
                deep_link_rx.recv_timeout(Duration::from_secs(5)).ok()
            };
            let result = network(deep_link);
            match &result {
                Ok(()) => tracing::info!("macOS viewer network session finished cleanly"),
                Err(error) => {
                    tracing::error!(error = ?error, "macOS viewer network session failed")
                }
            }
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
