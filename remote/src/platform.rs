#[cfg(windows)]
mod windows;

#[cfg(target_os = "macos")]
mod macos;

use std::sync::Arc;
use std::sync::Mutex;

/// Audio formats this viewer plays, most preferred first.
const AUDIO_FORMATS: &[meshrmm_protocol::AudioFormat] = &[
    meshrmm_protocol::AudioFormat::Opus,
    meshrmm_protocol::AudioFormat::Pcm16,
];

/// The Agent sends system audio only while the viewer is unmuted.
pub fn audio_preference(muted: bool) -> meshrmm_protocol::SessionMessage {
    meshrmm_protocol::SessionMessage::SetAudio {
        enabled: !muted,
        formats: AUDIO_FORMATS.to_vec(),
    }
}

/// A company policy for a per-session toggle, and the viewer's choice for
/// this session. Every new session starts from the company default.
#[derive(Default)]
pub struct PolicyChoice {
    pub policy: meshrmm_protocol::TogglePolicy,
    pub choice: Option<bool>,
}
impl PolicyChoice {
    fn enabled(&self) -> bool {
        self.policy.effective(self.choice)
    }

    /// Flips the choice, or returns `None` while the company manages it.
    fn toggle(&mut self) -> Option<bool> {
        if !self.policy.allow_override {
            return None;
        }
        let enabled = !self.enabled();
        self.choice = Some(enabled);
        Some(enabled)
    }
}

#[derive(Default, Clone)]
pub struct MaintenanceState {
    pub available: bool,
    pub error: Option<String>,
    pub agent_input_blocked: bool,
    pub blacked_out: bool,
    /// `Some(safe_mode)` once the agent reports it can restart its computer.
    pub power: Option<bool>,
}

/// Sends viewer control messages and keeps the transport's input gate in sync
/// with the native window's foreground state.
#[derive(Clone)]
pub struct ControlSink {
    idle: Arc<Mutex<PolicyChoice>>,
    idle_disconnect: Arc<Mutex<crate::idle_disconnect::IdleDisconnect>>,
    clear_clipboard: Arc<Mutex<PolicyChoice>>,
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
    restarting: Arc<Mutex<Option<bool>>>,
    quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
    chroma: Arc<Mutex<meshrmm_protocol::ChromaMode>>,
    #[cfg(windows)]
    profiles: Arc<Vec<meshrmm_protocol::VideoProfile>>,
}

/// What a [`ControlSink`] is made of: the session state the viewer window
/// reads and changes, and the transport callbacks it sends through.
pub struct ControlSinkParts {
    pub idle: Arc<Mutex<PolicyChoice>>,
    pub idle_disconnect: Arc<Mutex<crate::idle_disconnect::IdleDisconnect>>,
    pub clear_clipboard: Arc<Mutex<PolicyChoice>>,
    pub display_border: Arc<Mutex<Option<bool>>>,
    pub files: meshrmm_file_transfer::TransferSession,
    pub chat: meshrmm_chat::ChatSession,
    pub audio: meshrmm_audio::PlaybackState,
    pub recording: crate::recording::Recorder,
    pub send: Arc<dyn Fn(meshrmm_protocol::SessionMessage) + Send + Sync>,
    pub set_input_enabled: Arc<dyn Fn(bool) + Send + Sync>,
    pub maintenance: Arc<Mutex<MaintenanceState>>,
    pub credentials: Arc<Mutex<meshrmm_protocol::CredentialState>>,
    pub technician_blocked: Arc<std::sync::atomic::AtomicBool>,
    pub wallpaper_hidden: Arc<std::sync::atomic::AtomicBool>,
    pub remote_cursor_hidden: Arc<std::sync::atomic::AtomicBool>,
    pub session_close_action: Arc<Mutex<meshrmm_protocol::SessionCloseAction>>,
    /// Survives reconnects, so the window says the computer is restarting.
    pub restarting: Arc<Mutex<Option<bool>>>,
    pub quality: Arc<Mutex<meshrmm_protocol::QualityPreset>>,
    pub chroma: Arc<Mutex<meshrmm_protocol::ChromaMode>>,
    #[cfg(windows)]
    pub profiles: Arc<Vec<meshrmm_protocol::VideoProfile>>,
}

