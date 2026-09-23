use meshrmm_protocol::ClipboardContent;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use meshrmm_protocol::{
    ChromaMode, Codec, CursorShape, Display, DisplayId, EncodedFrame, PixelFormat, QualityPreset,
    RemoteInput, VideoFormat, VideoStreamId,
};

use super::video::LatestFrameSlot;

pub struct StartedScreen {
    pub displays: Vec<Display>,
    pub active_display: Display,
    pub format: VideoFormat,
}

pub trait ScreenStreamer: Send {
    fn start(
        &mut self,
        display_id: Option<DisplayId>,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen>;
    fn switch_display(
        &mut self,
        display_id: DisplayId,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        self.stop()?;
        self.start(Some(display_id), stream_id, slot)
    }
    fn stop(&mut self) -> anyhow::Result<()>;
    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop()
    }
    fn poll_ended(&mut self) -> Option<anyhow::Result<()>>;
    fn request_keyframe(&self) -> anyhow::Result<()>;
    /// Configure the bitrate for the next start; preset changes restart capture.
    fn set_bitrate(&mut self, bits_per_second: u32);
    /// Configure capture color/rate; return whether a restart is required.
    fn set_quality(&mut self, quality: QualityPreset) -> bool;
    fn set_adaptive_bitrate(&mut self, bits_per_second: u32) -> anyhow::Result<()>;
    fn set_codec(&mut self, codec: Codec);
    fn set_chroma(&mut self, chroma: ChromaMode);
    /// Returns true when the backend requires capture to be restarted.
    fn set_cursor_capture(&mut self, enabled: bool) -> anyhow::Result<bool>;
    fn set_display_border(&mut self, enabled: bool) -> anyhow::Result<()>;
    fn input_controller(&self) -> Arc<dyn ScreenInput>;
}

pub trait ScreenInput: Send + Sync {
    fn credential_command(&self, _message: meshrmm_protocol::SessionMessage) -> anyhow::Result<()> {
        anyhow::bail!("Credentials require the installed Windows service")
    }
    fn credential_state(&self) -> Option<meshrmm_protocol::CredentialState> {
        None
    }

    /// Session 0 audio capture and service SendSAS target only the console.
    fn is_console_session(&self) -> bool {
        !self.is_background()
    }

    fn is_background(&self) -> bool {
        false
    }
    fn set_wallpaper_hidden(&self, hidden: bool) -> anyhow::Result<()>;
    fn set_prevent_idle_lock(&self, enabled: bool) -> anyhow::Result<()>;
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()>;
    fn maintenance_state(&self) -> Option<meshrmm_protocol::SessionMessage>;
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()>;
    fn apply_files(&self, message: meshrmm_protocol::FileMessage) -> anyhow::Result<()>;
    fn poll_files(&self) -> Option<meshrmm_protocol::FileMessage>;
    fn files_ready(&self) -> Arc<tokio::sync::Notify>;
    fn apply(&self, input: RemoteInput) -> anyhow::Result<()>;
    fn release_all(&self) -> anyhow::Result<()>;
    fn cursor_shape(&self) -> CursorShape;
    fn agent_pointer_display(&self) -> Option<DisplayId>;
    fn viewer_controls_input(&self) -> bool;
    fn apply_clipboard(&self, text: ClipboardContent) -> anyhow::Result<()>;
    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>>;
    /// Helpers already detect changes; direct native implementations may poll.
    fn clipboard_ready(&self) -> Option<Arc<tokio::sync::Notify>> {
        None
    }
    fn start_chat(&self) -> anyhow::Result<()>;
    fn stop_chat(&self);
    fn apply_chat(&self, text: String) -> anyhow::Result<()>;
    fn poll_chat(&self) -> anyhow::Result<Option<String>>;
    fn chat_ready(&self) -> Arc<tokio::sync::Notify>;
}

