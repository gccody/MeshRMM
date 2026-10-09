//! The macOS connection approval prompt: a centered panel above other
//! windows with the message, the technician's reason, a countdown and Accept
//! and Deny buttons. The policy answers for the user when the Mac has been
//! locked and idle, or when nobody answers in time.
use std::cell::Cell;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSFont, NSFontWeightRegular,
    NSFontWeightSemibold, NSPanel, NSScreen, NSStatusWindowLevel, NSTextField, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString};
use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGEventType};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use super::on_main;
use crate::remote::connection_approval::{
    ApprovalPrompt, Decision, automatic_decision, remaining_seconds,
};

const TITLE: &str = "Remote connection request";
const WIDTH: f64 = 420.0;
const PADDING: f64 = 20.0;
const POLL: Duration = Duration::from_millis(250);

/// Asks the user of this session to accept the connection. Returns `None`
/// when `cancelled` says the caller gave up.
pub(crate) fn ask(prompt: &ApprovalPrompt, cancelled: impl Fn() -> bool) -> Option<Decision> {
    let started = Instant::now();
    let automatic = || automatic_decision(prompt, started.elapsed(), screen_locked(), input_idle());
    if let Some(decision) = automatic() {
        return Some(decision);
    }
    let (sender, answers) = mpsc::channel();
    let shown = prompt.clone();
    let deadline = started + prompt.timeout;
    let window = on_main(move |mtm| show(mtm, &shown, deadline, sender))
        .inspect_err(
            |error| tracing::warn!(%error, "could not show the connection approval prompt"),
        )
        .ok();
    let answer = loop {
        if let Ok(decision) = answers.recv_timeout(POLL) {
            break Some(decision);
        }
        if cancelled() {
            break None;
        }
        if let Some(decision) = automatic() {
            break Some(decision);
        }
    };
    if let Some(window) = window {
        on_main(move |_| close(window));
    }
    answer
}

/// Whether the console is at the lock screen or has nobody signed in.
fn screen_locked() -> bool {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGSessionCopyCurrentDictionary() -> Option<std::ptr::NonNull<CFDictionary>>;
    }
    // SAFETY: the function returns a +1 dictionary or null.
    let Some(session) = (unsafe { CGSessionCopyCurrentDictionary() }) else {
        // Outside a graphical session, as at the login window.
        return true;
    };
    // SAFETY: the dictionary was returned at +1.
    let session: CFRetained<CFDictionary<CFString, CFBoolean>> =
        unsafe { CFRetained::cast_unchecked(CFRetained::from_raw(session)) };
    let flag = |key: &'static str| {
        session
            .get(&CFString::from_static_str(key))
            .is_some_and(|value| value.as_bool())
    };
    flag("CGSSessionScreenIsLocked") || !flag("kCGSSessionOnConsoleKey")
}

/// How long the session has had no keyboard or pointer input.
fn input_idle() -> Duration {
    // kCGAnyInputEventType
    let any_input = CGEventType(u32::MAX);
    Duration::from_secs_f64(
        CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::CombinedSessionState,
            any_input,
        )
        .max(0.0),
    )
}

struct TargetIvars {
    answers: mpsc::Sender<Decision>,
    answered: Cell<bool>,
    countdown: Retained<NSTextField>,
    timeout: Duration,
    deadline: Instant,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeshRMMApprovalPrompt"]
    #[ivars = TargetIvars]
    struct PromptTarget;

    unsafe impl NSObjectProtocol for PromptTarget {}

    impl PromptTarget {
        #[unsafe(method(accept:))]
        fn accept(&self, _sender: Option<&AnyObject>) {
            self.answer(Decision::Accepted);
        }

        #[unsafe(method(deny:))]
        fn deny(&self, _sender: Option<&AnyObject>) {
            self.answer(Decision::Declined);
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: Option<&AnyObject>) {
            let ivars = self.ivars();
            ivars.countdown.setStringValue(&NSString::from_str(&countdown(ivars.timeout, ivars.deadline)));
        }
    }
);

impl PromptTarget {
    fn answer(&self, decision: Decision) {
        if !self.ivars().answered.replace(true) {
            let _ = self.ivars().answers.send(decision);
        }
    }
}

fn countdown(timeout: Duration, deadline: Instant) -> String {
    let elapsed = timeout.saturating_sub(deadline.saturating_duration_since(Instant::now()));
    let seconds = remaining_seconds(timeout, elapsed);
    format!(
        "Accepts automatically in {seconds} second{}.",
        if seconds == 1 { "" } else { "s" }
    )
}

/// The prompt's main-thread objects, as a raw ID the asking thread can hold.
struct Shown {
    panel: Retained<NSPanel>,
    _target: Retained<PromptTarget>,
    timer: Retained<NSTimer>,
}

thread_local! {
    static SHOWN: std::cell::RefCell<Vec<(u64, Shown)>> = const { std::cell::RefCell::new(Vec::new()) };
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn close(id: u64) {
    SHOWN.with(|shown| {
        let mut shown = shown.borrow_mut();
        if let Some(index) = shown.iter().position(|(shown_id, _)| *shown_id == id) {
            let (_, prompt) = shown.remove(index);
            prompt.timer.invalidate();
            prompt.panel.close();
        }
    });
}

fn text(
    mtm: MainThreadMarker,
    value: &str,
    size: f64,
    bold: bool,
    color: &NSColor,
) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(value), mtm);
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
    label.setPreferredMaxLayoutWidth(WIDTH - 2.0 * PADDING);
    label
}

