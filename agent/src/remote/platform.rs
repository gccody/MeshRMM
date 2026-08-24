use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use meshrmm_protocol::{
    Codec, Display, DisplayId, EncodedFrame, PixelFormat, VideoFormat, VideoStreamId,
};

use super::video::LatestFrameSlot;

pub trait ScreenStreamer: Send {
    fn displays(&self) -> anyhow::Result<Vec<Display>>;
    fn start(
        &mut self,
        display_id: DisplayId,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<VideoFormat>;
    fn stop(&mut self) -> anyhow::Result<()>;
    fn poll_ended(&mut self) -> Option<anyhow::Result<()>>;
    fn request_keyframe(&self) -> anyhow::Result<()>;
    fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()>;
}

#[cfg(windows)]
pub struct PlatformScreenStreamer {
    inner: CaptureBackend,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    next_frame_id: Arc<AtomicU64>,
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
                CaptureBackend::ActiveUser(super::capture_helper::UserCaptureStreamer::new())
            } else {
                CaptureBackend::Direct(meshrmm_remote_screen::WindowsScreenStreamer::new())
            },
            frames_per_second,
            bitrate_bits_per_second,
            next_frame_id: Arc::new(AtomicU64::new(1)),
        }
    }
}

#[cfg(windows)]
impl ScreenStreamer for PlatformScreenStreamer {
    fn displays(&self) -> anyhow::Result<Vec<Display>> {
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

    fn start(
        &mut self,
        display_id: DisplayId,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<VideoFormat> {
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
        };
        let active = match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer.start(config, display_id.0, sink)?,
            CaptureBackend::ActiveUser(streamer) => streamer.start(config, display_id.0, sink)?,
        };
        Ok(VideoFormat {
            width: active.width,
            height: active.height,
            frames_per_second: active.frames_per_second as u16,
            codec: Codec::H264,
            pixel_format: PixelFormat::Nv12,
            bitrate_bits_per_second: active.bitrate_bits_per_second,
        })
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer.stop().map_err(anyhow::Error::from),
            CaptureBackend::ActiveUser(streamer) => streamer.stop(),
        }
        .context("Windows capture stop failed")
    }

    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        match &mut self.inner {
            CaptureBackend::Direct(streamer) => streamer
                .poll_ended()
                .map(|result| result.map_err(anyhow::Error::from)),
            CaptureBackend::ActiveUser(streamer) => streamer.poll_ended(),
        }
        .map(|result| result.context("Windows GPU capture/encode worker stopped"))
    }

    fn request_keyframe(&self) -> anyhow::Result<()> {
        match &self.inner {
            CaptureBackend::Direct(streamer) => {
                streamer.request_keyframe().map_err(anyhow::Error::from)
            }
            CaptureBackend::ActiveUser(streamer) => streamer.request_keyframe(),
        }
        .context("hardware keyframe request failed")
    }

    fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        match &self.inner {
            CaptureBackend::Direct(streamer) => streamer
                .set_bitrate(bits_per_second)
                .map_err(anyhow::Error::from),
            CaptureBackend::ActiveUser(streamer) => streamer.set_bitrate(bits_per_second),
        }
        .context("hardware encoder bitrate change failed")
    }
}

#[cfg(windows)]
enum CaptureBackend {
    Direct(meshrmm_remote_screen::WindowsScreenStreamer),
    ActiveUser(super::capture_helper::UserCaptureStreamer),
}

#[cfg(windows)]
pub fn monotonic_timestamp_us() -> u64 {
    meshrmm_remote_screen::monotonic_timestamp_us().unwrap_or(0)
}
