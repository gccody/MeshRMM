//! The macOS main thread. AppKit, and the clipboard and file helpers that
//! dispatch to the main queue, need it to run its event loop, so the Agent's
//! own work runs on another thread and the process exits when that finishes.
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_core_foundation::CFRunLoop;

/// Runs `work` on a worker thread while the main thread runs its event loop.
/// With `gui`, the loop is AppKit's, as an accessory app without a Dock icon;
/// a process without a window server connection runs a plain run loop.
pub fn run_main_loop(
    gui: bool,
    work: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) -> anyhow::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("the Agent must start on the macOS main thread"))?;
    std::thread::Builder::new()
        .name("meshrmm-agent".into())
        .spawn(move || {
            let code = match work() {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("Error: {error:?}");
                    1
                }
            };
            std::process::exit(code);
        })?;
    if gui {
        let application = NSApplication::sharedApplication(mtm);
        application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        application.run();
    }
    loop {
        CFRunLoop::run();
    }
}