#[cfg(windows)]
pub struct PlatformScreenStreamer {
    inner: CaptureBackend,
    viewer_name: String,
    blackout_message: String,
    border_enabled: bool,
    border: Option<super::display_border::DisplayBorder>,
    border_display: Option<Display>,
    indicator: Option<super::indicator::SessionIndicator>,
    outline_dragging: Option<super::drag_windows::ConsoleOutlineDragging>,
    frames_per_second: u32,
    quality: QualityPreset,
    bitrate_bits_per_second: u32,
    codec: Codec,
    chroma: ChromaMode,
    capture_cursor: bool,
    next_frame_id: Arc<AtomicU64>,
    direct_files: Option<meshrmm_file_transfer::TransferSession>,
    direct_chat: meshrmm_chat::ChatSession,
    direct_input: Arc<Mutex<super::input::WindowsInputController>>,
    direct_clipboard: Option<Arc<Mutex<super::clipboard::ClipboardSync>>>,
}

#[cfg(windows)]
impl PlatformScreenStreamer {
    pub fn new(
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        capture_as_active_user: bool,
        viewer_name: String,
        blackout_message: String,
        credential_store: std::path::PathBuf,
    ) -> Self {
        Self {
            inner: if capture_as_active_user {
                CaptureBackend::Desktop(Box::new(
                    super::capture_helper::DesktopCaptureStreamer::new(
                        viewer_name.clone(),
                        blackout_message.clone(),
                        credential_store,
                    ),
                ))
            } else {
                CaptureBackend::Direct(meshrmm_remote_screen::WindowsScreenStreamer::new())
            },
            viewer_name,
            blackout_message,
            border_enabled: false,
            border: None,
            border_display: None,
            indicator: None,
            outline_dragging: None,
            frames_per_second,
            quality: QualityPreset::BestQuality,
            bitrate_bits_per_second,
            codec: Codec::H264,
            chroma: ChromaMode::Yuv420,
            capture_cursor: true,
            next_frame_id: Arc::new(AtomicU64::new(1)),
            direct_files: (!capture_as_active_user)
                .then(meshrmm_file_transfer::TransferSession::new),
            direct_chat: meshrmm_chat::ChatSession::with_peer("Viewer"),
            direct_input: Arc::new(Mutex::new(super::input::WindowsInputController::new())),
            // Service workers live in non-interactive Session 0. Their clipboard
            // is neither the user's clipboard nor a safe place to perform
            // desktop-bound operations, so the desktop helper owns clipboard
            // access for that mode. Console mode is already interactive.
            direct_clipboard: (!capture_as_active_user)
                .then(|| super::clipboard::ClipboardSync::new(false))
                .transpose()
                .ok()
                .flatten()
                .map(|clipboard| Arc::new(Mutex::new(clipboard))),
        }
    }
}

