//! What the Mac's user sees of a session: the menu bar item that opens the
//! chat, the session banner, the connection notification, the display border
//! and the technician's annotations.
//!
//! AppKit objects live on the main thread, in a registry keyed by the IDs the
//! handles here hold; dropping a handle closes its windows there.
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use meshrmm_protocol::Display;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSBezierPath, NSColor, NSFont, NSFontWeightRegular, NSFontWeightSemibold,
    NSImage, NSLineCapStyle, NSLineJoinStyle, NSPanel, NSScreenSaverWindowLevel, NSStatusBar,
    NSStatusItem, NSStatusWindowLevel, NSTextField, NSVariableStatusItemLength, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use super::on_main;

use crate::remote::annotation::{Dirty, Strokes, display_pixel};

/// The annotation ink and display border color, as on Windows.
const ACCENT: (f64, f64, f64) = (0.898, 0.208, 0.208);
const BORDER_WIDTH: f64 = 4.0;
const PEN_WIDTH: i32 = 5;
const NOTIFICATION_WIDTH: f64 = 360.0;
const NOTIFICATION_MARGIN: f64 = 16.0;
const NOTIFICATION_VISIBLE_FOR: Duration = Duration::from_secs(15);

thread_local! {
    /// Main-thread UI objects by handle ID.
    static OBJECTS: RefCell<HashMap<u64, Box<dyn Any>>> = RefCell::new(HashMap::new());
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A UI object on the main thread, closed when the handle is dropped.
struct MainThreadBox {
    id: u64,
}

impl MainThreadBox {
    fn new<T: 'static>(
        create: impl FnOnce(MainThreadMarker) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<Self> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        on_main(move |mtm| {
            let object = create(mtm)?;
            OBJECTS.with(|objects| objects.borrow_mut().insert(id, Box::new(object)));
            Ok(Self { id })
        })
    }

    fn with<T: 'static, R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut T) -> R + Send + 'static,
    ) -> Option<R> {
        let id = self.id;
        on_main(move |_| {
            OBJECTS.with(|objects| {
                objects
                    .borrow_mut()
                    .get_mut(&id)
                    .and_then(|object| object.downcast_mut::<T>())
                    .map(work)
            })
        })
    }
}

impl Drop for MainThreadBox {
    fn drop(&mut self) {
        let id = self.id;
        dispatch2::DispatchQueue::main().exec_async(move || {
            let object = OBJECTS.with(|objects| objects.borrow_mut().remove(&id));
            drop(object);
        });
    }
}

/// The global AppKit frame of a rectangle in Quartz display coordinates,
/// whose origin is the main display's top left.
fn appkit_frame(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    let main_height = CGDisplayBounds(CGMainDisplayID()).size.height;
    NSRect::new(
        NSPoint::new(x, main_height - y - height),
        NSSize::new(width, height),
    )
}

fn accent() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(ACCENT.0, ACCENT.1, ACCENT.2, 1.0)
}