fn show(
    mtm: MainThreadMarker,
    prompt: &ApprovalPrompt,
    deadline: Instant,
    answers: mpsc::Sender<Decision>,
) -> anyhow::Result<u64> {
    let (rows, countdown_label) = labels(mtm, prompt, deadline);
    let inner = WIDTH - 2.0 * PADDING;
    let buttons = 32.0;
    let height =
        PADDING * 2.0 + rows.iter().map(|(_, height)| height + 8.0).sum::<f64>() + buttons + 8.0;
    let panel = create_panel(mtm, height);
    let content = panel
        .contentView()
        .ok_or_else(|| anyhow::anyhow!("the prompt has no content view"))?;
    let mut top = height - PADDING;
    for (label, row_height) in &rows {
        top -= row_height;
        label.setFrame(NSRect::new(
            NSPoint::new(PADDING, top),
            NSSize::new(inner, *row_height),
        ));
        content.addSubview(label);
        top -= 8.0;
    }
    let target = PromptTarget::alloc(mtm).set_ivars(TargetIvars {
        answers,
        answered: Cell::new(false),
        countdown: countdown_label,
        timeout: prompt.timeout,
        deadline,
    });
    // SAFETY: NSObject's init is always valid.
    let target: Retained<PromptTarget> = unsafe { msg_send![super(target), init] };
    add_buttons(mtm, &content, &target, buttons);
    // SAFETY: the target and selector are valid for the timer's life.
    let timer = unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.25,
            &target,
            sel!(tick:),
            None,
            true,
        )
    };
    NSApplication::sharedApplication(mtm).activate();
    panel.makeKeyAndOrderFront(None);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SHOWN.with(|shown| {
        shown.borrow_mut().push((
            id,
            Shown {
                panel,
                _target: target,
                timer,
            },
        ))
    });
    Ok(id)
}

/// Returns the prompt's text rows with their heights, top to bottom, and the
/// countdown label, which is also the last row.
fn labels(
    mtm: MainThreadMarker,
    prompt: &ApprovalPrompt,
    deadline: Instant,
) -> (Vec<(Retained<NSTextField>, f64)>, Retained<NSTextField>) {
    let white = NSColor::whiteColor();
    let body = NSColor::colorWithSRGBRed_green_blue_alpha(0.886, 0.898, 0.922, 1.0);
    let muted = NSColor::colorWithSRGBRed_green_blue_alpha(0.580, 0.639, 0.722, 1.0);
    let title = text(mtm, TITLE, 17.0, true, &white);
    let message = text(mtm, &prompt.text, 14.0, false, &body);
    let reason = (!prompt.reason.is_empty()).then(|| {
        text(
            mtm,
            &format!("Reason: {}", prompt.reason),
            13.0,
            false,
            &body,
        )
    });
    let countdown_label = text(
        mtm,
        &countdown(prompt.timeout, deadline),
        12.0,
        false,
        &muted,
    );
    let height_of = |label: &NSTextField| label.fittingSize().height.min(240.0);
    let mut rows = vec![(title.clone(), height_of(&title))];
    rows.push((message.clone(), height_of(&message)));
    if let Some(reason) = &reason {
        rows.push((reason.clone(), height_of(reason)));
    }
    rows.push((countdown_label.clone(), height_of(&countdown_label)));
    (rows, countdown_label)
}

/// A titled panel of `height`, centered on the main screen above other windows
/// and on every Space.
fn create_panel(mtm: MainThreadMarker, height: f64) -> Retained<NSPanel> {
    let visible = NSScreen::mainScreen(mtm)
        .map(|screen| screen.visibleFrame())
        .unwrap_or(NSRect::new(NSPoint::ZERO, NSSize::new(1280.0, 800.0)));
    let frame = NSRect::new(
        NSPoint::new(
            visible.origin.x + (visible.size.width - WIDTH) / 2.0,
            visible.origin.y + (visible.size.height - height) / 2.0,
        ),
        NSSize::new(WIDTH, height),
    );
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        frame,
        NSWindowStyleMask::Titled,
        NSBackingStoreType::Buffered,
        false,
    );
    // SAFETY: the panel outlives no Rust reference to it.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setTitle(&NSString::from_str(TITLE));
    // The panel shows its title itself, on the same background.
    panel.setTitleVisibility(objc2_app_kit::NSWindowTitleVisibility::Hidden);
    panel.setTitlebarAppearsTransparent(true);
    panel.setLevel(NSStatusWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    panel.setHidesOnDeactivate(false);
    panel.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.129, 0.169, 0.220, 1.0,
    )));
    panel
}

fn add_buttons(mtm: MainThreadMarker, content: &NSView, target: &PromptTarget, height: f64) {
    // SAFETY: the target outlives the buttons' use of it.
    let (accept, deny) = unsafe {
        (
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Accept"),
                Some(target),
                Some(sel!(accept:)),
                mtm,
            ),
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Deny"),
                Some(target),
                Some(sel!(deny:)),
                mtm,
            ),
        )
    };
    accept.setKeyEquivalent(&NSString::from_str("\r"));
    accept.setFrame(NSRect::new(
        NSPoint::new(WIDTH - PADDING - 92.0, PADDING),
        NSSize::new(92.0, height),
    ));
    deny.setFrame(NSRect::new(
        NSPoint::new(WIDTH - PADDING - 2.0 * 92.0 - 8.0, PADDING),
        NSSize::new(92.0, height),
    ));
    content.addSubview(&accept);
    content.addSubview(&deny);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_down_whole_seconds() {
        let timeout = Duration::from_secs(30);
        assert_eq!(
            countdown(timeout, Instant::now() + Duration::from_millis(29_500)),
            "Accepts automatically in 30 seconds."
        );
        assert_eq!(
            countdown(timeout, Instant::now() + Duration::from_millis(500)),
            "Accepts automatically in 1 second."
        );
    }
}
