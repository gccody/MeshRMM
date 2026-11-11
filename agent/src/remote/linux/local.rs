//! The screen and input of the session this process runs in. A console Agent
//! uses them directly; the installed Agent's session helpers serve them to the
//! coordinator.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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

/// How long a capture the display server started has to deliver its first
/// frame, which tells the stream's size. The backends wait up to 10 seconds
/// for the display server to start a capture and the coordinator 15 for the
/// helper to answer, so a start that needs both still answers in time.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the wait for the first frame checks whether the capture ended.
const FIRST_FRAME_POLL: Duration = Duration::from_millis(50);

/// A started stream. The displays are in the desktop's logical layout (see
/// [`super::display`]), which the viewer's normalized pointer positions are
/// mapped onto; the format is the size of the H.264 pictures, from the
/// frames the capture delivers. With display scaling on Wayland the two
/// differ: a 3840x2160 panel at 200% is a 1920x1080 display streamed at
/// 3840x2160.
pub(crate) struct Started {
    pub displays: Vec<Display>,
    pub active_display: Display,
    pub format: VideoFormat,
}

/// A running capture, and what it needs to start again with the same
/// settings.
struct Running {
    output: Output,
    settings: StreamSettings,
    /// The size of the frames the capture delivers, whose encoded size the
    /// stream announced.
    size: (u32, u32),
    gate: Arc<SizeGate>,
    stream: Box<dyn Stream>,
    encoder: Arc<VideoEncoder>,
    sink: FrameSink,
}

impl Running {
    /// Starts capturing `output` into a new encoder. Without `size`, waits up
    /// to `first_frame` for the first frame, whose size the stream then
    /// keeps; with it, the capture must keep that size, as when a capture
    /// starts again under a format already announced.
    fn start(
        backend: &mut dyn Backend,
        output: Output,
        settings: StreamSettings,
        sink: FrameSink,
        size: Option<(u32, u32)>,
        first_frame: Duration,
    ) -> anyhow::Result<Self> {
        let encoder = Arc::new(VideoEncoder::start(
            EncoderSettings {
                frames_per_second: settings.frames_per_second,
                bitrate_bits_per_second: settings.bitrate_bits_per_second,
                grayscale: settings.grayscale,
            },
            Arc::clone(&sink),
        )?);
        let gate = Arc::new(SizeGate::new(size));
        let frames: capture::FrameSink = {
            let (gate, encoder) = (Arc::clone(&gate), Arc::clone(&encoder));
            Arc::new(move |frame: &RawFrame<'_>| {
                if frame.encodable() && gate.admit((frame.width, frame.height)) {
                    encoder.submit(frame);
                }
            })
        };
        let mut stream = backend.start(
            &output,
            CaptureSettings {
                frames_per_second: settings.frames_per_second,
                capture_cursor: settings.capture_cursor,
            },
            frames,
        )?;
        let size = match size {
            Some(size) => size,
            None => gate.wait_for_size(first_frame, || stream.poll_ended())?,
        };
        Ok(Self {
            output,
            settings,
            size,
            gate,
            stream,
            encoder,
            sink,
        })
    }

    /// The format the stream's frames are encoded in.
    fn format(&self) -> VideoFormat {
        stream_format(self.size, &self.settings)
    }

    /// Why the stream must end, once it must: the capture stopped, the
    /// encoder failed, or the frames changed size, which the viewer can
    /// learn only from a new stream.
    fn poll_ended(&mut self) -> Option<anyhow::Error> {
        if let Some(size) = self.gate.changed() {
            return Some(anyhow::anyhow!(
                "the captured display changed from {}x{} to {}x{} pixels",
                self.size.0,
                self.size.1,
                size.0,
                size.1
            ));
        }
        self.stream.poll_ended().or_else(|| {
            self.encoder
                .failure()
                .map(|failure| anyhow::anyhow!(failure).context("the H.264 encoder stopped"))
        })
    }
}

