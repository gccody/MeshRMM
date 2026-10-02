//! A window with a finished toolbox run's outcome and output, which the
//! technician can read, select and copy.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSFont, NSFontWeightRegular, NSScrollView,
    NSTextView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

thread_local! {
    /// Open output windows. AppKit does not keep a window alive by itself.
    static WINDOWS: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
}

/// Opens a window titled `title` showing `text`, in front of the viewer.
pub(super) fn show(mtm: MainThreadMarker, title: &str, text: &str) {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(720.0, 460.0));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::Resizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(title));
    window.setMinSize(NSSize::new(360.0, 200.0));

    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
    scroll.setHasVerticalScroller(true);
    scroll.setHasHorizontalScroller(false);
    scroll.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    let output = NSTextView::initWithFrame(NSTextView::alloc(mtm), frame);
    output.setEditable(false);
    output.setSelectable(true);
    output.setRichText(false);
    output.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    output.setTextContainerInset(NSSize::new(10.0, 10.0));
    output.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
        12.0,
        unsafe { NSFontWeightRegular },
    )));
    output.setString(&NSString::from_str(&text.replace("\r\n", "\n")));
    scroll.setDocumentView(Some(&output));
    window.setContentView(Some(&scroll));
    window.center();
    window.makeKeyAndOrderFront(None);

    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        // Closed windows only hide; forget them so they are released.
        windows.retain(|window| window.isVisible() || window.isMiniaturized());
        windows.push(window);
    });
}