impl ControlSink {
    pub fn new(parts: ControlSinkParts) -> Self {
        let ControlSinkParts {
            idle,
            idle_disconnect,
            clear_clipboard,
            display_border,
            files,
            chat,
            audio,
            recording,
            send,
            set_input_enabled,
            maintenance,
            credentials,
            technician_blocked,
            wallpaper_hidden,
            remote_cursor_hidden,
            session_close_action,
            restarting,
            quality,
            chroma,
            #[cfg(windows)]
            profiles,
        } = parts;
        Self {
            idle,
            idle_disconnect,
            clear_clipboard,
            display_border,
            files,
            chat,
            audio,
            recording,
            send,
            set_input_enabled,
            maintenance,
            credentials,
            technician_blocked,
            wallpaper_hidden,
            remote_cursor_hidden,
            session_close_action,
            restarting,
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

    /// Sends a message on the technician's behalf. Everything sent here
    /// comes from the technician, so it also counts as activity.
    pub fn send(&self, message: meshrmm_protocol::SessionMessage) {
        self.note_activity();
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

    /// Mutes or unmutes remote audio, tells the Agent, and remembers the
    /// choice for later sessions.
    pub fn toggle_audio(&self) {
        let muted = self.audio.toggle();
        self.send(audio_preference(muted));
        if let Err(error) = crate::preferences::set_audio_muted(muted)
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
    }

    /// Changes the size of the virtual display that an Agent without a
    /// monitor shows, and remembers it for later sessions.
    pub fn set_headless_resolution(&self, resolution: meshrmm_protocol::HeadlessResolution) {
        self.send(meshrmm_protocol::SessionMessage::SetHeadlessResolution { resolution });
        if let Err(error) = crate::preferences::set_headless_resolution(resolution)
            && let Ok(mut state) = self.maintenance.lock()
        {
            state.error = Some(error.to_string());
        }
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

    /// Whether the agent can restart its computer, and whether Windows is in
    /// Safe Mode.
    pub fn power_state(&self) -> Option<bool> {
        self.maintenance_state().power
    }

    /// Restarts the remote computer. The platform asks the technician first.
    pub fn restart(&self, safe_mode: bool) {
        if self.power_state().is_some() {
            self.send(meshrmm_protocol::SessionMessage::Restart { safe_mode });
            if let Ok(mut restarting) = self.restarting.lock() {
                *restarting = Some(safe_mode);
            }
        }
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

    /// Starts from the company default; changeable for this session only
    /// when the company allows it.
    pub fn clear_clipboard_on_close(&self) -> bool {
        self.clear_clipboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled()
    }

    pub fn allow_clear_clipboard_override(&self) -> bool {
        self.clear_clipboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .allow_override
    }

    pub fn toggle_clear_clipboard_on_close(&self) {
        let toggled = self
            .clear_clipboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .toggle();
        if let Some(enabled) = toggled {
            self.send(meshrmm_protocol::SessionMessage::SetClearClipboardOnClose { enabled });
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
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .enabled()
    }

    pub fn allow_idle_override(&self) -> bool {
        self.idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .allow_override
    }

    pub fn toggle_prevent_idle_lock(&self) {
        let toggled = self.idle.lock().unwrap_or_else(|e| e.into_inner()).toggle();
        if let Some(enabled) = toggled {
            self.send(meshrmm_protocol::SessionMessage::SetPreventIdleLock { enabled });
        }
    }

    fn idle_disconnect(&self) -> std::sync::MutexGuard<'_, crate::idle_disconnect::IdleDisconnect> {
        self.idle_disconnect
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Restarts the idle time before the session is disconnected.
    pub fn note_activity(&self) {
        self.idle_disconnect()
            .note_activity(std::time::Instant::now());
    }

    /// How long this session may be idle before it is disconnected, in
    /// minutes; `None` never disconnects.
    pub fn idle_disconnect_minutes(&self) -> Option<u32> {
        self.idle_disconnect().minutes()
    }

    pub fn allow_idle_disconnect_override(&self) -> bool {
        self.idle_disconnect().allow_override()
    }

    /// Chosen per remote session; every new session starts with the company
    /// default. Ignored when the company does not allow a choice.
    pub fn set_idle_disconnect_minutes(&self, minutes: Option<u32>) {
        self.idle_disconnect()
            .choose(minutes, std::time::Instant::now());
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
    Presenter, attach_parent_console, close_launch_status, enable_dpi_awareness,
    monotonic_timestamp_us, show_fatal_error, show_launch_status, show_notice,
    supported_video_profiles,
};

#[cfg(target_os = "macos")]
pub use macos::{
    Presenter, monotonic_timestamp_us, run_application, show_launch_status, show_notice,
    supported_video_profiles,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_preference_enables_audio_only_while_unmuted() {
        use meshrmm_protocol::{AudioFormat, SessionMessage};
        assert_eq!(
            audio_preference(true),
            SessionMessage::SetAudio {
                enabled: false,
                formats: vec![AudioFormat::Opus, AudioFormat::Pcm16],
            }
        );
        assert!(matches!(
            audio_preference(false),
            SessionMessage::SetAudio { enabled: true, .. }
        ));
        // Sessions start muted unless the saved preference says otherwise,
        // and tests never read the user's preference file.
        assert!(crate::transport::ViewerResumeState::default().audio_muted());
        assert!(!crate::transport::ViewerResumeState::with_audio_muted(false).audio_muted());
    }
}
