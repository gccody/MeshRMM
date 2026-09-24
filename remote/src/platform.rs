#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;
use std::sync::Mutex;

#[derive(Default)]
pub struct IdlePreference {
    pub policy: meshrmm_protocol::IdlePolicy,
    pub choice: Option<bool>,
}

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
    idle: Arc<Mutex<IdlePreference>>,
    display_border: Arc<Mutex<Option<bool>>>,
    files: meshrmm_file_transfer::TransferSession,
    chat: meshrmm_chat::ChatSession,
    audio: meshrmm_audio::PlaybackState,
    recording: crate::recording::Recorder,
    send: Arc<dyn Fn(meshrmm_protocol::SessionMessage) + Send + Sync>,
    set_input_enabled: Arc<dyn Fn(bool) + Send + Sync>,
    maintenance: Arc<Mutex<MaintenanceState>>,
    credentials: Arc<Mutex<meshrmm_protocol::CredentialState>>,
    technician_blocked: Arc<std::sync::atomic::AtomicBool>,
    wallpaper_hidden: Arc<std::sync::atomic::AtomicBool>,
    remote_cursor_hidden: Arc<std::sync::atomic::AtomicBool>,
    session_close_action: Arc<Mutex<meshrmm_protocol::SessionCloseAction>>,
    quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
    chroma: Arc<Mutex<meshrmm_protocol::ChromaMode>>,
    #[cfg(windows)]
    profiles: Arc<Vec<meshrmm_protocol::VideoProfile>>,
}

