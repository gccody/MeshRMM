use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr;

use thiserror::Error;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, Result as WindowsResult};

use crate::converter::SURFACE_COUNT;
use crate::{EncodedAccessUnit, VideoCodec, VideoPixelFormat};

impl VideoCodec {
    fn media_foundation_subtype(self) -> windows::core::GUID {
        match self {
            Self::H264 => MFVideoFormat_H264,
            Self::H265 => MFVideoFormat_HEVC,
        }
    }
}

impl VideoPixelFormat {
    fn media_foundation_subtype(self) -> windows::core::GUID {
        match self {
            Self::Yuv420 => MFVideoFormat_NV12,
            Self::Yuv444 => MFVideoFormat_AYUV,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Media Foundation startup failed: {0}")]
    Startup(#[source] windows::core::Error),
    #[error("no hardware Media Foundation {codec} encoder accepts {pixel_format}")]
    HardwareEncoderUnavailable {
        codec: VideoCodec,
        pixel_format: VideoPixelFormat,
    },
    #[error("Media Foundation encoder configuration failed: {0}")]
    Configuration(#[source] windows::core::Error),
    #[error("Media Foundation encoder input failed: {0}")]
    Input(#[source] windows::core::Error),
    #[error("Media Foundation encoder output failed: {0}")]
    Output(#[source] windows::core::Error),
    #[error("Media Foundation returned an output event without a sample")]
    MissingOutputSample,
    #[error("Media Foundation returned encoded output without matching input metadata")]
    MissingInputMetadata,
    #[error("encoded video output exceeded addressable memory")]
    EncodedOutputTooLarge,
    #[error("hardware encoder does not support runtime {0}")]
    RuntimeControlUnavailable(&'static str),
    #[error("performance counter is unavailable")]
    PerformanceCounter,
}

pub trait VideoEncoder {
    /// Drain completed output and refresh the transform's input demand.
    fn poll(&mut self) -> Result<Vec<EncodedAccessUnit>, Error>;
    fn wants_input(&self) -> bool;
    fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        capture_timestamp_us: u64,
    ) -> Result<Vec<EncodedAccessUnit>, Error>;
    fn request_keyframe(&self) -> Result<(), Error>;
    fn set_bitrate(&self, bits_per_second: u32) -> Result<(), Error>;
}

struct MediaFoundationRuntime;

impl MediaFoundationRuntime {
    fn start() -> Result<Self, Error> {
        // Safety: MFStartup/MFShutdown are balanced by this RAII guard on the
        // capture worker thread.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(Error::Startup)? };
        Ok(Self)
    }
}

impl Drop for MediaFoundationRuntime {
    fn drop(&mut self) {
        // Safety: balances the successful MFStartup owned by this guard.
        if let Err(error) = unsafe { MFShutdown() } {
            tracing::warn!(error = %error, "Media Foundation shutdown failed");
        }
    }
}

pub struct MediaFoundationVideoEncoder {
    _runtime: MediaFoundationRuntime,
    transform: IMFTransform,
    event_generator: IMFMediaEventGenerator,
    codec_api: ICodecAPI,
    output_info: MFT_OUTPUT_STREAM_INFO,
    device_manager: IMFDXGIDeviceManager,
    frame_duration_100ns: i64,
    need_input: u32,
    have_output: u32,
    sequence_header: Option<Vec<u8>>,
    sequence_header_sent: bool,
    pending_capture_timestamps: VecDeque<u64>,
    codec: VideoCodec,
}

impl MediaFoundationVideoEncoder {
    pub fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        codec: VideoCodec,
        pixel_format: VideoPixelFormat,
    ) -> Result<Self, Error> {
        let runtime = MediaFoundationRuntime::start()?;
        // Safety: MFT enumeration returns a COM-allocated array which is freed
        // after every returned activation object has been cloned/dropped.
        unsafe {
            let input_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: pixel_format.media_foundation_subtype(),
            };
            let output_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: codec.media_foundation_subtype(),
            };
            let mut activations_ptr: *mut Option<IMFActivate> = ptr::null_mut();
            let mut activation_count = 0_u32;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                // Hardware MFTs are their own enumeration category and are
                // always asynchronous. Including ASYNCMFT here would also
                // admit software asynchronous encoders.
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
                Some(&input_info),
                Some(&output_info),
                &mut activations_ptr,
                &mut activation_count,
            )
            .map_err(Error::Configuration)?;
            if activation_count == 0 || activations_ptr.is_null() {
                return Err(Error::HardwareEncoderUnavailable {
                    codec,
                    pixel_format,
                });
            }
            let activations =
                std::slice::from_raw_parts_mut(activations_ptr, activation_count as usize);
            let activation = activations.iter().find_map(Clone::clone);
            for item in activations.iter_mut() {
                let _ = item.take();
            }
            CoTaskMemFree(Some(activations_ptr.cast()));
            let activation = activation.ok_or(Error::HardwareEncoderUnavailable {
                codec,
                pixel_format,
            })?;
            let transform: IMFTransform =
                activation.ActivateObject().map_err(Error::Configuration)?;
            let attributes = transform.GetAttributes().map_err(Error::Configuration)?;
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(Error::Configuration)?;
            }
            let event_generator: IMFMediaEventGenerator =
                transform.cast().map_err(Error::Configuration)?;
            let codec_api: ICodecAPI = transform.cast().map_err(Error::Configuration)?;

            let mut reset_token = 0_u32;
            let mut device_manager = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)
                .map_err(Error::Configuration)?;
            let device_manager = device_manager.ok_or(Error::HardwareEncoderUnavailable {
                codec,
                pixel_format,
            })?;
            device_manager
                .ResetDevice(device, reset_token)
                .map_err(Error::Configuration)?;
            transform
                .ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    Interface::as_raw(&device_manager) as usize,
                )
                .map_err(Error::Configuration)?;

            configure_codec(&codec_api, bitrate_bits_per_second, frames_per_second)?;
            let output_type = make_video_type(
                codec.media_foundation_subtype(),
                width,
                height,
                frames_per_second,
                Some(bitrate_bits_per_second),
            )?;
            match (codec, pixel_format) {
                (VideoCodec::H264, VideoPixelFormat::Yuv420) => output_type
                    .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                    .map_err(Error::Configuration)?,
                (VideoCodec::H264, VideoPixelFormat::Yuv444) => output_type
                    .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_444.0 as u32)
                    .map_err(Error::Configuration)?,
                (VideoCodec::H265, VideoPixelFormat::Yuv420) => output_type
                    .SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)
                    .map_err(Error::Configuration)?,
                (VideoCodec::H265, VideoPixelFormat::Yuv444) => output_type
                    .SetUINT32(&MF_MT_VIDEO_PROFILE, eAVEncH265VProfile_Main_444_8.0 as u32)
                    .map_err(Error::Configuration)?,
            }
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(Error::Configuration)?;
            let input_type = make_video_type(
                pixel_format.media_foundation_subtype(),
                width,
                height,
                frames_per_second,
                None,
            )?;
            transform
                .SetInputType(0, &input_type, 0)
                .map_err(Error::Configuration)?;
            let output_info = transform
                .GetOutputStreamInfo(0)
                .map_err(Error::Configuration)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(Error::Configuration)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(Error::Configuration)?;
            let sequence_header = read_sequence_header(&transform);
            let mut encoder = Self {
                _runtime: runtime,
                transform,
                event_generator,
                codec_api,
                output_info,
                device_manager,
                frame_duration_100ns: 10_000_000_i64 / i64::from(frames_per_second.max(1)),
                need_input: 0,
                have_output: 0,
                sequence_header,
                sequence_header_sent: false,
                pending_capture_timestamps: VecDeque::with_capacity(SURFACE_COUNT),
                codec,
            };
            encoder.pump_events(false)?;
            Ok(encoder)
        }
    }

    fn pump_events(&mut self, blocking_once: bool) -> Result<(), Error> {
        let mut first = true;
        loop {
            let flags = if blocking_once && first {
                MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)
            } else {
                MF_EVENT_FLAG_NO_WAIT
            };
            first = false;
            // Safety: event generator is owned by this encoder and called only
            // from the capture worker thread.
            match unsafe { self.event_generator.GetEvent(flags) } {
                Ok(event) => match unsafe { event.GetType() } {
                    Ok(value) if value == METransformNeedInput.0 as u32 => self.need_input += 1,
                    Ok(value) if value == METransformHaveOutput.0 as u32 => self.have_output += 1,
                    Ok(_) => {}
                    Err(error) => return Err(Error::Output(error)),
                },
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => break,
                Err(error) => return Err(Error::Output(error)),
            }
        }
        Ok(())
    }

    fn take_output(&mut self) -> Result<EncodedAccessUnit, Error> {
        // Safety: samples and buffers are COM-owned for the duration of this
        // call. Locked buffer memory is copied before Unlock.
        unsafe {
            let provides_samples = self.output_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            let provided_sample = if provides_samples {
                None
            } else {
                let sample = MFCreateSample().map_err(Error::Output)?;
                let capacity = self.output_info.cbSize.max(1024 * 1024);
                let buffer = MFCreateMemoryBuffer(capacity).map_err(Error::Output)?;
                sample.AddBuffer(&buffer).map_err(Error::Output)?;
                Some(sample)
            };
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided_sample),
                ..Default::default()
            };
            let mut status = 0_u32;
            let process_result =
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
            let sample = ManuallyDrop::take(&mut output.pSample);
            let _ = ManuallyDrop::take(&mut output.pEvents);
            process_result.map_err(Error::Output)?;
            let sample = sample.ok_or(Error::MissingOutputSample)?;
            let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer().map_err(Error::Output)?;
            let mut data_ptr = ptr::null_mut();
            let mut current_length = 0_u32;
            buffer
                .Lock(&mut data_ptr, None, Some(&mut current_length))
                .map_err(Error::Output)?;
            let data = if data_ptr.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(data_ptr, current_length as usize).to_vec()
            };
            buffer.Unlock().map_err(Error::Output)?;
            // Several hardware MFTs omit CleanPoint on forced IDRs. Inspecting
            // the access unit prevents a valid recovery frame from being
            // mislabeled and discarded by a decoder waiting for an IDR.
            let keyframe = clean_point || contains_idr(self.codec, &data);
            let mut codec_config = None;
            // Some hardware MFTs omit MFSampleExtension_CleanPoint on their
            // first IDR. The decoder still needs SPS/PPS before that first
            // access unit, so attach the sequence header to the first output
            // regardless of the optional clean-point annotation.
            if !self.sequence_header_sent {
                if self.sequence_header.is_none() {
                    self.sequence_header = read_sequence_header(&self.transform);
                }
                codec_config = self.sequence_header.clone();
                self.sequence_header_sent = codec_config.is_some();
                tracing::info!(
                    encoded_bytes = data.len(),
                    codec_config_bytes = codec_config.as_ref().map_or(0, Vec::len),
                    keyframe,
                    annex_b = data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]),
                    codec = self.codec.name(),
                    "first access unit produced by hardware encoder"
                );
            }
            Ok(EncodedAccessUnit {
                capture_timestamp_us: self
                    .pending_capture_timestamps
                    .pop_front()
                    .ok_or(Error::MissingInputMetadata)?,
                encode_complete_timestamp_us: performance_counter_us()?,
                keyframe,
                codec_config,
                data,
            })
        }
    }
}

