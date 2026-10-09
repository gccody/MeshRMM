mod transform;
mod upload;

use super::*;
use crate::stream_reset::ResetPlan;
use transform::{
    attach_device, codec_subtype, decoded_subtype, output_size, provides_samples,
    select_output_type, video_type,
};
use upload::CpuUpload;

pub(super) struct WorkerPipeline {
    // Fields drop in order: the decoder before the device and runtimes it
    // uses.
    decoder: Decoder,
    first_presented: Arc<OnceLock<std::time::Instant>>,
    presentation: Presentation,
    decoded: u64,
    presented: u64,
    decoded_frames_dropped: u64,
    interval_decoded: u64,
    interval_presented: u64,
    stats_started_us: u64,
    statistics_log: crate::debug::StatisticsLog,
    debug: DebugInfo,
}

/// Everything the worker owns apart from the decoder: the COM and Media
/// Foundation runtimes, the D3D11 device, and the window with its renderer.
/// It is built as a separate step so a probe can present synthetic frames
/// without a decoder.
pub(super) struct Presentation {
    renderer: D3d11Renderer,
    device: ID3D11Device,
    _mf: MediaFoundationRuntime,
    _com: ComRuntime,
}

impl Presentation {
    pub(super) unsafe fn new(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        let com = unsafe { ComRuntime::start()? };
        let mf = unsafe { MediaFoundationRuntime::start()? };
        let (device, context) = unsafe { create_device()? };
        let renderer = unsafe {
            D3d11Renderer::new(
                &device,
                &context,
                format,
                active_display,
                displays,
                control,
                debug,
            )?
        };
        Ok(Self {
            renderer,
            device,
            _mf: mf,
            _com: com,
        })
    }

    pub(super) fn window(&self) -> HWND {
        self.renderer.window()
    }

    #[cfg(test)]
    pub(super) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    #[cfg(test)]
    pub(super) fn renderer(&mut self) -> &mut D3d11Renderer {
        &mut self.renderer
    }

