//! Who is using an X11 session's keyboard and pointer, and where the pointer
//! is. XInput 2 raw events name the device each event came from, so events
//! from the viewer's uinput devices are told apart from the user's own, as
//! the event tap's tag does on Windows and macOS. The X server also tells
//! where the pointer is, for the display menu's marker. Wayland compositors
//! tell clients neither.
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// The start of the viewer's uinput device names (see `input.rs`).
const VIEWER_DEVICE_PREFIX: &[u8] = b"MeshRMM virtual ";
/// How often the watch thread checks whether it should stop.
const STOP_CHECK_MILLISECONDS: libc::c_int = 250;

struct Shared {
    connection: RustConnection,
    root: u32,
    viewer_controls_input: AtomicBool,
    stopped: AtomicBool,
}

/// Watches the session's input until dropped.
pub(crate) struct PointerWatch {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PointerWatch {
    /// Starts watching `DISPLAY`. The user is taken to have the pointer
    /// until the viewer sends input, as on Windows.
    pub(crate) fn start() -> anyhow::Result<Self> {
        let (connection, screen) =
            RustConnection::connect(None).context("could not connect to the X server")?;
        let root = connection.setup().roots[screen].root;
        connection
            .xinput_xi_query_version(2, 2)?
            .reply()
            .context("the X server has no XInput 2")?;
        connection
            .xinput_xi_select_events(
                root,
                &[xinput::EventMask {
                    deviceid: xinput::Device::ALL_MASTER.into(),
                    mask: vec![
                        xinput::XIEventMask::RAW_KEY_PRESS
                            | xinput::XIEventMask::RAW_BUTTON_PRESS
                            | xinput::XIEventMask::RAW_MOTION,
                    ],
                }],
            )?
            .check()
            .context("the X server refused XInput 2 raw events")?;
        let shared = Arc::new(Shared {
            connection,
            root,
            viewer_controls_input: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        let watched = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("meshrmm-pointer-watch".into())
            .spawn(move || {
                if let Err(error) = watch(&watched) {
                    tracing::warn!(error = ?error, "stopped watching the session's input");
                }
            })
            .context("could not start watching the session's input")?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Whether the last keyboard or pointer event came from the viewer.
    pub(crate) fn viewer_controls_input(&self) -> bool {
        self.shared.viewer_controls_input.load(Ordering::SeqCst)
    }

    /// The pointer's position on the desktop.
    pub(crate) fn position(&self) -> Option<(i32, i32)> {
        let reply = self
            .shared
            .connection
            .query_pointer(self.shared.root)
            .ok()?
            .reply()
            .ok()?;
        reply
            .same_screen
            .then(|| (i32::from(reply.root_x), i32::from(reply.root_y)))
    }
}

impl Drop for PointerWatch {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Records whose device each raw event came from until stopped.
fn watch(shared: &Shared) -> anyhow::Result<()> {
    let mut viewer_devices = HashMap::new();
    while !shared.stopped.load(Ordering::SeqCst) {
        while let Some(event) = shared.connection.poll_for_event()? {
            let source = match event {
                Event::XinputRawKeyPress(event) => event.sourceid,
                Event::XinputRawButtonPress(event) | Event::XinputRawMotion(event) => {
                    event.sourceid
                }
                _ => continue,
            };
            if !viewer_devices.contains_key(&source) {
                // A device not seen yet, such as the viewer's, created when
                // its first input arrives.
                viewer_devices = viewer_device_map(&shared.connection)?;
            }
            let viewer = viewer_devices.get(&source).copied().unwrap_or(false);
            shared.viewer_controls_input.store(viewer, Ordering::SeqCst);
        }
        let mut descriptor = libc::pollfd {
            fd: shared.connection.stream().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for the duration of the call.
        unsafe { libc::poll(&mut descriptor, 1, STOP_CHECK_MILLISECONDS) };
    }
    Ok(())
}

/// Every input device, and whether it is one of the viewer's.
fn viewer_device_map(connection: &RustConnection) -> anyhow::Result<HashMap<u16, bool>> {
    Ok(connection
        .xinput_xi_query_device(xinput::Device::ALL)?
        .reply()?
        .infos
        .into_iter()
        .map(|device| (device.deviceid, is_viewer_device(&device.name)))
        .collect())
}

fn is_viewer_device(name: &[u8]) -> bool {
    name.starts_with(VIEWER_DEVICE_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_viewers_uinput_devices_are_recognized_by_name() {
        assert!(is_viewer_device(b"MeshRMM virtual pointer"));
        assert!(is_viewer_device(b"MeshRMM virtual keyboard"));
        assert!(!is_viewer_device(b"Virtual core XTEST pointer"));
        assert!(!is_viewer_device(b"Microsoft Vmbus HID-compliant Mouse"));
    }
}
