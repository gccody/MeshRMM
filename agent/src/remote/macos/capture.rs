//! ScreenCaptureKit display capture feeding the VideoToolbox encoder.
//!
//! ScreenCaptureKit delivers 4:2:0 frames only when the display changes, so a
//! pacer thread re-encodes the last frame when the viewer asks for a keyframe
//! on an idle display, and refines a static desktop after each keyframe as
//! the Windows capture paths do.
//!
//! All displays captures each display in its own stream, scaled to its place
//! in one picture, and draws their latest frames into that picture at most
//! at the frame rate.
use std::ptr::NonNull;
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
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};

use super::display::Layout;
use super::encoder::{EncodedAccessUnit, Encoder, FrameSink};

#[path = "../../../windows/remote-screen/src/refinement.rs"]
mod refinement;
use refinement::StaticRefinement;

/// How long ScreenCaptureKit may take to answer before capture is reported unavailable.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames ScreenCaptureKit may have outstanding; one is kept for re-encoding.
const QUEUE_DEPTH: isize = 5;
/// Video-range black in 4:2:0: luma 16, both chroma 128.
const BLACK: [u8; 2] = [16, 128];

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

/// A locked buffer's plane: where it starts, its row length in bytes, its
/// rows, and the bytes of each row that hold pixels.
struct Plane {
    base: *mut u8,
    stride: usize,
    rows: usize,
    width: usize,
}

/// Plane `index` of a locked 4:2:0 bi-planar buffer, whose chroma plane has
/// two bytes a sample.
///
/// # Safety
///
/// `buffer` must be locked.
unsafe fn plane(buffer: &CVPixelBuffer, index: usize) -> Plane {
    Plane {
        base: CVPixelBufferGetBaseAddressOfPlane(buffer, index).cast(),
        stride: CVPixelBufferGetBytesPerRowOfPlane(buffer, index),
        rows: CVPixelBufferGetHeightOfPlane(buffer, index),
        width: CVPixelBufferGetWidthOfPlane(buffer, index) * (index + 1),
    }
}

/// The All displays picture: each display's latest frame, drawn into one
/// buffer from a pool.
struct Composite {
    layout: Layout,
    frames: Vec<Option<CFRetained<CVPixelBuffer>>>,
    pool: CFRetained<CVPixelBufferPool>,
    /// A display sent a frame since the picture was last drawn.
    changed: bool,
}

// SAFETY: Core Video buffers and pools are reference counted thread-safely,
// and the composite is only used under the state's lock.
unsafe impl Send for Composite {}

impl Composite {
    fn new(layout: Layout) -> anyhow::Result<Self> {
        let format = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        let width = CFNumber::new_i64(layout.width as i64);
        let height = CFNumber::new_i64(layout.height as i64);
        // IOSurface-backed buffers go to the hardware encoder without a copy.
        let surface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: the keys are valid static Core Video constants.
        let attributes = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&format, &width, &height, &surface],
            )
        };
        let mut pool: *mut CVPixelBufferPool = std::ptr::null_mut();
        // SAFETY: the attributes are a valid dictionary and `pool` a valid
        // out pointer.
        let status = unsafe {
            CVPixelBufferPool::create(
                None,
                None,
                Some(attributes.as_opaque()),
                NonNull::from(&mut pool),
            )
        };
        let pool = NonNull::new(pool)
            .filter(|_| status == 0)
            .with_context(|| {
                format!("Core Video could not make buffers for All displays ({status})")
            })?;
        Ok(Self {
            frames: vec![None; layout.rects.len()],
            layout,
            // SAFETY: CVPixelBufferPoolCreate returned an owned (+1) pool.
            pool: unsafe { CFRetained::from_raw(pool) },
            changed: false,
        })
    }

    /// Draws each display's latest frame in its place in a new picture,
    /// black where no display is or none has sent a frame yet.
    fn draw(&self) -> anyhow::Result<CFRetained<CVPixelBuffer>> {
        let mut buffer: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: the pool is valid and `buffer` a valid out pointer.
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut buffer))
        };
        let buffer = NonNull::new(buffer)
            .filter(|_| status == 0)
            .with_context(|| format!("Core Video had no buffer for All displays ({status})"))?;
        // SAFETY: CVPixelBufferPoolCreatePixelBuffer returned an owned (+1) buffer.
        let picture = unsafe { CFRetained::from_raw(buffer) };
        // SAFETY: the picture is a 4:2:0 bi-planar buffer, locked while it is
        // written; so is each frame, locked while it is read; and every copy
        // stays inside both planes.
        unsafe {
            if CVPixelBufferLockBaseAddress(&picture, CVPixelBufferLockFlags(0)) != 0 {
                bail!("Core Video could not lock the All displays picture");
            }
            for (index, black) in BLACK.into_iter().enumerate() {
                let target = plane(&picture, index);
                std::ptr::write_bytes(target.base, black, target.stride * target.rows);
            }
            for (frame, &(left, top, width, height)) in self.frames.iter().zip(&self.layout.rects) {
                let Some(frame) = frame else {
                    continue;
                };
                if CVPixelBufferLockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly) != 0 {
                    continue;
                }
                // Chroma has half the rows, and two bytes for two pixels.
                for (index, (top, height)) in [(top, height), (top / 2, height / 2)]
                    .into_iter()
                    .enumerate()
                {
                    let (source, target) = (plane(frame, index), plane(&picture, index));
                    let bytes = width
                        .min(source.width)
                        .min(target.width.saturating_sub(left));
                    let rows = height.min(source.rows).min(target.rows.saturating_sub(top));
                    for row in 0..rows {
                        std::ptr::copy_nonoverlapping(
                            source.base.add(row * source.stride),
                            target.base.add((top + row) * target.stride + left),
                            bytes,
                        );
                    }
                }
                CVPixelBufferUnlockBaseAddress(frame, CVPixelBufferLockFlags::ReadOnly);
            }
            CVPixelBufferUnlockBaseAddress(&picture, CVPixelBufferLockFlags(0));
        }
        Ok(picture)
    }
}

