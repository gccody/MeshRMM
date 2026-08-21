use std::collections::{HashSet, VecDeque};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, bail};
use pulsermm_protocol::{
    Display, EncodedFrame, PointerButton, RemoteInput, SessionMessage, VideoFormat,
};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_F8};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, Interface, PCWSTR, w};

use super::ControlSink;

const MAX_DECODER_PENDING_FRAMES: usize = 4;

struct QueuedFrame {
    frame: EncodedFrame,
    received_at_us: u64,
}

struct Shared {
    latest: Mutex<Option<QueuedFrame>>,
    ready: Condvar,
    stopping: AtomicBool,
    running: AtomicBool,
    failure: Mutex<Option<String>>,
    replaced_frames: AtomicU64,
}

pub struct Presenter {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Presenter {
    pub fn start(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
    ) -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            ready: Condvar::new(),
            stopping: AtomicBool::new(false),
            running: AtomicBool::new(false),
            failure: Mutex::new(None),
            replaced_frames: AtomicU64::new(0),
        });
        let worker_shared = Arc::clone(&shared);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("pulsermm-decode-present".into())
            .spawn(move || {
                run_worker(
                    worker_shared,
                    format,
                    active_display,
                    displays,
                    control,
                    started_tx,
                )
            })
            .context("failed to spawn Windows decode/presentation worker")?;
        started_rx
            .recv()
            .context("Windows decode/presentation worker exited during startup")??;
        Ok(Self {
            shared,
            worker: Some(worker),
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
            self.shared.replaced_frames.fetch_add(1, Ordering::Relaxed);
        }
        self.shared.ready.notify_one();
    }

    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("Windows decode/presentation worker panicked");
        }
        tracing::info!(
            latest_frames_dropped = self.shared.replaced_frames.load(Ordering::Relaxed),
            "video presenter stopped"
        );
    }

    pub fn poll_ended(&self) -> Option<Result<(), String>> {
        if self.shared.running.load(Ordering::Acquire) {
            return None;
        }
        let failure = self
            .shared
            .failure
            .lock()
            .ok()
            .and_then(|mut failure| failure.take());
        Some(failure.map_or(Ok(()), Err))
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_worker(
    shared: Arc<Shared>,
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    started: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
) {
    let initialized = unsafe { WorkerPipeline::new(format, active_display, displays, control) };
    let mut pipeline = match initialized {
        Ok(pipeline) => {
            shared.running.store(true, Ordering::Release);
            let _ = started.send(Ok(()));
            pipeline
        }
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };

    while !shared.stopping.load(Ordering::Acquire) {
        if unsafe { pump_window_messages() } {
            break;
        }
        let queued = {
            let Ok(latest) = shared.latest.lock() else {
                break;
            };
            let Ok((mut latest, _)) =
                shared
                    .ready
                    .wait_timeout_while(latest, Duration::from_millis(4), |slot| {
                        slot.is_none() && !shared.stopping.load(Ordering::Acquire)
                    })
            else {
                break;
            };
            latest.take()
        };
        let Some(queued) = queued else {
            continue;
        };
        let frame_id = queued.frame.frame_id;
        match unsafe { pipeline.process(queued) } {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(error = %error, frame_id, "hardware decode/presentation failed");
                if let Ok(mut failure) = shared.failure.lock() {
                    *failure = Some(error.to_string());
                }
                break;
            }
        }
    }
    shared.running.store(false, Ordering::Release);
}

struct WorkerPipeline {
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
}

