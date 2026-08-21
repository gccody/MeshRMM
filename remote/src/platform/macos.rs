use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSEvent, NSEventModifierFlags, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_av_foundation::{AVLayerVideoGravityResizeAspect, AVSampleBufferDisplayLayer};
use objc2_core_foundation::{CFBoolean, CFMutableDictionary, CFRetained, CFString, kCFBooleanTrue};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMSampleAttachmentKey_DisplayImmediately,
    kCMTimeInvalid,
};
use objc2_foundation::{
    NSArray, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use objc2_quartz_core::CAAutoresizingMask;
use pulsermm_protocol::{
    Codec, Display, EncodedFrame, PointerButton, RemoteInput, SessionMessage, VideoFormat,
};

use super::ControlSink;
use crate::h264::annex_b_to_avcc;

static NEXT_PRESENTER_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
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
        fn application_open_urls(&self, _application: &NSApplication, urls: &NSArray<NSURL>) {
            let Some(url) = urls.firstObject() else {
                return;
            };
            let Some(value) = url.absoluteString() else {
                return;
            };
            let _ = self.ivars().deep_link_tx.send(value.to_string());
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

struct RemoteViewIvars {
    active_display: Display,
    displays: Vec<Display>,
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
    struct RemoteView;

    unsafe impl NSObjectProtocol for RemoteView {}

    unsafe impl NSWindowDelegate for RemoteView {
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
            self.send_pointer(event);
            let horizontal = (event.scrollingDeltaX() * 120.0)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            let vertical = (event.scrollingDeltaY() * 120.0)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
            self.send(SessionMessage::Input(RemoteInput::Wheel {
                display_id: self.ivars().active_display.id,
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
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RemoteViewIvars {
            active_display,
            displays,
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
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let bounds = self.bounds();
        let width = bounds.size.width.max(1.0);
        let height = bounds.size.height.max(1.0);
        let x = ((point.x / width).clamp(0.0, 1.0) * 65_535.0).round() as u16;
        let y = ((1.0 - point.y / height).clamp(0.0, 1.0) * 65_535.0).round() as u16;
        self.send(SessionMessage::Input(RemoteInput::PointerMove {
            display_id: self.ivars().active_display.id,
            x,
            y,
        }));
    }

    fn send_button(&self, event: &NSEvent, button: PointerButton, pressed: bool) {
        self.send_pointer(event);
        self.send(SessionMessage::Input(RemoteInput::PointerButton {
            display_id: self.ivars().active_display.id,
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

    fn release_input(&self) {
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
    /// AppKit and Core Animation objects never leave the main thread.
    static UI: RefCell<Option<MacUi>> = const { RefCell::new(None) };
}

struct QueuedFrame {
    frame: EncodedFrame,
    received_at_us: u64,
}

struct Shared {
    id: u64,
    latest: Mutex<Option<QueuedFrame>>,
    scheduled: AtomicBool,
    failure: Mutex<Option<String>>,
    replaced: AtomicU64,
    submitted: AtomicU64,
    dropped_by_renderer: AtomicU64,
}

pub struct Presenter {
    shared: Arc<Shared>,
    stopped: bool,
}

impl Presenter {
    pub fn start(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
    ) -> anyhow::Result<Self> {
        if format.codec != Codec::H264 {
            bail!("macOS viewer supports only H.264");
        }
        let id = NEXT_PRESENTER_ID.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            id,
            latest: Mutex::new(None),
            scheduled: AtomicBool::new(false),
            failure: Mutex::new(None),
            replaced: AtomicU64::new(0),
            submitted: AtomicU64::new(0),
            dropped_by_renderer: AtomicU64::new(0),
        });
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            let result = MacUi::new(id, format, active_display, displays, control).map(|ui| {
                UI.with(|state| {
                    if let Some(old) = state.borrow_mut().replace(ui) {
                        old.close();
                    }
                });
            });
            let _ = started_tx.send(result);
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .context("macOS main thread did not create the viewer window")??;
        Ok(Self {
            shared,
            stopped: false,
        })
    }

    pub fn publish(&self, frame: EncodedFrame, received_at_us: u64) {
        let Ok(mut latest) = self.shared.latest.lock() else {
            return;
        };
        if latest
            .replace(QueuedFrame {
                frame,
                received_at_us,
            })
            .is_some()
        {
            self.shared.replaced.fetch_add(1, Ordering::Relaxed);
        }
        drop(latest);
        schedule_latest(Arc::clone(&self.shared));
    }

    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        self.shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take())
            .map(Err)
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let id = self.shared.id;
        DispatchQueue::main().exec_async(move || {
            UI.with(|state| {
                let mut state = state.borrow_mut();
                if state.as_ref().is_some_and(|ui| ui.id == id)
                    && let Some(ui) = state.take()
                {
                    ui.close();
                }
            });
        });
        tracing::info!(
            latest_frames_dropped = self.shared.replaced.load(Ordering::Relaxed),
            renderer_frames_dropped = self.shared.dropped_by_renderer.load(Ordering::Relaxed),
            frames_submitted = self.shared.submitted.load(Ordering::Relaxed),
            "macOS video presenter stopped"
        );
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn schedule_latest(shared: Arc<Shared>) {
    if shared
        .scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    DispatchQueue::main().exec_async(move || {
        let queued = shared.latest.lock().ok().and_then(|mut frame| frame.take());
        if let Some(queued) = queued {
            let result = UI.with(|state| {
                let mut state = state.borrow_mut();
                let ui = state
                    .as_mut()
                    .filter(|ui| ui.id == shared.id)
                    .context("macOS viewer window is no longer available")?;
                ui.enqueue(queued)
            });
            match result {
                Ok(true) => {
                    shared.submitted.fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {
                    shared.dropped_by_renderer.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    if let Ok(mut failure) = shared.failure.lock() {
                        *failure = Some(error.to_string());
                    }
                }
            }
        }
        shared.scheduled.store(false, Ordering::Release);
        if shared.latest.lock().is_ok_and(|latest| latest.is_some()) {
            schedule_latest(shared);
        }
    });
}

struct MacUi {
    id: u64,
    window: Retained<NSWindow>,
    layer: Retained<AVSampleBufferDisplayLayer>,
    input_view: Retained<RemoteView>,
    format_description: Option<CFRetained<CMFormatDescription>>,
    waiting_for_keyframe: bool,
    frames_per_second: i32,
    frames_submitted: u64,
    stats_started: Instant,
}

impl MacUi {
    fn new(
        id: u64,
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
    ) -> anyhow::Result<Self> {
        let mtm =
            MainThreadMarker::new().context("AppKit must be initialized on the main thread")?;
        let scale = (1440.0_f64 / f64::from(format.width))
            .min(900.0_f64 / f64::from(format.height))
            .min(1.0);
        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: f64::from(format.width) * scale,
                height: f64::from(format.height) * scale,
            },
        };
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // Safety: all AppKit construction occurs on the process main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // NSWindow releases itself when the user closes it by default. Keep the
        // window alive until MacUi is dropped so queued frames can safely detect
        // that it is no longer visible and end the remote session.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(&format!(
            "PulseRMM Remote Desktop — {} — Control-Option-Arrow switches display",
            active_display.name
        )));
        let view = RemoteView::new(mtm, rect, active_display, displays, control);
        window.setContentView(Some(&view));
        window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
        let layer = unsafe { AVSampleBufferDisplayLayer::new() };
        // Safety: this reads an immutable AVFoundation framework constant.
        let video_gravity = unsafe { AVLayerVideoGravityResizeAspect }
            .context("AVFoundation video gravity constant is unavailable")?;
        unsafe {
            layer.setVideoGravity(video_gravity);
            layer.setFrame(view.bounds());
            layer.setAutoresizingMask(
                CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
            );
        }
        view.setWantsLayer(true);
        view.setLayer(Some(&layer));
        window.setAcceptsMouseMovedEvents(true);
        window.makeFirstResponder(Some(&view));
        window.center();
        window.makeKeyAndOrderFront(None);

        Ok(Self {
            id,
            window,
            layer,
            input_view: view,
            format_description: None,
            waiting_for_keyframe: true,
            frames_per_second: i32::from(format.frames_per_second.max(1)),
            frames_submitted: 0,
            stats_started: Instant::now(),
        })
    }

    #[allow(deprecated)]
    fn enqueue(&mut self, queued: QueuedFrame) -> anyhow::Result<bool> {
        if !self.window.isVisible() {
            bail!("macOS viewer window was closed");
        }
        if unsafe { self.layer.requiresFlushToResumeDecoding() } {
            unsafe { self.layer.flush() };
            self.format_description = None;
            self.waiting_for_keyframe = true;
        }
        if self.waiting_for_keyframe && !queued.frame.keyframe {
            return Ok(false);
        }

        let converted = annex_b_to_avcc(&queued.frame.data)?;
        if self.format_description.is_none() {
            let sps = converted
                .sequence_parameter_set
                .as_deref()
                .context("bootstrap keyframe is missing an H.264 SPS")?;
            let pps = converted
                .picture_parameter_set
                .as_deref()
                .context("bootstrap keyframe is missing an H.264 PPS")?;
            self.format_description = Some(create_h264_format_description(sps, pps)?);
            self.waiting_for_keyframe = false;
        }
        if !unsafe { self.layer.isReadyForMoreMediaData() } {
            return Ok(false);
        }

        let sample = create_sample_buffer(
            &converted.data,
            self.format_description
                .as_deref()
                .context("H.264 format description is unavailable")?,
            queued.frame.frame_id,
            self.frames_per_second,
        )?;
        unsafe { self.layer.enqueueSampleBuffer(&sample) };
        self.frames_submitted += 1;
        let elapsed = self.stats_started.elapsed();
        if elapsed >= Duration::from_secs(2) {
            tracing::info!(
                submit_fps = self.frames_submitted as f64 / elapsed.as_secs_f64(),
                frames_submitted = self.frames_submitted,
                receive_to_submit_us =
                    monotonic_timestamp_us().saturating_sub(queued.received_at_us),
                ready_for_display = unsafe { self.layer.isReadyForDisplay() },
                codec = "h264",
                "macOS hardware decode/presentation statistics"
            );
            self.frames_submitted = 0;
            self.stats_started = Instant::now();
        }
        Ok(true)
    }

    #[allow(deprecated)]
    fn close(self) {
        self.input_view.release_input();
        unsafe { self.layer.flushAndRemoveImage() };
        self.window.orderOut(None);
    }
}

fn create_h264_format_description(
    sps: &[u8],
    pps: &[u8],
) -> anyhow::Result<CFRetained<CMFormatDescription>> {
    let mut pointers = [
        NonNull::new(sps.as_ptr().cast_mut()).context("H.264 SPS is empty")?,
        NonNull::new(pps.as_ptr().cast_mut()).context("H.264 PPS is empty")?,
    ];
    let mut sizes = [sps.len(), pps.len()];
    let mut description: *const CMFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            pointers.len(),
            NonNull::new(pointers.as_mut_ptr()).context("H.264 parameter pointers are null")?,
            NonNull::new(sizes.as_mut_ptr()).context("H.264 parameter sizes are null")?,
            4,
            NonNull::from(&mut description),
        )
    };
    check_status(status, "CoreMedia rejected H.264 parameter sets")?;
    let description = NonNull::new(description.cast_mut())
        .context("CoreMedia returned no H.264 format description")?;
    // Safety: CoreMedia returned this object at +1 retain count.
    Ok(unsafe { CFRetained::from_raw(description) })
}

fn create_sample_buffer(
    data: &[u8],
    format: &CMFormatDescription,
    frame_id: u64,
    frames_per_second: i32,
) -> anyhow::Result<CFRetained<CMSampleBuffer>> {
    if data.is_empty() {
        bail!("cannot create an empty H.264 sample");
    }
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            None,
            ptr::null(),
            0,
            data.len(),
            0,
            NonNull::from(&mut block),
        )
    };
    check_status(status, "CoreMedia H.264 block allocation failed")?;
    let block = NonNull::new(block).context("CoreMedia returned no H.264 block buffer")?;
    // Safety: CoreMedia returned this object at +1 retain count.
    let block = unsafe { CFRetained::from_raw(block) };
    let source = NonNull::new(data.as_ptr().cast_mut().cast::<c_void>())
        .context("H.264 sample pointer is null")?;
    check_status(
        unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, data.len()) },
        "CoreMedia H.264 block copy failed",
    )?;

    let timing = CMSampleTimingInfo {
        duration: unsafe { CMTime::new(1, frames_per_second) },
        presentationTimeStamp: unsafe {
            CMTime::new(
                i64::try_from(frame_id).unwrap_or(i64::MAX),
                frames_per_second,
            )
        },
        decodeTimeStamp: unsafe { kCMTimeInvalid },
    };
    let size = data.len();
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    check_status(status, "CoreMedia H.264 sample creation failed")?;
    let sample = NonNull::new(sample).context("CoreMedia returned no H.264 sample buffer")?;
    // Safety: CoreMedia returned this object at +1 retain count.
    let sample = unsafe { CFRetained::from_raw(sample) };
    set_display_immediately(&sample)?;
    Ok(sample)
}

