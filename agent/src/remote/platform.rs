use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use meshrmm_protocol::{
    Codec, CursorShape, Display, DisplayId, EncodedFrame, PixelFormat, RemoteInput, VideoFormat,
    VideoStreamId,
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
    fn stop(&mut self) -> anyhow::Result<()>;
    fn poll_ended(&mut self) -> Option<anyhow::Result<()>>;
    fn request_keyframe(&self) -> anyhow::Result<()>;
    fn set_bitrate(&mut self, bits_per_second: u32) -> anyhow::Result<()>;
    fn set_codec(&mut self, codec: Codec);
    fn apply_input(&mut self, input: RemoteInput) -> anyhow::Result<()>;
    fn release_input(&mut self) -> anyhow::Result<()>;
    fn cursor_shape(&self) -> CursorShape;
}

#[cfg(windows)]
pub struct PlatformScreenStreamer {
    inner: CaptureBackend,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    codec: Codec,
    next_frame_id: Arc<AtomicU64>,
    direct_input: super::input::WindowsInputController,
}

#[cfg(windows)]
impl PlatformScreenStreamer {
    pub fn new(
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        capture_as_active_user: bool,
    ) -> Self {
        Self {
            inner: if capture_as_active_user {
                CaptureBackend::Desktop(super::capture_helper::DesktopCaptureStreamer::new())
            } else {
                CaptureBackend::Direct(meshrmm_remote_screen::WindowsScreenStreamer::new())
            },
            frames_per_second,
            bitrate_bits_per_second,
            codec: Codec::H264,
            next_frame_id: Arc::new(AtomicU64::new(1)),
            direct_input: super::input::WindowsInputController::new(),
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
            frames_per_second: self.frames_per_second,
            bitrate_bits_per_second: self.bitrate_bits_per_second,
            codec: remote_screen_codec(self.codec),
        };
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => {
                let displays = enumerate_displays()?;
                let active_display = choose_display(&displays, requested_display_id)?;
                self.direct_input
                    .set_active_display(active_display.clone())?;
                let active = streamer.start(config, active_display.id.0, sink)?;
                Ok(StartedScreen {
                    displays,
                    active_display,
                    format: video_format(active),
                })
            }
            CaptureBackend::Desktop(streamer) => {
                let started = streamer.start(config, requested_display_id, sink)?;
                Ok(StartedScreen {
                    displays: started.displays,
                    active_display: started.active_display,
                    format: video_format(started.format),
                })
            }
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer.stop().map_err(anyhow::Error::from),
            CaptureBackend::Desktop(streamer) => streamer.stop(),
        }
        .context("Windows capture stop failed")
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

    fn set_bitrate(&mut self, bits_per_second: u32) -> anyhow::Result<()> {
        self.bitrate_bits_per_second = bits_per_second.max(1);
        match &self.inner {
            CaptureBackend::Direct(streamer) => streamer
                .set_bitrate(bits_per_second)
                .map_err(anyhow::Error::from),
            CaptureBackend::Desktop(streamer) => streamer.set_bitrate(bits_per_second),
        }
        .context("hardware encoder bitrate change failed")
    }

    fn set_codec(&mut self, codec: Codec) {
        self.codec = codec;
    }

    fn apply_input(&mut self, input: RemoteInput) -> anyhow::Result<()> {
        match &mut self.inner {
            CaptureBackend::Direct(_) => self.direct_input.apply(input),
            CaptureBackend::Desktop(streamer) => streamer.apply_input(input),
        }
    }

    fn release_input(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            CaptureBackend::Direct(_) => self.direct_input.release_all(),
            CaptureBackend::Desktop(streamer) => streamer.release_input(),
        }
    }

    fn cursor_shape(&self) -> CursorShape {
        match &self.inner {
            CaptureBackend::Direct(_) => self.direct_input.cursor_shape(),
            CaptureBackend::Desktop(streamer) => streamer.cursor_shape(),
        }
    }
}

#[cfg(windows)]
enum CaptureBackend {
    Direct(meshrmm_remote_screen::WindowsScreenStreamer),
    Desktop(super::capture_helper::DesktopCaptureStreamer),
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
        pixel_format: PixelFormat::Nv12,
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
fn enumerate_displays() -> anyhow::Result<Vec<Display>> {
    meshrmm_remote_screen::enumerate_displays()
        .context("failed to enumerate Windows displays")?
        .into_iter()
        .map(|display| {
            Ok(Display {
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