struct State {
    encoder: Encoder,
    grayscale: bool,
    last_frame: Option<CFRetained<CVPixelBuffer>>,
    last_capture_us: u64,
    last_encoded: Instant,
    frame_interval: Duration,
    keyframe_requested: bool,
    /// The next frame is the stream's first, which is always a keyframe.
    first: bool,
    refinement: StaticRefinement,
    /// The All displays picture, when the capture shows several displays.
    composite: Option<Composite>,
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

    /// Draws the All displays picture and encodes it.
    fn encode_composite(&mut self, capture_us: u64) -> anyhow::Result<()> {
        let Some(composite) = self.composite.as_mut() else {
            return Ok(());
        };
        let picture = composite.draw()?;
        composite.changed = false;
        if self.grayscale {
            neutralize_chroma(&picture);
        }
        self.encode(&picture, capture_us)?;
        self.last_frame = Some(picture);
        self.last_capture_us = capture_us;
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

    /// A frame from the capture's source `index`.
    fn frame(&self, index: usize, sample: &CMSampleBuffer) {
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
        if let Some(composite) = state.composite.as_mut() {
            if let Some(slot) = composite.frames.get_mut(index) {
                *slot = Some(frame);
            }
            composite.changed = true;
            // The pacer draws a change that comes sooner than the frame rate.
            if state.last_encoded.elapsed() >= state.frame_interval
                && let Err(error) = state.encode_composite(capture_us)
            {
                drop(state);
                self.fail(format!("{error:#}"));
            }
            return;
        }
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

    /// Draws the All displays picture when a display changed sooner than the
    /// frame rate allowed, and re-encodes the last frame of an idle display
    /// when a keyframe or refinement is due.
    fn pace(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.last_encoded.elapsed() < state.frame_interval {
            return;
        }
        if state
            .composite
            .as_ref()
            .is_some_and(|composite| composite.changed)
        {
            if let Err(error) = state.encode_composite(super::monotonic_timestamp_us()) {
                drop(state);
                self.fail(format!("{error:#}"));
            }
            return;
        }
        if !(state.keyframe_requested || state.refinement.pending()) {
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

/// What a stream's output reports to: the capture, and which of its sources
/// the stream is.
struct OutputTarget {
    shared: Arc<Shared>,
    index: usize,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and the class
    // implements no Drop.
    #[unsafe(super(NSObject))]
    #[name = "MeshRMMScreenOutput"]
    #[ivars = OutputTarget]
    struct ScreenOutput;

    unsafe impl NSObjectProtocol for ScreenOutput {}

    unsafe impl SCStreamOutput for ScreenOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().shared.frame(self.ivars().index, sample);
            }
        }
    }

    unsafe impl SCStreamDelegate for ScreenOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            self.ivars().shared.fail(format!(
                "ScreenCaptureKit stopped capture: {}",
                error.localizedDescription()
            ));
        }
    }
);

