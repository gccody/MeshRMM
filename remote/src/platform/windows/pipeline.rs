use super::*;

pub(super) struct WorkerPipeline {
    _com: ComRuntime,
    _mf: MediaFoundationRuntime,
    decoder: HardwareDecoder,
    renderer: D3d11Renderer,
    decoded: u64,
    presented: u64,
    decoded_frames_dropped: u64,
    interval_decoded: u64,
    interval_presented: u64,
    stats_started_us: u64,
    debug: DebugInfo,
}

impl WorkerPipeline {
    pub(super) fn window(&self) -> HWND {
        self.renderer.window()
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
                debug.clone(),
            )?
        };
        let decoder = unsafe { HardwareDecoder::new(&device, format)? };
        Ok(Self {
            _com: com,
            _mf: mf,
            decoder,
            renderer,
            decoded: 0,
            presented: 0,
            decoded_frames_dropped: 0,
            interval_decoded: 0,
            interval_presented: 0,
            stats_started_us: monotonic_timestamp_us(),
            debug,
        })
    }

    pub(super) unsafe fn process(
        &mut self,
        queued: QueuedFrame,
        presenter_frames_dropped: u64,
    ) -> anyhow::Result<()> {
        let receive_to_decode_start_us =
            monotonic_timestamp_us().saturating_sub(queued.received_at_us);
        let frames = unsafe { self.decoder.decode(queued)? };
        self.decoded += frames.len() as u64;
        self.interval_decoded += frames.len() as u64;
        self.decoded_frames_dropped = self
            .decoded_frames_dropped
            .saturating_add(frames.len().saturating_sub(1) as u64);
        // The decoder may release more than one surface at once. Present only
        // the newest one so decoder scheduling cannot create a display queue.
        if let Some(frame) = frames.into_iter().last() {
            let render_start = monotonic_timestamp_us();
            unsafe { self.renderer.present(&frame.texture, frame.subresource)? };
            let presentation_us = monotonic_timestamp_us();
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
            tracing::info!(
                decode_fps,
                present_fps,
                frames_decoded = self.decoded,
                frames_presented = self.presented,
                decoded_frames_dropped = self.decoded_frames_dropped,
                "decoder/presentation statistics"
            );
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

unsafe fn create_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(
                D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 | D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0,
            ),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11 hardware video device creation failed")?;
    Ok((
        device.context("D3D11 returned no device")?,
        context.context("D3D11 returned no immediate context")?,
    ))
}

struct PendingMetadata {
    frame_id: u64,
    decode_start_us: u64,
}

struct DecodedFrame {
    texture: ID3D11Texture2D,
    subresource: u32,
    frame_id: u64,
    decode_start_us: u64,
    decode_complete_us: u64,
}

struct HardwareDecoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    asynchronous: bool,
    _device_manager: IMFDXGIDeviceManager,
    output_info: MFT_OUTPUT_STREAM_INFO,
    frame_duration_100ns: i64,
    need_input: u32,
    have_output: u32,
    pending: VecDeque<PendingMetadata>,
    first_input_logged: bool,
}