/// A borderless, click-through window above everything, on every Space.
fn overlay_window(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSWindow> {
    // SAFETY: a borderless window with a valid frame.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the window outlives no Rust reference to it.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setHasShadow(false);
    window.setIgnoresMouseEvents(true);
    window.setLevel(NSScreenSaverWindowLevel);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window
}

/// An outline around the shared display that the technician does not see:
/// capture leaves its windows out.
pub(crate) struct DisplayBorder {
    windows: MainThreadBox,
    window_ids: Vec<u32>,
}

impl DisplayBorder {
    pub(crate) fn show(display: &Display) -> anyhow::Result<Self> {
        let (x, y) = (f64::from(display.x), f64::from(display.y));
        let (width, height) = (f64::from(display.width), f64::from(display.height));
        let (sender, receiver) = std::sync::mpsc::channel();
        let windows = MainThreadBox::new(move |mtm| {
            let edges = [
                (x, y, width, BORDER_WIDTH),
                (x, y + height - BORDER_WIDTH, width, BORDER_WIDTH),
                (x, y, BORDER_WIDTH, height),
                (x + width - BORDER_WIDTH, y, BORDER_WIDTH, height),
            ];
            let windows = edges
                .into_iter()
                .map(|(x, y, width, height)| {
                    let window = overlay_window(mtm, appkit_frame(x, y, width, height));
                    window.setBackgroundColor(Some(&accent()));
                    window.setOpaque(true);
                    window.orderFrontRegardless();
                    window
                })
                .collect::<Vec<_>>();
            let _ = sender.send(
                windows
                    .iter()
                    .map(|window| window.windowNumber() as u32)
                    .collect::<Vec<_>>(),
            );
            Ok(windows)
        })?;
        Ok(Self {
            windows,
            window_ids: receiver.recv().unwrap_or_default(),
        })
    }

    pub(crate) fn window_ids(&self) -> &[u32] {
        &self.window_ids
    }
}

impl Drop for DisplayBorder {
    fn drop(&mut self) {
        self.windows.with(|windows: &mut Vec<Retained<NSWindow>>| {
            for window in windows {
                window.close();
            }
        });
    }
}

struct AnnotationIvars {
    strokes: Arc<Mutex<Strokes>>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeshRMMAnnotationView"]
    #[ivars = AnnotationIvars]
    struct AnnotationView;

    unsafe impl NSObjectProtocol for AnnotationView {}

    impl AnnotationView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let strokes = self.ivars().strokes.lock().unwrap_or_else(|e| e.into_inner());
            accent().setStroke();
            for stroke in &strokes.strokes {
                let path = NSBezierPath::bezierPath();
                path.setLineWidth(f64::from(strokes.pen_width));
                path.setLineCapStyle(NSLineCapStyle::Round);
                path.setLineJoinStyle(NSLineJoinStyle::Round);
                let mut points = stroke.iter();
                if let Some(&(x, y)) = points.next() {
                    path.moveToPoint(NSPoint::new(f64::from(x), f64::from(y)));
                    // A single point still shows as a dot.
                    path.lineToPoint(NSPoint::new(f64::from(x), f64::from(y)));
                }
                for &(x, y) in points {
                    path.lineToPoint(NSPoint::new(f64::from(x), f64::from(y)));
                }
                path.stroke();
            }
        }
    }
);

/// The technician's strokes over one display, which the capture includes.
pub(crate) struct AnnotationOverlay {
    window: MainThreadBox,
    strokes: Arc<Mutex<Strokes>>,
    width: u32,
    height: u32,
}

impl AnnotationOverlay {
    pub(crate) fn show(display: &Display) -> anyhow::Result<Self> {
        let strokes = Arc::new(Mutex::new(Strokes::new(PEN_WIDTH)));
        let painted = Arc::clone(&strokes);
        let frame = appkit_frame(
            f64::from(display.x),
            f64::from(display.y),
            f64::from(display.width),
            f64::from(display.height),
        );
        let window = MainThreadBox::new(move |mtm| {
            let window = overlay_window(mtm, frame);
            let view = AnnotationView::alloc(mtm).set_ivars(AnnotationIvars { strokes: painted });
            // SAFETY: NSView's initWithFrame: is its designated initializer.
            let view: Retained<AnnotationView> = unsafe {
                msg_send![super(view), initWithFrame: NSRect::new(NSPoint::ZERO, frame.size)]
            };
            window.setContentView(Some(&view));
            window.orderFrontRegardless();
            Ok(window)
        })?;
        Ok(Self {
            window,
            strokes,
            width: display.width,
            height: display.height,
        })
    }

    pub(crate) fn draw(&self, x: u16, y: u16, start: bool) {
        let point = display_pixel(self.width, self.height, x, y);
        let dirty = self
            .strokes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .add(point, start);
        if let Some(dirty) = dirty {
            self.window.with(move |window: &mut Retained<NSWindow>| {
                if let Some(view) = window.contentView() {
                    match dirty {
                        Dirty::All => view.setNeedsDisplay(true),
                        Dirty::Rect(left, top, right, bottom) => {
                            view.setNeedsDisplayInRect(NSRect::new(
                                NSPoint::new(f64::from(left), f64::from(top)),
                                NSSize::new(f64::from(right - left), f64::from(bottom - top)),
                            ))
                        }
                    }
                }
            });
        }
    }
}