impl ScreenOutput {
    fn new(shared: Arc<Shared>, index: usize) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputTarget { shared, index });
        // SAFETY: NSObject's init is always valid.
        unsafe { msg_send![super(this), init] }
    }
}

/// One display's stream.
struct Source {
    display_id: u32,
    stream: Retained<SCStream>,
    configuration: Retained<SCStreamConfiguration>,
    _output: Retained<ScreenOutput>,
    started: bool,
}

pub(crate) struct Capture {
    sources: Vec<Source>,
    queue: dispatch2::DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    pacer: Option<std::thread::JoinHandle<()>>,
    format: ActiveFormat,
}

// SAFETY: SCStream and SCStreamConfiguration are thread-safe Objective-C
// objects; the rest is synchronized.
unsafe impl Send for Capture {}
// SAFETY: as above; every method takes `&self` and only calls thread-safe
// ScreenCaptureKit methods or locks the shared state.
unsafe impl Sync for Capture {}

impl Capture {
    /// Captures `members`: one display, or every display of All displays.
    pub(crate) fn start(
        members: &[Display],
        config: CaptureConfig,
        excluded: &[u32],
        sink: impl Fn(EncodedAccessUnit) + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let first = members.first().context("there is no display to capture")?;
        let layout = (members.len() > 1)
            .then(|| {
                let scales: Vec<f64> = members
                    .iter()
                    .map(|member| {
                        f64::from(super::display::pixel_size(member).0)
                            / f64::from(member.width.max(1))
                    })
                    .collect();
                super::display::layout(members, &scales)
            })
            .flatten();
        // Each source's size in pixels, and the picture's.
        let (sizes, (width, height)) = match &layout {
            Some(layout) => (
                layout
                    .rects
                    .iter()
                    .map(|&(_, _, width, height)| (width as u32, height as u32))
                    .collect::<Vec<_>>(),
                (layout.width as u32, layout.height as u32),
            ),
            None => {
                let (width, height) = super::display::pixel_size(first);
                // 4:2:0 needs even dimensions.
                let size = (width & !1, height & !1);
                (vec![size], size)
            }
        };
        let frames_per_second = config.frames_per_second.max(1);
        let frame_interval = Duration::from_secs(1) / frames_per_second;
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
                frame_interval,
                keyframe_requested: false,
                first: true,
                refinement: StaticRefinement::new(
                    width,
                    height,
                    frames_per_second,
                    config.bitrate_bits_per_second,
                ),
                composite: layout.map(Composite::new).transpose()?,
            }),
            failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        });
        let mut capture = Self {
            sources: Vec::new(),
            queue: DispatchQueue::new("com.meshrmm.agent.screen", None),
            shared: Arc::clone(&shared),
            pacer: None,
            format: ActiveFormat {
                width,
                height,
                frames_per_second,
                bitrate_bits_per_second: config.bitrate_bits_per_second,
                codec: config.codec,
            },
        };
        // A capture that fails part way stops the streams it started when
        // it is dropped.
        for (index, (member, (width, height))) in members.iter().zip(sizes).enumerate() {
            let source = capture.source(index, member, width, height, &config, excluded)?;
            capture.sources.push(source);
            let source = capture
                .sources
                .last_mut()
                .expect("the source was just added");
            start_stream(&source.stream)?;
            source.started = true;
        }

        let pacer_shared = Arc::clone(&shared);
        capture.pacer = Some(
            std::thread::Builder::new()
                .name("meshrmm-screen-pacer".into())
                .spawn(move || {
                    while !pacer_shared.stopped.load(Ordering::Relaxed) {
                        std::thread::park_timeout(frame_interval);
                        pacer_shared.pace();
                    }
                })
                .context("could not start the screen pacer thread")?,
        );
        Ok(capture)
    }

    /// A configured stream of `display` at `width` by `height` pixels, not
    /// started yet.
    fn source(
        &self,
        index: usize,
        display: &Display,
        width: u32,
        height: u32,
        config: &CaptureConfig,
        excluded: &[u32],
    ) -> anyhow::Result<Source> {
        let filter = content_filter(display.id.0, excluded)?;
        // SAFETY: all ScreenCaptureKit objects are created and configured
        // before the stream starts, with valid arguments.
        unsafe {
            let configuration = SCStreamConfiguration::new();
            configuration.setWidth(width as usize);
            configuration.setHeight(height as usize);
            configuration.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            configuration
                .setMinimumFrameInterval(CMTime::new(1, self.format.frames_per_second as i32));
            configuration.setQueueDepth(QUEUE_DEPTH);
            configuration.setShowsCursor(config.capture_cursor);
            let output = ScreenOutput::new(Arc::clone(&self.shared), index);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &configuration,
                Some(ProtocolObject::from_ref(&*output)),
            );
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&self.queue),
                )
                .map_err(|error| {
                    anyhow::anyhow!(
                        "ScreenCaptureKit rejected the frame output: {}",
                        error.localizedDescription()
                    )
                })?;
            Ok(Source {
                display_id: display.id.0,
                stream,
                configuration,
                _output: output,
                started: false,
            })
        }
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

    /// Leaves the given windows out of the capture from now on.
    pub(crate) fn exclude_windows(&self, excluded: &[u32]) -> anyhow::Result<()> {
        for source in &self.sources {
            let filter = content_filter(source.display_id, excluded)?;
            // SAFETY: updating a live stream's filter is supported.
            unsafe {
                source
                    .stream
                    .updateContentFilter_completionHandler(&filter, None)
            };
        }
        Ok(())
    }

    pub(crate) fn set_cursor_capture(&self, enabled: bool) {
        for source in &self.sources {
            // SAFETY: updating a live stream's configuration is supported.
            unsafe {
                source.configuration.setShowsCursor(enabled);
                source
                    .stream
                    .updateConfiguration_completionHandler(&source.configuration, None);
            }
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

/// Starts a configured stream and waits until it runs.
fn start_stream(stream: &SCStream) -> anyhow::Result<()> {
    let (started, result) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: ScreenCaptureKit passes a valid error or null.
        let _ = started
            .send(unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string()));
    });
    // SAFETY: the stream is fully configured.
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    match result.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(None) => Ok(()),
        Ok(Some(error)) => bail!("ScreenCaptureKit could not start capture: {error}"),
        Err(_) => bail!("ScreenCaptureKit did not start capture in time"),
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        if let Some(pacer) = self.pacer.take() {
            pacer.thread().unpark();
            let _ = pacer.join();
        }
        for source in self.sources.iter().filter(|source| source.started) {
            let (stopped, result) = mpsc::channel();
            let handler = RcBlock::new(move |_error: *mut NSError| {
                let _ = stopped.send(());
            });
            // SAFETY: stopping a started stream is always valid.
            unsafe {
                source
                    .stream
                    .stopCaptureWithCompletionHandler(Some(&handler))
            };
            if result.recv_timeout(CAPTURE_TIMEOUT).is_err() {
                tracing::warn!("ScreenCaptureKit did not confirm that capture stopped");
            }
        }
    }
}