    /// Creates the decoder for a replacement stream, then moves the window
    /// and renderer to it. The decoder comes first: when the GPU cannot
    /// decode the new profile, nothing has changed yet.
    pub(super) unsafe fn reset_stream(
        &mut self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> anyhow::Result<Decoder> {
        let decoder = unsafe { Decoder::new(&self.device, format)? };
        unsafe { self.reset_presentation(format, display, displays)? };
        Ok(decoder)
    }

    /// Moves the window and renderer to a replacement stream. The window,
    /// its popups, placement and keyboard hook stay as they are.
    pub(super) unsafe fn reset_presentation(
        &mut self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> anyhow::Result<()> {
        let window = self.window();
        let current_display = unsafe { window::active_display_id(window) }
            .context("the viewer window is no longer available")?;
        let plan = ResetPlan::between(self.renderer.format(), format, current_display, display.id);
        let display_id = display.id;
        unsafe { self.renderer.reset_stream(&plan, format)? };
        unsafe { window::reset_stream(window, &plan, format, display, displays) };
        let layout = unsafe { window::client_layout(window) }
            .context("remote window client area is unavailable")?;
        unsafe { self.renderer.configure_layout(&layout)? };
        tracing::info!(
            display_id = display_id.0,
            width = format.width,
            height = format.height,
            recreated_processor = plan.recreate_processor,
            dropped_last_frame = plan.drop_last_frame,
            display_changed = plan.display_changed,
            "reset the Windows viewer window for a replacement stream"
        );
        Ok(())
    }
}

impl WorkerPipeline {
    pub(super) fn window(&self) -> HWND {
        self.presentation.window()
    }

    pub(super) unsafe fn set_cursor_shape(&self, shape: CursorShape) {
        unsafe { set_window_cursor(self.window(), shape) };
    }

    pub(super) unsafe fn new(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
        first_presented: Arc<OnceLock<std::time::Instant>>,
    ) -> anyhow::Result<Self> {
        let presentation =
            unsafe { Presentation::new(format, active_display, displays, control, debug.clone())? };
        let decoder = unsafe { Decoder::new(&presentation.device, format)? };
        Ok(Self {
            decoder,
            first_presented,
            presentation,
            decoded: 0,
            presented: 0,
            decoded_frames_dropped: 0,
            interval_decoded: 0,
            interval_presented: 0,
            stats_started_us: monotonic_timestamp_us(),
            statistics_log: Default::default(),
            debug,
        })
    }

    /// Replaces the decoder and moves the window to a replacement stream.
    /// On failure the old decoder keeps running.
    pub(super) unsafe fn reset_stream(
        &mut self,
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
    ) -> anyhow::Result<()> {
        self.decoder = unsafe { self.presentation.reset_stream(format, display, displays)? };
        Ok(())
    }

    pub(super) unsafe fn resize(&mut self, layout: &window::ClientLayout) -> anyhow::Result<()> {
        unsafe { self.presentation.renderer.resize(layout) }
    }

    pub(super) fn wants_input(&self) -> bool {
        self.decoder.wants_input()
    }

    pub(super) unsafe fn poll(&mut self, presenter_frames_dropped: u64) -> anyhow::Result<()> {
        let frames = unsafe { self.decoder.poll()? };
        unsafe { self.present_decoded(frames, presenter_frames_dropped) }
    }

    pub(super) unsafe fn process(
        &mut self,
        queued: QueuedFrame,
        presenter_frames_dropped: u64,
    ) -> anyhow::Result<Option<QueuedFrame>> {
        let decoded = unsafe { self.decoder.decode(&queued)? };
        let accepted = decoded.accepted;
        unsafe { self.present_decoded(decoded.frames, presenter_frames_dropped)? };
        Ok((!accepted).then_some(queued))
    }

    unsafe fn present_decoded(
        &mut self,
        frames: Vec<DecodedFrame>,
        presenter_frames_dropped: u64,
    ) -> anyhow::Result<()> {
        self.decoded += frames.len() as u64;
        self.interval_decoded += frames.len() as u64;
        self.decoded_frames_dropped = self
            .decoded_frames_dropped
            .saturating_add(frames.len().saturating_sub(1) as u64);
        // The decoder may release more than one surface at once. Present only
        // the newest one so decoder scheduling cannot create a display queue.
        if let Some(frame) = frames.into_iter().last() {
            let receive_to_decode_start_us =
                frame.decode_start_us.saturating_sub(frame.received_at_us);
            let render_start = monotonic_timestamp_us();
            unsafe {
                self.presentation
                    .renderer
                    .present(&frame.texture, frame.subresource)?
            };
            let presentation_us = monotonic_timestamp_us();
            self.first_presented.get_or_init(std::time::Instant::now);
            self.presented += 1;
            self.interval_presented += 1;
            tracing::debug!(
                frame_id = frame.frame_id,
                receive_to_decode_start_us,
                decode_us = frame
                    .decode_complete_us
                    .saturating_sub(frame.decode_start_us),
                render_present_us = presentation_us.saturating_sub(render_start),
                frames_decoded = self.decoded,
                frames_presented = self.presented,
                "video frame presented"
            );
        }
        let now_us = monotonic_timestamp_us();
        let elapsed_us = now_us.saturating_sub(self.stats_started_us);
        if elapsed_us >= 2_000_000 {
            let elapsed_seconds = elapsed_us as f64 / 1_000_000.0;
            let decode_fps = self.interval_decoded as f64 / elapsed_seconds;
            let present_fps = self.interval_presented as f64 / elapsed_seconds;
            self.debug.update_presentation(
                Some(decode_fps),
                present_fps,
                self.presented,
                Some(presenter_frames_dropped),
                self.decoded_frames_dropped,
            );
            if self.statistics_log.due() {
                tracing::info!(
                    decode_fps,
                    present_fps,
                    frames_decoded = self.decoded,
                    frames_presented = self.presented,
                    decoded_frames_dropped = self.decoded_frames_dropped,
                    "decoder/presentation statistics"
                );
            }
            self.interval_decoded = 0;
            self.interval_presented = 0;
            self.stats_started_us = now_us;
        }
        Ok(())
    }
}

struct ComRuntime;

impl ComRuntime {
    unsafe fn start() -> anyhow::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .context("COM MTA initialization failed")?;
        Ok(Self)
    }
}

impl Drop for ComRuntime {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    unsafe fn start() -> anyhow::Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
            .context("Media Foundation startup failed")?;
        Ok(Self)
    }
}

