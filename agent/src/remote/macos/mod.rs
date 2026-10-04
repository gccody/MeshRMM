//! The macOS screen streamer: ScreenCaptureKit capture, VideoToolbox
//! encoding and Quartz input, running in the signed-in user's session.
mod capture;
pub(crate) mod display;
mod encoder;
mod input;
mod keep_awake;
mod keymap;
mod wallpaper;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use meshrmm_protocol::{
    ChromaMode, ClipboardContent, Codec, CursorShape, DisplayId, EncodedFrame, HeadlessResolution,
    PixelFormat, QualityPreset, RemoteInput, VideoFormat, VideoStreamId,
};

use super::platform::{ScreenInput, ScreenStreamer, StartedScreen};
use super::video::LatestFrameSlot;

/// Microseconds of the host clock ScreenCaptureKit stamps frames with.
pub fn monotonic_timestamp_us() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: CLOCK_UPTIME_RAW, mach_absolute_time's clock, is valid and
    // `time` is a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) };
    time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1_000
}

/// Runs `work` on the main thread, which AppKit's desktop and screen APIs need.
fn on_main<T: Send + 'static>(
    work: impl FnOnce(objc2::MainThreadMarker) -> T + Send + 'static,
) -> T {
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        return work(mtm);
    }
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    dispatch2::DispatchQueue::main().exec_async(move || {
        let mtm = objc2::MainThreadMarker::new().expect("the main queue runs on the main thread");
        let _ = sender.send(work(mtm));
    });
    receiver
        .recv()
        .expect("the main thread runs its event loop for the Agent's lifetime")
}

pub struct PlatformScreenStreamer {
    capture: Option<capture::Capture>,
    frames_per_second: u32,
    quality: QualityPreset,
    bitrate_bits_per_second: u32,
    congestion_bitrate_bits_per_second: Option<u32>,
    codec: Codec,
    capture_cursor: bool,
    next_frame_id: Arc<AtomicU64>,
    input: Arc<MacInput>,
}

impl PlatformScreenStreamer {
    pub fn new(frames_per_second: u32, bitrate_bits_per_second: u32) -> anyhow::Result<Self> {
        wallpaper::restore_interrupted();
        Ok(Self {
            capture: None,
            frames_per_second,
            quality: QualityPreset::BestQuality,
            bitrate_bits_per_second,
            congestion_bitrate_bits_per_second: None,
            codec: Codec::H264,
            capture_cursor: true,
            next_frame_id: Arc::new(AtomicU64::new(1)),
            input: Arc::new(MacInput {
                controller: Mutex::new(input::InputController::new()?),
                files: meshrmm_file_transfer::TransferSession::agent(),
                chat: meshrmm_chat::ChatSession::with_peer("Viewer"),
                keep_awake: Mutex::new(None),
                wallpaper: Mutex::default(),
                clipboard: meshrmm_clipboard::ClipboardSync::new(false)
                    .inspect_err(|error| tracing::warn!(%error, "the clipboard is unavailable"))
                    .ok()
                    .map(Mutex::new),
            }),
        })
    }

    fn bitrate(&self) -> u32 {
        self.congestion_bitrate_bits_per_second
            .map_or(self.bitrate_bits_per_second, |limit| {
                limit.min(self.bitrate_bits_per_second)
            })
    }
}

impl ScreenStreamer for PlatformScreenStreamer {
    fn start(
        &mut self,
        display_id: Option<DisplayId>,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        self.capture = None;
        let displays = display::enumerate()?;
        let active_display = display::choose(&displays, display_id)?;
        self.input
            .controller()?
            .set_active_display(active_display.clone())?;
        let next_frame_id = Arc::clone(&self.next_frame_id);
        let sink = move |unit: encoder::EncodedAccessUnit| {
            let mut data = unit.codec_config.unwrap_or_default();
            data.extend_from_slice(&unit.data);
            slot.publish(EncodedFrame {
                stream_id,
                frame_id: next_frame_id.fetch_add(1, Ordering::Relaxed),
                capture_timestamp_us: unit.capture_timestamp_us,
                encode_complete_timestamp_us: unit.encode_complete_timestamp_us,
                send_timestamp_us: 0,
                keyframe: unit.keyframe,
                data,
            });
        };
        let capture = capture::Capture::start(
            &active_display,
            capture::CaptureConfig {
                frames_per_second: self.quality.frames_per_second(self.frames_per_second),
                bitrate_bits_per_second: self.bitrate(),
                codec: self.codec,
                capture_cursor: self.capture_cursor,
                grayscale: self.quality.grayscale(),
            },
            sink,
        )?;
        let format = capture.format();
        self.capture = Some(capture);
        Ok(StartedScreen {
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

    fn stop(&mut self) -> anyhow::Result<()> {
        self.capture = None;
        Ok(())
    }

    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        let error = self.capture.as_ref()?.poll_ended()?;
        self.capture = None;
        Some(Err(error.context("macOS screen capture stopped")))
    }

    fn request_keyframe(&self) -> anyhow::Result<()> {
        if let Some(capture) = &self.capture {
            capture.request_keyframe();
        }
        Ok(())
    }

    fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate_bits_per_second = bits_per_second.max(1);
        self.congestion_bitrate_bits_per_second = None;
    }

    fn set_congestion_bitrate(&mut self, bits_per_second: Option<u32>) {
        self.congestion_bitrate_bits_per_second = bits_per_second.map(|value| value.max(1));
    }

    fn set_quality(&mut self, quality: QualityPreset) -> bool {
        let changed = self.quality.grayscale() != quality.grayscale()
            || self.quality.frames_per_second(self.frames_per_second)
                != quality.frames_per_second(self.frames_per_second);
        self.quality = quality;
        changed
    }

    fn set_adaptive_bitrate(&mut self, bits_per_second: u32) -> anyhow::Result<()> {
        match &self.capture {
            Some(capture) => capture
                .set_bitrate(bits_per_second.max(1))
                .context("encoder bitrate change failed"),
            None => Ok(()),
        }
    }

    fn set_codec(&mut self, codec: Codec) {
        self.codec = codec;
    }

    /// VideoToolbox has no 4:4:4 hardware encoder, so viewers only ever get 4:2:0.
    fn set_chroma(&mut self, _chroma: ChromaMode) {}

    fn set_cursor_capture(&mut self, enabled: bool) -> anyhow::Result<bool> {
        self.capture_cursor = enabled;
        if let Some(capture) = &self.capture {
            capture.set_cursor_capture(enabled);
        }
        Ok(false)
    }

    fn set_display_border(&mut self, _enabled: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn set_headless_resolution(&mut self, _resolution: HeadlessResolution) -> bool {
        false
    }

    fn input_controller(&self) -> Arc<dyn ScreenInput> {
        Arc::clone(&self.input) as Arc<dyn ScreenInput>
    }
}

struct MacInput {
    controller: Mutex<input::InputController>,
    files: meshrmm_file_transfer::TransferSession,
    chat: meshrmm_chat::ChatSession,
    keep_awake: Mutex<Option<keep_awake::KeepAwake>>,
    wallpaper: Mutex<wallpaper::Wallpaper>,
    clipboard: Option<Mutex<meshrmm_clipboard::ClipboardSync>>,
}

impl MacInput {
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
}

impl ScreenInput for MacInput {
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