#[cfg(windows)]
impl ScreenStreamer for PlatformScreenStreamer {
    fn start(
        &mut self,
        requested_display_id: Option<DisplayId>,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        let next_frame_id = Arc::clone(&self.next_frame_id);
        let sink = Arc::new(
            move |access_unit: meshrmm_remote_screen::EncodedAccessUnit| {
                let frame_id = next_frame_id.fetch_add(1, Ordering::Relaxed);
                let mut data = access_unit.codec_config.unwrap_or_default();
                data.extend_from_slice(&access_unit.data);
                let frame = EncodedFrame {
                    stream_id,
                    frame_id,
                    capture_timestamp_us: access_unit.capture_timestamp_us,
                    encode_complete_timestamp_us: access_unit.encode_complete_timestamp_us,
                    send_timestamp_us: 0,
                    keyframe: access_unit.keyframe,
                    data,
                };
                slot.publish(frame);
            },
        );
        let config = meshrmm_remote_screen::StreamConfig {
            frames_per_second: self.quality.frames_per_second(self.frames_per_second),
            grayscale: self.quality.grayscale(),
            bitrate_bits_per_second: self.bitrate_bits_per_second,
            codec: remote_screen_codec(self.codec),
            pixel_format: remote_screen_pixel_format(self.chroma),
            capture_cursor: self.capture_cursor,
        };
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => {
                if self.outline_dragging.is_none() {
                    self.outline_dragging = super::drag_windows::ConsoleOutlineDragging::new()
                        .inspect_err(|error| {
                            tracing::warn!(%error, "could not disable window contents while dragging");
                        })
                        .ok();
                }
                let displays = enumerate_displays()?;
                meshrmm_file_transfer::windows::set_displays(displays.clone());
                let active_display = choose_display(&displays, requested_display_id)?;
                self.direct_input
                    .lock()
                    .map_err(|_| anyhow::anyhow!("direct input controller lock was poisoned"))?
                    .set_active_display(active_display.clone())?;
                let active = streamer.start(config, active_display.id.0, sink)?;
                self.border = None;
                self.border_display = Some(active_display.clone());
                if self.border_enabled {
                    self.border =
                        Some(super::display_border::DisplayBorder::show(&active_display)?);
                }
                if self.indicator.is_none() {
                    self.indicator = Some(super::indicator::SessionIndicator::show(
                        &self.viewer_name,
                        self.direct_chat.clone(),
                    )?);
                }
                Ok(StartedScreen {
                    displays,
                    active_display,
                    format: video_format(active),
                })
            }
            CaptureBackend::Desktop(streamer) => {
                let started = streamer.start(config, requested_display_id, sink)?;
                streamer.set_display_border(self.border_enabled)?;
                Ok(StartedScreen {
                    displays: started.displays,
                    active_display: started.active_display,
                    format: video_format(started.format),
                })
            }
        }
    }

    fn switch_display(
        &mut self,
        display_id: DisplayId,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        if let CaptureBackend::Direct(streamer) = &mut self.inner {
            streamer.stop()?;
        }
        self.start(Some(display_id), stream_id, slot)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.indicator = None;
        self.border = None;
        self.border_display = None;
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer.stop().map_err(anyhow::Error::from),
            CaptureBackend::Desktop(streamer) => streamer.stop(),
        }
        .context("Windows capture stop failed")
    }

    fn shutdown(&mut self) -> anyhow::Result<()> {
        self.indicator = None;
        self.border = None;
        self.border_display = None;
        self.outline_dragging = None;
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer.stop().map_err(anyhow::Error::from),
            CaptureBackend::Desktop(streamer) => streamer.shutdown(),
        }
    }

    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer
                .poll_ended()
                .map(|result| result.map_err(anyhow::Error::from)),
            CaptureBackend::Desktop(streamer) => streamer.poll_ended(),
        }
        .map(|result| result.context("Windows GPU capture/encode worker stopped"))
    }

    fn request_keyframe(&self) -> anyhow::Result<()> {
        match &self.inner {
            CaptureBackend::Direct(streamer) => {
                streamer.request_keyframe().map_err(anyhow::Error::from)
            }
            CaptureBackend::Desktop(streamer) => streamer.request_keyframe(),
        }
        .context("hardware keyframe request failed")
    }

    fn set_bitrate(&mut self, bits_per_second: u32) {
        self.bitrate_bits_per_second = bits_per_second.max(1);
    }

    fn set_quality(&mut self, quality: QualityPreset) -> bool {
        let changed = self.quality.grayscale() != quality.grayscale()
            || self.quality.frames_per_second(self.frames_per_second)
                != quality.frames_per_second(self.frames_per_second);
        self.quality = quality;
        changed
    }

    fn set_adaptive_bitrate(&mut self, bits_per_second: u32) -> anyhow::Result<()> {
        if self.codec == Codec::H265 {
            tracing::info!(
                bits_per_second,
                "skipping an unsafe live HEVC bitrate adjustment"
            );
            return Ok(());
        }
        match &self.inner {
            CaptureBackend::Direct(streamer) => streamer
                .set_bitrate(bits_per_second.max(1))
                .map_err(anyhow::Error::from),
            CaptureBackend::Desktop(streamer) => streamer.set_bitrate(bits_per_second.max(1)),
        }
        .context("hardware encoder bitrate change failed")
    }

    fn set_codec(&mut self, codec: Codec) {
        self.codec = codec;
    }

    fn set_chroma(&mut self, chroma: ChromaMode) {
        self.chroma = chroma;
    }

    fn set_cursor_capture(&mut self, enabled: bool) -> anyhow::Result<bool> {
        let changed = self.capture_cursor != enabled;
        self.capture_cursor = enabled;
        match &self.inner {
            // windows-capture does not expose runtime WGC cursor settings.
            CaptureBackend::Direct(_) => Ok(changed),
            CaptureBackend::Desktop(streamer) => {
                streamer.set_cursor_capture(enabled)?;
                Ok(false)
            }
        }
    }

    fn set_display_border(&mut self, enabled: bool) -> anyhow::Result<()> {
        self.border_enabled = enabled;
        match &self.inner {
            CaptureBackend::Desktop(streamer) => streamer.set_display_border(enabled),
            CaptureBackend::Direct(_) => {
                self.border = None;
                if enabled && let Some(display) = &self.border_display {
                    self.border = Some(super::display_border::DisplayBorder::show(display)?);
                }
                Ok(())
            }
        }
    }

    fn input_controller(&self) -> Arc<dyn ScreenInput> {
        match &self.inner {
            CaptureBackend::Direct(_) => Arc::new(DirectInputController {
                wallpaper: Mutex::new(None),
                keep_awake: Mutex::new(None),
                blackout_message: self.blackout_message.clone(),
                controller: Arc::clone(&self.direct_input),
                files: self
                    .direct_files
                    .clone()
                    .expect("direct input owns a transfer session"),
                clipboard: self.direct_clipboard.clone(),
                chat: self.direct_chat.clone(),
            }),
            CaptureBackend::Desktop(streamer) => streamer.input_controller(),
        }
    }
}

