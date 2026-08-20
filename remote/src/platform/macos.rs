use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
};
use objc2_av_foundation::{AVLayerVideoGravityResizeAspect, AVSampleBufferDisplayLayer};
use objc2_core_foundation::{CFBoolean, CFMutableDictionary, CFRetained, CFString, kCFBooleanTrue};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMSampleAttachmentKey_DisplayImmediately,
    kCMTimeInvalid,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CAAutoresizingMask;
use pulsermm_protocol::{Codec, EncodedFrame, VideoFormat};

use crate::h264::annex_b_to_avcc;

static NEXT_PRESENTER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// AppKit and Core Animation objects never leave the main thread.
    static UI: RefCell<Option<MacUi>> = const { RefCell::new(None) };
}

struct QueuedFrame {
    frame: EncodedFrame,
    received_at_us: u64,
}

struct Shared {
    id: u64,
    latest: Mutex<Option<QueuedFrame>>,
    scheduled: AtomicBool,
    failure: Mutex<Option<String>>,
    replaced: AtomicU64,
    submitted: AtomicU64,
    dropped_by_renderer: AtomicU64,
}

pub struct Presenter {
    shared: Arc<Shared>,
    stopped: bool,
}

impl Presenter {
    pub fn start(format: VideoFormat) -> anyhow::Result<Self> {
        if format.codec != Codec::H264 {
            bail!("macOS viewer supports only H.264");
        }
        let id = NEXT_PRESENTER_ID.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            id,
            latest: Mutex::new(None),
            scheduled: AtomicBool::new(false),
            failure: Mutex::new(None),
            replaced: AtomicU64::new(0),
            submitted: AtomicU64::new(0),
            dropped_by_renderer: AtomicU64::new(0),
        });
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            let result = MacUi::new(id, format).map(|ui| {
                UI.with(|state| {
                    if let Some(old) = state.borrow_mut().replace(ui) {
                        old.close();
                    }
                });
            });
            let _ = started_tx.send(result);
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .context("macOS main thread did not create the viewer window")??;
        Ok(Self {
            shared,
            stopped: false,
        })
    }

    pub fn publish(&self, frame: EncodedFrame, received_at_us: u64) {
        let Ok(mut latest) = self.shared.latest.lock() else {
            return;
        };
        if latest
            .replace(QueuedFrame {
                frame,
                received_at_us,
            })
            .is_some()
        {
            self.shared.replaced.fetch_add(1, Ordering::Relaxed);
        }
        drop(latest);
        schedule_latest(Arc::clone(&self.shared));
    }

    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        self.shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take())
            .map(Err)
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let id = self.shared.id;
        DispatchQueue::main().exec_async(move || {
            UI.with(|state| {
                let mut state = state.borrow_mut();
                if state.as_ref().is_some_and(|ui| ui.id == id)
                    && let Some(ui) = state.take()
                {
                    ui.close();
                }
            });
        });
        tracing::info!(
            latest_frames_dropped = self.shared.replaced.load(Ordering::Relaxed),
            renderer_frames_dropped = self.shared.dropped_by_renderer.load(Ordering::Relaxed),
            frames_submitted = self.shared.submitted.load(Ordering::Relaxed),
            "macOS video presenter stopped"
        );
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn schedule_latest(shared: Arc<Shared>) {
    if shared
        .scheduled
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    DispatchQueue::main().exec_async(move || {
        let queued = shared.latest.lock().ok().and_then(|mut frame| frame.take());
        if let Some(queued) = queued {
            let result = UI.with(|state| {
                let mut state = state.borrow_mut();
                let ui = state
                    .as_mut()
                    .filter(|ui| ui.id == shared.id)
                    .context("macOS viewer window is no longer available")?;
                ui.enqueue(queued)
            });
            match result {
                Ok(true) => {
                    shared.submitted.fetch_add(1, Ordering::Relaxed);
                }
                Ok(false) => {
                    shared.dropped_by_renderer.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    if let Ok(mut failure) = shared.failure.lock() {
                        *failure = Some(error.to_string());
                    }
                }
            }
        }
        shared.scheduled.store(false, Ordering::Release);
        if shared.latest.lock().is_ok_and(|latest| latest.is_some()) {
            schedule_latest(shared);
        }
    });
}