/// The format of a stream whose frames are `size`: their encoded size,
/// whatever size the display has in the desktop's layout.
fn stream_format(size: (u32, u32), settings: &StreamSettings) -> VideoFormat {
    let (width, height) = super::encoder::encoded_size(size.0, size.1);
    VideoFormat {
        width,
        height,
        frames_per_second: settings.frames_per_second as u16,
        codec: Codec::H264,
        pixel_format: PixelFormat::Nv12,
        bitrate_bits_per_second: settings.bitrate_bits_per_second,
    }
}

/// The frame sizes a stream saw.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Sizes {
    /// The stream's size: its first frame's, or the one already announced.
    stream: Option<(u32, u32)>,
    /// The first size that differed from it.
    changed: Option<(u32, u32)>,
}

impl Sizes {
    /// Whether a frame of `size` belongs to the stream. The first frame
    /// fixes the stream's size; a frame of another size is kept from the
    /// encoder, so nothing is encoded at a size the viewer was not told.
    fn admit(&mut self, size: (u32, u32)) -> bool {
        match self.stream {
            None => {
                self.stream = Some(size);
                true
            }
            Some(stream) if stream == size => true,
            Some(_) => {
                self.changed.get_or_insert(size);
                false
            }
        }
    }
}

/// Stands between a capture and its encoder, letting through only frames of
/// the stream's size.
struct SizeGate {
    sizes: Mutex<Sizes>,
    first: Condvar,
}

impl SizeGate {
    fn new(stream: Option<(u32, u32)>) -> Self {
        Self {
            sizes: Mutex::new(Sizes {
                stream,
                changed: None,
            }),
            first: Condvar::new(),
        }
    }

    fn admit(&self, size: (u32, u32)) -> bool {
        let mut sizes = self.sizes.lock().unwrap_or_else(|e| e.into_inner());
        let first = sizes.stream.is_none();
        let admitted = sizes.admit(size);
        drop(sizes);
        if first {
            self.first.notify_all();
        }
        admitted
    }