#[cfg(windows)]
struct DirectInputController {
    keep_awake: Mutex<Option<super::keep_awake::KeepAwake>>,
    wallpaper: Mutex<Option<super::wallpaper::HiddenWallpaper>>,
    blackout_message: String,
    files: meshrmm_file_transfer::TransferSession,
    chat: meshrmm_chat::ChatSession,
    controller: Arc<Mutex<super::input::WindowsInputController>>,
    clipboard: Option<Arc<Mutex<super::clipboard::ClipboardSync>>>,
}

#[cfg(windows)]
impl ScreenInput for DirectInputController {
    fn set_prevent_idle_lock(&self, enabled: bool) -> anyhow::Result<()> {
        super::keep_awake::set_enabled(
            &mut self.keep_awake.lock().unwrap_or_else(|e| e.into_inner()),
            enabled,
        )
    }
    fn set_wallpaper_hidden(&self, hidden: bool) -> anyhow::Result<()> {
        super::wallpaper::set_hidden(
            &mut *self
                .wallpaper
                .lock()
                .map_err(|_| anyhow::anyhow!("wallpaper lock poisoned"))?,
            hidden,
        )
    }
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()> {
        self.controller
            .lock()
            .map_err(|_| anyhow::anyhow!("input lock poisoned"))?
            .set_blackout(enabled, &self.blackout_message)
    }
    fn maintenance_state(&self) -> Option<meshrmm_protocol::SessionMessage> {
        self.controller.lock().ok().map(|input| {
            meshrmm_protocol::SessionMessage::MaintenanceState {
                agent_input_blocked: input.blocked(),
                blacked_out: input.blacked_out(),
            }
        })
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        self.controller
            .lock()
            .map_err(|_| anyhow::anyhow!("input lock poisoned"))?
            .set_blocked(blocked)
    }
    fn stop_chat(&self) {
        self.chat.set_available(false);
    }
    fn start_chat(&self) -> anyhow::Result<()> {
        self.chat.set_available(true);
        Ok(())
    }
    fn apply_chat(&self, text: String) -> anyhow::Result<()> {
        if self.chat.available() {
            self.chat.receive(text);
        }
        Ok(())
    }
    fn chat_ready(&self) -> Arc<tokio::sync::Notify> {
        self.chat.outgoing_ready()
    }
    fn poll_chat(&self) -> anyhow::Result<Option<String>> {
        Ok(self.chat.poll())
    }
    fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
        self.controller
            .lock()
            .map_err(|_| anyhow::anyhow!("direct input controller lock was poisoned"))?
            .apply(input)
    }

    fn release_all(&self) -> anyhow::Result<()> {
        self.controller
            .lock()
            .map_err(|_| anyhow::anyhow!("direct input controller lock was poisoned"))?
            .release_all()
    }

    fn viewer_controls_input(&self) -> bool {
        self.controller
            .lock()
            .is_ok_and(|input| input.viewer_controls_input())
    }

    fn agent_pointer_display(&self) -> Option<DisplayId> {
        self.controller
            .lock()
            .ok()
            .and_then(|input| input.agent_pointer_display())
    }

    fn cursor_shape(&self) -> CursorShape {
        self.controller
            .lock()
            .map_or(CursorShape::Default, |input| input.cursor_shape())
    }

    fn apply_files(&self, message: meshrmm_protocol::FileMessage) -> anyhow::Result<()> {
        self.files.receive(message);
        Ok(())
    }
    fn files_ready(&self) -> Arc<tokio::sync::Notify> {
        self.files.outgoing_ready()
    }
    fn poll_files(&self) -> Option<meshrmm_protocol::FileMessage> {
        self.files.poll()
    }
    fn apply_clipboard(&self, text: ClipboardContent) -> anyhow::Result<()> {
        self.clipboard
            .as_ref()
            .context("interactive Windows clipboard is unavailable")?
            .lock()
            .map_err(|_| anyhow::anyhow!("direct clipboard lock was poisoned"))?
            .apply(text)
    }

    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
        self.clipboard
            .as_ref()
            .context("interactive Windows clipboard is unavailable")?
            .lock()
            .map_err(|_| anyhow::anyhow!("direct clipboard lock was poisoned"))?
            .poll()
    }
}

