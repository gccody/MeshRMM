//! The screen and input of the session this process runs in. A console Agent
//! uses them directly; the installed Agent's session helpers serve them to the
//! coordinator.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use meshrmm_protocol::{
    ClipboardContent, Codec, CursorShape, Display, DisplayId, PixelFormat, RemoteInput, VideoFormat,
};

use super::capture::{self, Backend, CaptureSettings, Stream};
use super::display::Output;
use super::encoder::{EncodedAccessUnit, EncoderSettings, FrameSink, RawFrame, VideoEncoder};
use super::{input, keep_awake, pointer, ui, wallpaper};
use crate::remote::helper::protocol::{InputState, SessionUi, StreamSettings};
use crate::remote::platform::ScreenInput;

pub(crate) struct Started {
    pub displays: Vec<Display>,
    pub active_display: Display,
    pub format: VideoFormat,
}

/// What a running capture needs to start again with the same settings.
struct Running {
    output: Output,
    settings: StreamSettings,
    stream: Box<dyn Stream>,
    encoder: Arc<VideoEncoder>,
    sink: FrameSink,
}

/// Captures and encodes one display at a time.
pub(crate) struct LocalScreen {
    backend: Option<Box<dyn Backend>>,
    running: Option<Running>,
    input: Arc<LocalInput>,
}

impl LocalScreen {
    pub(crate) fn new(input: Arc<LocalInput>) -> Self {
        Self {
            backend: None,
            running: None,
            input,
        }
    }

    /// The display server's capture backend, connected once and again after
    /// it fails.
    fn backend(&mut self) -> anyhow::Result<&mut Box<dyn Backend>> {
        if self.backend.is_none() {
            let backend = capture::detect()?;
            tracing::info!(backend = backend.name(), "capturing the session");
            self.backend = Some(backend);
        }
        Ok(self
            .backend
            .as_mut()
            .expect("the backend was just connected"))
    }

    /// Linux display servers draw nothing the capture could leave out, so
    /// there is no border around the shared display. Viewers don't ask for
    /// one from a Linux device.
    pub(crate) fn set_display_border(&mut self, enabled: bool) -> anyhow::Result<()> {
        if enabled {
            bail!("the border around the viewed monitor is not available on Linux");
        }
        Ok(())
    }

    pub(crate) fn start(
        &mut self,
        display_id: Option<DisplayId>,
        settings: StreamSettings,
        sink: impl Fn(EncodedAccessUnit) + Send + Sync + 'static,
    ) -> anyhow::Result<Started> {
        self.running = None;
        let outputs = match self.backend()?.outputs() {
            Ok(outputs) => outputs,
            Err(error) => {
                // The connection may have gone stale; reconnect once.
                self.backend = None;
                self.backend()?.outputs().context(format!("{error:#}"))?
            }
        };
        let output = super::display::choose(&outputs, display_id)?;
        let displays: Vec<Display> = outputs.iter().map(Output::display).collect();
        let active_display = output.display();
        self.input
            .controller()?
            .set_active_display(active_display.clone(), displays.clone())?;
        let running = self.capture(output, settings, Arc::new(sink))?;
        self.running = Some(running);
        Ok(Started {
            displays,
            active_display,
            format: VideoFormat {
                width: active_display_size(&self.running).0,
                height: active_display_size(&self.running).1,
                frames_per_second: settings.frames_per_second as u16,
                codec: Codec::H264,
                pixel_format: PixelFormat::Nv12,
                bitrate_bits_per_second: settings.bitrate_bits_per_second,
            },
        })
    }

