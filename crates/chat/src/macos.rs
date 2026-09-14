use super::*;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};

thread_local! { static UI: RefCell<Option<Ui>> = const { RefCell::new(None) }; }
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// Draw the badge directly: a text-field glyph can be clipped by its cell's
// text margins, even when the field itself fits inside the button.
define_class!(
    #[unsafe(super = NSView)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct UnreadBadge;
    unsafe impl NSObjectProtocol for UnreadBadge {}
    impl UnreadBadge {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            NSColor::systemRedColor().setFill();
            NSBezierPath::bezierPathWithOvalInRect(rect(1., 1., 10., 10.)).fill();
        }
    }
);
pub(super) struct Window {
    id: u64,
}
impl Window {
    pub fn open(state: Arc<Mutex<State>>) -> anyhow::Result<Self> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            let ui = Ui::new(id, state);
            UI.with(|slot| {
                if let Some(old) = slot.borrow_mut().replace(ui) {
                    old.window.close();
                }
            });
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { id })
    }
    pub fn refresh(&self) {
        let id = self.id;
        DispatchQueue::main().exec_async(move || {
            UI.with(|slot| {
                if let Some(ui) = slot.borrow_mut().as_mut()
                    && ui.id == id
                {
                    ui.refresh();
                }
            })
        });
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        let id = self.id;
        DispatchQueue::main().exec_async(move || {
            UI.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.as_ref().is_some_and(|ui| ui.id == id)
                    && let Some(ui) = slot.take()
                {
                    ui.window.close();
                }
            })
        });
    }
}
// Handle standard editing shortcuts within chat without changing the viewer's
// application menus or intercepting shortcuts in its remote-desktop window.
define_class!(
    #[unsafe(super = NSTextField)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ()]
    struct ChatEntry;
    unsafe impl NSObjectProtocol for ChatEntry {}
    impl ChatEntry {
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            self.handle_edit_shortcut(event)
        }
    }
);

impl ChatEntry {
    fn handle_edit_shortcut(&self, event: &NSEvent) -> bool {
        if event
            .modifierFlags()
            .contains(NSEventModifierFlags::Command)
            && let Some(editor) = self.currentEditor()
            && let Some(chars) = event.charactersIgnoringModifiers()
        {
            // These NSText actions accept a nil sender and operate only on
            // this field's active editor on the AppKit main thread.
            unsafe {
                match chars.to_string().as_str() {
                    "a" => {
                        editor.selectAll(None);
                        return true;
                    }
                    "c" => {
                        editor.copy(None);
                        return true;
                    }
                    "v" => {
                        editor.paste(None);
                        return true;
                    }
                    "x" => {
                        editor.cut(None);
                        return true;
                    }
                    _ => {}
                }
            }
        }
        unsafe { msg_send![super(self), performKeyEquivalent: event] }
    }
}