impl Drop for AnnotationOverlay {
    fn drop(&mut self) {
        self.window
            .with(|window: &mut Retained<NSWindow>| window.close());
    }
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    bold: bool,
    color: &NSColor,
) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    // SAFETY: the weights are valid static AppKit constants.
    let weight = unsafe {
        if bold {
            NSFontWeightSemibold
        } else {
            NSFontWeightRegular
        }
    };
    label.setFont(Some(&NSFont::systemFontOfSize_weight(size, weight)));
    label.setTextColor(Some(color));
    label
}

/// A panel that floats above other windows without taking focus.
fn floating_panel(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSPanel> {
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: the panel outlives no Rust reference to it.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setFloatingPanel(true);
    panel.setHidesOnDeactivate(false);
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel
}

/// A rounded dark view to put a panel's content on.
fn card(mtm: MainThreadMarker, size: NSSize, radius: f64) -> Retained<NSView> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::ZERO, size));
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        let background = NSColor::colorWithSRGBRed_green_blue_alpha(0.129, 0.169, 0.220, 0.97);
        layer.setBackgroundColor(Some(&background.CGColor()));
        layer.setCornerRadius(radius);
    }
    view
}

/// "Remote session started" in the main display's bottom right corner, for
/// 15 seconds. The capture includes it, so the technician sees it too.
pub(crate) struct ConnectionNotification(MainThreadBox);

impl ConnectionNotification {
    pub(crate) fn show(text: &str) -> anyhow::Result<Self> {
        let text = text.to_owned();
        let notification = MainThreadBox::new(move |mtm| {
            let white = NSColor::whiteColor();
            let body_color = NSColor::colorWithSRGBRed_green_blue_alpha(0.886, 0.898, 0.922, 1.0);
            let title = label(mtm, "Remote session started", 15.0, true, &white);
            let body = label(mtm, &text, 13.0, false, &body_color);
            let inner = NOTIFICATION_WIDTH - 32.0;
            body.setPreferredMaxLayoutWidth(inner);
            let body_height = body.fittingSize().height.min(320.0);
            let height = 16.0 + 20.0 + 6.0 + body_height + 16.0;
            let visible = objc2_app_kit::NSScreen::mainScreen(mtm)
                .map(|screen| screen.visibleFrame())
                .unwrap_or(NSRect::new(NSPoint::ZERO, NSSize::new(1280.0, 800.0)));
            let frame = NSRect::new(
                NSPoint::new(
                    visible.origin.x + visible.size.width
                        - NOTIFICATION_WIDTH
                        - NOTIFICATION_MARGIN,
                    visible.origin.y + NOTIFICATION_MARGIN,
                ),
                NSSize::new(NOTIFICATION_WIDTH, height),
            );
            let panel = floating_panel(mtm, frame);
            let content = card(mtm, frame.size, 10.0);
            title.setFrame(NSRect::new(
                NSPoint::new(16.0, height - 16.0 - 20.0),
                NSSize::new(inner, 20.0),
            ));
            body.setFrame(NSRect::new(
                NSPoint::new(16.0, 16.0),
                NSSize::new(inner, body_height),
            ));
            content.addSubview(&title);
            content.addSubview(&body);
            panel.setContentView(Some(&content));
            // Clicks pass through; it closes by itself.
            panel.setIgnoresMouseEvents(true);
            panel.orderFrontRegardless();
            let closing = panel.clone();
            let block = RcBlock::new(move |_timer: std::ptr::NonNull<NSTimer>| closing.close());
            // SAFETY: the block closes the panel once.
            let timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                    NOTIFICATION_VISIBLE_FOR.as_secs_f64(),
                    false,
                    &block,
                )
            };
            Ok((panel, timer))
        })?;
        Ok(Self(notification))
    }
}