    fn capture(
        &mut self,
        output: Output,
        settings: StreamSettings,
        sink: FrameSink,
    ) -> anyhow::Result<Running> {
        let encoder = Arc::new(VideoEncoder::start(
            EncoderSettings {
                frames_per_second: settings.frames_per_second,
                bitrate_bits_per_second: settings.bitrate_bits_per_second,
                grayscale: settings.grayscale,
            },
            Arc::clone(&sink),
        )?);
        let frames = Arc::clone(&encoder);
        let stream = self.backend()?.start(
            &output,
            CaptureSettings {
                frames_per_second: settings.frames_per_second,
                capture_cursor: settings.capture_cursor,
            },
            Arc::new(move |frame: &RawFrame<'_>| frames.submit(frame)),
        );
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                self.backend = None;
                return Err(error);
            }
        };
        Ok(Running {
            output,
            settings,
            stream,
            encoder,
            sink,
        })
    }

    pub(crate) fn stop(&mut self) {
        self.running = None;
    }

    pub(crate) fn poll_ended(&mut self) -> Option<anyhow::Error> {
        let running = self.running.as_mut()?;
        self.input.set_cursor(running.stream.cursor_shape());
        let error = running.stream.poll_ended().or_else(|| {
            running
                .encoder
                .failure()
                .map(|failure| anyhow::anyhow!(failure).context("the H.264 encoder stopped"))
        })?;
        self.running = None;
        // The display server may have restarted; connect again next time.
        self.backend = None;
        Some(error.context("Linux screen capture stopped"))
    }

    pub(crate) fn request_keyframe(&self) {
        if let Some(running) = &self.running {
            running.encoder.request_keyframe();
        }
    }

    pub(crate) fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        if let Some(running) = &self.running {
            running.encoder.set_bitrate(bits_per_second.max(1));
        }
        Ok(())
    }

    /// Shows or hides the pointer, restarting the capture where the display
    /// server cannot change it on the fly.
    pub(crate) fn set_cursor_capture(&mut self, enabled: bool) {
        let Some(running) = &mut self.running else {
            return;
        };
        if running.settings.capture_cursor == enabled {
            return;
        }
        running.settings.capture_cursor = enabled;
        if running.stream.set_cursor_capture(enabled) {
            return;
        }
        let Some(Running {
            output,
            settings,
            sink,
            ..
        }) = self.running.take()
        else {
            return;
        };
        match self.capture(output, settings, sink) {
            Ok(running) => self.running = Some(running),
            Err(error) => {
                tracing::warn!(error = ?error, "could not restart capture for the pointer")
            }
        }
    }
}

fn active_display_size(running: &Option<Running>) -> (u32, u32) {
    running.as_ref().map_or((0, 0), |running| {
        super::encoder::encoded_size(running.output.width, running.output.height)
    })
}

/// Keyboard, pointer, clipboard, files, chat and the maintenance controls of
/// this session.
pub(crate) struct LocalInput {
    controller: Arc<Mutex<input::InputController>>,
    /// The remote session's file worker, which announces itself to each
    /// viewer when it starts.
    files: Mutex<Option<meshrmm_file_transfer::TransferSession>>,
    chat: meshrmm_chat::ChatSession,
    keep_awake: Mutex<Option<keep_awake::KeepAwake>>,
    wallpaper: Mutex<wallpaper::Wallpaper>,
    clipboard: Option<Mutex<meshrmm_clipboard::ClipboardSync>>,
    indicator: Mutex<Option<ui::SessionIndicator>>,
    notification: Mutex<Option<ui::Notice>>,
    /// Who uses the keyboard and pointer, in an X11 session.
    pointer: Mutex<Option<pointer::PointerWatch>>,
    cursor: Mutex<CursorShape>,
    /// Whether the annotation warning was logged this session.
    annotations_refused: AtomicBool,
}

/// Presses Ctrl+V on the display the viewer pasted files on, once they are
/// on the clipboard.
fn paste_shortcut(
    controller: &Arc<Mutex<input::InputController>>,
) -> meshrmm_file_transfer::PasteShortcut {
    let controller = Arc::clone(controller);
    Box::new(move |display_id| {
        let mut controller = controller
            .lock()
            .map_err(|_| anyhow::anyhow!("input controller lock was poisoned"))?;
        for (scan_code, extended, pressed) in [
            (0x1d, false, true),
            (0x2f, false, true),
            (0x2f, false, false),
            (0x1d, false, false),
        ] {
            controller.apply(RemoteInput::Key {
                display_id,
                scan_code,
                extended,
                pressed,
            })?;
        }
        Ok(())
    })
}

impl LocalInput {
    pub(crate) fn new() -> anyhow::Result<Self> {
        wallpaper::restore_interrupted();
        let controller = Arc::new(Mutex::new(input::InputController::new()));
        Ok(Self {
            controller,
            files: Mutex::default(),
            chat: meshrmm_chat::ChatSession::with_peer("Viewer"),
            keep_awake: Mutex::new(None),
            wallpaper: Mutex::default(),
            clipboard: meshrmm_clipboard::ClipboardSync::new(false)
                .inspect_err(|error| tracing::warn!(%error, "the clipboard is unavailable"))
                .ok()
                .map(Mutex::new),
            indicator: Mutex::default(),
            notification: Mutex::default(),
            cursor: Mutex::new(CursorShape::Default),
            pointer: Mutex::default(),
            annotations_refused: AtomicBool::new(false),
        })
    }