impl Drop for MediaFoundationRuntime {
    fn drop(&mut self) {
        if let Err(error) = unsafe { MFShutdown() } {
            tracing::warn!(error = %error, "Media Foundation shutdown failed");
        }
    }
}

/// The GPU's device with video support. Without one, such as on a machine
/// with no GPU, a device without it: the renderer then converts video with
/// a shader, and only software decoding works.
unsafe fn create_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut failures = Vec::new();
    for (driver_type, video) in [
        (D3D_DRIVER_TYPE_HARDWARE, true),
        (D3D_DRIVER_TYPE_HARDWARE, false),
        (D3D_DRIVER_TYPE_WARP, false),
    ] {
        match unsafe { create_device_of(driver_type, video) } {
            Ok(device) => {
                if !failures.is_empty() {
                    tracing::warn!(
                        ?driver_type,
                        failures = failures.join("; "),
                        "no D3D11 hardware video device; presenting without GPU video support"
                    );
                }
                return Ok(device);
            }
            Err(error) => failures.push(format!("{error:#}")),
        }
    }
    bail!("D3D11 device creation failed: {}", failures.join("; "))
}

unsafe fn create_device_of(
    driver_type: D3D_DRIVER_TYPE,
    video: bool,
) -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    if video {
        flags |= D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    }
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver_type,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .with_context(|| format!("D3D11 {driver_type:?} device creation (video: {video}) failed"))?;
    Ok((
        device.context("D3D11 returned no device")?,
        context.context("D3D11 returned no immediate context")?,
    ))
}

pub(super) unsafe fn supported_video_profiles(format: VideoFormat) -> Vec<VideoProfile> {
    let Ok(_com) = (unsafe { ComRuntime::start() }) else {
        return vec![VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv420,
        }];
    };
    let Ok(_mf) = (unsafe { MediaFoundationRuntime::start() }) else {
        return vec![VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv420,
        }];
    };
    let Ok((device, _context)) = (unsafe { create_device() }) else {
        return vec![VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv420,
        }];
    };
    let mut supported = Vec::new();
    for chroma in [ChromaMode::Yuv444, ChromaMode::Yuv420] {
        for codec in [Codec::H265, Codec::H264] {
            let mut candidate = format;
            candidate.codec = codec;
            candidate.pixel_format = match chroma {
                ChromaMode::Yuv420 => meshrmm_protocol::PixelFormat::Nv12,
                ChromaMode::Yuv444 => meshrmm_protocol::PixelFormat::Ayuv,
            };
            match unsafe { Decoder::new(&device, candidate) } {
                Ok(_) => supported.push(VideoProfile { codec, chroma }),
                Err(error) => tracing::info!(
                    ?codec,
                    ?chroma,
                    error = %error,
                    "video decoder profile unavailable"
                ),
            }
        }
    }
    let mandatory = VideoProfile {
        codec: Codec::H264,
        chroma: ChromaMode::Yuv420,
    };
    if !supported.contains(&mandatory) {
        // The active presenter has already proven the mandatory H.264 path.
        supported.push(mandatory);
    }
    supported
}

struct PendingMetadata {
    frame_id: u64,
    received_at_us: u64,
    decode_start_us: u64,
}

struct DecodedFrame {
    texture: ID3D11Texture2D,
    subresource: u32,
    frame_id: u64,
    received_at_us: u64,
    decode_start_us: u64,
    decode_complete_us: u64,
}

struct DecodeResult {
    accepted: bool,
    frames: Vec<DecodedFrame>,
}

