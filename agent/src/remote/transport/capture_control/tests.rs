//! What each restart cause, and a pause, asks of the screen streamer.
use std::sync::atomic::AtomicUsize;

use meshrmm_protocol::EncodedFrame;

use super::*;
use crate::remote::platform::ScreenInput;

/// A streamer that records the calls the capture control makes. Each start
/// publishes the new stream's keyframe and the delta after it, as an
/// encoder does.
struct Recorder {
    calls: Arc<Mutex<Vec<&'static str>>>,
    codec: Codec,
    chroma: ChromaMode,
    /// The displays to offer once, as a session list change does.
    catalog: Option<Vec<Display>>,
    /// How many of the next starts fail, as on a desktop that is changing.
    failing_starts: Arc<AtomicUsize>,
}

impl Recorder {
    fn new(calls: &Arc<Mutex<Vec<&'static str>>>, profile: VideoProfile) -> Self {
        Self {
            calls: Arc::clone(calls),
            codec: profile.codec,
            chroma: profile.chroma,
            catalog: None,
            failing_starts: Arc::default(),
        }
    }

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

fn frame(stream_id: VideoStreamId, frame_id: u64, keyframe: bool) -> EncodedFrame {
    EncodedFrame {
        stream_id,
        frame_id,
        capture_timestamp_us: 1,
        encode_complete_timestamp_us: 2,
        send_timestamp_us: 0,
        keyframe,
        data: vec![0; 8].into(),
    }
}

impl ScreenStreamer for Recorder {
    fn start(
        &mut self,
        _display_id: Option<DisplayId>,
        stream_id: VideoStreamId,
        slot: Arc<LatestFrameSlot>,
    ) -> anyhow::Result<StartedScreen> {
        self.record("start");
        let failing = self.failing_starts.load(Ordering::Acquire);
        if failing > 0 {
            self.failing_starts.store(failing - 1, Ordering::Release);
            anyhow::bail!("the desktop is changing");
        }
        slot.publish(frame(stream_id, 1, true));
        slot.publish(frame(stream_id, 2, false));
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
    fn pause(&mut self) -> anyhow::Result<()> {
        self.record("pause");
        Ok(())
    }
    fn take_display_catalog(&mut self) -> Option<Vec<Display>> {
        self.catalog.take()
    }
    fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        None
    }
    fn request_keyframe(&self) -> anyhow::Result<()> {
        self.record("request_keyframe");
        Ok(())
    }
    fn set_bitrate(&mut self, _bits_per_second: u32) {}
    fn set_congestion_bitrate(&mut self, _bits_per_second: Option<u32>) {}
    fn set_quality(&mut self, _quality: QualityPreset) -> bool {
        false
    }
    fn set_adaptive_bitrate(&mut self, _bits_per_second: u32) -> anyhow::Result<()> {
        self.record("set_adaptive_bitrate");
        Ok(())
    }
    fn set_codec(&mut self, codec: Codec) {
        self.codec = codec;
    }
    fn set_chroma(&mut self, chroma: ChromaMode) {
        self.chroma = chroma;
    }
    fn set_cursor_capture(&mut self, _enabled: bool) -> anyhow::Result<bool> {
        // As the console's Windows.Graphics.Capture does, which restarts
        // capture for it.
        Ok(true)
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

const H265_420: VideoProfile = VideoProfile {
    codec: Codec::H265,
    chroma: ChromaMode::Yuv420,
};

/// Capture control streaming `profile` from `streamer` on stream
/// `stream_id`, as the viewer chose it. The control channel is closed, so
/// each configuration it sends fails to send.
fn control(
    streamer: Recorder,
    profile: VideoProfile,
    selection: ProfileSelection,
    stream_id: VideoStreamId,
) -> CaptureControl {
    CaptureControl {
        streamer: Arc::new(Mutex::new(Box::new(streamer))),
        slot: Arc::new(LatestFrameSlot::default()),
        control_channel: Arc::new(RTCDataChannel::default()),
        quality_ceiling: Arc::new(AtomicU32::new(8_000_000)),
        stream_id,
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
        paused: false,
        video_pause: Arc::new(tokio::sync::watch::Sender::new(VideoPause::default())),
    }
}

/// A selection of H.265 and H.264 at both chromas, at Best quality 4:2:0.
fn negotiated() -> ProfileSelection {
    let mut selection = ProfileSelection::new(H265_420);
    selection.negotiate(
        vec![
            H265_420,
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
    selection
}

#[tokio::test(flavor = "current_thread")]
async fn encoder_restarts_end_the_stream_and_never_stop_capture() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut capture = control(
        Recorder::new(&calls, H265_420),
        H265_420,
        negotiated(),
        VideoStreamId(1),
    );
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

#[test]
fn a_changed_display_list_is_offered_without_touching_the_stream() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let rdp = Display {
        session: meshrmm_protocol::DesktopSession::Rdp {
            id: 6,
            user: "bob".into(),
        },
        id: DisplayId(0x8000_0600),
        ..display()
    };
    let streamer = Recorder {
        catalog: Some(vec![display(), rdp.clone()]),
        ..Recorder::new(&calls, H265_420)
    };
    let mut capture = control(
        streamer,
        H265_420,
        ProfileSelection::new(H265_420),
        VideoStreamId(3),
    );

    let message = capture.take_catalog().unwrap();
    assert_eq!(
        message,
        Some(SessionMessage::DisplayCatalog {
            displays: vec![display(), rdp.clone()]
        })
    );
    // The viewer can now choose Bob's display, and the stream, its display
    // and its encoder are as they were.
    assert_eq!(capture.displays, vec![display(), rdp]);
    assert_eq!(capture.stream_id, VideoStreamId(3));
    assert_eq!(capture.active_display, display());
    assert!(capture.capture_running);
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(capture.take_catalog().unwrap(), None);
}

/// Lets capture control's desktop check run now, as its timer does.
async fn check_desktop(capture: &mut CaptureControl) {
    capture.capture_retry_after = std::time::Instant::now();
    let _ = capture.poll_desktop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_paused_stream_captures_and_sends_nothing_until_the_viewer_shows_it() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut capture = control(
        Recorder::new(&calls, H265_420),
        H265_420,
        negotiated(),
        VideoStreamId(1),
    );
    let pause = capture.video_pause.subscribe();
    // A delta of the running stream waits to be sent.
    capture.slot.publish(frame(VideoStreamId(1), 7, false));

    capture
        .apply(ControlCommand::VideoPaused(true))
        .await
        .unwrap();
    assert_eq!(*calls.lock().unwrap(), ["pause"]);
    assert_eq!(
        capture.slot.len(),
        0,
        "nothing of the paused stream goes out"
    );
    assert!(!capture.capture_running);
    assert_eq!(
        *pause.borrow(),
        VideoPause {
            paused: true,
            pauses: 1
        }
    );

    // Neither the viewer's or the sender's keyframe requests, nor the rate
    // feedback, nor what would restart the stream, nor the desktop checks
    // touch capture while it is paused; a second pause changes nothing.
    for command in [
        ControlCommand::VideoPaused(true),
        ControlCommand::Keyframe,
        ControlCommand::Bitrate(2_000_000),
        ControlCommand::RestartBitrate(2_000_000),
        ControlCommand::Quality(QualityPreset::DataSaver),
        ControlCommand::Chroma(ChromaMode::Yuv444),
        ControlCommand::CursorCapture(false),
        ControlCommand::SelectDisplay(DisplayId(1)),
    ] {
        capture.apply(command).await.unwrap();
        check_desktop(&mut capture).await;
    }
    assert_eq!(*calls.lock().unwrap(), ["pause"]);
    assert_eq!(capture.slot.len(), 0);
    assert_eq!(pause.borrow().pauses, 1);

    // Showing the video starts the next stream at once, with what was
    // chosen meanwhile, and its keyframe is the first frame to send.
    let _ = capture.apply(ControlCommand::VideoPaused(false)).await;
    assert_eq!(*calls.lock().unwrap(), ["pause", "start"]);
    assert!(capture.capture_running);
    assert!(!pause.borrow().paused);
    assert_eq!(
        capture.active_profile,
        VideoProfile {
            codec: Codec::H265,
            chroma: ChromaMode::Yuv444,
        }
    );
    let first = capture.slot.next().await;
    assert!(first.keyframe);
    assert_eq!(first.stream_id, VideoStreamId(2));
    assert_eq!(capture.stream_id, VideoStreamId(2));
}

#[tokio::test(flavor = "current_thread")]
async fn a_pause_while_capture_restarts_holds_until_the_viewer_shows_the_video() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let streamer = Recorder::new(&calls, H265_420);
    let failing_starts = Arc::clone(&streamer.failing_starts);
    let mut capture = control(streamer, H265_420, negotiated(), VideoStreamId(1));

    // A congestion step whose encoder does not start: capture waits for the
    // desktop check to start it again.
    failing_starts.store(1, Ordering::Release);
    capture
        .apply(ControlCommand::RestartBitrate(2_000_000))
        .await
        .unwrap();
    assert!(!capture.capture_running);
    capture
        .apply(ControlCommand::VideoPaused(true))
        .await
        .unwrap();
    for _ in 0..5 {
        check_desktop(&mut capture).await;
    }
    assert_eq!(
        *calls.lock().unwrap(),
        ["end_stream", "start", "pause"],
        "the retry waits for the pause to end"
    );

    // Showing the video starts capture, and a start that fails then is
    // retried by the desktop check as before.
    failing_starts.store(1, Ordering::Release);
    capture
        .apply(ControlCommand::VideoPaused(false))
        .await
        .unwrap();
    assert!(!capture.capture_running);
    check_desktop(&mut capture).await;
    assert!(capture.capture_running);
    assert_eq!(
        *calls.lock().unwrap(),
        ["end_stream", "start", "pause", "start", "start"]
    );
    let first = capture.slot.next().await;
    assert!(first.keyframe);
    assert_eq!(first.stream_id, capture.stream_id);
}
