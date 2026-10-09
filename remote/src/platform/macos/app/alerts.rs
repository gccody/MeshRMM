use super::*;

thread_local! {
    static PENDING_ALERTS: RefCell<AlertQueue> = const { RefCell::new(AlertQueue::new()) };
}

/// Informational alerts raised while presenter state is borrowed. `runModal`
/// pumps the main queue, whose blocks borrow that state, so alerts are shown
/// one at a time from a separate main-queue block instead.
struct AlertQueue {
    pending: VecDeque<(String, String)>,
    scheduled: bool,
}

impl AlertQueue {
    const fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            scheduled: false,
        }
    }

    /// Returns whether the caller must schedule a presentation block.
    fn push(&mut self, title: impl Into<String>, message: String) -> bool {
        self.pending.push_back((title.into(), message));
        !std::mem::replace(&mut self.scheduled, true)
    }

    /// Alerts raised while one is open are shown after it by the same block.
    fn next(&mut self) -> Option<(String, String)> {
        let next = self.pending.pop_front();
        self.scheduled = next.is_some();
        next
    }
}

pub(super) fn queue_alert(title: impl Into<String>, message: String) {
    if PENDING_ALERTS.with(|alerts| alerts.borrow_mut().push(title, message)) {
        DispatchQueue::main().exec_async(present_queued_alerts);
    }
}

/// Shows the next queued alert as a sheet on the session window, then the
/// one after it once that is dismissed. A sheet leaves the run loop in its
/// default mode, so the remote screen keeps updating behind it; `runModal`
/// would stop it.
fn present_queued_alerts() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    while let Some((title, message)) = PENDING_ALERTS.with(|alerts| alerts.borrow_mut().next()) {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&title));
        alert.setInformativeText(&NSString::from_str(&message));
        let window = NSApplication::sharedApplication(mtm)
            .mainWindow()
            .filter(|window| window.attachedSheet().is_none());
        let Some(window) = window else {
            alert.runModal();
            continue;
        };
        let next = block2::RcBlock::new(|_response: objc2_app_kit::NSModalResponse| {
            present_queued_alerts()
        });
        alert.beginSheetModalForWindow_completionHandler(&window, Some(&next));
        return;
    }
}

/// Shows a notice from the network thread, such as where a recording was
/// saved, and waits until the user dismisses it.
pub fn show_notice(title: &'static str, message: &str) {
    let message = message.to_owned();
    DispatchQueue::main().exec_sync(move || {
        if let Some(mtm) = MainThreadMarker::new() {
            activate_application(mtm);
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(title));
            alert.setInformativeText(&NSString::from_str(&message));
            alert.runModal();
        }
    });
}

#[cfg(test)]
mod alert_tests {
    use super::*;

    #[test]
    fn alerts_raised_while_one_is_pending_share_one_presentation_block() {
        let mut alerts = AlertQueue::new();
        assert!(alerts.push("first", "one".into()));
        assert!(!alerts.push("second", "two".into()));
        assert_eq!(alerts.next(), Some(("first".into(), "one".into())));
        // Raised while the first modal pumps the main queue.
        assert!(!alerts.push("third", "three".into()));
        assert_eq!(alerts.next(), Some(("second".into(), "two".into())));
        assert_eq!(alerts.next(), Some(("third".into(), "three".into())));
        assert_eq!(alerts.next(), None);
        assert!(alerts.push("fourth", "four".into()));
    }
}
