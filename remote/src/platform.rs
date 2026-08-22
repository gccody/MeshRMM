#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;

/// Sends viewer control messages and keeps the transport's input gate in sync
/// with the native window's foreground state.
#[derive(Clone)]
pub struct ControlSink {
    send: Arc<dyn Fn(pulsermm_protocol::SessionMessage) + Send + Sync>,
    set_input_enabled: Arc<dyn Fn(bool) + Send + Sync>,
}

impl ControlSink {
    pub fn new(
        send: impl Fn(pulsermm_protocol::SessionMessage) + Send + Sync + 'static,
        set_input_enabled: impl Fn(bool) + Send + Sync + 'static,
    ) -> Self {
        Self {
            send: Arc::new(send),
            set_input_enabled: Arc::new(set_input_enabled),
        }
    }

    pub fn send(&self, message: pulsermm_protocol::SessionMessage) {
        (self.send)(message);
    }

    pub fn set_input_enabled(&self, enabled: bool) {
        (self.set_input_enabled)(enabled);
    }
}

#[cfg(windows)]
pub use windows::{Presenter, monotonic_timestamp_us};

#[cfg(target_os = "macos")]
pub use macos::{Presenter, monotonic_timestamp_us, run_application};