impl VideoEncoder for MediaFoundationVideoEncoder {
    fn poll(&mut self) -> Result<Vec<EncodedAccessUnit>, Error> {
        self.pump_events(false)?;
        self.take_available_outputs()
    }

    fn wants_input(&self) -> bool {
        self.need_input > 0 && self.pending_capture_timestamps.len() < SURFACE_COUNT
    }

    fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        capture_timestamp_us: u64,
    ) -> Result<Vec<EncodedAccessUnit>, Error> {
        debug_assert!(self.wants_input());
        // Safety: the DXGI surface buffer holds a COM reference to a texture in
        // the converter pool until the transform releases the input sample.
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
                .map_err(Error::Input)?;
            let sample = MFCreateSample().map_err(Error::Input)?;
            sample.AddBuffer(&buffer).map_err(Error::Input)?;
            sample
                .SetSampleTime((capture_timestamp_us.saturating_mul(10)) as i64)
                .map_err(Error::Input)?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(Error::Input)?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(Error::Input)?;
        }
        self.need_input -= 1;
        self.pending_capture_timestamps
            .push_back(capture_timestamp_us);
        self.pump_events(false)?;
        self.take_available_outputs()
    }

    fn request_keyframe(&self) -> Result<(), Error> {
        set_optional_codec_value(
            &self.codec_api,
            &CODECAPI_AVEncVideoForceKeyFrame,
            true.into(),
            "force keyframe",
        )
    }

    fn set_bitrate(&self, bits_per_second: u32) -> Result<(), Error> {
        set_required_runtime_codec_value(
            &self.codec_api,
            &CODECAPI_AVEncCommonMeanBitRate,
            bits_per_second.into(),
            "dynamic bitrate",
        )
    }
}

