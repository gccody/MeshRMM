use super::connecting::show_connecting_window;
use super::*;
use objc2_app_kit::NSMenu;

struct AppDelegateIvars {
    deep_link_tx: Sender<String>,
}

/// How long a replaced viewer waits for its session to end before it starts
/// the replacement anyway. Ending a session retries for up to ~16 seconds.
const REPLACEMENT_TIMEOUT: Duration = Duration::from_secs(20);

/// A later dashboard link, started once this viewer's session has ended.
static REPLACEMENT: Mutex<Option<String>> = Mutex::new(None);

/// Whether the network session is still running, so quitting must end it.
static SESSION_RUNNING: AtomicBool = AtomicBool::new(false);

/// Starts the viewer for a pending replacement link. Returns whether one was
/// pending; each link is launched at most once.
fn launch_replacement() -> bool {
    let Some(link) = REPLACEMENT
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    else {
        return false;
    };
    let launched = std::env::current_exe()
        .context("could not locate the macOS viewer executable")
        .and_then(|executable| {
            std::process::Command::new(executable)
                .env_remove("MESHRMM_SESSION_BOOTSTRAP")
                .env_remove("MESHRMM_UPDATE_READY_FILE")
                .arg(link)
                .spawn()
                .context("could not launch the replacement macOS viewer")
        });
    match launched {
        Ok(_) => tracing::info!("started the macOS viewer for the new dashboard link"),
        Err(error) => tracing::error!(error = %error, "failed to restart the macOS viewer"),
    }
    true
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
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(
            &self,
            _application: &NSApplication,
        ) -> objc2_app_kit::NSApplicationTerminateReply {
            if presenter::request_user_disconnect() {
                objc2_app_kit::NSApplicationTerminateReply::TerminateCancel
            } else if SESSION_RUNNING.load(Ordering::Acquire) {
                // No window, for example while reconnecting or connecting:
                // end the session so the server releases the device, and
                // stop once the network thread is done.
                end_running_session("the user quit the viewer");
                objc2_app_kit::NSApplicationTerminateReply::TerminateCancel
            } else {
                objc2_app_kit::NSApplicationTerminateReply::TerminateNow
            }
        }
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, _application: &NSApplication, urls: &NSArray<NSURL>) {
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
                // replacing a stale/broken session. End this session first:
                // the server refuses the new handoff while its lease is active.
                if !presenter::confirm_session_replacement() {
                    return;
                }
                let first = REPLACEMENT
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .replace(error.0)
                    .is_none();
                tracing::warn!("ending this session to replace it with a new dashboard handoff");
                crate::shutdown::request("a new dashboard link replaces this session");
                if first {
                    let when = dispatch2::DispatchTime::try_from(REPLACEMENT_TIMEOUT)
                        .unwrap_or(dispatch2::DispatchTime::NOW);
                    let scheduled = DispatchQueue::main().after(when, || {
                        if launch_replacement() {
                            tracing::warn!(
                                "the replaced session did not end in time; exiting without its cleanup"
                            );
                            std::process::exit(0);
                        }
                    });
                    if scheduled.is_err() {
                        tracing::warn!("could not schedule the viewer replacement deadline");
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

/// Ends the network session, then the viewer once it is done; exits anyway
/// if the session does not end in time. Used by Quit and Cancel.
pub(super) fn end_running_session(reason: &'static str) {
    crate::shutdown::request(reason);
    end_after(REPLACEMENT_TIMEOUT, "the session did not end in time");
}

/// Exits after `timeout` if the network session is still running then.
fn end_after(timeout: Duration, reason: &'static str) {
    let when = dispatch2::DispatchTime::try_from(timeout).unwrap_or(dispatch2::DispatchTime::NOW);
    let scheduled = DispatchQueue::main().after(when, move || {
        if SESSION_RUNNING.load(Ordering::Acquire) {
            tracing::warn!(reason, "exiting without the session's cleanup");
            std::process::exit(0);
        }
    });
    if scheduled.is_err() {
        tracing::warn!("could not schedule the viewer exit deadline");
    }
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
    let menu = NSMenu::new(mtm);
    let item = NSMenuItem::new(mtm);
    let submenu = NSMenu::new(mtm);
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit MeshRMM Remote"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    unsafe {
        quit.setTarget(Some(&application));
    }
    submenu.addItem(&quit);
    item.setSubmenu(Some(&submenu));
    menu.addItem(&item);
    application.setMainMenu(Some(&menu));
    application.finishLaunching();
    show_connecting_window(mtm)?;

    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let has_command_line_session = std::env::args_os()
        .skip(1)
        .any(|argument| !argument.to_string_lossy().starts_with("-psn_"))
        || std::env::var_os("MESHRMM_HANDOFF_TOKEN").is_some();
    SESSION_RUNNING.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("meshrmm-network".into())
        .spawn(move || {
            let deep_link = receive_launch_link(deep_link_rx, has_command_line_session);
            let result = network(deep_link);
            match &result {
                Ok(()) => tracing::info!("macOS viewer network session finished cleanly"),
                Err(error) => {
                    tracing::error!(error = ?error, "macOS viewer network session failed")
                }
            }
            let error = result.as_ref().err().map(crate::errors::user_message);
            let _ = result_tx.send(result);
            DispatchQueue::main().exec_async(move || {
                SESSION_RUNNING.store(false, Ordering::Release);
                if let Some(mtm) = MainThreadMarker::new() {
                    // A replaced session starts its successor instead of
                    // reporting how its own cleanup went.
                    let replaced = launch_replacement();
                    match error.as_deref() {
                        Some(error) if !replaced => {
                            show_connection_error(mtm, error);
                            // A link opened while the error was shown.
                            launch_replacement();
                        }
                        _ => close_connecting_window(),
                    }
                    let application = NSApplication::sharedApplication(mtm);
                    application.stop(None);
                    // stop: changes the run-loop flag but does not wake an
                    // outstanding nextEvent wait. The network finishes on a
                    // dispatch callback, so enqueue a harmless event to let
                    // run() return even when the user provides no more input.
                    if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                        objc2_app_kit::NSEventType::ApplicationDefined,
                        NSPoint::new(0.0, 0.0),
                        NSEventModifierFlags::empty(),
                        0.0,
                        0,
                        None,
                        0,
                        0,
                        0,
                    ) {
                        application.postEvent_atStart(&event, true);
                    }
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

fn receive_launch_link(
    receiver: std::sync::mpsc::Receiver<String>,
    command_line: bool,
) -> Option<String> {
    if command_line {
        None
    } else {
        receiver.recv_timeout(Duration::from_secs(5)).ok()
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    #[test]
    fn later_links_trigger_replacement_for_both_launch_modes() {
        for command_line in [false, true] {
            let (sender, receiver) = std::sync::mpsc::channel();
            sender.send("first".to_owned()).unwrap();
            let link = receive_launch_link(receiver, command_line);
            assert_eq!(link.is_some(), !command_line);
            assert!(sender.send("second".to_owned()).is_err());
        }
    }
}
