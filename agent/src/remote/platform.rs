use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use pulsermm_protocol::{Codec, EncodedFrame, PixelFormat, VideoFormat, VideoStreamId};

use super::video::LatestFrameSlot;

pub trait ScreenStreamer: Send {
    fn start(&mut self, slot: Arc<LatestFrameSlot>) -> anyhow::Result<VideoFormat>;
    fn stop(&mut self) -> anyhow::Result<()>;
    fn poll_ended(&mut self) -> Option<anyhow::Result<()>>;
    fn request_keyframe(&self) -> anyhow::Result<()>;
    fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()>;
}

#[cfg(windows)]
pub struct PlatformScreenStreamer {
    inner: pulsermm_remote_screen::WindowsScreenStreamer,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
}

#[cfg(windows)]
impl PlatformScreenStreamer {
    pub fn new(frames_per_second: u32, bitrate_bits_per_second: u32) -> Self {
        Self {
            inner: pulsermm_remote_screen::WindowsScreenStreamer::new(),
            frames_per_second,
            bitrate_bits_per_second,
        }
    }
}

#[cfg(windows)]
impl ScreenStreamer for PlatformScreenStreamer {
    fn start(&mut self, slot: Arc<LatestFrameSlot>) -> anyhow::Result<VideoFormat> {
        let next_frame_id = Arc::new(AtomicU64::new(1));
        let runtime = tokio::runtime::Handle::current();
        let sink = Arc::new(
            move |access_unit: pulsermm_remote_screen::EncodedAccessUnit| {
                let slot = Arc::clone(&slot);
                let frame_id = next_frame_id.fetch_add(1, Ordering::Relaxed);
                let mut data = access_unit.codec_config.unwrap_or_default();
                data.extend_from_slice(&access_unit.data);
                let frame = EncodedFrame {
                    stream_id: VideoStreamId(1),
                    frame_id,
                    capture_timestamp_us: access_unit.capture_timestamp_us,
                    encode_complete_timestamp_us: access_unit.encode_complete_timestamp_us,
                    send_timestamp_us: 0,
                    keyframe: access_unit.keyframe,
                    data,
                };
                runtime.spawn(async move { slot.publish(frame).await });
            },
        );
        let active = self
            .inner
            .start(
                pulsermm_remote_screen::StreamConfig {
                    frames_per_second: self.frames_per_second,
                    bitrate_bits_per_second: self.bitrate_bits_per_second,
                },
                sink,
            )
            .context("Windows GPU capture/encode startup failed")?;
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
        self.inner.stop().context("Windows capture stop failed")
    }

    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        self.inner
            .poll_ended()
            .map(|result| result.context("Windows GPU capture/encode worker stopped"))
    }

    fn request_keyframe(&self) -> anyhow::Result<()> {
        self.inner
            .request_keyframe()
            .context("hardware keyframe request failed")
    }

    fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        self.inner
            .set_bitrate(bits_per_second)
            .context("hardware encoder bitrate change failed")
    }
}

#[cfg(windows)]
pub fn monotonic_timestamp_us() -> u64 {
    pulsermm_remote_screen::monotonic_timestamp_us().unwrap_or(0)
}