struct MacUi {
    id: u64,
    window: Retained<NSWindow>,
    layer: Retained<AVSampleBufferDisplayLayer>,
    format_description: Option<CFRetained<CMFormatDescription>>,
    waiting_for_keyframe: bool,
    frames_per_second: i32,
    frames_submitted: u64,
    stats_started: Instant,
}

impl MacUi {
    fn new(id: u64, format: VideoFormat) -> anyhow::Result<Self> {
        let mtm =
            MainThreadMarker::new().context("AppKit must be initialized on the main thread")?;
        let scale = (1440.0_f64 / f64::from(format.width))
            .min(900.0_f64 / f64::from(format.height))
            .min(1.0);
        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: f64::from(format.width) * scale,
                height: f64::from(format.height) * scale,
            },
        };
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // Safety: all AppKit construction occurs on the process main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("PulseRMM Remote Desktop"));
        let view = window
            .contentView()
            .context("AppKit window has no content view")?;
        let layer = unsafe { AVSampleBufferDisplayLayer::new() };
        // Safety: this reads an immutable AVFoundation framework constant.
        let video_gravity = unsafe { AVLayerVideoGravityResizeAspect }
            .context("AVFoundation video gravity constant is unavailable")?;
        unsafe {
            layer.setVideoGravity(video_gravity);
            layer.setFrame(view.bounds());
            layer.setAutoresizingMask(
                CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
            );
        }
        view.setWantsLayer(true);
        view.setLayer(Some(&layer));
        window.center();
        window.makeKeyAndOrderFront(None);

        Ok(Self {
            id,
            window,
            layer,
            format_description: None,
            waiting_for_keyframe: true,
            frames_per_second: i32::from(format.frames_per_second.max(1)),
            frames_submitted: 0,
            stats_started: Instant::now(),
        })
    }

    #[allow(deprecated)]
    fn enqueue(&mut self, queued: QueuedFrame) -> anyhow::Result<bool> {
        if !self.window.isVisible() {
            bail!("macOS viewer window was closed");
        }
        if unsafe { self.layer.requiresFlushToResumeDecoding() } {
            unsafe { self.layer.flush() };
            self.format_description = None;
            self.waiting_for_keyframe = true;
        }
        if self.waiting_for_keyframe && !queued.frame.keyframe {
            return Ok(false);
        }

        let converted = annex_b_to_avcc(&queued.frame.data)?;
        if self.format_description.is_none() {
            let sps = converted
                .sequence_parameter_set
                .as_deref()
                .context("bootstrap keyframe is missing an H.264 SPS")?;
            let pps = converted
                .picture_parameter_set
                .as_deref()
                .context("bootstrap keyframe is missing an H.264 PPS")?;
            self.format_description = Some(create_h264_format_description(sps, pps)?);
            self.waiting_for_keyframe = false;
        }
        if !unsafe { self.layer.isReadyForMoreMediaData() } {
            return Ok(false);
        }

        let sample = create_sample_buffer(
            &converted.data,
            self.format_description
                .as_deref()
                .context("H.264 format description is unavailable")?,
            queued.frame.frame_id,
            self.frames_per_second,
        )?;
        unsafe { self.layer.enqueueSampleBuffer(&sample) };
        self.frames_submitted += 1;
        let elapsed = self.stats_started.elapsed();
        if elapsed >= Duration::from_secs(2) {
            tracing::info!(
                submit_fps = self.frames_submitted as f64 / elapsed.as_secs_f64(),
                frames_submitted = self.frames_submitted,
                receive_to_submit_us =
                    monotonic_timestamp_us().saturating_sub(queued.received_at_us),
                ready_for_display = unsafe { self.layer.isReadyForDisplay() },
                codec = "h264",
                "macOS hardware decode/presentation statistics"
            );
            self.frames_submitted = 0;
            self.stats_started = Instant::now();
        }
        Ok(true)
    }

    #[allow(deprecated)]
    fn close(self) {
        unsafe { self.layer.flushAndRemoveImage() };
        self.window.orderOut(None);
    }
}