impl MediaFoundationVideoEncoder {
    fn take_available_outputs(&mut self) -> Result<Vec<EncodedAccessUnit>, Error> {
        let mut outputs = Vec::with_capacity(self.have_output as usize);
        while self.have_output > 0 {
            outputs.push(self.take_output()?);
            self.have_output -= 1;
        }
        Ok(outputs)
    }
}

impl Drop for MediaFoundationVideoEncoder {
    fn drop(&mut self) {
        // Safety: these messages terminate the transform owned by this object.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
        let _keep_manager_alive = &self.device_manager;
    }
}

fn make_video_type(
    subtype: windows::core::GUID,
    width: u32,
    height: u32,
    frames_per_second: u32,
    bitrate: Option<u32>,
) -> Result<IMFMediaType, Error> {
    // Safety: attribute values follow Media Foundation's packed UINT64 format.
    unsafe {
        let media_type = MFCreateMediaType().map_err(Error::Configuration)?;
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(Error::Configuration)?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &subtype)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            )
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(frames_per_second) << 32) | 1)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .map_err(Error::Configuration)?;
        media_type
            .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .map_err(Error::Configuration)?;
        if let Some(bitrate) = bitrate {
            media_type
                .SetUINT32(&MF_MT_AVG_BITRATE, bitrate)
                .map_err(Error::Configuration)?;
        }
        Ok(media_type)
    }
}

