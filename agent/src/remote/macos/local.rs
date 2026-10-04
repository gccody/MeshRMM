//! The screen and input of the session this process runs in. A console Agent
//! uses them directly; the installed Agent's session helpers serve them to the
//! coordinator.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::Context;
use meshrmm_protocol::{
    ClipboardContent, CursorShape, Display, DisplayId, PixelFormat, RemoteInput, VideoFormat,
};

use super::capture::{Capture, CaptureConfig};
use super::encoder::EncodedAccessUnit;
use super::helper::protocol::{InputState, SessionUi, StreamSettings};
use super::platform::ScreenInput;
use super::{display, input, keep_awake, ui, wallpaper};

pub(crate) struct Started {
    pub displays: Vec<Display>,
    pub active_display: Display,
    pub format: VideoFormat,
}

/// Windows the capture leaves out: the display border and the blackout,
/// which only the Mac's user sees.
#[derive(Default)]
pub(crate) struct Excluded {
    windows: Mutex<BTreeMap<&'static str, Vec<u32>>>,
    capture: Mutex<Weak<Capture>>,
}

impl Excluded {
    fn all(&self) -> Vec<u32> {
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .flatten()
            .copied()
            .collect()
    }

    /// Replaces `owner`'s windows and updates the running capture.
    fn set(&self, owner: &'static str, windows: Vec<u32>) -> anyhow::Result<()> {
        self.windows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(owner, windows);
        let capture = self
            .capture
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade();
        match capture {
            Some(capture) => capture.exclude_windows(&self.all()),
            None => Ok(()),
        }
    }

    fn attach(&self, capture: &Arc<Capture>) {
        *self.capture.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(capture);
    }
}

/// Captures and encodes one display at a time.
pub(crate) struct LocalScreen {
    capture: Option<Arc<Capture>>,
    input: Arc<LocalInput>,
    border_enabled: bool,
    /// The border around the captured display, which capture leaves out.
    border: Option<ui::DisplayBorder>,
    active_display: Option<meshrmm_protocol::Display>,
}

impl LocalScreen {
    pub(crate) fn new(input: Arc<LocalInput>) -> Self {
        Self {
            capture: None,
            input,
            border_enabled: false,
            border: None,
            active_display: None,
        }
    }

    /// Shows or hides the outline around the shared display.
    pub(crate) fn set_display_border(&mut self, enabled: bool) -> anyhow::Result<()> {
        self.border_enabled = enabled;
        self.border = None;
        let mut windows = Vec::new();
        if enabled && let Some(display) = &self.active_display {
            let border = ui::DisplayBorder::show(display)?;
            windows = border.window_ids().to_vec();
            self.border = Some(border);
        }
        self.input.excluded.set("border", windows)
    }