fn create_h264_format_description(
    sps: &[u8],
    pps: &[u8],
) -> anyhow::Result<CFRetained<CMFormatDescription>> {
    let mut pointers = [
        NonNull::new(sps.as_ptr().cast_mut()).context("H.264 SPS is empty")?,
        NonNull::new(pps.as_ptr().cast_mut()).context("H.264 PPS is empty")?,
    ];
    let mut sizes = [sps.len(), pps.len()];
    let mut description: *const CMFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            pointers.len(),
            NonNull::new(pointers.as_mut_ptr()).context("H.264 parameter pointers are null")?,
            NonNull::new(sizes.as_mut_ptr()).context("H.264 parameter sizes are null")?,
            4,
            NonNull::from(&mut description),
        )
    };
    check_status(status, "CoreMedia rejected H.264 parameter sets")?;
    let description = NonNull::new(description.cast_mut())
        .context("CoreMedia returned no H.264 format description")?;
    // Safety: CoreMedia returned this object at +1 retain count.
    Ok(unsafe { CFRetained::from_raw(description) })
}

fn create_sample_buffer(
    data: &[u8],
    format: &CMFormatDescription,
    frame_id: u64,
    frames_per_second: i32,
) -> anyhow::Result<CFRetained<CMSampleBuffer>> {
    if data.is_empty() {
        bail!("cannot create an empty H.264 sample");
    }
    let mut block: *mut CMBlockBuffer = ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            None,
            ptr::null(),
            0,
            data.len(),
            0,
            NonNull::from(&mut block),
        )
    };
    check_status(status, "CoreMedia H.264 block allocation failed")?;
    let block = NonNull::new(block).context("CoreMedia returned no H.264 block buffer")?;
    // Safety: CoreMedia returned this object at +1 retain count.
    let block = unsafe { CFRetained::from_raw(block) };
    let source = NonNull::new(data.as_ptr().cast_mut().cast::<c_void>())
        .context("H.264 sample pointer is null")?;
    check_status(
        unsafe { CMBlockBuffer::replace_data_bytes(source, &block, 0, data.len()) },
        "CoreMedia H.264 block copy failed",
    )?;

    let timing = CMSampleTimingInfo {
        duration: unsafe { CMTime::new(1, frames_per_second) },
        presentationTimeStamp: unsafe {
            CMTime::new(
                i64::try_from(frame_id).unwrap_or(i64::MAX),
                frames_per_second,
            )
        },
        decodeTimeStamp: unsafe { kCMTimeInvalid },
    };
    let size = data.len();
    let mut sample: *mut CMSampleBuffer = ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    check_status(status, "CoreMedia H.264 sample creation failed")?;
    let sample = NonNull::new(sample).context("CoreMedia returned no H.264 sample buffer")?;
    // Safety: CoreMedia returned this object at +1 retain count.
    let sample = unsafe { CFRetained::from_raw(sample) };
    set_display_immediately(&sample)?;
    Ok(sample)
}

fn set_display_immediately(sample: &CMSampleBuffer) -> anyhow::Result<()> {
    let attachments = unsafe { sample.sample_attachments_array(true) }
        .context("CoreMedia returned no sample attachment array")?;
    if attachments.count() == 0 {
        bail!("CoreMedia sample attachment array is empty");
    }
    let dictionary = unsafe { attachments.value_at_index(0) } as *const CFMutableDictionary;
    let dictionary =
        unsafe { dictionary.as_ref() }.context("CoreMedia sample attachment dictionary is null")?;
    let value: &CFBoolean =
        unsafe { kCFBooleanTrue }.context("CoreFoundation true value is unavailable")?;
    unsafe {
        CFMutableDictionary::set_value(
            Some(dictionary),
            kCMSampleAttachmentKey_DisplayImmediately as *const CFString as *const c_void,
            value as *const CFBoolean as *const c_void,
        )
    };
    Ok(())
}

fn check_status(status: i32, context: &'static str) -> anyhow::Result<()> {
    if status == 0 {
        Ok(())
    } else {
        bail!("{context}: OSStatus {status}")
    }
}

pub fn monotonic_timestamp_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn run_application<F>(network: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    let mtm = MainThreadMarker::new().context("PulseRMM must start on the macOS main thread")?;
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    application.finishLaunching();

    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pulsermm-network".into())
        .spawn(move || {
            let result = network();
            let _ = result_tx.send(result);
            DispatchQueue::main().exec_async(|| {
                if let Some(mtm) = MainThreadMarker::new() {
                    NSApplication::sharedApplication(mtm).stop(None);
                }
            });
        })
        .context("failed to start macOS network runtime")?;

    // `activate` is newer than the MVP's macOS 12 deployment target.
    #[allow(deprecated)]
    application.activateIgnoringOtherApps(true);
    application.run();
    result_rx
        .recv()
        .context("macOS network runtime exited without a result")?
}
