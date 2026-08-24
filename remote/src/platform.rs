#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;
use std::sync::Mutex;

/// Sends viewer control messages and keeps the transport's input gate in sync
/// with the native window's foreground state.
#[derive(Clone)]
pub struct ControlSink {
    send: Arc<dyn Fn(meshrmm_protocol::SessionMessage) + Send + Sync>,
    set_input_enabled: Arc<dyn Fn(bool) + Send + Sync>,
    quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
}

impl ControlSink {
    pub fn new(
        send: impl Fn(meshrmm_protocol::SessionMessage) + Send + Sync + 'static,
        set_input_enabled: impl Fn(bool) + Send + Sync + 'static,
        quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
    ) -> Self {
        Self {
            send: Arc::new(send),
            set_input_enabled: Arc::new(set_input_enabled),
            quality,
        }
    }

    pub fn send(&self, message: meshrmm_protocol::SessionMessage) {
        if let meshrmm_protocol::SessionMessage::SetQuality { preset } = &message
            && let Ok(mut quality) = self.quality.lock()
        {
            *quality = *preset;
        }
        (self.send)(message);
    }

    pub fn set_input_enabled(&self, enabled: bool) {
        (self.set_input_enabled)(enabled);
    }

    pub fn quality_preset(&self) -> meshrmm_protocol::QualityPreset {
        self.quality.lock().map_or_else(
            |_| meshrmm_protocol::QualityPreset::default(),
            |quality| *quality,
        )
    }
}

#[cfg(windows)]
pub use windows::{Presenter, monotonic_timestamp_us, supported_video_codecs};

#[cfg(target_os = "macos")]
pub use macos::{Presenter, monotonic_timestamp_us, run_application, supported_video_codecs};
