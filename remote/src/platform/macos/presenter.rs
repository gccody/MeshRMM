use super::app::{RemoteView, VideoHostView, activate_application};
use super::*;

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
    queued: Mutex<VecDeque<QueuedFrame>>,
    scheduled: AtomicBool,
    failure: Mutex<Option<String>>,
    replaced: AtomicU64,
    submitted: AtomicU64,
    dropped_by_renderer: AtomicU64,
    recovering: AtomicBool,
    resetting: AtomicBool,
    control: ControlSink,
    debug: DebugInfo,
}

const MAX_PRESENTER_QUEUE_FRAMES: usize = 15;

impl Shared {
    fn begin_recovery(&self, stream_id: VideoStreamId) {
        if !self.recovering.swap(true, Ordering::AcqRel) {
            self.control
                .send(SessionMessage::RequestKeyframe { stream_id });
            tracing::warn!(
                stream_id = stream_id.0,
                "macOS presenter dropped a video reference; requesting a recovery keyframe"
            );
        }
    }
}

pub struct Presenter {
    shared: Arc<Shared>,
    stopped: bool,
}

impl Presenter {
    pub fn start(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        if !hardware_decode_supported(format.codec) {
            bail!("macOS has no hardware decoder for {:?}", format.codec);
        }
        let id = NEXT_PRESENTER_ID.fetch_add(1, Ordering::Relaxed);
        tracing::info!(
            presenter_id = id,
            display_id = active_display.id.0,
            width = format.width,
            height = format.height,
            fps = format.frames_per_second,
            bitrate_bits_per_second = format.bitrate_bits_per_second,
            codec = ?format.codec,
            "starting macOS video presenter"
        );
        let shared = Arc::new(Shared {
            id,
            queued: Mutex::new(VecDeque::with_capacity(MAX_PRESENTER_QUEUE_FRAMES)),
            scheduled: AtomicBool::new(false),
            failure: Mutex::new(None),
            replaced: AtomicU64::new(0),
            submitted: AtomicU64::new(0),
            dropped_by_renderer: AtomicU64::new(0),
            recovering: AtomicBool::new(false),
            resetting: AtomicBool::new(false),
            control: control.clone(),
            debug: debug.clone(),
        });
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            let result =
                MacUi::new(id, format, active_display, displays, control, debug).map(|ui| {
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

    pub fn publish(&self, frame: EncodedFrame, received_at_us: u64) -> bool {
        let stream_id = frame.stream_id;
        let Ok(mut queued) = self.shared.queued.lock() else {
            return false;
        };
        if self.shared.resetting.load(Ordering::Acquire) {
            self.shared.replaced.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if self.shared.recovering.load(Ordering::Acquire) && !frame.keyframe {
            self.shared.replaced.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if queued.len() >= MAX_PRESENTER_QUEUE_FRAMES && !frame.keyframe {
            self.shared
                .replaced
                .fetch_add(queued.len() as u64 + 1, Ordering::Relaxed);
            queued.clear();
            drop(queued);
            self.shared.begin_recovery(stream_id);
            return false;
        }
        if frame.keyframe
            && (self.shared.recovering.load(Ordering::Acquire)
                || queued.len() >= MAX_PRESENTER_QUEUE_FRAMES)
        {
            self.shared
                .replaced
                .fetch_add(queued.len() as u64, Ordering::Relaxed);
            queued.clear();
            self.shared.debug.set_presenter_frames_dropped(
                self.shared.replaced.load(Ordering::Relaxed)
                    + self.shared.dropped_by_renderer.load(Ordering::Relaxed),
            );
        }
        queued.push_back(QueuedFrame {
            frame,
            received_at_us,
        });
        self.shared.recovering.store(false, Ordering::Release);
        drop(queued);
        schedule_latest(Arc::clone(&self.shared));
        true
    }

    pub fn reset_stream(&self, format: VideoFormat) -> anyhow::Result<()> {
        self.shared.resetting.store(true, Ordering::Release);
        self.shared.recovering.store(true, Ordering::Release);
        if let Ok(mut queued) = self.shared.queued.lock() {
            self.shared
                .replaced
                .fetch_add(queued.len() as u64, Ordering::Relaxed);
            queued.clear();
        }

        let id = self.shared.id;
        let (reset_tx, reset_rx) = std::sync::mpsc::sync_channel(1);
        DispatchQueue::main().exec_async(move || {
            let result = UI.with(|state| {
                let mut state = state.borrow_mut();
                let ui = state
                    .as_mut()
                    .filter(|ui| ui.id == id)
                    .context("macOS viewer window is no longer available")?;
                ui.reset_stream(format)
            });
            let _ = reset_tx.send(result);
        });
        let result = reset_rx
            .recv_timeout(Duration::from_secs(5))
            .context("macOS main thread did not reset the video decoder")
            .and_then(|result| result);
        self.shared.resetting.store(false, Ordering::Release);
        result
    }

    pub fn set_cursor_shape(&self, shape: CursorShape) {
        let id = self.shared.id;
        DispatchQueue::main().exec_async(move || {
            UI.with(|state| {
                if let Some(ui) = state.borrow().as_ref().filter(|ui| ui.id == id) {
                    ui.input_view.set_cursor_shape(shape);
                }
            });
        });
    }

    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        let id = self.shared.id;
        let shared = Arc::clone(&self.shared);
        // Decoder failures happen asynchronously and may be the last activity
        // on a static desktop. Poll UI health independently of frame arrival
        // so HEVC failures still trigger the negotiated H.264 fallback.
        DispatchQueue::main().exec_async(move || {
            let failure = UI.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .filter(|ui| ui.id == id)
                    .and_then(MacUi::presentation_failure)
            });
            if let Some(failure) = failure
                && let Ok(mut current) = shared.failure.lock()
                && current.is_none()
            {
                *current = Some(failure);
            }
        });
        self.shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take())
            .map(|failure| {
                if failure == "macOS viewer window was closed" {
                    Ok(())
                } else {
                    Err(failure)
                }
            })
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
            presenter_id = self.shared.id,
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
        let queued = shared
            .queued
            .lock()
            .ok()
            .and_then(|mut frames| frames.pop_front());
        if let Some(queued) = queued {
            let stream_id = queued.frame.stream_id;
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
                    shared.begin_recovery(stream_id);
                    if let Ok(mut frames) = shared.queued.lock() {
                        shared
                            .replaced
                            .fetch_add(frames.len() as u64, Ordering::Relaxed);
                        frames.clear();
                    }
                    shared.debug.set_presenter_frames_dropped(
                        shared.replaced.load(Ordering::Relaxed)
                            + shared.dropped_by_renderer.load(Ordering::Relaxed),
                    );
                }
                Err(error) => {
                    if let Ok(mut failure) = shared.failure.lock() {
                        *failure = Some(error.to_string());
                    }
                }
            }
        }
        shared.scheduled.store(false, Ordering::Release);
        if shared.queued.lock().is_ok_and(|queued| !queued.is_empty()) {
            schedule_latest(shared);
        }
    });
}

