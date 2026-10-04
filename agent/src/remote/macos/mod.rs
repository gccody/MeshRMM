//! The macOS screen streamer: ScreenCaptureKit capture, VideoToolbox
//! encoding and Quartz input. A console Agent captures its own session; the
//! installed coordinator, which runs as root outside any graphical session,
//! uses the session helper of whichever session is on the console.
pub(crate) mod approval;
mod capture;
mod cursor;
pub(crate) mod display;
mod encoder;
pub(crate) mod helper;
mod input;
mod keep_awake;
mod keymap;
pub(crate) mod local;
pub(crate) mod session_close;
pub(crate) mod snapshot;
pub(crate) mod ui;
mod wallpaper;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meshrmm_protocol::{
    ChromaMode, Codec, DisplayId, EncodedFrame, HeadlessResolution, QualityPreset, VideoStreamId,
};

pub(crate) use self::helper::protocol::SessionUi;
use self::helper::protocol::StreamSettings;
use super::platform;
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
    frames_per_second: u32,
    quality: QualityPreset,
    bitrate_bits_per_second: u32,
    congestion_bitrate_bits_per_second: Option<u32>,
    codec: Codec,
    capture_cursor: bool,
    next_frame_id: Arc<AtomicU64>,
    backend: Backend,
}

enum Backend {
    Direct {
        screen: Box<local::LocalScreen>,
        input: Arc<local::LocalInput>,
    },
    Helper(Arc<helper::coordinator::Remote>),
}

impl PlatformScreenStreamer {
    /// With `use_helpers`, the streamer works through the console's session
    /// helper; otherwise it captures this process's own session.
    pub fn new(
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        use_helpers: bool,
        ui: SessionUi,
    ) -> anyhow::Result<Self> {
        let backend = if use_helpers {
            Backend::Helper(helper::coordinator::Remote::new(ui)?)
        } else {
            let input = Arc::new(local::LocalInput::new()?);
            input.begin_session(&ui);
            Backend::Direct {
                screen: Box::new(local::LocalScreen::new(Arc::clone(&input))),
                input,
            }
        };
        Ok(Self {
            frames_per_second,
            quality: QualityPreset::BestQuality,
            bitrate_bits_per_second,
            congestion_bitrate_bits_per_second: None,
            codec: Codec::H264,
            capture_cursor: true,
            next_frame_id: Arc::new(AtomicU64::new(1)),
            backend,
        })
    }

    fn settings(&self) -> StreamSettings {
        StreamSettings {
            frames_per_second: self.quality.frames_per_second(self.frames_per_second),
            bitrate_bits_per_second: self
                .congestion_bitrate_bits_per_second
                .map_or(self.bitrate_bits_per_second, |limit| {
                    limit.min(self.bitrate_bits_per_second)
                }),
            codec: self.codec,
            capture_cursor: self.capture_cursor,
            grayscale: self.quality.grayscale(),
        }
    }
}

impl ScreenStreamer for PlatformScreenStreamer {
    fn start(
        &mut self,
        display_id: Option<DisplayId>,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        let settings = self.settings();
        let next_frame_id = Arc::clone(&self.next_frame_id);
        let publish = move |data: Vec<u8>, keyframe: bool, capture_us: u64, encoded_us: u64| {
            slot.publish(EncodedFrame {
                stream_id,
                frame_id: next_frame_id.fetch_add(1, Ordering::Relaxed),
                capture_timestamp_us: capture_us,
                encode_complete_timestamp_us: encoded_us,
                send_timestamp_us: 0,
                keyframe,
                data,
            });
        };
        let started = match &mut self.backend {
            Backend::Direct { screen, .. } => screen.start(display_id, settings, move |unit| {
                let mut data = unit.codec_config.unwrap_or_default();
                data.extend_from_slice(&unit.data);
                publish(
                    data,
                    unit.keyframe,
                    unit.capture_timestamp_us,
                    unit.encode_complete_timestamp_us,
                );
            })?,
            Backend::Helper(remote) => remote.start(display_id, settings, move |frame| {
                publish(
                    frame.data,
                    frame.keyframe,
                    frame.capture_timestamp_us,
                    frame.encode_complete_timestamp_us,
                );
            })?,
        };
        Ok(StartedScreen {
            displays: started.displays,
            active_display: started.active_display,
            format: started.format,
        })
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        match &mut self.backend {
            Backend::Direct { screen, .. } => {
                screen.stop();
                Ok(())
            }
            Backend::Helper(remote) => remote.stop(),
        }
    }

    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        match &mut self.backend {
            Backend::Direct { screen, .. } => screen.poll_ended(),
            Backend::Helper(remote) => remote.poll_ended(),
        }
        .map(Err)
    }

    fn request_keyframe(&self) -> anyhow::Result<()> {
        match &self.backend {
            Backend::Direct { screen, .. } => {
                screen.request_keyframe();
                Ok(())
            }
            Backend::Helper(remote) => remote.request_keyframe(),
        }
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
        match &self.backend {
            Backend::Direct { screen, .. } => screen.set_bitrate(bits_per_second),
            Backend::Helper(remote) => remote.set_bitrate(bits_per_second.max(1)),
        }
    }

    fn set_codec(&mut self, codec: Codec) {
        self.codec = codec;
    }

    /// VideoToolbox has no 4:4:4 hardware encoder, so viewers only ever get 4:2:0.
    fn set_chroma(&mut self, _chroma: ChromaMode) {}

    fn set_cursor_capture(&mut self, enabled: bool) -> anyhow::Result<bool> {
        self.capture_cursor = enabled;
        match &self.backend {
            Backend::Direct { screen, .. } => screen.set_cursor_capture(enabled),
            Backend::Helper(remote) => remote.set_cursor_capture(enabled)?,
        }
        Ok(false)
    }

    fn set_display_border(&mut self, enabled: bool) -> anyhow::Result<()> {
        match &mut self.backend {
            Backend::Direct { screen, .. } => screen.set_display_border(enabled),
            Backend::Helper(remote) => remote.set_display_border(enabled),
        }
    }

    fn set_headless_resolution(&mut self, _resolution: HeadlessResolution) -> bool {
        false
    }

    fn input_controller(&self) -> Arc<dyn ScreenInput> {
        match &self.backend {
            Backend::Direct { input, .. } => Arc::clone(input) as Arc<dyn ScreenInput>,
            Backend::Helper(remote) => {
                Arc::new(helper::coordinator::HelperInput(Arc::clone(remote)))
            }
        }
    }
}

impl Drop for PlatformScreenStreamer {
    fn drop(&mut self) {
        // A session helper ends its own session when the coordinator leaves it.
        if let Backend::Direct { input, .. } = &self.backend {
            input.end_session();
        }
    }
}
