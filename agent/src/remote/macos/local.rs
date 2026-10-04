//! The screen and input of the session this process runs in. A console Agent
//! uses them directly; the installed Agent's session helpers serve them to the
//! coordinator.
use std::sync::{Arc, Mutex};

use anyhow::Context;
use meshrmm_protocol::{
    ClipboardContent, CursorShape, Display, DisplayId, PixelFormat, RemoteInput, VideoFormat,
};

use super::capture::{Capture, CaptureConfig};
use super::encoder::EncodedAccessUnit;
use super::helper::protocol::{InputState, StreamSettings};
use super::platform::ScreenInput;
use super::{display, input, keep_awake, wallpaper};

pub(crate) struct Started {
    pub displays: Vec<Display>,
    pub active_display: Display,
    pub format: VideoFormat,
}

/// Captures and encodes one display at a time.
pub(crate) struct LocalScreen {
    capture: Option<Capture>,
    input: Arc<LocalInput>,
}

impl LocalScreen {
    pub(crate) fn new(input: Arc<LocalInput>) -> Self {
        Self {
            capture: None,
            input,
        }
    }

    pub(crate) fn start(
        &mut self,
        display_id: Option<DisplayId>,
        settings: StreamSettings,
        sink: impl Fn(EncodedAccessUnit) + Send + Sync + 'static,
    ) -> anyhow::Result<Started> {
        self.capture = None;
        let displays = display::enumerate()?;
        let active_display = display::choose(&displays, display_id)?;
        self.input
            .controller()?
            .set_active_display(active_display.clone())?;
        let capture = Capture::start(
            &active_display,
            CaptureConfig {
                frames_per_second: settings.frames_per_second,
                bitrate_bits_per_second: settings.bitrate_bits_per_second,
                codec: settings.codec,
                capture_cursor: settings.capture_cursor,
                grayscale: settings.grayscale,
            },
            sink,
        )?;
        let format = capture.format();
        self.capture = Some(capture);
        Ok(Started {
            displays,
            active_display,
            format: VideoFormat {
                width: format.width,
                height: format.height,
                frames_per_second: format.frames_per_second as u16,
                codec: format.codec,
                pixel_format: PixelFormat::Nv12,
                bitrate_bits_per_second: format.bitrate_bits_per_second,
            },
        })
    }

    pub(crate) fn stop(&mut self) {
        self.capture = None;
    }

    pub(crate) fn poll_ended(&mut self) -> Option<anyhow::Error> {
        let error = self.capture.as_ref()?.poll_ended()?;
        self.capture = None;
        Some(error.context("macOS screen capture stopped"))
    }

    pub(crate) fn request_keyframe(&self) {
        if let Some(capture) = &self.capture {
            capture.request_keyframe();
        }
    }

    pub(crate) fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        match &self.capture {
            Some(capture) => capture
                .set_bitrate(bits_per_second.max(1))
                .context("encoder bitrate change failed"),
            None => Ok(()),
        }
    }

    pub(crate) fn set_cursor_capture(&self, enabled: bool) {
        if let Some(capture) = &self.capture {
            capture.set_cursor_capture(enabled);
        }
    }
}

/// Keyboard, pointer, clipboard, files, chat and the maintenance controls of
/// this session.
pub(crate) struct LocalInput {
    controller: Mutex<input::InputController>,
    files: meshrmm_file_transfer::TransferSession,
    chat: meshrmm_chat::ChatSession,
    keep_awake: Mutex<Option<keep_awake::KeepAwake>>,
    wallpaper: Mutex<wallpaper::Wallpaper>,
    clipboard: Option<Mutex<meshrmm_clipboard::ClipboardSync>>,
}

impl LocalInput {
    pub(crate) fn new() -> anyhow::Result<Self> {
        wallpaper::restore_interrupted();
        Ok(Self {
            controller: Mutex::new(input::InputController::new()?),
            files: meshrmm_file_transfer::TransferSession::agent(),
            chat: meshrmm_chat::ChatSession::with_peer("Viewer"),
            keep_awake: Mutex::new(None),
            wallpaper: Mutex::default(),
            clipboard: meshrmm_clipboard::ClipboardSync::new(false)
                .inspect_err(|error| tracing::warn!(%error, "the clipboard is unavailable"))
                .ok()
                .map(Mutex::new),
        })
    }