/// A content filter for a Quartz display without the given windows.
pub(crate) fn content_filter(
    display_id: u32,
    excluded: &[u32],
) -> anyhow::Result<Retained<SCContentFilter>> {
    // Without the permission, ScreenCaptureKit may ask the user for it on
    // every call, and thumbnails make this one in the background. The login
    // window's helper runs as root, which macOS asks nothing.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 && !CGPreflightScreenCaptureAccess() {
        bail!(
            "the MeshRMM Agent is not allowed to record the screen; allow it under Screen & System Audio Recording in System Settings"
        );
    }
    let excluded = excluded.to_vec();
    let (sender, receiver) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes valid objects or null.
            let result = match unsafe { (content.as_ref(), error.as_ref()) } {
                (Some(content), _) => Ok(unsafe { content.displays() }
                    .iter()
                    .find(|display| unsafe { display.displayID() } == display_id)
                    .map(|display| {
                        let windows = unsafe { content.windows() }
                            .iter()
                            .filter(|window| excluded.contains(&unsafe { window.windowID() }))
                            .collect::<Vec<_>>();
                        // SAFETY: the display and windows come from this content.
                        unsafe {
                            SCContentFilter::initWithDisplay_excludingWindows(
                                SCContentFilter::alloc(),
                                &display,
                                &NSArray::from_retained_slice(&windows),
                            )
                        }
                    })),
                (None, Some(error)) => Err(error.localizedDescription().to_string()),
                (None, None) => Err("no shareable content".to_owned()),
            };
            let _ = sender.send(result);
        },
    );
    // Windows the Agent has just opened may not count as on screen yet.
    // SAFETY: the handler matches the expected block signature.
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &handler,
        )
    };
    match receiver.recv_timeout(CAPTURE_TIMEOUT) {
        Ok(Ok(Some(filter))) => Ok(filter),
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
            std::slice::from_ref(&display),
            CaptureConfig {
                frames_per_second: 30,
                bitrate_bits_per_second: 6_000_000,
                codec,
                capture_cursor: true,
                grayscale,
            },
            &[],
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