impl WorkerPipeline {
    unsafe fn new(
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
    ) -> anyhow::Result<Self> {
        let com = unsafe { ComRuntime::start()? };
        let mf = unsafe { MediaFoundationRuntime::start()? };
        let (device, context) = unsafe { create_device()? };
        let renderer = unsafe {
            D3d11Renderer::new(&device, &context, format, active_display, displays, control)?
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
        })
    }

    unsafe fn process(&mut self, queued: QueuedFrame) -> anyhow::Result<()> {
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
            tracing::info!(
                decode_fps = self.interval_decoded as f64 / elapsed_seconds,
                present_fps = self.interval_presented as f64 / elapsed_seconds,
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
    unsafe fn new(device: &ID3D11Device, format: VideoFormat) -> anyhow::Result<Self> {
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

struct D3d11Renderer {
    window: HWND,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
    swap_chain: IDXGISwapChain2,
}

impl D3d11Renderer {
    unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
    ) -> anyhow::Result<Self> {
        let window = unsafe { create_window(format, active_display, displays, control)? };
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .context("DXGI factory creation failed")?;
        let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: format.width,
            Height: format.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
        };
        let swap_chain: IDXGISwapChain2 =
            unsafe { factory.CreateSwapChainForHwnd(device, window, &swap_desc, None, None) }
                .context("low-latency DXGI swap chain creation failed")?
                .cast()?;
        unsafe { swap_chain.SetMaximumFrameLatency(1) }
            .context("DXGI maximum frame latency configuration failed")?;

        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: u32::from(format.frames_per_second),
                Denominator: 1,
            },
            InputWidth: format.width,
            InputHeight: format.height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: u32::from(format.frames_per_second),
                Denominator: 1,
            },
            OutputWidth: format.width,
            OutputHeight: format.height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("D3D11 presentation video processor enumeration failed")?;
        let input_support = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }?;
        let output_support =
            unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }?;
        if input_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            bail!("GPU cannot convert NV12 decoder surfaces to BGRA presentation surfaces");
        }
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("D3D11 presentation video processor creation failed")?;
        let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }
            .context("DXGI swap chain returned no back buffer")?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &back_buffer,
                &enumerator,
                &output_desc,
                Some(&mut output_view),
            )
        }
        .context("swap-chain video output view creation failed")?;
        let rect = RECT {
            left: 0,
            top: 0,
            right: format.width as i32,
            bottom: format.height as i32,
        };
        unsafe { video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect)) };
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            )
        };
        unsafe {
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&rect))
        };
        unsafe { video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect)) };
        Ok(Self {
            window,
            video_device,
            video_context,
            enumerator,
            processor,
            output_view: output_view.context("D3D11 returned no video output view")?,
            swap_chain,
        })
    }

    unsafe fn present(&self, texture: &ID3D11Texture2D, subresource: u32) -> anyhow::Result<()> {
        let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut texture_desc) };
        let array_slice = subresource.checked_div(texture_desc.MipLevels).unwrap_or(0);
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: array_slice,
                },
            },
        };
        let mut input_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &input_desc,
                Some(&mut input_view),
            )
        }
        .context("decoded texture input view creation failed")?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(input_view),
            ..Default::default()
        };
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        let _ = unsafe { ManuallyDrop::take(&mut stream.pInputSurface) };
        result.context("GPU NV12-to-BGRA presentation blit failed")?;
        // One-interval presentation avoids tearing. Flip-discard plus maximum
        // frame latency 1 prevents an additional multi-frame swap-chain queue.
        unsafe { self.swap_chain.Present(1, DXGI_PRESENT(0)) }
            .ok()
            .context("DXGI presentation failed")
    }
}

impl Drop for D3d11Renderer {
    fn drop(&mut self) {
        if !self.window.is_invalid() {
            let _ = unsafe { DestroyWindow(self.window) };
        }
    }
}

struct WindowContext {
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
    pressed_keys: HashSet<(u16, bool)>,
    pressed_buttons: HashSet<PointerButton>,
}

impl WindowContext {
    fn send(&self, message: SessionMessage) {
        (self.control)(message);
    }

    fn pointer_position(&self, window: HWND, lparam: LPARAM) -> Option<(u16, u16)> {
        self.normalized_client_position(
            window,
            signed_low_word(lparam.0),
            signed_high_word(lparam.0),
        )
    }

