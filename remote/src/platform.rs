#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

pub type ControlSink = std::sync::Arc<dyn Fn(pulsermm_protocol::SessionMessage) + Send + Sync>;

#[cfg(windows)]
pub use windows::{Presenter, monotonic_timestamp_us};

#[cfg(target_os = "macos")]
pub use macos::{Presenter, monotonic_timestamp_us, run_application};