impl Drop for ConnectionNotification {
    fn drop(&mut self) {
        self.0.with(
            |(panel, timer): &mut (Retained<NSPanel>, Retained<NSTimer>)| {
                timer.invalidate();
                panel.close();
            },
        );
    }
}

struct IndicatorIvars {
    popup: RefCell<Option<meshrmm_chat::ChatPopup>>,
    button: RefCell<Option<Retained<objc2_app_kit::NSStatusBarButton>>>,
    chat: meshrmm_chat::ChatSession,
    /// Unread messages already shown, so a dismissed chat does not reopen
    /// until another message arrives.
    seen_unread: Cell<usize>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeshRMMSessionIndicator"]
    #[ivars = IndicatorIvars]
    struct IndicatorTarget;

    unsafe impl NSObjectProtocol for IndicatorTarget {}

    impl IndicatorTarget {
        #[unsafe(method(toggleChat:))]
        fn toggle_chat(&self, _sender: Option<&AnyObject>) {
            self.toggle();
        }

        #[unsafe(method(checkChat:))]
        fn check_chat(&self, _timer: Option<&AnyObject>) {
            let unread = self.ivars().chat.unread();
            if unread > self.ivars().seen_unread.get() && !self.ivars().chat.visible() {
                self.toggle();
            }
            self.ivars().seen_unread.set(unread);
        }
    }
);

impl IndicatorTarget {
    fn toggle(&self) {
        let ivars = self.ivars();
        if let (Some(popup), Some(button)) = (&*ivars.popup.borrow(), &*ivars.button.borrow()) {
            popup.toggle(button.bounds());
        }
    }
}

struct Indicator {
    item: Retained<NSStatusItem>,
    banner: Option<Retained<NSPanel>>,
    target: Retained<IndicatorTarget>,
    timer: Retained<NSTimer>,
}

/// The session's menu bar item, which opens the chat, and the banner that
/// says who is connected unless company policy hides it.
pub(crate) struct SessionIndicator(MainThreadBox);

impl SessionIndicator {
    pub(crate) fn show(
        viewer_name: &str,
        chat: meshrmm_chat::ChatSession,
        show_banner: bool,
    ) -> anyhow::Result<Self> {
        let name = meshrmm_protocol::session_viewer_name(viewer_name);
        let indicator = MainThreadBox::new(move |mtm| {
            let item =
                NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
            let button = item
                .button(mtm)
                .ok_or_else(|| anyhow::anyhow!("the menu bar item has no button"))?;
            let tooltip =
                NSString::from_str(&format!("{name} is connected remotely; click to chat"));
            match NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str("display.and.arrow.down"),
                Some(&tooltip),
            ) {
                Some(image) => button.setImage(Some(&image)),
                None => button.setTitle(&NSString::from_str("MeshRMM")),
            }
            button.setToolTip(Some(&tooltip));
            let target = IndicatorTarget::alloc(mtm).set_ivars(IndicatorIvars {
                popup: RefCell::new(None),
                button: RefCell::new(Some(button.clone())),
                chat: chat.clone(),
                seen_unread: Cell::new(chat.unread()),
            });
            // SAFETY: NSObject's init is always valid.
            let target: Retained<IndicatorTarget> = unsafe { msg_send![super(target), init] };
            *target.ivars().popup.borrow_mut() =
                Some(meshrmm_chat::ChatPopup::new(chat, &button, |_| {}));
            // SAFETY: the target outlives the button's use of it.
            unsafe {
                button.setTarget(Some(&target));
                button.setAction(Some(sel!(toggleChat:)));
            }
            // SAFETY: the target and selector are valid for the timer's life.
            let timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.5,
                    &target,
                    sel!(checkChat:),
                    None,
                    true,
                )
            };
            let banner = show_banner.then(|| banner(mtm, &name));
            Ok(Indicator {
                item,
                banner,
                target,
                timer,
            })
        })?;
        Ok(Self(indicator))
    }
}