fn configure_codec(
    codec_api: &ICodecAPI,
    bitrate_bits_per_second: u32,
    frames_per_second: u32,
) -> Result<(), Error> {
    // Sunshine uses a single-frame VBV/HRD budget for its ultra-low-latency
    // path. Preserve the historical 16 KiB floor so low-bandwidth presets do
    // not starve detailed text and keyframes below a useful hardware budget.
    let single_frame_buffer_bytes = bitrate_bits_per_second
        .div_ceil(8)
        .div_ceil(frames_per_second.max(1))
        .max(16 * 1024);
    let settings = [
        (&CODECAPI_AVEncCommonLowLatency, VARIANT::from(true)),
        (&CODECAPI_AVEncCommonRealTime, VARIANT::from(true)),
        (
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        ),
        (
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(bitrate_bits_per_second),
        ),
        (&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0_u32)),
        (
            &CODECAPI_AVEncMPVGOPSize,
            // Recovery requests are the fast path. A two-second fallback avoids
            // the visible bitrate spike caused by forcing a full IDR each second.
            VARIANT::from(frames_per_second.saturating_mul(2)),
        ),
    ];
    for (key, value) in settings {
        // IsModifiable describes live changes, not whether an initial value can
        // be supplied. These are all configured before streaming begins.
        unsafe {
            if codec_api.IsSupported(key).is_ok() {
                codec_api
                    .SetValue(key, &value)
                    .map_err(Error::Configuration)?;
            } else {
                tracing::warn!(setting = ?key, "hardware encoder does not support requested low-latency setting");
            }
        }
    }
    // Sunshine defaults to NVENC P1 with ultra-low-latency tuning and Parsec
    // prioritizes sub-frame encode latency. Media Foundation's portable
    // equivalent retains the fast path while allowing modestly better motion
    // estimation at a fixed bitrate.
    set_optional_initial_codec_value(
        codec_api,
        &CODECAPI_AVEncCommonQualityVsSpeed,
        VARIANT::from(35_u32),
        "quality versus speed",
    )?;
    // Hint that this is mixed desktop/UI content rather than a full-screen
    // camera or movie. Supporting hardware can choose its screen-content path.
    set_optional_initial_codec_value(
        codec_api,
        &CODECAPI_VideoEncoderDisplayContentType,
        VARIANT::from(0_u32),
        "desktop content type",
    )?;
    // Bound destructive quantization during complex desktop updates. Drivers
    // may ignore this optional hint to preserve their CBR guarantees.
    set_optional_initial_codec_value(
        codec_api,
        &CODECAPI_AVEncVideoMaxQP,
        VARIANT::from(36_u32),
        "maximum quantizer",
    )?;
    set_optional_initial_codec_value(
        codec_api,
        &CODECAPI_AVEncCommonBufferSize,
        VARIANT::from(single_frame_buffer_bytes),
        "single-frame encoder buffer",
    )?;
    Ok(())
}