struct ActionsIvars {
    state: Arc<Mutex<State>>,
    entry: Retained<NSTextField>,
    status: Retained<NSTextField>,
}
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = ActionsIvars]
    struct Actions;
    unsafe impl NSObjectProtocol for Actions {}
    impl Actions {
        #[unsafe(method(sendChat:))]
        fn send_chat(&self, _sender: &NSObject) {
            let vars = self.ivars();
            if vars.state.lock().unwrap_or_else(|e| e.into_inner()).send(vars.entry.stringValue().to_string()) {
                vars.entry.setStringValue(&NSString::from_str(""));
                vars.status.setStringValue(&NSString::from_str("Messages are not saved. Maximum 4 KiB per message."));
            } else {
                vars.status.setStringValue(&NSString::from_str("Enter 1–4096 UTF-8 bytes, or wait for the send queue."));
            }
        }
    }
);
struct Ui {
    id: u64,
    window: Retained<NSWindow>,
    content: Content,
}
fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}
impl Ui {
    fn new(id: u64, state: Arc<Mutex<State>>) -> Self {
        let mtm = MainThreadMarker::new().expect("chat UI runs on the main thread");
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect(80., 80., 480., 400.),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe {
            window.setReleasedWhenClosed(false);
        }
        window.setTitle(&NSString::from_str("MeshRMM Chat — this session only"));
        let content = Content::new(state);
        window.setContentView(Some(&content.view));
        window.orderFront(None);
        Self {
            id,
            window,
            content,
        }
    }
    fn refresh(&mut self) {
        self.content.refresh();
    }
}
struct Content {
    view: Retained<NSView>,
    history: Retained<NSTextView>,
    actions: Retained<Actions>,
    revision: u64,
}
impl Content {
    fn new(state: Arc<Mutex<State>>) -> Self {
        let mtm = MainThreadMarker::new().expect("chat content uses the main thread");
        let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0., 0., 480., 400.));
        let status = NSTextField::labelWithString(
            &NSString::from_str("Messages are not saved. Maximum 4 KiB per message."),
            mtm,
        );
        status.setFrame(rect(12., 370., 456., 20.));
        view.addSubview(&status);
        let scroll =
            NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(12., 60., 456., 300.));
        scroll.setHasVerticalScroller(true);
        let history = NSTextView::initWithFrame(NSTextView::alloc(mtm), rect(0., 0., 436., 300.));
        history.setEditable(false);
        history.setRichText(false);
        scroll.setDocumentView(Some(&history));
        view.addSubview(&scroll);
        let entry = ChatEntry::alloc(mtm).set_ivars(());
        let entry: Retained<ChatEntry> =
            unsafe { msg_send![super(entry), initWithFrame: rect(12., 16., 360., 30.)] };
        let entry: Retained<NSTextField> = entry.into_super();
        entry.setStringValue(&NSString::from_str(
            &state.lock().unwrap_or_else(|e| e.into_inner()).draft,
        ));
        entry.setPlaceholderString(Some(&NSString::from_str("Type a message…")));
        view.addSubview(&entry);
        let actions = Actions::alloc(mtm).set_ivars(ActionsIvars {
            state,
            entry: entry.clone(),
            status,
        });
        let actions: Retained<Actions> = unsafe { msg_send![super(actions), init] };
        let send = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Send"),
                Some(&*actions),
                Some(sel!(sendChat:)),
                mtm,
            )
        };
        send.setFrame(rect(382., 16., 86., 30.));
        unsafe {
            entry.setTarget(Some(&*actions));
            entry.setAction(Some(sel!(sendChat:)));
        }
        view.addSubview(&send);
        Self {
            view,
            history,
            actions,
            revision: u64::MAX,
        }
    }
    fn save_draft(&self) {
        self.actions
            .ivars()
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .draft = self.actions.ivars().entry.stringValue().to_string();
    }
    fn refresh(&mut self) {
        let state = self
            .actions
            .ivars()
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.revision != self.revision {
            self.revision = state.revision;
            self.history.setString(&NSString::from_str(&state.text()));
            self.history
                .scrollRangeToVisible(objc2_foundation::NSRange::new(
                    self.history.string().len(),
                    0,
                ));
        }
    }
}

