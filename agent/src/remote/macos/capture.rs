//! ScreenCaptureKit display capture feeding the VideoToolbox encoder.
//!
//! ScreenCaptureKit delivers 4:2:0 frames only when the display changes, so a
//! pacer thread re-encodes the last frame when the viewer asks for a keyframe
//! on an idle display, and refines a static desktop after each keyframe as
//! the Windows capture paths do.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use meshrmm_protocol::{Codec, Display};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::encoder::{EncodedAccessUnit, Encoder, FrameSink};

#[path = "../../../windows/remote-screen/src/refinement.rs"]
mod refinement;
use refinement::StaticRefinement;

/// How long ScreenCaptureKit may take to answer before capture is reported unavailable.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames ScreenCaptureKit may have outstanding; one is kept for re-encoding.
const QUEUE_DEPTH: isize = 5;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CaptureConfig {
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub codec: Codec,
    pub capture_cursor: bool,
    pub grayscale: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ActiveFormat {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub codec: Codec,
}

struct State {
    encoder: Encoder,
    grayscale: bool,
    last_frame: Option<CFRetained<CVPixelBuffer>>,
    last_capture_us: u64,
    last_encoded: Instant,
    keyframe_requested: bool,
    /// The next frame is the stream's first, which is always a keyframe.
    first: bool,
    refinement: StaticRefinement,
}

// SAFETY: Core Video buffers are reference counted thread-safely, and the
// state is only used under its lock.
unsafe impl Send for State {}

impl State {
    fn encode(&mut self, frame: &CVPixelBuffer, capture_us: u64) -> anyhow::Result<()> {
        let keyframe = std::mem::take(&mut self.keyframe_requested) || self.first;
        self.encoder.encode(frame, capture_us, keyframe)?;
        self.first = false;
        self.last_encoded = Instant::now();
        self.refinement.encoded(keyframe);
        Ok(())
    }
}

struct Shared {
    state: Mutex<State>,
    failure: Mutex<Option<String>>,
    stopped: AtomicBool,
}

impl Shared {
    fn fail(&self, error: impl std::fmt::Display) {
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }

    fn frame(&self, sample: &CMSampleBuffer) {
        if self.stopped.load(Ordering::Relaxed) {
            return;
        }
        // Idle and blank updates carry no image.
        // SAFETY: the sample buffer is valid for the duration of the callback.
        let Some(frame) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        // SAFETY: as above.
        let presentation = unsafe { sample.presentation_time_stamp() };
        // ScreenCaptureKit stamps frames with the host clock.
        // SAFETY: CMTimeGetSeconds has no preconditions.
        let seconds = unsafe { presentation.seconds() };
        let capture_us = if seconds.is_finite() && seconds > 0.0 {
            (seconds * 1_000_000.0) as u64
        } else {
            super::monotonic_timestamp_us()
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.grayscale {
            neutralize_chroma(&frame);
        }
        if let Err(error) = state.encode(&frame, capture_us) {
            drop(state);
            self.fail(format!("{error:#}"));
            return;
        }
        state.last_frame = Some(frame);
        state.last_capture_us = capture_us;
    }

    /// Re-encodes the last frame of an idle display when a keyframe or
    /// refinement is due.
    fn pace(&self, frame_interval: Duration) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.last_encoded.elapsed() < frame_interval
            || !(state.keyframe_requested || state.refinement.pending())
        {
            return;
        }
        let Some(frame) = state.last_frame.clone() else {
            return;
        };
        let refining = !state.keyframe_requested;
        let capture_us = state.last_capture_us;
        if let Err(error) = state.encode(&frame, capture_us) {
            drop(state);
            self.fail(format!("{error:#}"));
            return;
        }
        if refining {
            state.refinement.refined();
        }
    }
}

/// Grayscale video: the luma plane is already the picture, so the chroma
/// plane is set to neutral.
fn neutralize_chroma(frame: &CVPixelBuffer) {
    // SAFETY: the frame is a valid bi-planar buffer, locked while written.
    unsafe {
        if CVPixelBufferLockBaseAddress(frame, CVPixelBufferLockFlags(0)) != 0 {
            return;
        }
        let chroma = CVPixelBufferGetBaseAddressOfPlane(frame, 1);
        if !chroma.is_null() {
            let bytes = CVPixelBufferGetBytesPerRowOfPlane(frame, 1)
                * CVPixelBufferGetHeightOfPlane(frame, 1);
            std::ptr::write_bytes(chroma.cast::<u8>(), 128, bytes);
        }
        CVPixelBufferUnlockBaseAddress(frame, CVPixelBufferLockFlags(0));
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[name = "MeshRMMScreenOutput"]
    #[ivars = Arc<Shared>]
    struct ScreenOutput;

    unsafe impl NSObjectProtocol for ScreenOutput {}

    unsafe impl SCStreamOutput for ScreenOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().frame(sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for ScreenOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars()
                .fail(format!("ScreenCaptureKit stopped capture: {}", error.localizedDescription()));
        }
    }
);

impl ScreenOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(shared);
        // SAFETY: NSObject's init is always valid.
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) struct Capture {
    stream: Retained<SCStream>,
    configuration: Retained<SCStreamConfiguration>,
    _output: Retained<ScreenOutput>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    pacer: Option<std::thread::JoinHandle<()>>,
    format: ActiveFormat,
}