fn contains_idr(codec: VideoCodec, data: &[u8]) -> bool {
    let mut index = 0;
    while index + 4 <= data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        let nal = index + start_len;
        if nal < data.len() {
            let nal_type = match codec {
                VideoCodec::H264 => data[nal] & 0x1f,
                VideoCodec::H265 => (data[nal] >> 1) & 0x3f,
            };
            if matches!(
                (codec, nal_type),
                (VideoCodec::H264, 5) | (VideoCodec::H265, 16..=21)
            ) {
                return true;
            }
        }
        index = nal.saturating_add(1);
    }
    false
}

fn set_required_runtime_codec_value(
    codec_api: &ICodecAPI,
    key: &windows::core::GUID,
    value: VARIANT,
    setting: &'static str,
) -> Result<(), Error> {
    // Report unsupported runtime controls to the capture backend. It keeps the
    // current encoder alive and disables further live updates for this start;
    // a later legitimate stream restart applies the requested rate through the
    // static output media type instead.
    unsafe {
        if codec_api.IsSupported(key).is_err() || codec_api.IsModifiable(key).is_err() {
            return Err(Error::RuntimeControlUnavailable(setting));
        }
        codec_api
            .SetValue(key, &value)
            .map_err(Error::Configuration)
    }
}

fn set_optional_initial_codec_value(
    codec_api: &ICodecAPI,
    key: &windows::core::GUID,
    value: VARIANT,
    setting: &'static str,
) -> Result<(), Error> {
    // IsModifiable only reports whether a value can change after the codec is
    // running. Startup-only properties must still be attempted when supported.
    unsafe {
        if codec_api.IsSupported(key).is_err() {
            tracing::warn!(setting, "hardware encoder does not support initial control");
            return Ok(());
        }
        if let Err(error) = codec_api.SetValue(key, &value) {
            tracing::warn!(setting, %error, "hardware encoder rejected optional initial control");
        }
        Ok(())
    }
}

fn set_optional_codec_value(
    codec_api: &ICodecAPI,
    key: &windows::core::GUID,
    value: VARIANT,
    setting: &'static str,
) -> Result<(), Error> {
    // Safety: the VARIANT remains alive for the duration of SetValue. Runtime
    // controls are optional because some hardware MFTs advertise ICodecAPI but
    // return E_NOTIMPL for individual settings. Losing a keyframe request is
    // preferable to terminating the entire remote session.
    unsafe {
        if codec_api.IsSupported(key).is_err() || codec_api.IsModifiable(key).is_err() {
            tracing::warn!(setting, "hardware encoder does not support runtime control");
            return Ok(());
        }
        if let Err(error) = codec_api.SetValue(key, &value) {
            tracing::warn!(setting, %error, "hardware encoder rejected optional runtime control");
        }
        Ok(())
    }
}

fn read_sequence_header(transform: &IMFTransform) -> Option<Vec<u8>> {
    // Safety: buffer is sized using the attribute's reported blob size.
    unsafe {
        let media_type = transform.GetOutputCurrentType(0).ok()?;
        let size = media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut bytes = vec![0_u8; size as usize];
        let mut actual = 0_u32;
        media_type
            .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut bytes, Some(&mut actual))
            .ok()?;
        bytes.truncate(actual as usize);
        Some(bytes)
    }
}

pub(crate) fn performance_counter_us() -> Result<u64, Error> {
    // Safety: Windows writes one signed 64-bit counter/frequency to each pointer.
    unsafe {
        let mut counter = 0_i64;
        let mut frequency = 0_i64;
        QueryPerformanceCounter(&mut counter).map_err(|_| Error::PerformanceCounter)?;
        QueryPerformanceFrequency(&mut frequency).map_err(|_| Error::PerformanceCounter)?;
        if counter < 0 || frequency <= 0 {
            return Err(Error::PerformanceCounter);
        }
        Ok((counter as u64).saturating_mul(1_000_000) / frequency as u64)
    }
}

#[allow(dead_code)]
fn _windows_result_type(_: WindowsResult<()>) {}

#[cfg(test)]
mod tests {
    use super::contains_idr;
    use crate::VideoCodec;

    #[test]
    fn detects_idr_in_annex_b_access_units() {
        assert!(contains_idr(
            VideoCodec::H264,
            &[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]
        ));
        assert!(!contains_idr(VideoCodec::H264, &[0, 0, 1, 0x41, 1, 2, 3]));
        assert!(contains_idr(VideoCodec::H265, &[0, 0, 0, 1, 19 << 1, 1]));
    }
}