    fn controller(&self) -> anyhow::Result<std::sync::MutexGuard<'_, input::InputController>> {
        self.controller
            .lock()
            .map_err(|_| anyhow::anyhow!("input controller lock was poisoned"))
    }

    fn clipboard(
        &self,
    ) -> anyhow::Result<std::sync::MutexGuard<'_, meshrmm_clipboard::ClipboardSync>> {
        self.clipboard
            .as_ref()
            .context("the macOS clipboard is unavailable")?
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard lock was poisoned"))
    }

    pub(crate) fn input_state(&self) -> InputState {
        self.controller().map_or_else(
            |_| InputState::default(),
            |input| InputState {
                cursor: input.cursor_shape(),
                viewer_controls_input: input.viewer_controls_input(),
                agent_pointer_display: input.agent_pointer_display(),
            },
        )
    }

    /// Undoes everything a session changed, for a helper the session left.
    pub(crate) fn end_session(&self) {
        let _ = self.release_all();
        let _ = self.set_prevent_idle_lock(false);
        let _ = self.set_wallpaper_hidden(false);
        self.stop_chat();
    }
}

impl ScreenInput for LocalInput {
    fn set_wallpaper_hidden(&self, hidden: bool) -> anyhow::Result<()> {
        self.wallpaper
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_hidden(hidden)
    }
    fn set_prevent_idle_lock(&self, enabled: bool) -> anyhow::Result<()> {
        keep_awake::set_enabled(
            &mut self.keep_awake.lock().unwrap_or_else(|e| e.into_inner()),
            enabled,
        )
    }
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !enabled,
            "blacking out the screen is not available on macOS yet"
        );
        Ok(())
    }
    fn maintenance_state(&self) -> Option<meshrmm_protocol::SessionMessage> {
        None
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !blocked,
            "blocking local input is not available on macOS yet"
        );
        Ok(())
    }
    fn apply_files(&self, message: meshrmm_protocol::FileMessage) -> anyhow::Result<()> {
        self.files.receive(message);
        Ok(())
    }
    fn poll_files(&self) -> Option<meshrmm_protocol::FileMessage> {
        self.files.poll()
    }
    fn files_ready(&self) -> Arc<tokio::sync::Notify> {
        self.files.outgoing_ready()
    }
    fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
        self.controller()?.apply(input)
    }
    fn annotate(&self, _annotation: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
        anyhow::bail!("annotation is not available on macOS yet")
    }
    fn release_all(&self) -> anyhow::Result<()> {
        self.controller()?.release_all()
    }
    fn cursor_shape(&self) -> CursorShape {
        self.controller()
            .map_or(CursorShape::Default, |input| input.cursor_shape())
    }
    fn agent_pointer_display(&self) -> Option<DisplayId> {
        self.controller().ok()?.agent_pointer_display()
    }
    fn viewer_controls_input(&self) -> bool {
        self.controller()
            .is_ok_and(|input| input.viewer_controls_input())
    }
    fn apply_clipboard(&self, content: ClipboardContent) -> anyhow::Result<()> {
        self.clipboard()?.apply(content)
    }
    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
        self.clipboard()?.poll()
    }
    fn start_chat(&self) -> anyhow::Result<()> {
        self.chat.set_available(true);
        Ok(())
    }
    fn stop_chat(&self) {
        self.chat.set_available(false);
    }
    fn apply_chat(&self, text: String) -> anyhow::Result<()> {
        if self.chat.available() {
            self.chat.receive(text);
        }
        Ok(())
    }
    fn poll_chat(&self) -> anyhow::Result<Option<String>> {
        Ok(self.chat.poll())
    }
    fn chat_ready(&self) -> Arc<tokio::sync::Notify> {
        self.chat.outgoing_ready()
    }
}
