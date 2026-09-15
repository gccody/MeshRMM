#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;
use std::sync::Mutex;

#[derive(Default, Clone)]
pub struct MaintenanceState {
    pub available: bool,
    pub error: Option<String>,
    pub agent_input_blocked: bool,
    pub blacked_out: bool,
}

/// Sends viewer control messages and keeps the transport's input gate in sync
/// with the native window's foreground state.
#[derive(Clone)]
pub struct ControlSink {
    files: meshrmm_file_transfer::TransferSession,
    chat: meshrmm_chat::ChatSession,
    send: Arc<dyn Fn(meshrmm_protocol::SessionMessage) + Send + Sync>,
    set_input_enabled: Arc<dyn Fn(bool) + Send + Sync>,
    maintenance: Arc<Mutex<MaintenanceState>>,
    technician_blocked: Arc<std::sync::atomic::AtomicBool>,
    quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
    chroma: Arc<Mutex<meshrmm_protocol::ChromaMode>>,
    #[cfg(windows)]
    profiles: Arc<Vec<meshrmm_protocol::VideoProfile>>,
}

impl ControlSink {
    pub fn new(
        files: meshrmm_file_transfer::TransferSession,
        send: impl Fn(meshrmm_protocol::SessionMessage) + Send + Sync + 'static,
        set_input_enabled: impl Fn(bool) + Send + Sync + 'static,
        chat: meshrmm_chat::ChatSession,
        technician_blocked: Arc<std::sync::atomic::AtomicBool>,
        maintenance: Arc<Mutex<MaintenanceState>>,
        quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
        chroma: Arc<Mutex<meshrmm_protocol::ChromaMode>>,
        #[cfg(windows)] profiles: Arc<Vec<meshrmm_protocol::VideoProfile>>,
    ) -> Self {
        Self {
            files,
            chat,
            technician_blocked,
            maintenance,
            send: Arc::new(send),
            set_input_enabled: Arc::new(set_input_enabled),
            quality,
            chroma,
            #[cfg(windows)]
            profiles,
        }
    }

    pub fn send(&self, message: meshrmm_protocol::SessionMessage) {
        if self.technician_blocked() && matches!(&message, meshrmm_protocol::SessionMessage::Input(_)) {
            return;
        }
        if let meshrmm_protocol::SessionMessage::SetQuality { preset } = &message
            && let Ok(mut quality) = self.quality.lock()
        {
            *quality = *preset;
        }
        if let meshrmm_protocol::SessionMessage::SetChroma { mode } = &message
            && let Ok(mut chroma) = self.chroma.lock()
        {
            *chroma = *mode;
        }
        (self.send)(message);
    }

    pub fn files(&self) -> meshrmm_file_transfer::TransferSession {
        self.files.clone()
    }

    pub fn chat(&self) -> meshrmm_chat::ChatSession {
        self.chat.clone()
    }

    pub fn maintenance_state(&self) -> MaintenanceState {
        self.maintenance.lock().map(|s| s.clone()).unwrap_or_default()
    }
    pub fn take_maintenance_error(&self) -> Option<String> {
        self.maintenance.lock().ok().and_then(|mut s| s.error.take())
    }
    pub fn toggle_blackout(&self) {
        let state = self.maintenance_state();
        if state.available { self.send(meshrmm_protocol::SessionMessage::SetBlackout { enabled: !state.blacked_out }); }
    }
    pub fn toggle_agent_input(&self) {
        let state = self.maintenance_state();
        if state.available {
            self.send(meshrmm_protocol::SessionMessage::SetAgentInputBlocked { blocked: !state.agent_input_blocked });
        }
    }
    pub fn agent_blocked(&self) -> bool { self.maintenance_state().agent_input_blocked }

    pub fn technician_blocked(&self) -> bool {
        self.technician_blocked.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Call after releasing held keys/buttons, before changing the gate.
    pub fn set_technician_blocked(&self, blocked: bool) {
        self.technician_blocked.store(blocked, std::sync::atomic::Ordering::SeqCst);
        if blocked { (self.set_input_enabled)(false); }
    }

    pub fn set_input_enabled(&self, enabled: bool) {
        (self.set_input_enabled)(enabled && !self.chat.visible() && !self.technician_blocked());
    }

    pub fn quality_preset(&self) -> meshrmm_protocol::QualityPreset {
        self.quality.lock().map_or_else(
            |_| meshrmm_protocol::QualityPreset::default(),
            |quality| *quality,
        )
    }

    pub fn chroma_mode(&self) -> meshrmm_protocol::ChromaMode {
        self.chroma.lock().map_or_else(
            |_| meshrmm_protocol::ChromaMode::default(),
            |chroma| *chroma,
        )
    }

    #[cfg(windows)]
    pub fn supports_chroma(&self, chroma: meshrmm_protocol::ChromaMode) -> bool {
        self.profiles.iter().any(|profile| profile.chroma == chroma)
    }
}

#[cfg(windows)]
pub use windows::{Presenter, monotonic_timestamp_us, supported_video_profiles};

#[cfg(target_os = "macos")]
pub use macos::{Presenter, monotonic_timestamp_us, run_application, supported_video_profiles};