struct MacUi {
    id: u64,
    window: Retained<NSWindow>,
    layer: Retained<AVSampleBufferDisplayLayer>,
    input_view: Retained<RemoteView>,
    format_description: Option<CFRetained<CMFormatDescription>>,
    waiting_for_keyframe: bool,
    frames_per_second: i32,
    frames_submitted: u64,
    total_frames_submitted: u64,
    stats_started: Instant,
    debug: DebugInfo,
    codec: Codec,
    layer_backpressured: bool,
}

impl MacUi {
    fn new(
        id: u64,
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        let mtm =
            MainThreadMarker::new().context("AppKit must be initialized on the main thread")?;
        tracing::info!(
            presenter_id = id,
            display_id = active_display.id.0,
            codec = ?format.codec,
            "creating macOS viewer window"
        );
        let scale = (1440.0_f64 / f64::from(format.width))
            .min((900.0_f64 - VIEWER_TOOLBAR_HEIGHT) / f64::from(format.height))
            .min(1.0);
        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: f64::from(format.width) * scale,
                height: f64::from(format.height) * scale + VIEWER_TOOLBAR_HEIGHT,
            },
        };
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
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
        // NSWindow releases itself when the user closes it by default. Keep the
        // window alive until MacUi is dropped so queued frames can safely detect
        // that it is no longer visible and end the remote session.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setTitlebarAppearsTransparent(true);
        // The content view handles mouse input for the remote desktop. Making
        // the window movable by its background causes AppKit to turn drags on
        // that view into local window moves instead of remote pointer drags.
        // The native title bar remains draggable without this setting.
        window.setMovableByWindowBackground(false);
        window.setTitle(&NSString::from_str(&format!(
            "MeshRMM Remote Desktop — {} — Control-Option-Arrow display · F12 diagnostics",
            active_display.name
        )));
        let view = RemoteView::new(
            mtm,
            rect,
            active_display,
            displays,
            format.width,
            format.height,
            control,
            debug.clone(),
        );
        window.setContentView(Some(&view));
        window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
        let layer = unsafe { AVSampleBufferDisplayLayer::new() };
        // Safety: this reads an immutable AVFoundation framework constant.
        let video_gravity = unsafe { AVLayerVideoGravityResizeAspect }
            .context("AVFoundation video gravity constant is unavailable")?;
        unsafe {
            layer.setVideoGravity(video_gravity);
        }
        let mut video_bounds = view.bounds();
        video_bounds.size.height = (video_bounds.size.height - VIEWER_TOOLBAR_HEIGHT).max(1.0);
        let video_host = VideoHostView::new(mtm, video_bounds);
        video_host.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        // This child has no subviews, so it can safely be layer-hosting. Keep
        // RemoteView itself as a normal AppKit hierarchy for the title bar controls.
        video_host.setLayer(Some(&layer));
        video_host.setWantsLayer(true);
        view.addSubview_positioned_relativeTo(&video_host, NSWindowOrderingMode::Below, None);
        window.setAcceptsMouseMovedEvents(true);
        window.center();
        close_connecting_window();
        activate_application(mtm);
        window.makeKeyAndOrderFront(None);
        window.orderFrontRegardless();
        if !window.makeFirstResponder(Some(&view)) {
            tracing::warn!("macOS viewer could not acquire remote-input focus");
        }
        tracing::info!(
            presenter_id = id,
            codec = ?format.codec,
            "macOS viewer window is visible"
        );

        Ok(Self {
            id,
            window,
            layer,
            input_view: view,
            format_description: None,
            waiting_for_keyframe: true,
            frames_per_second: i32::from(format.frames_per_second.max(1)),
            frames_submitted: 0,
            total_frames_submitted: 0,
            stats_started: Instant::now(),
            debug,
            codec: format.codec,
            layer_backpressured: false,
        })
    }

    #[allow(deprecated)]
    fn reset_stream(&mut self, format: VideoFormat) -> anyhow::Result<()> {
        if self.codec != format.codec {
            bail!(
                "cannot reset a {:?} presenter for a {:?} stream",
                self.codec,
                format.codec
            );
        }
        tracing::info!(
            presenter_id = self.id,
            codec = ?self.codec,
            bitrate_bits_per_second = format.bitrate_bits_per_second,
            "resetting macOS video decoder without replacing its window"
        );
        // Keep the last decoded desktop visible while the replacement capture
        // stream produces its bootstrap keyframe. Removing the current image
        // makes every Agent-side encoder restart look like a viewer refresh.
        unsafe { self.layer.flush() };
        self.format_description = None;
        self.waiting_for_keyframe = true;
        self.frames_per_second = i32::from(format.frames_per_second.max(1));
        self.frames_submitted = 0;
        self.stats_started = Instant::now();
        self.layer_backpressured = false;
        Ok(())
    }

    #[allow(deprecated)]
    fn enqueue(&mut self, queued: QueuedFrame) -> anyhow::Result<bool> {
        if !self.window.isVisible() {
            bail!("macOS viewer window was closed");
        }
        if unsafe { self.layer.requiresFlushToResumeDecoding() } {
            tracing::warn!(
                presenter_id = self.id,
                codec = ?self.codec,
                "AVSampleBufferDisplayLayer requires a flush to resume decoding"
            );
            unsafe { self.layer.flush() };
            self.format_description = None;
            self.waiting_for_keyframe = true;
        }
        if let Some(failure) = self.decoder_failure() {
            bail!(failure);
        }
        if self.waiting_for_keyframe && !queued.frame.keyframe {
            return Ok(false);
        }

        let converted = annex_b_to_length_prefixed(&queued.frame.data, self.codec)?;
        if self.format_description.is_none() {
            let sps = converted
                .sequence_parameter_set
                .as_deref()
                .with_context(|| format!("bootstrap keyframe is missing a {:?} SPS", self.codec))?;
            let pps = converted
                .picture_parameter_set
                .as_deref()
                .with_context(|| format!("bootstrap keyframe is missing a {:?} PPS", self.codec))?;
            self.format_description = Some(match self.codec {
                Codec::H264 => create_h264_format_description(sps, pps)?,
                Codec::H265 => {
                    let vps = converted
                        .video_parameter_set
                        .as_deref()
                        .context("bootstrap keyframe is missing an H.265 VPS")?;
                    create_h265_format_description(vps, sps, pps)?
                }
            });
            self.waiting_for_keyframe = false;
        }
        if !unsafe { self.layer.isReadyForMoreMediaData() } {
            if !self.layer_backpressured {
                tracing::warn!(
                    presenter_id = self.id,
                    codec = ?self.codec,
                    frame_id = queued.frame.frame_id,
                    "macOS video layer stopped accepting media data"
                );
                self.layer_backpressured = true;
            }
            return Ok(false);
        }
        if self.layer_backpressured {
            tracing::info!(
                presenter_id = self.id,
                codec = ?self.codec,
                frame_id = queued.frame.frame_id,
                "macOS video layer resumed accepting media data"
            );
            self.layer_backpressured = false;
        }

        let sample = create_sample_buffer(
            &converted.data,
            self.format_description
                .as_deref()
                .context("video format description is unavailable")?,
            queued.frame.frame_id,
            self.frames_per_second,
        )?;
        unsafe { self.layer.enqueueSampleBuffer(&sample) };
        self.frames_submitted += 1;
        self.total_frames_submitted += 1;
        self.input_view.refresh_debug(false);
        let elapsed = self.stats_started.elapsed();
        if elapsed >= Duration::from_secs(2) {
            let present_fps = self.frames_submitted as f64 / elapsed.as_secs_f64();
            self.debug
                .update_presentation(None, present_fps, self.total_frames_submitted, None, 0);
            tracing::info!(
                submit_fps = present_fps,
                frames_submitted = self.frames_submitted,
                receive_to_submit_us =
                    monotonic_timestamp_us().saturating_sub(queued.received_at_us),
                ready_for_display = unsafe { self.layer.isReadyForDisplay() },
                codec = ?self.codec,
                "macOS hardware decode/presentation statistics"
            );
            self.frames_submitted = 0;
            self.stats_started = Instant::now();
        }
        Ok(true)
    }

    #[allow(deprecated)]
    fn decoder_failure(&self) -> Option<String> {
        if unsafe { self.layer.status() } != AVQueuedSampleBufferRenderingStatus::Failed
            || unsafe { self.layer.requiresFlushToResumeDecoding() }
        {
            return None;
        }
        let detail = unsafe { self.layer.error() }
            .map(|error| error.localizedDescription().to_string())
            .unwrap_or_else(|| "unknown AVFoundation decoder error".into());
        Some(format!(
            "macOS {:?} hardware decoder failed: {detail}",
            self.codec
        ))
    }

    fn presentation_failure(&self) -> Option<String> {
        if !self.window.isVisible() {
            return Some("macOS viewer window was closed".into());
        }
        self.decoder_failure()
    }

    #[allow(deprecated)]
    fn close(self) {
        tracing::info!(
            presenter_id = self.id,
            codec = ?self.codec,
            visible = self.window.isVisible(),
            key_window = self.window.isKeyWindow(),
            "closing macOS viewer window"
        );
        if self.window.isKeyWindow() {
            self.input_view.disable_input();
        } else {
            // A replaced/background window already disabled input when it
            // resigned key status. Do not disable a newer foreground window
            // that shares the same transport gate.
            self.input_view.release_input();
        }
        unsafe { self.layer.flushAndRemoveImage() };
        self.window.orderOut(None);
    }
}

fn create_h265_format_description(
    vps: &[u8],
    sps: &[u8],
    pps: &[u8],
) -> anyhow::Result<CFRetained<CMFormatDescription>> {
    let mut pointers = [
        NonNull::new(vps.as_ptr().cast_mut()).context("H.265 VPS is empty")?,
        NonNull::new(sps.as_ptr().cast_mut()).context("H.265 SPS is empty")?,
        NonNull::new(pps.as_ptr().cast_mut()).context("H.265 PPS is empty")?,
    ];
    let mut sizes = [vps.len(), sps.len(), pps.len()];
    let mut description: *const CMFormatDescription = ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromHEVCParameterSets(
            None,
            pointers.len(),
            NonNull::new(pointers.as_mut_ptr()).context("H.265 parameter pointers are null")?,
            NonNull::new(sizes.as_mut_ptr()).context("H.265 parameter sizes are null")?,
            4,
            None,
            NonNull::from(&mut description),
        )
    };
    check_status(status, "CoreMedia rejected H.265 parameter sets")?;
    let description = NonNull::new(description.cast_mut())
        .context("CoreMedia returned no H.265 format description")?;
    Ok(unsafe { CFRetained::from_raw(description) })
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
        bail!("cannot create an empty video sample");
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