/// Which transform decodes the stream, and where its pictures land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecoderKind {
    /// A GPU vendor's hardware MFT.
    Hardware,
    /// Microsoft's H.264 decoder with the D3D11 device: it decodes with
    /// DXVA where the GPU can. NVIDIA drivers register no H.264 hardware
    /// MFT, so this is their GPU decoder.
    MicrosoftGpu,
    /// Microsoft's H.264 decoder in software, for a machine whose GPU
    /// cannot decode, or that has none.
    MicrosoftSoftware,
}

pub(super) struct Decoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    asynchronous: bool,
    _device_manager: Option<IMFDXGIDeviceManager>,
    kind: DecoderKind,
    output_info: MFT_OUTPUT_STREAM_INFO,
    /// The decoder's picture size, which can be padded beyond the stream's.
    output_size: (u32, u32),
    /// Uploads pictures that a software decoder leaves in system memory.
    upload: CpuUpload,
    frame_duration_100ns: i64,
    need_input: u32,
    have_output: u32,
    pending: VecDeque<PendingMetadata>,
    first_input_logged: bool,
    codec: Codec,
    pixel_format: meshrmm_protocol::PixelFormat,
}

impl Decoder {
    /// A hardware decoder for `format`. H.264 4:2:0 without one falls back
    /// to Microsoft's decoder: on the GPU through DXVA, else in software.
    /// A device without video support gets software decoding only: its
    /// renderer's shader cannot read GPU decoder surfaces.
    pub(super) unsafe fn new(device: &ID3D11Device, format: VideoFormat) -> anyhow::Result<Self> {
        let software_profile = format.codec == Codec::H264
            && format.pixel_format == meshrmm_protocol::PixelFormat::Nv12;
        if device.cast::<ID3D11VideoDevice>().is_err() {
            if !software_profile {
                bail!(
                    "without GPU video support only H.264 4:2:0 can be decoded, not {:?} {:?}",
                    format.codec,
                    format.pixel_format
                );
            }
            return unsafe { Self::microsoft_h264(device, format, false) }
                .context("Microsoft's software H.264 decoder is unavailable");
        }
        let hardware = match unsafe { Self::hardware(device, format) } {
            Ok(decoder) => return Ok(decoder),
            Err(error) => error,
        };
        if !software_profile {
            return Err(hardware);
        }
        tracing::info!(error = %format!("{hardware:#}"), "no H.264 hardware decoder MFT; trying Microsoft's H.264 decoder");
        match unsafe { Self::microsoft_h264(device, format, true) } {
            Ok(decoder) => Ok(decoder),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "Microsoft's H.264 decoder cannot use the GPU; decoding in software");
                unsafe { Self::microsoft_h264(device, format, false) }
                    .context("Microsoft's software H.264 decoder is unavailable")
            }
        }
    }

    unsafe fn hardware(device: &ID3D11Device, format: VideoFormat) -> anyhow::Result<Self> {
        let subtype = codec_subtype(format.codec);
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: subtype,
        };
        let mut activations_ptr: *mut Option<IMFActivate> = ptr::null_mut();
        let mut activation_count = 0;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                // Keep software synchronous/asynchronous MFT categories out
                // of the candidate list. Hardware MFTs are always async.
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
                Some(&input_info),
                // Hardware decoder activation objects frequently advertise a
                // driver-specific output type. Negotiate the requested GPU YUV
                // surface after attaching our D3D11 device manager.
                None,
                &mut activations_ptr,
                &mut activation_count,
            )
        }
        .with_context(|| format!("hardware {:?} decoder enumeration failed", format.codec))?;
        if activation_count == 0 || activations_ptr.is_null() {
            bail!(
                "no Media Foundation {:?} hardware decoder is installed",
                format.codec
            );
        }
        let activations =
            unsafe { std::slice::from_raw_parts_mut(activations_ptr, activation_count as usize) };
        let activation = activations.iter().find_map(Clone::clone);
        for item in activations.iter_mut() {
            let _ = item.take();
        }
        unsafe { CoTaskMemFree(Some(activations_ptr.cast())) };
        let activation = activation.context("hardware decoder activation was empty")?;
        let transform: IMFTransform = unsafe { activation.ActivateObject() }
            .with_context(|| format!("hardware {:?} decoder activation failed", format.codec))?;
        let attributes =
            unsafe { transform.GetAttributes() }.context("decoder attributes unavailable")?;
        let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if asynchronous {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .context("failed to unlock asynchronous decoder")?;
        }
        if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
            bail!(
                "{:?} decoder is not D3D11-aware and cannot guarantee GPU decoding",
                format.codec
            );
        }
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };
        let manager = unsafe { attach_device(&transform, device)? };

        let input_type = unsafe { video_type(subtype, format)? };
        let output_type = unsafe { video_type(decoded_subtype(format.pixel_format), format)? };
        unsafe { transform.SetInputType(0, &input_type, 0) }
            .with_context(|| format!("decoder rejected {:?} input type", format.codec))?;
        unsafe { transform.SetOutputType(0, &output_type, 0) }.with_context(|| {
            format!("decoder rejected GPU {:?} output type", format.pixel_format)
        })?;
        let output_info = unsafe { transform.GetOutputStreamInfo(0) }
            .context("decoder output stream info unavailable")?;
        if !provides_samples(&output_info) {
            bail!("hardware decoder requires caller-allocated output surfaces");
        }
        unsafe {
            Self::start(
                transform,
                Some(manager),
                DecoderKind::Hardware,
                asynchronous,
                device,
                format,
            )
        }
    }

    /// Microsoft's H.264 decoder. With `gpu`, it gets the D3D11 device and
    /// decodes with DXVA where it can; otherwise it decodes in software.
    unsafe fn microsoft_h264(
        device: &ID3D11Device,
        format: VideoFormat,
        gpu: bool,
    ) -> anyhow::Result<Self> {
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
                .context("Microsoft's H.264 decoder is not installed")?;
        let attributes =
            unsafe { transform.GetAttributes() }.context("decoder attributes unavailable")?;
        // Without it, the decoder holds frames back for reordering.
        unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) }
            .context("decoder low-latency mode failed")?;
        let manager = if gpu {
            if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
                bail!("Microsoft's H.264 decoder is not D3D11-aware");
            }
            Some(unsafe { attach_device(&transform, device)? })
        } else {
            None
        };
        let input_type = unsafe { video_type(MFVideoFormat_H264, format)? };
        unsafe { transform.SetInputType(0, &input_type, 0) }
            .context("decoder rejected H264 input type")?;
        // Its output types follow from the input; take its NV12 one.
        unsafe { select_output_type(&transform, MFVideoFormat_NV12)? };
        let kind = if gpu {
            DecoderKind::MicrosoftGpu
        } else {
            DecoderKind::MicrosoftSoftware
        };
        unsafe { Self::start(transform, manager, kind, false, device, format) }
    }

    unsafe fn start(
        transform: IMFTransform,
        device_manager: Option<IMFDXGIDeviceManager>,
        kind: DecoderKind,
        asynchronous: bool,
        device: &ID3D11Device,
        format: VideoFormat,
    ) -> anyhow::Result<Self> {
        let output_info = unsafe { transform.GetOutputStreamInfo(0) }
            .context("decoder output stream info unavailable")?;
        let output_size = unsafe { output_size(&transform)? };
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) }
            .context("decoder begin-streaming failed")?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) }
            .context("decoder start-of-stream failed")?;
        let events = if asynchronous {
            Some(
                transform
                    .cast()
                    .context("asynchronous decoder has no event generator")?,
            )
        } else {
            None
        };
        let mut decoder = Self {
            transform,
            events,
            asynchronous,
            _device_manager: device_manager,
            kind,
            output_info,
            output_size,
            upload: CpuUpload::new(device),
            frame_duration_100ns: 10_000_000 / i64::from(format.frames_per_second.max(1)),
            need_input: 0,
            have_output: 0,
            pending: VecDeque::new(),
            first_input_logged: false,
            codec: format.codec,
            pixel_format: format.pixel_format,
        };
        unsafe { decoder.pump_events()? };
        tracing::info!(
            kind = ?kind,
            codec = ?format.codec,
            pixel_format = ?format.pixel_format,
            "video decoder created"
        );
        Ok(decoder)
    }

    unsafe fn pump_events(&mut self) -> anyhow::Result<()> {
        let Some(events) = self.events.as_ref() else {
            return Ok(());
        };
        loop {
            // This decoder runs on the same thread as the native window pump.
            // A blocking GetEvent call can therefore freeze the entire viewer
            // when a hardware MFT stops requesting input while the desktop is
            // idle. Poll and retain the encoded frame until the MFT is ready.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    unsafe { event.GetStatus() }
                        .context("decoder event status unavailable")?
                        .ok()
                        .context("hardware decoder reported an asynchronous failure")?;
                    match unsafe { event.GetType() } {
                        Ok(value) if value == METransformNeedInput.0 as u32 => self.need_input += 1,
                        Ok(value) if value == METransformHaveOutput.0 as u32 => {
                            self.have_output += 1
                        }
                        Ok(_) => {}
                        Err(error) => return Err(error).context("decoder event type failed"),
                    }
                }
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(error).context("decoder event pump failed"),
            }
        }
        Ok(())
    }

    fn wants_input(&self) -> bool {
        !self.asynchronous || self.need_input > 0
    }

    unsafe fn poll(&mut self) -> anyhow::Result<Vec<DecodedFrame>> {
        if self.asynchronous {
            unsafe { self.pump_events()? };
            let mut decoded = Vec::with_capacity(self.have_output as usize);
            while self.have_output > 0 {
                if let Some(frame) = unsafe { self.take_output()? } {
                    decoded.push(frame);
                }
                self.have_output -= 1;
            }
            return Ok(decoded);
        }
        Ok(Vec::new())
    }

    unsafe fn decode(&mut self, queued: &QueuedFrame) -> anyhow::Result<DecodeResult> {
        let mut decoded = unsafe { self.poll()? };
        if !self.wants_input() {
            return Ok(DecodeResult {
                accepted: false,
                frames: decoded,
            });
        }
        if self.pending.len() >= MAX_DECODER_PENDING_FRAMES {
            bail!(
                "video decoder buffered more than {MAX_DECODER_PENDING_FRAMES} frames; stopping instead of accumulating latency"
            );
        }
        let decode_start_us = monotonic_timestamp_us();
        if !self.first_input_logged {
            tracing::info!(
                frame_id = queued.frame.frame_id,
                keyframe = queued.frame.keyframe,
                encoded_bytes = queued.frame.data.len(),
                annex_b = queued.frame.data.starts_with(&[0, 0, 1])
                    || queued.frame.data.starts_with(&[0, 0, 0, 1]),
                codec = ?self.codec,
                kind = ?self.kind,
                "first access unit submitted to the video decoder"
            );
            self.first_input_logged = true;
        }
        let size = u32::try_from(queued.frame.data.len())
            .context("encoded frame is too large for Media Foundation")?;
        let buffer = unsafe { MFCreateMemoryBuffer(size) }
            .context("decoder input buffer allocation failed")?;
        let mut destination = ptr::null_mut();
        unsafe { buffer.Lock(&mut destination, None, None) }
            .context("decoder input buffer lock failed")?;
        if destination.is_null() {
            let _ = unsafe { buffer.Unlock() };
            bail!("decoder input buffer lock returned null");
        }
        unsafe {
            ptr::copy_nonoverlapping(
                queued.frame.data.as_ptr(),
                destination,
                queued.frame.data.len(),
            )
        };
        unsafe { buffer.Unlock() }.context("decoder input buffer unlock failed")?;
        unsafe { buffer.SetCurrentLength(size) }.context("decoder input length failed")?;
        let sample =
            unsafe { MFCreateSample() }.context("decoder input sample allocation failed")?;
        unsafe { sample.AddBuffer(&buffer) }.context("decoder input sample buffer failed")?;
        if queued.frame.keyframe {
            unsafe { sample.SetUINT32(&MFSampleExtension_CleanPoint, 1) }
                .context("decoder clean-point annotation failed")?;
            unsafe { sample.SetUINT32(&MFSampleExtension_Discontinuity, 1) }
                .context("decoder discontinuity annotation failed")?;
        }
        unsafe {
            sample.SetSampleTime(
                (queued.frame.frame_id as i64).saturating_mul(self.frame_duration_100ns),
            )
        }
        .context("decoder input timestamp failed")?;
        unsafe { sample.SetSampleDuration(self.frame_duration_100ns) }
            .context("decoder input duration failed")?;
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .with_context(|| format!("{:?} {:?} decoder rejected input", self.kind, self.codec))?;
        if self.asynchronous {
            self.need_input -= 1;
        }
        self.pending.push_back(PendingMetadata {
            frame_id: queued.frame.frame_id,
            received_at_us: queued.received_at_us,
            decode_start_us,
        });
        if self.asynchronous {
            decoded.extend(unsafe { self.poll()? });
        } else {
            while let Some(frame) = unsafe { self.take_output()? } {
                decoded.push(frame);
            }
        }
        Ok(DecodeResult {
            accepted: true,
            frames: decoded,
        })
    }

    unsafe fn take_output(&mut self) -> anyhow::Result<Option<DecodedFrame>> {
        // Software decoders write into a sample the caller provides.
        let provided = if provides_samples(&self.output_info) {
            None
        } else {
            let sample = unsafe { MFCreateSample() }.context("decoder output sample failed")?;
            let buffer = unsafe { MFCreateMemoryBuffer(self.output_info.cbSize.max(1)) }
                .context("decoder output buffer allocation failed")?;
            unsafe { sample.AddBuffer(&buffer) }.context("decoder output sample buffer failed")?;
            Some(sample)
        };
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            ..Default::default()
        };
        let mut status = 0;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        let _ = unsafe { ManuallyDrop::take(&mut output.pEvents) };
        if let Err(error) = result {
            if error.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                let wanted = decoded_subtype(self.pixel_format);
                unsafe { select_output_type(&self.transform, wanted)? };
                self.output_info = unsafe { self.transform.GetOutputStreamInfo(0) }
                    .context("decoder output stream info unavailable")?;
                self.output_size = unsafe { output_size(&self.transform)? };
                tracing::info!(
                    codec = ?self.codec,
                    pixel_format = ?self.pixel_format,
                    kind = ?self.kind,
                    width = self.output_size.0,
                    height = self.output_size.1,
                    "video decoder applied a stream format change"
                );
                return unsafe { self.take_output() };
            }
            if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            return Err(error).context("video decoder output failed");
        }
        let sample = sample.context("video decoder returned no sample")?;
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .context("decoded sample has no surface buffer")?;
        let (texture, subresource) = match buffer.cast::<IMFDXGIBuffer>() {
            Ok(dxgi) => {
                let mut raw: *mut c_void = ptr::null_mut();
                unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
                    .context("decoded DXGI texture lookup failed")?;
                if raw.is_null() {
                    bail!("decoded DXGI texture was null");
                }
                let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
                let subresource = unsafe { dxgi.GetSubresourceIndex() }
                    .context("decoded texture subresource unavailable")?;
                (texture, subresource)
            }
            // A software decoder's picture is in system memory.
            Err(_) => (unsafe { self.upload.upload(&buffer, self.output_size)? }, 0),
        };
        let metadata = self
            .pending
            .pop_front()
            .context("decoder output had no input metadata")?;
        Ok(Some(DecodedFrame {
            texture,
            subresource,
            frame_id: metadata.frame_id,
            received_at_us: metadata.received_at_us,
            decode_start_us: metadata.decode_start_us,
            decode_complete_us: monotonic_timestamp_us(),
        }))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

#[cfg(test)]
mod tests;