    fn normalized_client_position(&self, window: HWND, x: i32, y: i32) -> Option<(u16, u16)> {
        let mut rect = RECT::default();
        if unsafe { GetClientRect(window, &mut rect) }.is_err() {
            return None;
        }
        let width = rect.right.saturating_sub(rect.left).max(1);
        let height = rect.bottom.saturating_sub(rect.top).max(1);
        let x = x.clamp(0, width - 1);
        let y = y.clamp(0, height - 1);
        Some((
            (i64::from(x) * 65_535 / i64::from((width - 1).max(1))) as u16,
            (i64::from(y) * 65_535 / i64::from((height - 1).max(1))) as u16,
        ))
    }

    fn move_pointer(&self, window: HWND, lparam: LPARAM) {
        if let Some((x, y)) = self.pointer_position(window, lparam) {
            self.send(SessionMessage::Input(RemoteInput::PointerMove {
                display_id: self.active_display.id,
                x,
                y,
            }));
        }
    }

    fn move_pointer_from_screen(&self, window: HWND, lparam: LPARAM) {
        let mut point = windows::Win32::Foundation::POINT {
            x: signed_low_word(lparam.0),
            y: signed_high_word(lparam.0),
        };
        if unsafe { ScreenToClient(window, &mut point) }.as_bool()
            && let Some((x, y)) = self.normalized_client_position(window, point.x, point.y)
        {
            self.send(SessionMessage::Input(RemoteInput::PointerMove {
                display_id: self.active_display.id,
                x,
                y,
            }));
        }
    }

    fn button(&mut self, window: HWND, lparam: LPARAM, button: PointerButton, pressed: bool) {
        self.move_pointer(window, lparam);
        self.send(SessionMessage::Input(RemoteInput::PointerButton {
            display_id: self.active_display.id,
            button,
            pressed,
        }));
        if pressed {
            self.pressed_buttons.insert(button);
            let _ = unsafe { SetCapture(window) };
        } else {
            self.pressed_buttons.remove(&button);
            if self.pressed_buttons.is_empty() {
                let _ = unsafe { ReleaseCapture() };
            }
        }
    }

    fn release_input(&mut self) {
        for (scan_code, extended) in self.pressed_keys.drain().collect::<Vec<_>>() {
            self.send(SessionMessage::Input(RemoteInput::Key {
                display_id: self.active_display.id,
                scan_code,
                extended,
                pressed: false,
            }));
        }
        for button in self.pressed_buttons.drain().collect::<Vec<_>>() {
            self.send(SessionMessage::Input(RemoteInput::PointerButton {
                display_id: self.active_display.id,
                button,
                pressed: false,
            }));
        }
    }

    fn select_next_display(&self) {
        if self.displays.len() < 2 {
            return;
        }
        let current = self
            .displays
            .iter()
            .position(|display| display.id == self.active_display.id)
            .unwrap_or(0);
        let next = self.displays[(current + 1) % self.displays.len()].id;
        self.send(SessionMessage::SelectDisplay { display_id: next });
    }
}

unsafe fn window_context(window: HWND) -> Option<&'static mut WindowContext> {
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowContext;
    unsafe { pointer.as_mut() }
}

fn signed_low_word(value: isize) -> i32 {
    i32::from(value as u16 as i16)
}

fn signed_high_word(value: isize) -> i32 {
    i32::from(((value as usize >> 16) as u16) as i16)
}