    pub(crate) fn start(
        &mut self,
        display_id: Option<DisplayId>,
        settings: StreamSettings,
        sink: impl Fn(EncodedAccessUnit) + Send + Sync + 'static,
    ) -> anyhow::Result<Started> {
        self.capture = None;
        self.border = None;
        let displays = display::enumerate()?;
        let active_display = display::choose(&displays, display_id)?;
        self.input
            .controller()?
            .set_active_display(active_display.clone())?;
        self.input.clear_annotations_unless(active_display.id);
        let mut border_windows = Vec::new();
        if self.border_enabled {
            let border = ui::DisplayBorder::show(&active_display)?;
            border_windows = border.window_ids().to_vec();
            self.border = Some(border);
        }
        self.input.excluded.set("border", border_windows)?;
        self.active_display = Some(active_display.clone());
        let excluded = self.input.excluded.all();
        let capture = Capture::start(
            &active_display,
            CaptureConfig {
                frames_per_second: settings.frames_per_second,
                bitrate_bits_per_second: settings.bitrate_bits_per_second,
                codec: settings.codec,
                capture_cursor: settings.capture_cursor,
                grayscale: settings.grayscale,
            },
            &excluded,
            sink,
        )?;
        let format = capture.format();
        let capture = Arc::new(capture);
        self.input.excluded.attach(&capture);
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
        self.border = None;
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
    indicator: Mutex<Option<ui::SessionIndicator>>,
    excluded: Arc<Excluded>,
    blackout: Mutex<Option<ui::Blackout>>,
    blackout_message: Mutex<String>,
    /// Whether the technician blocked local input, apart from the blackout.
    manually_blocked: AtomicBool,
    notification: Mutex<Option<ui::ConnectionNotification>>,
    annotations: Mutex<Option<(meshrmm_protocol::DisplayId, ui::AnnotationOverlay)>>,
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
            indicator: Mutex::default(),
            excluded: Arc::default(),
            blackout: Mutex::default(),
            blackout_message: Mutex::default(),
            manually_blocked: AtomicBool::new(false),
            notification: Mutex::default(),
            annotations: Mutex::default(),
        })
    }

    /// Shows the user that a technician connected: the menu bar item with the
    /// chat, the banner when company policy shows it, and the connection
    /// notification once.
    pub(crate) fn begin_session(&self, session: &SessionUi) {
        *self
            .blackout_message
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = session.blackout_message.clone();
        match ui::SessionIndicator::show(
            &session.viewer_name,
            self.chat.clone(),
            session.show_banner,
        ) {
            Ok(indicator) => {
                *self.indicator.lock().unwrap_or_else(|e| e.into_inner()) = Some(indicator)
            }
            Err(error) => tracing::warn!(error = ?error, "could not show the session indicator"),
        }
        if let Some(text) = &session.notification {
            match ui::ConnectionNotification::show(text) {
                Ok(notification) => {
                    *self.notification.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(notification)
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "could not show the connection notification")
                }
            }
        }
    }

    /// Erases the drawing when the session shows another display.
    fn clear_annotations_unless(&self, display: meshrmm_protocol::DisplayId) {
        let mut annotations = self.annotations.lock().unwrap_or_else(|e| e.into_inner());
        if annotations
            .as_ref()
            .is_some_and(|(shown, _)| *shown != display)
        {
            annotations.take();
        }
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
                agent_input_blocked: input.blocked(),
                blacked_out: self.blacked_out(),
            },
        )
    }

    /// Undoes everything a session changed, for a helper the session left.
    fn blacked_out(&self) -> bool {
        self.blackout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub(crate) fn end_session(&self) {
        let _ = self.set_blackout(false);
        let _ = self.set_agent_input_blocked(false);
        let _ = self.release_all();
        let _ = self.set_prevent_idle_lock(false);
        let _ = self.set_wallpaper_hidden(false);
        self.stop_chat();
        self.indicator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.notification
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.annotations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
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
    /// Blacks out every screen for the Mac's user, and blocks their input,
    /// while the technician keeps seeing and controlling the Mac.
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()> {
        let mut blackout = self.blackout.lock().unwrap_or_else(|e| e.into_inner());
        if enabled && blackout.is_none() {
            // Block input before hiding the screens; undo it if that fails.
            self.controller()?.set_blocked(true)?;
            let message = self
                .blackout_message
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let shown = ui::Blackout::show(&message).and_then(|shown| {
                self.excluded.set("blackout", shown.window_ids().to_vec())?;
                Ok(shown)
            });
            match shown {
                Ok(shown) => *blackout = Some(shown),
                Err(error) => {
                    let _ = self
                        .controller()?
                        .set_blocked(self.manually_blocked.load(Ordering::SeqCst));
                    return Err(error);
                }
            }
        } else if !enabled && blackout.take().is_some() {
            self.excluded.set("blackout", Vec::new())?;
            self.controller()?
                .set_blocked(self.manually_blocked.load(Ordering::SeqCst))?;
        }
        Ok(())
    }
    fn maintenance_state(&self) -> Option<meshrmm_protocol::SessionMessage> {
        Some(meshrmm_protocol::SessionMessage::MaintenanceState {
            agent_input_blocked: self.controller().ok()?.blocked(),
            blacked_out: self.blacked_out(),
        })
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        self.controller()?
            .set_blocked(blocked || self.blacked_out())?;
        self.manually_blocked.store(blocked, Ordering::SeqCst);
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
    /// Draws over the active display. Points for another display are
    /// discarded: they were drawn before a display switch.
    fn annotate(&self, annotation: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
        use meshrmm_protocol::Annotation;

        let (x, y, start) = match annotation {
            Annotation::Start { x, y, .. } => (x, y, true),
            Annotation::Extend { x, y, .. } => (x, y, false),
            Annotation::Clear => {
                self.annotations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                return Ok(());
            }
        };
        let display = self
            .controller()?
            .active_display()
            .context("an annotation arrived before a display was selected")?;
        if annotation.display_id() != Some(display.id) {
            return Ok(());
        }
        let mut annotations = self.annotations.lock().unwrap_or_else(|e| e.into_inner());
        let overlay = match &mut *annotations {
            Some((_, overlay)) => overlay,
            // Only a new stroke shows the overlay, so one that failed to show
            // is reported once per stroke rather than once per point.
            None if !start => return Ok(()),
            None => {
                &mut annotations
                    .insert((display.id, ui::AnnotationOverlay::show(&display)?))
                    .1
            }
        };
        overlay.draw(x, y, start);
        Ok(())
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