#[cfg(windows)]
enum CaptureBackend {
    Direct(meshrmm_remote_screen::WindowsScreenStreamer),
    Desktop(Box<super::capture_helper::DesktopCaptureStreamer>),
}

#[cfg(windows)]
fn video_format(active: meshrmm_remote_screen::ActiveFormat) -> VideoFormat {
    VideoFormat {
        width: active.width,
        height: active.height,
        frames_per_second: active.frames_per_second as u16,
        codec: match active.codec {
            meshrmm_remote_screen::VideoCodec::H264 => Codec::H264,
            meshrmm_remote_screen::VideoCodec::H265 => Codec::H265,
        },
        pixel_format: match active.pixel_format {
            meshrmm_remote_screen::VideoPixelFormat::Yuv420 => PixelFormat::Nv12,
            meshrmm_remote_screen::VideoPixelFormat::Yuv444 => PixelFormat::Ayuv,
        },
        bitrate_bits_per_second: active.bitrate_bits_per_second,
    }
}

#[cfg(windows)]
fn remote_screen_codec(codec: Codec) -> meshrmm_remote_screen::VideoCodec {
    match codec {
        Codec::H264 => meshrmm_remote_screen::VideoCodec::H264,
        Codec::H265 => meshrmm_remote_screen::VideoCodec::H265,
    }
}

#[cfg(windows)]
fn remote_screen_pixel_format(chroma: ChromaMode) -> meshrmm_remote_screen::VideoPixelFormat {
    match chroma {
        ChromaMode::Yuv420 => meshrmm_remote_screen::VideoPixelFormat::Yuv420,
        ChromaMode::Yuv444 => meshrmm_remote_screen::VideoPixelFormat::Yuv444,
    }
}

#[cfg(windows)]
pub(super) fn enumerate_displays() -> anyhow::Result<Vec<Display>> {
    meshrmm_remote_screen::enumerate_displays()
        .context("failed to enumerate Windows displays")?
        .into_iter()
        .map(|display| {
            Ok(Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: DisplayId(display.id),
                name: display.name,
                x: display.x,
                y: display.y,
                width: display.width,
                height: display.height,
                primary: display.primary,
            })
        })
        .collect()
}

#[cfg(windows)]
fn choose_display(displays: &[Display], requested: Option<DisplayId>) -> anyhow::Result<Display> {
    requested
        .and_then(|id| displays.iter().find(|display| display.id == id))
        .or_else(|| displays.iter().find(|display| display.primary))
        .or_else(|| displays.first())
        .cloned()
        .context("Windows reported no active displays")
}

#[cfg(windows)]
pub fn monotonic_timestamp_us() -> u64 {
    meshrmm_remote_screen::monotonic_timestamp_us().unwrap_or(0)
}