impl ControlSink {
    // Keep the shared session handles and transport callbacks explicit at construction.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        idle: Arc<Mutex<IdlePreference>>,
        display_border: Arc<Mutex<Option<bool>>>,
        files: meshrmm_file_transfer::TransferSession,
        send: impl Fn(meshrmm_protocol::SessionMessage) + Send + Sync + 'static,
        set_input_enabled: impl Fn(bool) + Send + Sync + 'static,
        chat: meshrmm_chat::ChatSession,
        audio: meshrmm_audio::PlaybackState,
        recording: crate::recording::Recorder,
        technician_blocked: Arc<std::sync::atomic::AtomicBool>,
        remote_cursor_hidden: Arc<std::sync::atomic::AtomicBool>,
        wallpaper_hidden: Arc<std::sync::atomic::AtomicBool>,
        session_close_action: Arc<Mutex<meshrmm_protocol::SessionCloseAction>>,
        maintenance: Arc<Mutex<MaintenanceState>>,
        quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
        chroma: Arc<Mutex<meshrmm_protocol::ChromaMode>>,
        #[cfg(windows)] profiles: Arc<Vec<meshrmm_protocol::VideoProfile>>,
    ) -> Self {
        Self {
            idle,
            display_border,
            files,
            chat,
            audio,
            recording,
            technician_blocked,
            remote_cursor_hidden,
            session_close_action,
            wallpaper_hidden,
            maintenance,
            credentials: Arc::new(Mutex::new(Default::default())),
            send: Arc::new(send),
            set_input_enabled: Arc::new(set_input_enabled),
            quality,
            chroma,
            #[cfg(windows)]
            profiles,
        }
    }

    pub fn credential_state(&self) -> meshrmm_protocol::CredentialState {
        self.credentials
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
    pub fn with_credentials(
        mut self,
        state: Arc<Mutex<meshrmm_protocol::CredentialState>>,
    ) -> Self {
        self.credentials = state;
        self
    }

    pub fn send(&self, message: meshrmm_protocol::SessionMessage) {
        if self.technician_blocked()
            && matches!(
                &message,
                meshrmm_protocol::SessionMessage::Input(_)
                    | meshrmm_protocol::SessionMessage::SendSecureAttention
                    | meshrmm_protocol::SessionMessage::PromptForCredentials
                    | meshrmm_protocol::SessionMessage::AutofillCredentials
            )
        {
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

    pub fn recording(&self) -> &crate::recording::Recorder {
        &self.recording
    }

    pub fn toggle_recording(&self) {
        if let Some(stream_id) = self.recording.toggle() {
            self.send(meshrmm_protocol::SessionMessage::RequestKeyframe { stream_id });
        }
    }

    pub fn audio_muted(&self) -> bool {
        self.audio.muted()
    }

    pub fn toggle_audio(&self) {
        self.audio.toggle();
    }

    pub fn type_clipboard(&self, display_id: meshrmm_protocol::DisplayId) {
        if self.technician_blocked() {
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            let text = crate::clipboard::ClipboardSync::new(false)?.text()?;
            anyhow::ensure!(
                text.len() <= meshrmm_protocol::MAX_CLIPBOARD_TEXT_BYTES,
                "Clipboard text is too large to type (maximum 60 KiB)"
            );
            anyhow::ensure!(
                !text.contains('\0'),
                "Clipboard text contains a null character"
            );
            if !text.is_empty() {
                self.set_input_enabled(true);
                self.send(meshrmm_protocol::SessionMessage::Input(
                    meshrmm_protocol::RemoteInput::TypeText { display_id, text },
                ));
            }
            Ok(())
        })();
        if let Err(error) = result
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(format!("Type clipboard: {error}"));
        }
    }

    pub fn send_secure_attention(&self) {
        self.send(meshrmm_protocol::SessionMessage::SendSecureAttention);
    }

    pub fn files(&self) -> meshrmm_file_transfer::TransferSession {
        self.files.clone()
    }

    pub fn chat(&self) -> meshrmm_chat::ChatSession {
        self.chat.clone()
    }

    pub fn maintenance_state(&self) -> MaintenanceState {
        self.maintenance
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
    pub fn take_maintenance_error(&self) -> Option<String> {
        self.maintenance
            .lock()
            .ok()
            .and_then(|mut s| s.error.take())
    }
    pub fn toggle_blackout(&self) {
        let state = self.maintenance_state();
        if state.available {
            self.send(meshrmm_protocol::SessionMessage::SetBlackout {
                enabled: !state.blacked_out,
            });
        }
    }
    pub fn toggle_agent_input(&self) {
        let state = self.maintenance_state();
        if state.available && !state.blacked_out {
            self.send(meshrmm_protocol::SessionMessage::SetAgentInputBlocked {
                blocked: !state.agent_input_blocked,
            });
        }
    }
    pub fn agent_blocked(&self) -> bool {
        self.maintenance_state().agent_input_blocked
    }

    pub fn technician_blocked(&self) -> bool {
        self.technician_blocked
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn disconnect_confirmation(&self) -> bool {
        crate::preferences::disconnect_confirmation()
    }

    pub fn toggle_disconnect_confirmation(&self) {
        if let Err(error) = crate::preferences::toggle_disconnect_confirmation()
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
    }

    #[cfg(target_os = "macos")]
    pub fn command_as_control(&self) -> bool {
        crate::preferences::command_as_control()
    }

    #[cfg(target_os = "macos")]
    pub fn toggle_command_as_control(&self) {
        if let Err(error) = crate::preferences::toggle_command_as_control()
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
    }

    pub fn shortcut_key(
        &self,
        shortcut: crate::shortcuts::ViewerShortcut,
    ) -> crate::shortcuts::ShortcutKey {
        crate::preferences::shortcut_key(shortcut)
    }

    pub fn set_shortcut_key(
        &self,
        shortcut: crate::shortcuts::ViewerShortcut,
        key: crate::shortcuts::ShortcutKey,
    ) {
        if let Err(error) = crate::preferences::set_shortcut_key(shortcut, key)
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
    }

    #[cfg(windows)]
    pub fn send_windows_shortcuts(&self) -> bool {
        crate::preferences::send_windows_shortcuts()
    }

    #[cfg(windows)]
    pub fn toggle_send_windows_shortcuts(&self) {
        if let Err(error) = crate::preferences::toggle_send_windows_shortcuts()
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
    }

    pub fn clipboard_sync(&self) -> bool {
        crate::preferences::clipboard_sync()
    }

    pub fn toggle_clipboard_sync(&self) {
        if let Err(error) = crate::preferences::toggle_clipboard_sync()
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
    }

    pub fn clear_clipboard_on_close(&self) -> bool {
        crate::preferences::clear_clipboard_on_close()
    }

    pub fn toggle_clear_clipboard_on_close(&self) {
        match crate::preferences::toggle_clear_clipboard_on_close() {
            Ok(()) => self.send(meshrmm_protocol::SessionMessage::SetClearClipboardOnClose {
                enabled: self.clear_clipboard_on_close(),
            }),
            Err(error) => {
                if let Ok(mut state) = self.maintenance.lock() {
                    state.error = Some(error.to_string());
                }
            }
        }
    }

    /// Chosen per remote session; every new session starts with No action.
    pub fn session_close_action(&self) -> meshrmm_protocol::SessionCloseAction {
        *self
            .session_close_action
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_session_close_action(&self, action: meshrmm_protocol::SessionCloseAction) {
        *self
            .session_close_action
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = action;
        self.send(meshrmm_protocol::SessionMessage::SetSessionCloseAction { action });
    }

    pub fn prevent_idle_lock(&self) -> bool {
        let idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.policy.effective(idle.choice)
    }

    pub fn allow_idle_override(&self) -> bool {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .allow_override
    }

    pub fn toggle_prevent_idle_lock(&self) {
        let enabled = {
            let mut idle = self.idle.lock().unwrap_or_else(|e| e.into_inner());
            if !idle.policy.allow_override {
                return;
            }
            let enabled = !idle.policy.effective(idle.choice);
            idle.choice = Some(enabled);
            enabled
        };
        self.send(meshrmm_protocol::SessionMessage::SetPreventIdleLock { enabled });
    }

    pub fn display_border(&self) -> bool {
        self.display_border
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(true)
    }

    pub fn toggle_display_border(&self) {
        let enabled = {
            let mut choice = self
                .display_border
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let enabled = !choice.unwrap_or(true);
            *choice = Some(enabled);
            enabled
        };
        self.send(meshrmm_protocol::SessionMessage::SetDisplayBorder { enabled });
    }

    pub fn wallpaper_hidden(&self) -> bool {
        self.wallpaper_hidden
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn toggle_wallpaper(&self) {
        let hidden = !self
            .wallpaper_hidden
            .fetch_xor(true, std::sync::atomic::Ordering::SeqCst);
        self.send(meshrmm_protocol::SessionMessage::SetWallpaperHidden { hidden });
    }

    pub fn show_remote_cursor(&self) -> bool {
        !self
            .remote_cursor_hidden
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn toggle_remote_cursor(&self) {
        let was_hidden = self
            .remote_cursor_hidden
            .fetch_xor(true, std::sync::atomic::Ordering::SeqCst);
        self.send(meshrmm_protocol::SessionMessage::SetCursorCapture {
            enabled: was_hidden,
        });
    }

    pub fn effective_cursor_shape(
        &self,
        shape: meshrmm_protocol::CursorShape,
    ) -> meshrmm_protocol::CursorShape {
        if self.technician_blocked() {
            meshrmm_protocol::CursorShape::Default
        } else {
            shape
        }
    }

    /// Call after releasing held keys/buttons, before changing the gate.
    pub fn set_technician_blocked(&self, blocked: bool) {
        self.technician_blocked
            .store(blocked, std::sync::atomic::Ordering::SeqCst);
        if blocked {
            (self.set_input_enabled)(false);
        }
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
pub use windows::{
    Presenter, attach_parent_console, enable_dpi_awareness, monotonic_timestamp_us,
    show_fatal_error, show_notice, supported_video_profiles,
};

#[cfg(target_os = "macos")]
pub use macos::{
    Presenter, monotonic_timestamp_us, run_application, show_notice, supported_video_profiles,
};