    /// The size the frames changed to, once they have.
    fn changed(&self) -> Option<(u32, u32)> {
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).changed
    }

    /// The stream's size, once the first frame arrives within `timeout`.
    /// Fails sooner when `ended` says the capture stopped.
    fn wait_for_size(
        &self,
        timeout: Duration,
        mut ended: impl FnMut() -> Option<anyhow::Error>,
    ) -> anyhow::Result<(u32, u32)> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let sizes = self.sizes.lock().unwrap_or_else(|e| e.into_inner());
                let wait = deadline.saturating_duration_since(Instant::now());
                let (sizes, _) = self
                    .first
                    .wait_timeout_while(sizes, wait.min(FIRST_FRAME_POLL), |sizes| {
                        sizes.stream.is_none()
                    })
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(size) = sizes.stream {
                    return Ok(size);
                }
            }
            if let Some(error) = ended() {
                return Err(error.context("the capture stopped before its first frame"));
            }
            if Instant::now() >= deadline {
                bail!(
                    "the display server sent no picture within {} seconds of starting the capture",
                    timeout.as_secs_f32()
                );
            }
        }
    }
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
        // The format is announced once the first frame tells its size.
        let running = self.capture(output, settings, Arc::new(sink), None)?;
        let format = running.format();
        self.running = Some(running);
        Ok(Started {
            displays,
            active_display,
            format,
        })
    }

    /// Starts capturing `output`; see [`Running::start`].
    fn capture(
        &mut self,
        output: Output,
        settings: StreamSettings,
        sink: FrameSink,
        size: Option<(u32, u32)>,
    ) -> anyhow::Result<Running> {
        let backend = self.backend()?.as_mut();
        let running = Running::start(backend, output, settings, sink, size, FIRST_FRAME_TIMEOUT);
        if running.is_err() {
            self.backend = None;
        }
        running
    }

    pub(crate) fn stop(&mut self) {
        self.running = None;
    }

    pub(crate) fn poll_ended(&mut self) -> Option<anyhow::Error> {
        let running = self.running.as_mut()?;
        self.input.set_cursor(running.stream.cursor_shape());
        let error = running.poll_ended()?;
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

    /// Changes the running encoder's bitrate, and the bitrate a capture
    /// restarted for the pointer starts at.
    pub(crate) fn set_bitrate(&mut self, bits_per_second: u32) -> anyhow::Result<()> {
        if let Some(running) = &mut self.running {
            running.settings.bitrate_bits_per_second = bits_per_second.max(1);
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
            size,
            sink,
            ..
        }) = self.running.take()
        else {
            return;
        };
        // The restarted capture keeps to the announced size; a frame of
        // another size ends the stream.
        match self.capture(output, settings, sink, Some(size)) {
            Ok(running) => self.running = Some(running),
            Err(error) => {
                tracing::warn!(error = ?error, "could not restart capture for the pointer")
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::linux::encoder::PixelLayout;

    fn settings() -> StreamSettings {
        StreamSettings {
            frames_per_second: 30,
            bitrate_bits_per_second: 2_000_000,
            codec: Codec::H264,
            capture_cursor: true,
            grayscale: false,
        }
    }

    /// A display as a Wayland backend enumerates it, in the logical layout.
    fn output(width: u32, height: u32) -> Output {
        Output {
            name: "eDP-1".to_owned(),
            x: 0,
            y: 0,
            width,
            height,
            primary: true,
        }
    }

    fn deliver(sink: &capture::FrameSink, (width, height): (u32, u32)) {
        let data = vec![0x80; width as usize * height as usize * 4];
        sink(&RawFrame {
            data: &data,
            width,
            height,
            stride: width as usize * 4,
            layout: PixelLayout::Bgrx,
            timestamp_us: 1,
        });
    }

    /// A capture backend that delivers frames of the given sizes as the
    /// capture starts, whatever the display's size, as PipeWire does with a
    /// scaled display.
    #[derive(Default)]
    struct Fake {
        frames: Vec<(u32, u32)>,
        /// Why the stream ended, when it ends at once.
        ended: Option<&'static str>,
        /// The capture's sink, for frames sent later.
        sink: Arc<Mutex<Option<capture::FrameSink>>>,
    }

    impl Backend for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn outputs(&mut self) -> anyhow::Result<Vec<Output>> {
            Ok(Vec::new())
        }

        fn start(
            &mut self,
            _output: &Output,
            _settings: CaptureSettings,
            sink: capture::FrameSink,
        ) -> anyhow::Result<Box<dyn Stream>> {
            for &size in &self.frames {
                deliver(&sink, size);
            }
            *self.sink.lock().unwrap() = Some(sink);
            Ok(Box::new(FakeStream { ended: self.ended }))
        }
    }

    struct FakeStream {
        ended: Option<&'static str>,
    }

    impl Stream for FakeStream {
        fn poll_ended(&mut self) -> Option<anyhow::Error> {
            self.ended.take().map(|reason| anyhow::anyhow!(reason))
        }

        fn set_cursor_capture(&mut self, _enabled: bool) -> bool {
            true
        }
    }

    fn start(
        fake: &mut Fake,
        display: Output,
        size: Option<(u32, u32)>,
        first_frame: Duration,
    ) -> anyhow::Result<Running> {
        Running::start(
            fake,
            display,
            settings(),
            Arc::new(|_| {}),
            size,
            first_frame,
        )
    }

    #[test]
    fn the_format_is_the_frames_encoded_size_whatever_the_displays_scale() {
        // A 3840x2160 panel at 100%, 150% and 200%: its display in the
        // logical layout, and the frames PipeWire delivers.
        for (display, frames) in [
            ((3840, 2160), (3840, 2160)),
            ((2560, 1440), (3840, 2160)),
            ((1920, 1080), (3840, 2160)),
        ] {
            let format = stream_format(frames, &settings());
            assert_eq!((format.width, format.height), (3840, 2160), "{display:?}");
        }
        // Odd sizes lose a row and a column, and larger than 4K is scaled
        // to fit: a 5120x2880 panel at 200% is a 2560x1440 display.
        let format = stream_format((1365, 767), &settings());
        assert_eq!((format.width, format.height), (1364, 766));
        let format = stream_format((5120, 2880), &settings());
        assert_eq!((format.width, format.height), (3840, 2160));
        assert_eq!(format.frames_per_second, 30);
        assert_eq!(format.bitrate_bits_per_second, 2_000_000);
    }

    #[test]
    fn the_first_frame_fixes_the_size_and_another_size_is_kept_from_the_encoder() {
        let mut sizes = Sizes::default();
        assert!(sizes.admit((3840, 2160)));
        assert!(sizes.admit((3840, 2160)));
        // The scale went from 200% to 150% with the stream at 4K, then the
        // resolution changed again.
        assert!(!sizes.admit((2560, 1440)));
        assert!(!sizes.admit((1920, 1080)));
        assert_eq!(
            sizes,
            Sizes {
                stream: Some((3840, 2160)),
                changed: Some((2560, 1440)),
            }
        );
        // A capture started again under an announced size keeps to it.
        let mut restarted = Sizes {
            stream: Some((3840, 2160)),
            changed: None,
        };
        assert!(!restarted.admit((1920, 1080)));
        assert_eq!(restarted.changed, Some((1920, 1080)));
    }

    #[test]
    fn announces_the_size_the_capture_delivers_rather_than_the_displays() {
        // A 640x360 panel at 200% and 150%, and frames of an odd size.
        for (display, frames, announced) in [
            ((320, 180), (640, 360), (640, 360)),
            ((427, 240), (640, 360), (640, 360)),
            ((320, 180), (641, 361), (640, 360)),
        ] {
            let mut fake = Fake {
                frames: vec![frames],
                ..Fake::default()
            };
            let running = start(
                &mut fake,
                output(display.0, display.1),
                None,
                Duration::from_secs(5),
            )
            .unwrap();
            let format = running.format();
            assert_eq!((format.width, format.height), announced, "{display:?}");
            // The display keeps its logical size, which input maps onto.
            let shown = running.output.display();
            assert_eq!((shown.width, shown.height), display);
        }
    }

    #[test]
    fn a_capture_without_a_first_frame_does_not_start() {
        let mut fake = Fake::default();
        let Err(error) = start(
            &mut fake,
            output(320, 180),
            None,
            Duration::from_millis(200),
        ) else {
            panic!("a stream started without a frame to tell its size");
        };
        assert!(
            format!("{error:#}").contains("sent no picture"),
            "{error:#}"
        );
        // One that ends first says why, without waiting out the time.
        let mut fake = Fake {
            ended: Some("the compositor ended the screencast"),
            ..Fake::default()
        };
        let waited = Instant::now();
        let Err(error) = start(&mut fake, output(320, 180), None, Duration::from_secs(30)) else {
            panic!("a stream started after its capture ended");
        };
        assert!(waited.elapsed() < Duration::from_secs(5));
        assert!(
            format!("{error:#}").contains("the compositor ended the screencast"),
            "{error:#}"
        );
    }

    #[test]
    fn a_size_change_mid_stream_ends_the_stream() {
        let mut fake = Fake {
            frames: vec![(640, 360)],
            ..Fake::default()
        };
        let sink = Arc::clone(&fake.sink);
        let mut running = start(&mut fake, output(320, 180), None, Duration::from_secs(5)).unwrap();
        let sink = sink.lock().unwrap().clone().unwrap();
        deliver(&sink, (640, 360));
        assert!(running.poll_ended().is_none());
        // The user changes the scale and the compositor renegotiates.
        deliver(&sink, (960, 540));
        let error = running
            .poll_ended()
            .expect("the stream went on at a new size");
        assert!(
            format!("{error:#}").contains("from 640x360 to 960x540"),
            "{error:#}"
        );
        let format = running.format();
        assert_eq!((format.width, format.height), (640, 360));
    }

    #[test]
    fn a_capture_started_again_keeps_to_the_announced_size() {
        // As when the pointer setting restarts the capture.
        let mut fake = Fake {
            frames: vec![(960, 540)],
            ..Fake::default()
        };
        let mut running = start(
            &mut fake,
            output(320, 180),
            Some((640, 360)),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(running.poll_ended().is_some());
    }
}