unsafe fn create_window(
    format: VideoFormat,
    active_display: Display,
    displays: Vec<Display>,
    control: ControlSink,
) -> anyhow::Result<HWND> {
    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_NCCREATE {
            let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe {
                SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
            }
        }
        let context = unsafe { window_context(window) };
        match message {
            WM_MOUSEMOVE => {
                if let Some(context) = context {
                    context.move_pointer(window, lparam);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                if let Some(context) = context {
                    let button = match message {
                        WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Left,
                        WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Right,
                        WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
                        _ if ((wparam.0 >> 16) as u16) == XBUTTON1 => PointerButton::Back,
                        _ => PointerButton::Forward,
                    };
                    let pressed = matches!(
                        message,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                    );
                    context.button(window, lparam, button, pressed);
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                if let Some(context) = context {
                    context.move_pointer_from_screen(window, lparam);
                    let delta = ((wparam.0 >> 16) as u16) as i16;
                    context.send(SessionMessage::Input(RemoteInput::Wheel {
                        display_id: context.active_display.id,
                        horizontal: if message == WM_MOUSEHWHEEL { delta } else { 0 },
                        vertical: if message == WM_MOUSEWHEEL { delta } else { 0 },
                    }));
                }
                LRESULT(0)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                if let Some(context) = context {
                    let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
                    if wparam.0 as u16 == VK_F8.0 && pressed {
                        if lparam.0 & (1 << 30) == 0 {
                            context.select_next_display();
                        }
                        return LRESULT(0);
                    }
                    if wparam.0 as u16 == VK_F8.0 {
                        return LRESULT(0);
                    }
                    let scan_code = ((lparam.0 >> 16) & 0xff) as u16;
                    let extended = lparam.0 & (1 << 24) != 0;
                    if scan_code != 0 {
                        context.send(SessionMessage::Input(RemoteInput::Key {
                            display_id: context.active_display.id,
                            scan_code,
                            extended,
                            pressed,
                        }));
                        if pressed {
                            context.pressed_keys.insert((scan_code, extended));
                        } else {
                            context.pressed_keys.remove(&(scan_code, extended));
                        }
                    }
                }
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                if let Some(context) = context {
                    context.release_input();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if let Some(context) = context {
                    context.release_input();
                }
                let _ = unsafe { DestroyWindow(window) };
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            WM_NCDESTROY => {
                let pointer =
                    unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowContext;
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                if !pointer.is_null() {
                    drop(unsafe { Box::from_raw(pointer) });
                }
                unsafe { DefWindowProcW(window, message, wparam, lparam) }
            }
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    let module =
        unsafe { GetModuleHandleW(None) }.context("application module handle unavailable")?;
    let instance = HINSTANCE(module.0);
    let class = w!("PulseRmmRemoteDesktopWindow");
    let window_class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        // A second session in the same process may find the class registered.
        let error = windows::core::Error::from_thread();
        if error.code() != windows::core::HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(error).context("remote desktop window class registration failed");
        }
    }
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: format.width as i32,
        bottom: format.height as i32,
    };
    unsafe { AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false) }
        .context("remote window bounds calculation failed")?;
    let title = HSTRING::from(format!(
        "PulseRMM Remote Desktop — {} ({}) — F8 switches display",
        active_display.name,
        if active_display.primary {
            "primary"
        } else {
            "secondary"
        }
    ));
    let context = Box::new(WindowContext {
        active_display,
        displays,
        control,
        pressed_keys: HashSet::new(),
        pressed_buttons: HashSet::new(),
    });
    let context = Box::into_raw(context);
    let window = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance),
            Some(context.cast()),
        )
    };
    let window = match window {
        Ok(window) => window,
        Err(error) => {
            drop(unsafe { Box::from_raw(context) });
            return Err(error).context("native remote desktop window creation failed");
        }
    };
    let _ = unsafe { ShowWindow(window, SW_SHOW) };
    Ok(window)
}

unsafe fn pump_window_messages() -> bool {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        if message.message == WM_QUIT {
            return true;
        }
        let _ = unsafe { TranslateMessage(&message) };
        unsafe { DispatchMessageW(&message) };
    }
    false
}

pub fn monotonic_timestamp_us() -> u64 {
    unsafe {
        let mut counter = 0_i64;
        let mut frequency = 0_i64;
        if QueryPerformanceCounter(&mut counter).is_err()
            || QueryPerformanceFrequency(&mut frequency).is_err()
            || counter < 0
            || frequency <= 0
        {
            return 0;
        }
        (counter as u64).saturating_mul(1_000_000) / frequency as u64
    }
}