impl HardwareDecoder {
    pub(super) unsafe fn new(device: &ID3D11Device, format: VideoFormat) -> anyhow::Result<Self> {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activations_ptr: *mut Option<IMFActivate> = ptr::null_mut();
        let mut activation_count = 0;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG(
                    MFT_ENUM_FLAG_HARDWARE.0
                        | MFT_ENUM_FLAG_ASYNCMFT.0
                        | MFT_ENUM_FLAG_SYNCMFT.0
                        | MFT_ENUM_FLAG_SORTANDFILTER.0,
                ),
                Some(&input_info),
                // Hardware decoder activation objects frequently advertise a
                // driver-specific output type. Negotiate NV12 with the active
                // transform after attaching our D3D11 device manager.
                None,
                &mut activations_ptr,
                &mut activation_count,
            )
        }
        .context("hardware H.264 decoder enumeration failed")?;
        if activation_count == 0 || activations_ptr.is_null() {
            bail!("no Media Foundation H.264 decoder is installed");
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
            .context("hardware H.264 decoder activation failed")?;
        let attributes =
            unsafe { transform.GetAttributes() }.context("decoder attributes unavailable")?;
        let asynchronous = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if asynchronous {
            unsafe { attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }
                .context("failed to unlock asynchronous decoder")?;
        }
        if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
            bail!("H.264 decoder is not D3D11-aware and cannot guarantee GPU decoding");
        }
        let _ = unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) };

        let mut reset_token = 0;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
            .context("decoder D3D manager creation failed")?;
        let manager = manager.context("Media Foundation returned no decoder D3D manager")?;
        unsafe { manager.ResetDevice(device, reset_token) }
            .context("decoder D3D manager reset failed")?;
        unsafe {
            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                Interface::as_raw(&manager) as usize,
            )
        }
        .context("failed to attach D3D manager to decoder")?;

        let input_type = unsafe { video_type(MFVideoFormat_H264, format)? };
        let output_type = unsafe { video_type(MFVideoFormat_NV12, format)? };
        unsafe { transform.SetInputType(0, &input_type, 0) }
            .context("decoder rejected H.264 input type")?;
        unsafe { transform.SetOutputType(0, &output_type, 0) }
            .context("decoder rejected GPU NV12 output type")?;
        let output_info = unsafe { transform.GetOutputStreamInfo(0) }
            .context("decoder output stream info unavailable")?;
        let provides_samples = output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;
        if !provides_samples {
            bail!("hardware decoder requires caller-allocated output surfaces");
        }
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
            _device_manager: manager,
            output_info,
            frame_duration_100ns: 10_000_000 / i64::from(format.frames_per_second.max(1)),
            need_input: 0,
            have_output: 0,
            pending: VecDeque::new(),
            first_input_logged: false,
        };
        unsafe { decoder.pump_events(false)? };
        Ok(decoder)
    }

    unsafe fn pump_events(&mut self, blocking_once: bool) -> anyhow::Result<()> {
        let Some(events) = self.events.as_ref() else {
            return Ok(());
        };
        let mut first = true;
        loop {
            let flags = if blocking_once && first {
                MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)
            } else {
                MF_EVENT_FLAG_NO_WAIT
            };
            first = false;
            match unsafe { events.GetEvent(flags) } {
                Ok(event) => match unsafe { event.GetType() } {
                    Ok(value) if value == METransformNeedInput.0 as u32 => self.need_input += 1,
                    Ok(value) if value == METransformHaveOutput.0 as u32 => self.have_output += 1,
                    Ok(_) => {}
                    Err(error) => return Err(error).context("decoder event type failed"),
                },
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(error).context("decoder event pump failed"),
            }
        }
        Ok(())
    }

    unsafe fn decode(&mut self, queued: QueuedFrame) -> anyhow::Result<Vec<DecodedFrame>> {
        if self.asynchronous {
            unsafe { self.pump_events(self.need_input == 0)? };
            if self.need_input == 0 {
                return Ok(Vec::new());
            }
        }
        if self.pending.len() >= MAX_DECODER_PENDING_FRAMES {
            bail!(
                "hardware decoder buffered more than {MAX_DECODER_PENDING_FRAMES} frames; stopping instead of accumulating latency"
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
                "first H.264 access unit submitted to hardware decoder"
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
            .context("hardware H.264 decoder rejected input")?;
        if self.asynchronous {
            self.need_input -= 1;
        }
        self.pending.push_back(PendingMetadata {
            frame_id: queued.frame.frame_id,
            decode_start_us,
        });
        let mut decoded = Vec::with_capacity(self.have_output.max(1) as usize);
        if self.asynchronous {
            unsafe { self.pump_events(false)? };
            while self.have_output > 0 {
                if let Some(frame) = unsafe { self.take_output()? } {
                    decoded.push(frame);
                }
                self.have_output -= 1;
            }
        } else {
            while let Some(frame) = unsafe { self.take_output()? } {
                decoded.push(frame);
            }
        }
        Ok(decoded)
    }

    unsafe fn take_output(&mut self) -> anyhow::Result<Option<DecodedFrame>> {
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
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
                unsafe { self.select_nv12_output_type()? };
                return unsafe { self.take_output() };
            }
            if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            return Err(error).context("hardware decoder output failed");
        }
        let sample = sample.context("hardware decoder returned no GPU sample")?;
        let buffer = unsafe { sample.GetBufferByIndex(0) }
            .context("decoded sample has no surface buffer")?;
        let dxgi: IMFDXGIBuffer = buffer
            .cast()
            .context("decoded sample is not a DXGI surface")?;
        let mut raw: *mut c_void = ptr::null_mut();
        unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }
            .context("decoded DXGI texture lookup failed")?;
        if raw.is_null() {
            bail!("decoded DXGI texture was null");
        }
        let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
        let subresource = unsafe { dxgi.GetSubresourceIndex() }
            .context("decoded texture subresource unavailable")?;
        let metadata = self
            .pending
            .pop_front()
            .context("decoder output had no input metadata")?;
        Ok(Some(DecodedFrame {
            texture,
            subresource,
            frame_id: metadata.frame_id,
            decode_start_us: metadata.decode_start_us,
            decode_complete_us: monotonic_timestamp_us(),
        }))
    }

    unsafe fn select_nv12_output_type(&self) -> anyhow::Result<()> {
        for index in 0.. {
            let media_type = match unsafe { self.transform.GetOutputAvailableType(0, index) } {
                Ok(media_type) => media_type,
                Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
                Err(error) => {
                    return Err(error).context("decoder output-type enumeration failed");
                }
            };
            if unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
                unsafe { self.transform.SetOutputType(0, &media_type, 0) }
                    .context("decoder rejected its available NV12 output type")?;
                tracing::info!("hardware decoder applied an H.264 stream format change");
                return Ok(());
            }
        }
        bail!("hardware decoder stream changed without an NV12 GPU output type")
    }
}

impl Drop for HardwareDecoder {
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
        let _ = self.output_info;
    }
}

unsafe fn video_type(
    subtype: windows::core::GUID,
    format: VideoFormat,
) -> anyhow::Result<IMFMediaType> {
    let media_type = unsafe { MFCreateMediaType() }.context("video media type creation failed")?;
    unsafe { media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video) }?;
    unsafe { media_type.SetGUID(&MF_MT_SUBTYPE, &subtype) }?;
    unsafe {
        media_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(format.width) << 32) | u64::from(format.height),
        )
    }?;
    unsafe {
        media_type.SetUINT64(
            &MF_MT_FRAME_RATE,
            (u64::from(format.frames_per_second) << 32) | 1,
        )
    }?;
    unsafe { media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1) }?;
    unsafe { media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32) }?;
    Ok(media_type)
}