impl Drop for SessionIndicator {
    fn drop(&mut self) {
        self.0.with(|indicator: &mut Indicator| {
            indicator.timer.invalidate();
            indicator.target.ivars().popup.borrow_mut().take();
            NSStatusBar::systemStatusBar().removeStatusItem(&indicator.item);
            if let Some(banner) = &indicator.banner {
                banner.close();
            }
        });
    }
}

/// "● {name} is connected remotely" under the menu bar, centered.
fn banner(mtm: MainThreadMarker, name: &str) -> Retained<NSPanel> {
    let text = label(
        mtm,
        &format!("● {name} is connected remotely"),
        13.0,
        true,
        &NSColor::whiteColor(),
    );
    text.setMaximumNumberOfLines(1);
    let size = text.fittingSize();
    let width = (size.width + 28.0).min(640.0);
    let height = size.height + 12.0;
    let visible = objc2_app_kit::NSScreen::mainScreen(mtm)
        .map(|screen| screen.visibleFrame())
        .unwrap_or(NSRect::new(NSPoint::ZERO, NSSize::new(1280.0, 800.0)));
    let frame = NSRect::new(
        NSPoint::new(
            visible.origin.x + (visible.size.width - width) / 2.0,
            visible.origin.y + visible.size.height - height - 6.0,
        ),
        NSSize::new(width, height),
    );
    let panel = floating_panel(mtm, frame);
    let content = card(mtm, frame.size, height / 2.0);
    text.setFrame(NSRect::new(
        NSPoint::new(14.0, 6.0),
        NSSize::new(width - 28.0, size.height),
    ));
    content.addSubview(&text);
    panel.setContentView(Some(&content));
    // It only informs; clicks reach the window under it.
    panel.setIgnoresMouseEvents(true);
    panel.orderFrontRegardless();
    panel
}

/// Black screens with the company's maintenance message, which the
/// technician does not see: capture leaves them out.
pub(crate) struct Blackout {
    windows: MainThreadBox,
    window_ids: Vec<u32>,
}

impl Blackout {
    pub(crate) fn show(message: &str) -> anyhow::Result<Self> {
        let message = message.to_owned();
        let (sender, receiver) = std::sync::mpsc::channel();
        let windows = MainThreadBox::new(move |mtm| {
            let windows = objc2_app_kit::NSScreen::screens(mtm)
                .iter()
                .map(|screen| {
                    let frame = screen.frame();
                    let window = overlay_window(mtm, frame);
                    window.setBackgroundColor(Some(&NSColor::blackColor()));
                    window.setOpaque(true);
                    // Clicks land on the black screen, not the apps under it.
                    window.setIgnoresMouseEvents(false);
                    let text = label(mtm, &message, 22.0, false, &NSColor::whiteColor());
                    text.setAlignment(objc2_app_kit::NSTextAlignment::Center);
                    let width = (frame.size.width - 160.0).max(200.0);
                    text.setPreferredMaxLayoutWidth(width);
                    let height = text.fittingSize().height;
                    text.setFrame(NSRect::new(
                        NSPoint::new(
                            (frame.size.width - width) / 2.0,
                            (frame.size.height - height) / 2.0,
                        ),
                        NSSize::new(width, height),
                    ));
                    if let Some(content) = window.contentView() {
                        content.addSubview(&text);
                    }
                    window.orderFrontRegardless();
                    window
                })
                .collect::<Vec<_>>();
            let _ = sender.send(
                windows
                    .iter()
                    .map(|window| window.windowNumber() as u32)
                    .collect::<Vec<_>>(),
            );
            Ok(windows)
        })?;
        Ok(Self {
            windows,
            window_ids: receiver.recv().unwrap_or_default(),
        })
    }

    pub(crate) fn window_ids(&self) -> &[u32] {
        &self.window_ids
    }
}

impl Drop for Blackout {
    fn drop(&mut self) {
        self.windows.with(|windows: &mut Vec<Retained<NSWindow>>| {
            for window in windows {
                window.close();
            }
        });
    }
}