fn set_display_immediately(sample: &CMSampleBuffer) -> anyhow::Result<()> {
    let attachments = unsafe { sample.sample_attachments_array(true) }
        .context("CoreMedia returned no sample attachment array")?;
    if attachments.count() == 0 {
        bail!("CoreMedia sample attachment array is empty");
    }
    let dictionary = unsafe { attachments.value_at_index(0) } as *const CFMutableDictionary;
    let dictionary =
        unsafe { dictionary.as_ref() }.context("CoreMedia sample attachment dictionary is null")?;
    let value: &CFBoolean =
        unsafe { kCFBooleanTrue }.context("CoreFoundation true value is unavailable")?;
    unsafe {
        CFMutableDictionary::set_value(
            Some(dictionary),
            kCMSampleAttachmentKey_DisplayImmediately as *const CFString as *const c_void,
            value as *const CFBoolean as *const c_void,
        )
    };
    Ok(())
}

fn check_status(status: i32, context: &'static str) -> anyhow::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        bail!("{context}: OSStatus {status}")
    }
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
            let _ = result_tx.send(result);
            DispatchQueue::main().exec_async(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    NSApplication::sharedApplication(mtm).stop(None);
                }
            });
        })
        .context("failed to start macOS network runtime")?;

    // `activate` is newer than the MVP's macOS 12 deployment target.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
    application.run();
    drop(delegate);
    result_rx
        .recv()
        .context("macOS network runtime exited without a result")?
}
