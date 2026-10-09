use super::*;

/// Shown over the video while the connection is being restored: the reason,
/// how long it has been down, when the next attempt starts, and "Retry now".
pub(super) struct ReconnectPanel {
    pub(super) panel: Retained<NSView>,
    pub(super) title: Retained<NSTextField>,
    pub(super) detail: Retained<NSTextField>,
    pub(super) retry: Retained<NSButton>,
}

impl ReconnectPanel {
    const WIDTH: f64 = 500.0;
    const HEIGHT: f64 = 120.0;

    /// A hidden panel centered over the video area of a view of `frame`.
    pub(super) fn new(mtm: MainThreadMarker, frame: NSRect) -> Self {
        let panel = NSView::initWithFrame(
            NSView::alloc(mtm),
            NSRect::new(
                NSPoint::new(
                    ((frame.size.width - Self::WIDTH) / 2.0).max(0.0),
                    ((frame.size.height - VIEWER_TOOLBAR_HEIGHT - Self::HEIGHT) / 2.0).max(0.0),
                ),
                NSSize::new(Self::WIDTH, Self::HEIGHT),
            ),
        );
        panel.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewMinXMargin
                | NSAutoresizingMaskOptions::ViewMaxXMargin
                | NSAutoresizingMaskOptions::ViewMinYMargin
                | NSAutoresizingMaskOptions::ViewMaxYMargin,
        );
        panel.setWantsLayer(true);
        if let Some(layer) = panel.layer() {
            let background = NSColor::colorWithWhite_alpha(0.04, 0.88).CGColor();
            layer.setBackgroundColor(Some(&background));
            layer.setCornerRadius(10.0);
        }
        let label = |y: f64, height: f64, font: &NSFont, white: f64| {
            let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            label.setAlignment(NSTextAlignment::Center);
            label.setTextColor(Some(&NSColor::colorWithWhite_alpha(white, 1.0)));
            label.setFont(Some(font));
            label.setFrame(NSRect::new(
                NSPoint::new(16.0, y),
                NSSize::new(Self::WIDTH - 32.0, height),
            ));
            panel.addSubview(&label);
            label
        };
        let title = label(78.0, 24.0, &NSFont::boldSystemFontOfSize(16.0), 1.0);
        let detail = label(52.0, 20.0, &NSFont::systemFontOfSize(13.0), 0.8);
        // The target is set once the view exists.
        let retry = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Retry now"),
                None,
                None,
                mtm,
            )
        };
        retry.setFrame(NSRect::new(
            NSPoint::new((Self::WIDTH - 120.0) / 2.0, 12.0),
            NSSize::new(120.0, 28.0),
        ));
        retry.setEnabled(false);
        panel.addSubview(&retry);
        panel.setHidden(true);
        Self {
            panel,
            title,
            detail,
            retry,
        }
    }
}

impl RemoteView {
    /// Shows why and for how long the connection is being restored, or
    /// hides that (`None`); input waits until then. Returns whether the
    /// caller should start refreshing the elapsed time and countdown.
    pub(in crate::platform::macos) fn set_reconnect_status(
        &self,
        status: Option<ReconnectStatus>,
    ) -> bool {
        self.ivars().reconnect_status.set(status);
        self.refresh_reconnect_status();
        status.is_some() && !self.ivars().reconnect_ticking.replace(status.is_some())
    }

    /// Redraws the reconnect panel for the current time. Returns whether it
    /// is still shown, so the refresh continues.
    pub(in crate::platform::macos) fn refresh_reconnect_status(&self) -> bool {
        let panel = &self.ivars().reconnect_panel;
        let Some(status) = self.ivars().reconnect_status.get() else {
            panel.panel.setHidden(true);
            self.ivars().reconnect_ticking.set(false);
            return false;
        };
        let text = status.render(Instant::now());
        panel.title.setStringValue(&NSString::from_str(text.title));
        panel
            .detail
            .setStringValue(&NSString::from_str(&text.detail));
        panel.retry.setEnabled(text.retry_enabled);
        panel.panel.setHidden(false);
        true
    }
}