    /// Shows the user that a technician connected: the banner when company
    /// policy shows it, and the connection notification once.
    pub(crate) fn begin_session(&self, session: &SessionUi) {
        if capture::x11_session() {
            match pointer::PointerWatch::start() {
                Ok(watch) => *self.pointer.lock().unwrap_or_else(|e| e.into_inner()) = Some(watch),
                Err(error) => tracing::warn!(error = ?error, "could not watch the session's input"),
            }
        }
        *self.files.lock().unwrap_or_else(|e| e.into_inner()) = Some(
            meshrmm_file_transfer::TransferSession::agent(paste_shortcut(&self.controller)),
        );
        match ui::SessionIndicator::show(
            &session.viewer_name,
            self.chat.clone(),
            session.show_banner,
        ) {
            Ok(indicator) => {
                *self.indicator.lock().unwrap_or_else(|e| e.into_inner()) = Some(indicator)
            }
            Err(error) => tracing::warn!(error = ?error, "could not show the session banner"),
        }
        if let Some(text) = &session.notification {
            match ui::connection_notification(text) {
                Ok(notice) => {
                    *self.notification.lock().unwrap_or_else(|e| e.into_inner()) = Some(notice)
                }
                Err(error) => {
                    tracing::warn!(error = ?error, "could not show the connection notification")
                }
            }
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
            .context("the clipboard is unavailable")?
            .lock()
            .map_err(|_| anyhow::anyhow!("clipboard lock was poisoned"))
    }

    /// Keeps the pointer's shape while the viewer controls input. While the
    /// user does, the video shows their pointer and the viewer keeps the last
    /// shape.
    fn set_cursor(&self, cursor: CursorShape) {
        if self.viewer_controls_input() {
            *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = cursor;
        }
    }

    pub(crate) fn input_state(&self) -> InputState {
        InputState {
            cursor: *self.cursor.lock().unwrap_or_else(|e| e.into_inner()),
            viewer_controls_input: self.viewer_controls_input(),
            agent_pointer_display: self.agent_pointer_display(),
            agent_input_blocked: self.controller().is_ok_and(|input| input.blocked()),
            blacked_out: false,
        }
    }

    /// Undoes everything a session changed, for a helper the session left.
    pub(crate) fn end_session(&self) {
        let _ = self.set_agent_input_blocked(false);
        let _ = self.release_all();
        let _ = self.set_prevent_idle_lock(false);
        let _ = self.set_wallpaper_hidden(false);
        self.stop_chat();
        self.files.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.indicator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.notification
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.pointer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.annotations_refused.store(false, Ordering::SeqCst);
    }
}

/// The tray icon opens a chat reply while a session offers chat.
impl super::tray::TrayChat for LocalInput {
    fn chat_available(&self) -> bool {
        self.chat.available()
            && self
                .indicator
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
    }
    fn ask_for_reply(&self) {
        if let Some(indicator) = &*self.indicator.lock().unwrap_or_else(|e| e.into_inner()) {
            indicator.ask_for_reply();
        }
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
    /// Linux display servers cannot show a window to the user that capture
    /// leaves out, so there is no blackout.
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()> {
        if enabled {
            bail!("blacking out the screen is not available on Linux");
        }
        Ok(())
    }
    fn maintenance_state(&self) -> Option<meshrmm_protocol::SessionMessage> {
        Some(meshrmm_protocol::SessionMessage::MaintenanceState {
            agent_input_blocked: self.controller().ok()?.blocked(),
            blacked_out: false,
        })
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        self.controller()?.set_blocked(blocked)
    }
    fn apply_files(&self, message: meshrmm_protocol::FileMessage) -> anyhow::Result<()> {
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .context("file transfers need a remote session")?
            .receive(message);
        Ok(())
    }
    fn poll_files(&self) -> Option<meshrmm_protocol::FileMessage> {
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .poll()
    }
    fn files_ready(&self) -> Arc<tokio::sync::Notify> {
        self.files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or_else(Arc::default, |files| files.outgoing_ready())
    }
    fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
        self.controller()?.apply(input)
    }
    /// Annotations need a window above everything that capture leaves out,
    /// which Linux display servers do not offer.
    fn annotate(&self, annotation: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
        if matches!(annotation, meshrmm_protocol::Annotation::Start { .. })
            && !self.annotations_refused.swap(true, Ordering::SeqCst)
        {
            bail!("drawing on the screen is not available on Linux");
        }
        Ok(())
    }
    fn release_all(&self) -> anyhow::Result<()> {
        self.controller()?.release_all()
    }
    fn cursor_shape(&self) -> CursorShape {
        *self.cursor.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// The display with the user's pointer while they use it, in an X11
    /// session.
    fn agent_pointer_display(&self) -> Option<DisplayId> {
        if self.viewer_controls_input() {
            return None;
        }
        let (x, y) = self
            .pointer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .position()?;
        super::display::at(self.controller().ok()?.displays(), x, y)
    }
    /// Wayland compositors don't tell whose input moved the pointer, so
    /// there the viewer is always taken to control it.
    fn viewer_controls_input(&self) -> bool {
        self.pointer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(pointer::PointerWatch::viewer_controls_input)
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
            self.chat.receive(text.clone());
            if let Some(indicator) = &*self.indicator.lock().unwrap_or_else(|e| e.into_inner()) {
                indicator.show_message(&text);
            }
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