// SAFETY: SCStream and SCStreamConfiguration are thread-safe Objective-C
// objects; the rest is synchronized.
unsafe impl Send for Capture {}

impl Capture {
    pub(crate) fn start(
        display: &Display,
        config: CaptureConfig,
        sink: impl Fn(EncodedAccessUnit) + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let sc_display = shareable_display(display.id.0)?;
        let (width, height) = super::display::pixel_size(display);
        // 4:2:0 needs even dimensions.
        let (width, height) = (width & !1, height & !1);
        let frames_per_second = config.frames_per_second.max(1);
        let sink: FrameSink = Arc::new(sink);
        let encoder = Encoder::new(
            config.codec,
            width,
            height,
            frames_per_second,
            config.bitrate_bits_per_second,
            sink,
        )?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                encoder,
                grayscale: config.grayscale,
                last_frame: None,
                last_capture_us: 0,
                last_encoded: Instant::now(),
                keyframe_requested: false,
                first: true,
                refinement: StaticRefinement::new(
                    width,
                    height,
                    frames_per_second,
                    config.bitrate_bits_per_second,
                ),
            }),
            failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        });

        // SAFETY: all ScreenCaptureKit objects are created and configured
        // before the stream starts, with valid arguments.
        let (stream, configuration, output, queue) = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &sc_display,
                &NSArray::new(),
            );
            let configuration = SCStreamConfiguration::new();
            configuration.setWidth(width as usize);
            configuration.setHeight(height as usize);
            configuration.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            configuration.setMinimumFrameInterval(CMTime::new(1, frames_per_second as i32));
            configuration.setQueueDepth(QUEUE_DEPTH);
            configuration.setShowsCursor(config.capture_cursor);
            let output = ScreenOutput::new(Arc::clone(&shared));
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(ProtocolObject::from_ref(&*output)),
            );
            let queue = DispatchQueue::new("com.meshrmm.agent.screen", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|error| {
                    anyhow::anyhow!(
                        "ScreenCaptureKit rejected the frame output: {}",
                        error.localizedDescription()
                    )
                })?;
            (stream, configuration, output, queue)
        };
        let (started, result) = mpsc::channel();
        let handler = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes a valid error or null.
            let _ = started.send(
                unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string()),
            );
        });
        // SAFETY: the stream is fully configured.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
        match result.recv_timeout(CAPTURE_TIMEOUT) {
            Ok(None) => {}
            Ok(Some(error)) => bail!("ScreenCaptureKit could not start capture: {error}"),
            Err(_) => bail!("ScreenCaptureKit did not start capture in time"),
        }

        let frame_interval = Duration::from_secs(1) / frames_per_second;
        let pacer_shared = Arc::clone(&shared);
        let pacer = std::thread::Builder::new()
            .name("meshrmm-screen-pacer".into())
            .spawn(move || {
                while !pacer_shared.stopped.load(Ordering::Relaxed) {
                    std::thread::park_timeout(frame_interval);
                    pacer_shared.pace(frame_interval);
                }
            })
            .context("could not start the screen pacer thread")?;
        Ok(Self {
            stream,
            configuration,
            _output: output,
            _queue: queue,
            shared,
            pacer: Some(pacer),
            format: ActiveFormat {
                width,
                height,
                frames_per_second,
                bitrate_bits_per_second: config.bitrate_bits_per_second,
                codec: config.codec,
            },
        })
    }

    pub(crate) fn format(&self) -> ActiveFormat {
        self.format
    }

    pub(crate) fn request_keyframe(&self) {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keyframe_requested = true;
        if let Some(pacer) = &self.pacer {
            pacer.thread().unpark();
        }
    }

    pub(crate) fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.encoder.set_bitrate(bits_per_second)?;
        state.refinement.set_bitrate(bits_per_second);
        Ok(())
    }

    pub(crate) fn set_cursor_capture(&self, enabled: bool) {
        // SAFETY: updating a live stream's configuration is supported.
        unsafe {
            self.configuration.setShowsCursor(enabled);
            self.stream
                .updateConfiguration_completionHandler(&self.configuration, None);
        }
    }

    /// The error that stopped capture, once it has stopped.
    pub(crate) fn poll_ended(&self) -> Option<anyhow::Error> {
        self.shared
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(anyhow::Error::msg)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(pacer) = self.pacer.take() {
            pacer.thread().unpark();
            let _ = pacer.join();
        }
        let (stopped, result) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = stopped.send(());
        });
        // SAFETY: stopping a started stream is always valid.
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        if result.recv_timeout(CAPTURE_TIMEOUT).is_err() {
            tracing::warn!("ScreenCaptureKit did not confirm that capture stopped");
        }
    }
}

