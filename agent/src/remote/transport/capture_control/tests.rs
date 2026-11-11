//! What each restart cause asks of the screen streamer.
use super::*;
use crate::remote::platform::ScreenInput;

/// A streamer that records the calls the capture control makes.
struct Recorder {
    calls: Arc<Mutex<Vec<&'static str>>>,
    codec: Codec,
    chroma: ChromaMode,
}

impl Recorder {
    fn record(&self, call: &'static str) {
        self.calls.lock().unwrap().push(call);
    }
}

fn display() -> Display {
    Display {
        session: meshrmm_protocol::DesktopSession::Console,
        id: DisplayId(1),
        name: "Display".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: true,
    }
}

fn format(codec: Codec, chroma: ChromaMode) -> VideoFormat {
    VideoFormat {
        width: 1920,
        height: 1080,
        frames_per_second: 30,
        codec,
        pixel_format: match chroma {
            ChromaMode::Yuv420 => meshrmm_protocol::PixelFormat::Nv12,
            ChromaMode::Yuv444 => meshrmm_protocol::PixelFormat::Ayuv,
        },
        bitrate_bits_per_second: 8_000_000,
    }
}

impl ScreenStreamer for Recorder {
    fn start(
        &mut self,
        _display_id: Option<DisplayId>,
        _stream_id: VideoStreamId,
        _slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        self.record("start");
        Ok(StartedScreen {
            displays: vec![display()],
            active_display: display(),
            format: format(self.codec, self.chroma),
        })
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        self.record("stop");
        Ok(())
    }
    fn end_stream(&mut self) -> anyhow::Result<()> {
        self.record("end_stream");
        Ok(())
    }
    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        None
    }
    fn request_keyframe(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_bitrate(&mut self, _bits_per_second: u32) {}
    fn set_congestion_bitrate(&mut self, _bits_per_second: Option<u32>) {}
    fn set_quality(&mut self, _quality: QualityPreset) -> bool {
        false
    }
    fn set_adaptive_bitrate(&mut self, _bits_per_second: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_codec(&mut self, codec: Codec) {
        self.codec = codec;
    }
    fn set_chroma(&mut self, chroma: ChromaMode) {
        self.chroma = chroma;
    }
    fn set_cursor_capture(&mut self, _enabled: bool) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn set_display_border(&mut self, _enabled: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_headless_resolution(&mut self, _resolution: HeadlessResolution) -> bool {
        false
    }
    fn input_controller(&self) -> Arc<dyn ScreenInput> {
        unreachable!("capture control never asks for input")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn encoder_restarts_end_the_stream_and_never_stop_capture() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let profile = VideoProfile {
        codec: Codec::H265,
        chroma: ChromaMode::Yuv420,
    };
    let streamer: Box<dyn ScreenStreamer> = Box::new(Recorder {
        calls: Arc::clone(&calls),
        codec: profile.codec,
        chroma: profile.chroma,
    });
    let mut selection = ProfileSelection::new(profile);
    selection.negotiate(
        vec![
            profile,
            VideoProfile {
                codec: Codec::H265,
                chroma: ChromaMode::Yuv444,
            },
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv444,
            },
        ],
        QualityPreset::BestQuality,
        ChromaMode::Yuv420,
    );
    let mut capture = CaptureControl {
        streamer: Arc::new(Mutex::new(streamer)),
        slot: Arc::new(LatestFrameSlot::default()),
        control_channel: Arc::new(RTCDataChannel::default()),
        quality_ceiling: Arc::new(AtomicU32::new(8_000_000)),
        stream_id: VideoStreamId(1),
        displays: vec![display()],
        active_display: display(),
        format: format(profile.codec, profile.chroma),
        configured_maximum_bitrate: 8_000_000,
        active_profile: profile,
        selection,
        capture_cursor: true,
        recording: false,
        viewer_controls_input: false,
        capture_running: true,
        capture_unavailable_since: None,
        capture_retry_after: std::time::Instant::now(),
    };
    // A congestion step, a quality and a chroma choice, and a profile the
    // viewer could not present. The test's control channel is closed, so
    // the configuration sent after each restart fails to send.
    let causes = [
        ControlCommand::RestartBitrate(2_000_000),
        ControlCommand::Quality(QualityPreset::DataSaver),
        ControlCommand::Chroma(ChromaMode::Yuv444),
        ControlCommand::VideoProfileRejected {
            profile: VideoProfile {
                codec: Codec::H265,
                chroma: ChromaMode::Yuv444,
            },
            reason: "no decoder".into(),
        },
    ];
    for (restarts, cause) in causes.into_iter().enumerate() {
        let _ = capture.apply(cause).await;
        assert_eq!(capture.stream_id, VideoStreamId(restarts as u32 + 2));
    }
    assert_eq!(
        *calls.lock().unwrap(),
        ["end_stream", "start"].repeat(4),
        "every restart ends the stream and starts the next; none stops capture"
    );
}