struct PopupIvars {
    session: ChatSession,
    popover: Retained<NSPopover>,
    button: Retained<NSButton>,
    badge: Retained<UnreadBadge>,
    content: RefCell<Content>,
    input_gate: Box<dyn Fn(bool)>,
}
define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = PopupIvars]
    struct PopupController;
    unsafe impl NSObjectProtocol for PopupController {}
    unsafe impl NSPopoverDelegate for PopupController {
        #[unsafe(method(popoverDidClose:))]
        fn did_close(&self, _notification: &objc2_foundation::NSNotification) {
            self.ivars().content.borrow().save_draft();
            self.ivars().session.set_visible(false);
            (self.ivars().input_gate)(self.ivars().button.window().is_some_and(|w| w.isKeyWindow()));
            self.refresh();
        }
    }
    impl PopupController {
        #[unsafe(method(refreshChat:))]
        fn tick(&self, _timer: &objc2_foundation::NSTimer) { self.refresh(); }
    }
);
impl PopupController {
    fn refresh(&self) {
        let vars = self.ivars();
        let available = vars.session.available();
        vars.button.setEnabled(available);
        let unread = vars.session.unread();
        vars.badge.setHidden(unread == 0);
        let label = if !available {
            "Chat is unavailable until the agent connects".to_owned()
        } else if unread > 0 {
            format!(
                "Chat — {unread} unread {}",
                if unread == 1 { "message" } else { "messages" }
            )
        } else {
            "Chat".to_owned()
        };
        vars.button.setToolTip(Some(&NSString::from_str(&label)));
        vars.button
            .setAccessibilityLabel(Some(&NSString::from_str(&label)));
        if vars.popover.isShown() {
            if !available {
                vars.popover.close();
            } else {
                vars.content.borrow_mut().refresh();
            }
        }
    }
}
/// Main-thread native popover anchored to the viewer's toolbar button.
pub struct Popup {
    controller: Retained<PopupController>,
    timer: Retained<objc2_foundation::NSTimer>,
}
impl Popup {
    pub fn new(
        session: ChatSession,
        button: &Retained<NSButton>,
        input_gate: impl Fn(bool) + 'static,
    ) -> Self {
        let mtm = MainThreadMarker::new().expect("chat popup uses the main thread");
        let popover = NSPopover::new(mtm);
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(false);
        let content = Content::new(Arc::clone(&session.state));
        let vc = NSViewController::new(mtm);
        vc.setView(&content.view);
        popover.setContentViewController(Some(&vc));
        popover.setContentSize(NSSize::new(480., 400.));
        if let Some(icon) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("bubble.left"),
            Some(&NSString::from_str("Chat")),
        ) {
            button.setImage(Some(&icon));
            button.setTitle(&NSString::from_str(""));
        }
        let badge = UnreadBadge::alloc(mtm).set_ivars(());
        let badge: Retained<UnreadBadge> =
            unsafe { msg_send![super(badge), initWithFrame: rect(32., -5., 12., 12.)] };
        badge.setHidden(true);
        button.setClipsToBounds(false);
        button.addSubview(&badge);
        let controller = PopupController::alloc(mtm).set_ivars(PopupIvars {
            session,
            popover,
            button: button.clone(),
            badge,
            content: RefCell::new(content),
            input_gate: Box::new(input_gate),
        });
        let controller: Retained<PopupController> = unsafe { msg_send![super(controller), init] };
        controller
            .ivars()
            .popover
            .setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*controller)));
        let timer = unsafe {
            objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(0.2, &controller, sel!(refreshChat:), None, true)
        };
        controller.refresh();
        Self { controller, timer }
    }
    pub fn toggle(&self) {
        let vars = self.controller.ivars();
        if vars.popover.isShown() {
            vars.popover.close();
        } else if vars.session.available() {
            vars.session.set_visible(true);
            (vars.input_gate)(false);
            vars.content.borrow_mut().refresh();
            vars.popover.showRelativeToRect_ofView_preferredEdge(
                vars.button.bounds(),
                &vars.button,
                objc2_foundation::NSRectEdge::MinY,
            );
            if let Some(window) = vars.content.borrow().view.window() {
                window.makeKeyWindow();
                window.makeFirstResponder(Some(&*vars.content.borrow().actions.ivars().entry));
            }
        }
        self.controller.refresh();
    }
}
impl Drop for Popup {
    fn drop(&mut self) {
        self.timer.invalidate();
        let vars = self.controller.ivars();
        vars.popover.setDelegate(None);
        vars.content.borrow().save_draft();
        vars.session.set_visible(false);
        vars.popover.close();
    }
}