/// The ScreenCaptureKit display for a Quartz display ID.
fn shareable_display(display_id: u32) -> anyhow::Result<Retained<SCDisplay>> {
    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes valid objects or null.
            let result = match unsafe { (content.as_ref(), error.as_ref()) } {
                (Some(content), _) => Ok(unsafe { content.displays() }
                    .iter()
                    .find(|display| unsafe { display.displayID() } == display_id)),
                (None, Some(error)) => Err(error.localizedDescription().to_string()),
                (None, None) => Err("no shareable content".to_owned()),
            };
            let _ = sender.send(result);
        },
    );
    // SAFETY: the handler matches the expected block signature.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, true, &handler,
        )
    };
    match receiver.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(Ok(Some(display))) => Ok(display),
        Ok(Ok(None)) => bail!("ScreenCaptureKit cannot capture display {display_id}"),
        Ok(Err(error)) => bail!(
            "ScreenCaptureKit cannot capture the screen; allow Screen Recording for the MeshRMM Agent in System Settings ({error})"
        ),
        Err(_) => bail!("ScreenCaptureKit did not list the displays in time"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captures the main display, which needs the Screen Recording permission.
    fn capture_frames(codec: Codec, grayscale: bool) -> Vec<EncodedAccessUnit> {
        let displays = super::super::display::enumerate().unwrap();
        let display = super::super::display::choose(&displays, None).unwrap();
        let (sender, frames) = mpsc::channel();
        let capture = Capture::start(
            &display,
            CaptureConfig {
                frames_per_second: 30,
                bitrate_bits_per_second: 6_000_000,
                codec,
                capture_cursor: true,
                grayscale,
            },
            move |unit| {
                let _ = sender.send(unit);
            },
        )
        .unwrap();
        let first = frames.recv_timeout(Duration::from_secs(5)).unwrap();
        // An idle display delivers no new frames, so this keyframe has to come
        // from re-encoding the last one.
        std::thread::sleep(Duration::from_secs(4));
        let refined = frames.try_iter().count();
        capture.request_keyframe();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut units = vec![first];
        while Instant::now() < deadline {
            if let Ok(unit) = frames.recv_timeout(Duration::from_millis(100)) {
                let keyframe = unit.keyframe;
                units.push(unit);
                if keyframe {
                    break;
                }
            }
        }
        assert!(capture.poll_ended().is_none());
        assert!(refined > 0, "a static keyframe is refined");
        units
    }

    #[test]
    #[ignore = "captures the screen; needs the Screen Recording permission"]
    fn streams_h264_and_answers_keyframe_requests_on_an_idle_display() {
        let units = capture_frames(Codec::H264, false);
        let first = &units[0];
        assert!(first.keyframe);
        let config = first.codec_config.as_ref().unwrap();
        // SPS (7) and PPS (8).
        assert_eq!(config[4] & 0x1f, 7);
        assert!(
            units.last().unwrap().keyframe,
            "a requested keyframe arrives"
        );
    }

    #[test]
    #[ignore = "captures the screen; needs the Screen Recording permission"]
    fn streams_hevc_with_its_parameter_sets() {
        let units = capture_frames(Codec::H265, false);
        let config = units[0].codec_config.as_ref().unwrap();
        // VPS (32) comes first.
        assert_eq!((config[4] >> 1) & 0x3f, 32);
        assert!(units.last().unwrap().keyframe);
    }

    #[test]
    #[ignore = "captures the screen; needs the Screen Recording permission"]
    fn streams_grayscale() {
        let units = capture_frames(Codec::H264, true);
        assert!(units[0].keyframe);
    }
}
