use super::launch::end_running_session;
use super::*;

thread_local! {
    static CONNECTING_WINDOW: RefCell<Option<ConnectingWindow>> = const { RefCell::new(None) };
}

define_class!(
    // Safety: NSObject has no subclassing requirements, and the controller
    // stays on the main thread and does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    struct ConnectingWindowController;

    // Safety: these protocols have no additional safety requirements.
    unsafe impl NSObjectProtocol for ConnectingWindowController {}

    unsafe impl NSWindowDelegate for ConnectingWindowController {
        /// The close box cancels; the launch closes the window itself.
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _window: &NSWindow) -> bool {
            cancel_connection();
            false
        }
    }

    impl ConnectingWindowController {
        #[unsafe(method(cancelConnection:))]
        fn cancel_connection_action(&self, _sender: Option<&AnyObject>) {
            cancel_connection();
        }
    }
);

impl ConnectingWindowController {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // Safety: this invokes NSObject's parameterless initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// Shown from launch until the first remote display window replaces it. Its
/// label says what the launch is waiting on.
struct ConnectingWindow {
    window: Retained<NSWindow>,
    status: Retained<NSTextField>,
    cancel: Retained<NSButton>,
    /// The Cancel button's target and the window's delegate, which AppKit
    /// holds weakly.
    _controller: Retained<ConnectingWindowController>,
    cancelling: bool,
}

/// Cancel, Esc, or the close box: end the launch without an error.
fn cancel_connection() {
    let first = CONNECTING_WINDOW.with(|state| {
        let mut state = state.borrow_mut();
        let Some(connecting) = state.as_mut() else {
            return false;
        };
        if connecting.cancelling || !connecting.cancel.isEnabled() {
            return false;
        }
        connecting.cancelling = true;
        connecting
            .status
            .setStringValue(&NSString::from_str("Cancelling…"));
        connecting.cancel.setEnabled(false);
        true
    });
    if first {
        end_running_session("the user cancelled the connection");
    }
}

pub(super) fn show_connecting_window(mtm: MainThreadMarker) -> anyhow::Result<()> {
    tracing::info!("showing macOS viewer connecting window");
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 460.0,
            height: 180.0,
        },
    };
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
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
        origin: NSPoint { x: 218.0, y: 128.0 },
        size: NSSize {
            width: 24.0,
            height: 24.0,
        },
    });
    unsafe { spinner.startAnimation(None) };
    view.addSubview(&spinner);

    // Two lines fit the longest status, an update with its restart notice.
    let status = NSTextField::wrappingLabelWithString(
        &NSString::from_str("Connecting to the remote computer…"),
        mtm,
    );
    status.setAlignment(NSTextAlignment::Center);
    status.setFrame(NSRect {
        origin: NSPoint { x: 30.0, y: 70.0 },
        size: NSSize {
            width: 400.0,
            height: 44.0,
        },
    });
    view.addSubview(&status);

    let controller = ConnectingWindowController::new(mtm);
    let cancel = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Cancel"),
            Some(&controller),
            Some(sel!(cancelConnection:)),
            mtm,
        )
    };
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    cancel.setFrame(NSRect {
        origin: NSPoint { x: 185.0, y: 18.0 },
        size: NSSize {
            width: 90.0,
            height: 32.0,
        },
    });
    view.addSubview(&cancel);
    window.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

    window.center();
    activate_application(mtm);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
    CONNECTING_WINDOW.with(|state| {
        if let Some(old) = state.borrow_mut().replace(ConnectingWindow {
            window,
            status,
            cancel,
            _controller: controller,
            cancelling: false,
        }) {
            old.window.setDelegate(None);
            old.window.orderOut(None);
        }
    });
    Ok(())
}

/// Shows what the launch is waiting on, and whether it can be cancelled now.
/// Does nothing once the connecting window has closed. After Cancel the
/// window keeps saying so.
pub fn show_launch_status(message: String, cancellable: bool) {
    DispatchQueue::main().exec_async(move || {
        CONNECTING_WINDOW.with(|state| {
            if let Some(connecting) = state.borrow().as_ref()
                && !connecting.cancelling
            {
                connecting
                    .status
                    .setStringValue(&NSString::from_str(&message));
                connecting.cancel.setEnabled(cancellable);
            }
        });
    });
}

pub(in crate::platform::macos) fn close_connecting_window() {
    CONNECTING_WINDOW.with(|state| {
        if let Some(connecting) = state.borrow_mut().take() {
            tracing::info!("closing macOS viewer connecting window");
            connecting.window.setDelegate(None);
            connecting.window.orderOut(None);
        }
    });
}
