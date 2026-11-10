//! Video without a GPU encoder: in Safe Mode, where Windows loads no GPU
//! vendor driver, and as the final fallback on machines whose GPU cannot
//! encode or convert video (or that have no GPU). There may be no hardware
//! encoder and no D3D11 video processor, so frames are copied to the CPU,
//! converted to NV12, and encoded with Microsoft's software H.264 encoder.

use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CLEANBOOT};
use windows::core::{GUID, IUnknown_Vtbl, Interface};

use crate::converter::{self, SURFACE_COUNT};
use crate::encoder::{
    self, Error, MediaFoundationRuntime, OnPlatform, OutputFramer, Transform, VideoEncoder,
    make_video_type, set_optional_initial_codec_value, set_required_runtime_codec_value,
};
use crate::{EncodedAccessUnit, VideoCodec, VideoPixelFormat};

/// Safe Mode has no GPU drivers and disables Media Foundation's hardware
/// encoders, so it skips straight to software encoding.
fn safe_mode() -> bool {
    unsafe { GetSystemMetrics(SM_CLEANBOOT) != 0 }
}

/// Keeps the CPU encoder from starving the rest of the machine.
pub(crate) const MAX_FRAMES_PER_SECOND: u32 = 30;

/// The private data of a [`StagingPool`] texture that numbers the copy it
/// holds: a `u64` from [`COPIES`].
const STAGING_COPY: GUID = GUID::from_u128(0x6f3b1c8e_4a52_4d0e_9b7a_2c61d5e8f904);

/// Copy numbers, unique in the process, so a number names one copy's pixels.
static COPIES: AtomicU64 = AtomicU64::new(1);

/// The copy number a [`StagingPool`] texture holds, if it is one.
fn staging_copy(texture: &ID3D11Texture2D) -> Option<u64> {
    let mut copy = 0_u64;
    let mut size = size_of::<u64>() as u32;
    unsafe {
        texture.GetPrivateData(
            &STAGING_COPY,
            &mut size,
            Some(ptr::from_mut(&mut copy).cast()),
        )
    }
    .ok()
    .filter(|()| size == size_of::<u64>() as u32)
    .map(|()| copy)
}

/// CPU-readable copies of captured frames. Like the GPU converter's pool, a
/// copy stays valid until two more frames are copied, so a capture loop can
/// encode the last frame again. Each copy is numbered, so the encoder can
/// tell the same frame encoded again and skip converting it again.
pub(crate) struct StagingPool {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    textures: Vec<ID3D11Texture2D>,
    next: usize,
}

impl StagingPool {
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            device: device.clone(),
            context: context.clone(),
            width,
            height,
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            textures: Vec::new(),
            next: 0,
        }
    }

    pub(crate) fn copy(
        &mut self,
        source: &ID3D11Texture2D,
    ) -> Result<&ID3D11Texture2D, converter::Error> {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        if self.textures.is_empty() || source_desc.Format != self.format {
            self.allocate(source_desc.Format)?;
        }
        let target = &self.textures[self.next];
        self.next = (self.next + 1) % self.textures.len();
        // Capture textures can be a row or column larger than the even
        // dimensions the encoder needs.
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: self.width,
            bottom: self.height,
            back: 1,
        };
        let copy = COPIES.fetch_add(1, Ordering::Relaxed);
        unsafe {
            self.context
                .CopySubresourceRegion(target, 0, 0, 0, 0, source, 0, Some(&region));
            target
                .SetPrivateData(
                    &STAGING_COPY,
                    size_of::<u64>() as u32,
                    Some(ptr::from_ref(&copy).cast()),
                )
                .map_err(converter::Error::Processor)?;
        }
        Ok(target)
    }

    fn allocate(&mut self, format: DXGI_FORMAT) -> Result<(), converter::Error> {
        if format != DXGI_FORMAT_B8G8R8A8_UNORM && format != DXGI_FORMAT_R8G8B8A8_UNORM {
            return Err(converter::Error::UnsupportedCaptureFormat(format.0));
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut textures = Vec::with_capacity(SURFACE_COUNT);
        for _ in 0..SURFACE_COUNT {
            let mut texture = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(converter::Error::Processor)?;
            textures.push(texture.ok_or(converter::Error::MissingTexture)?);
        }
        self.textures = textures;
        self.format = format;
        self.next = 0;
        Ok(())
    }
}

/// Microsoft's synchronous H.264 encoder, fed from [`StagingPool`] copies,
/// on COM and, outside Safe Mode, Media Foundation. COM, entered first, is
/// left last.
pub(crate) type SoftwareH264Encoder =
    OnPlatform<SoftwareEncoder, (Option<MediaFoundationRuntime>, ComApartment)>;

/// Fields drop in declaration order: the transform is ended first.
pub(crate) struct SoftwareEncoder {
    transform: Transform,
    codec_api: ICodecAPI,
    output_info: MFT_OUTPUT_STREAM_INFO,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    grayscale: bool,
    frame_duration_100ns: i64,
    output: OutputFramer,
    inputs: InputSamples,
}

/// The most input samples kept. Should the transform hold this many, the
/// pool gives them up to it in turn instead of growing without bound.
const MAX_INPUT_SAMPLES: usize = SURFACE_COUNT + 1;

/// NV12 input samples, each with one buffer of a whole frame, written again
/// for later frames once the transform has let go of them. The synchronous
/// encoder may hold a sample after `ProcessInput` returns, and a held sample
/// is never written: [`InputSample::free`] tells which are held.
struct InputSamples {
    samples: Vec<InputSample>,
    length: u32,
    /// The sample written last and the staging copy it holds.
    newest: Option<(usize, u64)>,
    /// The sample given up next when all of them are held.
    evict: usize,
}

struct InputSample {
    sample: IMFSample,
    buffer: IMFMediaBuffer,
}

impl InputSample {
    fn new(length: u32) -> Result<Self, Error> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(length).map_err(Error::Input)?;
            buffer.SetCurrentLength(length).map_err(Error::Input)?;
            let sample = MFCreateSample().map_err(Error::Input)?;
            sample.AddBuffer(&buffer).map_err(Error::Input)?;
            Ok(Self { sample, buffer })
        }
    }

    /// Whether only this pool refers to the sample, and to its buffer only
    /// it and the sample. Media Foundation's samples and buffers count their
    /// references exactly; a count too high only costs a new sample.
    fn free(&self) -> bool {
        references(&self.sample) == 1 && references(&self.buffer) == 2
    }
}

/// The references to `object`, the caller's included.
fn references(object: &impl Interface) -> u32 {
    let raw = object.as_raw();
    // Safety: every COM interface's table starts with IUnknown's. The
    // reference added is released at once, and `object` keeps the object
    // alive meanwhile. Release returns the count left.
    unsafe {
        let table = &**raw.cast::<*const IUnknown_Vtbl>();
        (table.AddRef)(raw);
        (table.Release)(raw)
    }
}

impl InputSamples {
    fn new(length: u32) -> Self {
        Self {
            samples: Vec::with_capacity(MAX_INPUT_SAMPLES),
            length,
            newest: None,
            evict: 0,
        }
    }

    /// The sample that already holds staging copy `copy`, if the transform
    /// has let go of it.
    fn holding(&self, copy: Option<u64>) -> Option<usize> {
        let (index, newest) = self.newest?;
        (copy == Some(newest) && self.samples[index].free()).then_some(index)
    }

    /// A sample the transform does not hold, to write a frame into.
    fn writable(&mut self) -> Result<usize, Error> {
        self.newest = None;
        if let Some(index) = self.samples.iter().position(InputSample::free) {
            return Ok(index);
        }
        let sample = InputSample::new(self.length)?;
        if self.samples.len() < MAX_INPUT_SAMPLES {
            self.samples.push(sample);
            return Ok(self.samples.len() - 1);
        }
        // The transform keeps its own references to the one given up.
        let index = self.evict;
        self.evict = (index + 1) % MAX_INPUT_SAMPLES;
        self.samples[index] = sample;
        Ok(index)
    }
}

/// COM on the capture thread for the encoder's lifetime. Media Foundation
/// does not initialize it, and Safe Mode skips Media Foundation.
pub(crate) struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self, Error> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // A thread already in another apartment can still create the
        // in-process encoder.
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        result.ok().map_err(Error::Configuration)?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

impl SoftwareH264Encoder {
    pub(crate) fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        grayscale: bool,
    ) -> Result<Self, Error> {
        let com = ComApartment::enter()?;
        // Safe Mode disables the Media Foundation platform: MFStartup fails,
        // and with it everything that needs its work queues. The software
        // encoder transform and the media types, samples and buffers that
        // feed it are plain COM objects that work without it.
        let runtime = match MediaFoundationRuntime::start() {
            Ok(runtime) => Some(runtime),
            Err(Error::Startup(error)) if error.code() == MF_E_DISABLED_IN_SAFEMODE => None,
            Err(error) => return Err(error),
        };
        let encoder = SoftwareEncoder::new(
            device,
            width,
            height,
            frames_per_second,
            bitrate_bits_per_second,
            grayscale,
        )?;
        Ok(OnPlatform::hold((runtime, com), encoder))
    }
}

impl SoftwareEncoder {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        grayscale: bool,
    ) -> Result<Self, Error> {
        let context = unsafe { device.GetImmediateContext() }.map_err(Error::Configuration)?;
        // Safety: the transform and its interfaces stay on this capture thread.
        unsafe {
            let transform = Transform::new(
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|_| Error::SoftwareEncoderUnavailable)?,
            );
            let codec_api: ICodecAPI = transform.cast().map_err(Error::Configuration)?;

            // Rate control applies only when set before the output type.
            for (key, value, setting) in [
                (
                    &CODECAPI_AVLowLatencyMode,
                    VARIANT::from(true),
                    "low-latency mode",
                ),
                (
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    "constant bitrate",
                ),
                (
                    &CODECAPI_AVEncCommonMeanBitRate,
                    VARIANT::from(bitrate_bits_per_second),
                    "bitrate",
                ),
                (
                    &CODECAPI_AVEncMPVDefaultBPictureCount,
                    VARIANT::from(0_u32),
                    "no B-frames",
                ),
                // Favor speed: this encoder shares the CPU with everything else.
                (
                    &CODECAPI_AVEncCommonQualityVsSpeed,
                    VARIANT::from(0_u32),
                    "quality versus speed",
                ),
                // Zero means the encoder's default GOP, an IDR every second.
                (
                    &CODECAPI_AVEncMPVGOPSize,
                    VARIANT::from(u32::MAX),
                    "request-driven keyframes",
                ),
            ] {
                set_optional_initial_codec_value(&codec_api, key, value, setting)?;
            }
            let output_type = make_video_type(
                MFVideoFormat_H264,
                width,
                height,
                frames_per_second,
                Some(bitrate_bits_per_second),
            )?;
            output_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)
                .map_err(Error::Configuration)?;
            transform
                .SetOutputType(0, &output_type, 0)
                .map_err(Error::Configuration)?;
            let input_type =
                make_video_type(MFVideoFormat_NV12, width, height, frames_per_second, None)?;
            input_type
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
                .map_err(Error::Configuration)?;
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
            tracing::info!(
                width,
                height,
                frames_per_second,
                bitrate_bits_per_second,
                safe_mode = safe_mode(),
                "encoding video in software"
            );
            let output = OutputFramer::new(&transform, VideoCodec::H264);
            Ok(Self {
                transform,
                codec_api,
                output_info,
                context,
                width,
                height,
                grayscale,
                frame_duration_100ns: 10_000_000_i64 / i64::from(frames_per_second.max(1)),
                output,
                inputs: InputSamples::new(width * height * 3 / 2),
            })
        }
    }

    /// The input sample for staging copy `staging`: the sample that already
    /// holds it, when the same frame is encoded again, or a free one it is
    /// converted into.
    fn input_sample(&mut self, staging: &ID3D11Texture2D) -> Result<IMFSample, Error> {
        let copy = staging_copy(staging);
        let index = match self.inputs.holding(copy) {
            Some(index) => index,
            None => {
                let index = self.inputs.writable()?;
                self.write_nv12(staging, &self.inputs.samples[index].buffer)?;
                self.inputs.newest = copy.map(|copy| (index, copy));
                index
            }
        };
        Ok(self.inputs.samples[index].sample.clone())
    }

    /// Converts a staging copy into `buffer`, which holds a whole NV12 frame.
    fn write_nv12(&self, staging: &ID3D11Texture2D, buffer: &IMFMediaBuffer) -> Result<(), Error> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { staging.GetDesc(&mut desc) };
        let rgba = desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM;
        let (width, height) = (self.width as usize, self.height as usize);
        let length = width * height * 3 / 2;
        // Safety: the mapped texture and locked buffer are each released before
        // returning, and every row access stays within their reported sizes.
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(Error::Input)?;
            let mut target = ptr::null_mut();
            let mut capacity = 0;
            let locked = buffer.Lock(&mut target, Some(&mut capacity), None);
            if let Err(error) = locked {
                self.context.Unmap(staging, 0);
                return Err(Error::Input(error));
            }
            assert!(capacity as usize >= length);
            let source = std::slice::from_raw_parts(
                mapped.pData.cast::<u8>(),
                mapped.RowPitch as usize * (height - 1) + width * 4,
            );
            let target = std::slice::from_raw_parts_mut(target, length);
            bgra_to_nv12(
                source,
                mapped.RowPitch as usize,
                width,
                height,
                rgba,
                self.grayscale,
                target,
            );
            self.context.Unmap(staging, 0);
            buffer.Unlock().map_err(Error::Input)
        }
    }

    fn drain(&mut self, outputs: &mut Vec<EncodedAccessUnit>) -> Result<(), Error> {
        while let Some(output) = self.output.take(&self.transform, &self.output_info)? {
            outputs.push(output);
        }
        Ok(())
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn poll(&mut self) -> Result<Vec<EncodedAccessUnit>, Error> {
        // Output is drained as each frame is submitted.
        Ok(Vec::new())
    }

    fn wants_input(&self) -> bool {
        true
    }

    fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        capture_timestamp_us: u64,
    ) -> Result<Vec<EncodedAccessUnit>, Error> {
        let sample = self.input_sample(texture)?;
        let mut outputs = Vec::new();
        // Safety: the transform takes its own references to the sample and
        // its buffer if it keeps them past ProcessInput, and the pool writes
        // only samples no one else refers to.
        unsafe {
            sample
                .SetSampleTime(capture_timestamp_us.saturating_mul(10) as i64)
                .map_err(Error::Input)?;
            sample
                .SetSampleDuration(self.frame_duration_100ns)
                .map_err(Error::Input)?;
            if let Err(error) = self.transform.ProcessInput(0, &sample, 0) {
                if error.code() != MF_E_NOTACCEPTING {
                    return Err(Error::Input(error));
                }
                self.drain(&mut outputs)?;
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .map_err(Error::Input)?;
            }
        }
        self.output.submitted(capture_timestamp_us);
        self.drain(&mut outputs)?;
        Ok(outputs)
    }

    fn request_keyframe(&self) -> Result<(), Error> {
        unsafe {
            self.codec_api
                .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &1_u32.into())
                .map_err(Error::Configuration)
        }
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

/// Full-range RGB to studio-range BT.709 NV12, matching the GPU converter and
/// the color metadata in the encoder's media types. Chroma is the average of
/// each 2x2 block. `width` and `height` are even, `source` holds `height`
/// rows `pitch` bytes apart and `target` the NV12 frame.
fn bgra_to_nv12(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    rgba: bool,
    grayscale: bool,
    target: &mut [u8],
) {
    convert_frame::<true>(source, pitch, width, height, rgba, grayscale, target);
}

/// [`bgra_to_nv12`] a pair of rows at a time: their two luma rows, then their
/// chroma row. With `SIMD`, x86-64 converts 16 pixels at a time with SSE2,
/// which every x86-64 CPU has, and the scalar code takes the rest of a row.
/// Both compute the same fixed-point integers.
fn convert_frame<const SIMD: bool>(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
    rgba: bool,
    grayscale: bool,
    target: &mut [u8],
) {
    let (luma, chroma) = target[..width * height * 3 / 2].split_at_mut(width * height);
    let rows = luma
        .chunks_exact_mut(2 * width)
        .zip(chroma.chunks_exact_mut(width));
    for (pair, (luma, chroma)) in rows.enumerate() {
        let (top_luma, bottom_luma) = luma.split_at_mut(width);
        let rows = RowPair {
            top: &source[2 * pair * pitch..][..width * 4],
            bottom: &source[(2 * pair + 1) * pitch..][..width * 4],
            top_luma,
            bottom_luma,
            chroma,
        };
        let rows = if SIMD {
            vectorized(rows, rgba, grayscale)
        } else {
            rows
        };
        if rgba {
            rows.convert::<0, 2>(grayscale);
        } else {
            rows.convert::<2, 0>(grayscale);
        }
    }
}

/// Converts what the vector code can of `rows` and returns the rest.
#[cfg(target_arch = "x86_64")]
fn vectorized(rows: RowPair<'_>, rgba: bool, grayscale: bool) -> RowPair<'_> {
    // Safety: every x86-64 CPU has SSE2.
    unsafe { sse2::convert(rows, rgba, grayscale) }
}

/// Without vector code the scalar code converts all of `rows`.
#[cfg(not(target_arch = "x86_64"))]
fn vectorized(rows: RowPair<'_>, _rgba: bool, _grayscale: bool) -> RowPair<'_> {
    rows
}

/// Two rows of pixels and the NV12 rows made from them.
struct RowPair<'a> {
    top: &'a [u8],
    bottom: &'a [u8],
    top_luma: &'a mut [u8],
    bottom_luma: &'a mut [u8],
    chroma: &'a mut [u8],
}

impl RowPair<'_> {
    /// The scalar conversion. `RED` and `BLUE` are the byte offsets of those
    /// channels in a pixel.
    fn convert<const RED: usize, const BLUE: usize>(self, grayscale: bool) {
        for (pixels, luma) in [(self.top, self.top_luma), (self.bottom, self.bottom_luma)] {
            for (y, pixel) in luma.iter_mut().zip(pixels.chunks_exact(4)) {
                let (r, g, b) = (
                    i32::from(pixel[RED]),
                    i32::from(pixel[1]),
                    i32::from(pixel[BLUE]),
                );
                *y = ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
            }
        }
        if grayscale {
            self.chroma.fill(128);
            return;
        }
        let blocks = self
            .chroma
            .chunks_exact_mut(2)
            .zip(self.top.chunks_exact(8).zip(self.bottom.chunks_exact(8)));
        for (uv, (top, bottom)) in blocks {
            let sum = |channel: usize| {
                i32::from(top[channel])
                    + i32::from(top[channel + 4])
                    + i32::from(bottom[channel])
                    + i32::from(bottom[channel + 4])
            };
            let (r, g, b) = (sum(RED), sum(1), sum(BLUE));
            // The sums are four pixels: shift by two more bits.
            uv[0] = (((-26 * r - 87 * g + 112 * b + 512) >> 10) + 128) as u8;
            uv[1] = (((112 * r - 102 * g - 10 * b + 512) >> 10) + 128) as u8;
        }
    }
}

/// The conversion 16 pixels at a time. A pixel's 32 bits split into two
/// pairs of 16-bit lanes, blue and red (red and blue for RGBA) and green and
/// alpha, which `_mm_madd_epi16` weighs and adds in one step. Every value fits
/// those lanes: a channel's sum over a 2x2 block is at most 1020. Results are
/// in range before they are packed to bytes, so packing never saturates.
#[cfg(target_arch = "x86_64")]
mod sse2 {
    use std::arch::x86_64::*;

    use super::RowPair;

    /// Pixels per step.
    const STEP: usize = 16;

    /// Weights for the red-and-blue and the green-and-alpha lanes.
    type Weights = (__m128i, __m128i);

    /// Converts the whole steps of `rows` and returns the rest of them.
    #[target_feature(enable = "sse2")]
    pub(super) fn convert(rows: RowPair<'_>, rgba: bool, grayscale: bool) -> RowPair<'_> {
        // Red-and-blue weights in the order of those channels in a pixel.
        let order = |red: i16, blue: i16| {
            if rgba {
                pair(red, blue)
            } else {
                pair(blue, red)
            }
        };
        let y = (order(47, 16), pair(157, 0));
        let u = (order(-26, 112), pair(-87, 0));
        let v = (order(112, -10), pair(-102, 0));
        let pixels = rows.top_luma.len() / STEP * STEP;
        let (top, top_rest) = rows.top.split_at(pixels * 4);
        let (bottom, bottom_rest) = rows.bottom.split_at(pixels * 4);
        let (top_luma, top_luma_rest) = rows.top_luma.split_at_mut(pixels);
        let (bottom_luma, bottom_luma_rest) = rows.bottom_luma.split_at_mut(pixels);
        let (chroma, chroma_rest) = rows.chroma.split_at_mut(pixels);
        if grayscale {
            chroma.fill(128);
        }
        let steps = top
            .chunks_exact(STEP * 4)
            .zip(bottom.chunks_exact(STEP * 4))
            .zip(top_luma.chunks_exact_mut(STEP))
            .zip(bottom_luma.chunks_exact_mut(STEP))
            .zip(chroma.chunks_exact_mut(STEP));
        for ((((top, bottom), top_luma), bottom_luma), chroma) in steps {
            let top = load(top).map(|pixels| split(pixels));
            let bottom = load(bottom).map(|pixels| split(pixels));
            store(top_luma, luma(&top, y));
            store(bottom_luma, luma(&bottom, y));
            if grayscale {
                continue;
            }
            // Each pixel's channels summed with those of the pixel below,
            // then the even columns' sums with the odd ones', four 2x2
            // blocks from each half of the step.
            let columns = [0, 1, 2, 3].map(|index| {
                (
                    _mm_add_epi16(top[index].0, bottom[index].0),
                    _mm_add_epi16(top[index].1, bottom[index].1),
                )
            });
            let [first, second] = [0, 2].map(|half| {
                let blocks = (
                    add_pairs(columns[half].0, columns[half + 1].0),
                    add_pairs(columns[half].1, columns[half + 1].1),
                );
                // The sums are four pixels: shift by two more bits.
                let chroma = |weights| {
                    let sum = weigh(blocks, weights, 512 + (128 << 10));
                    _mm_srai_epi32::<10>(sum)
                };
                (chroma(u), chroma(v))
            });
            let u = _mm_packs_epi32(first.0, second.0);
            let v = _mm_packs_epi32(first.1, second.1);
            let interleaved = (_mm_unpacklo_epi16(u, v), _mm_unpackhi_epi16(u, v));
            store(chroma, _mm_packus_epi16(interleaved.0, interleaved.1));
        }
        RowPair {
            top: top_rest,
            bottom: bottom_rest,
            top_luma: top_luma_rest,
            bottom_luma: bottom_luma_rest,
            chroma: chroma_rest,
        }
    }

    /// The weight `low` in the low 16-bit lane of every 32-bit lane and `high`
    /// in the high one.
    #[target_feature(enable = "sse2")]
    fn pair(low: i16, high: i16) -> __m128i {
        _mm_set1_epi32((i32::from(high) << 16) | i32::from(low as u16))
    }

    /// The red-and-blue and green-and-alpha lanes of each pixel.
    #[target_feature(enable = "sse2")]
    fn split(pixels: __m128i) -> (__m128i, __m128i) {
        (
            _mm_and_si128(pixels, _mm_set1_epi32(0x00ff_00ff)),
            _mm_srli_epi16::<8>(pixels),
        )
    }

    /// The 16 luma bytes of a step's row.
    #[target_feature(enable = "sse2")]
    fn luma(lanes: &[(__m128i, __m128i); 4], weights: Weights) -> __m128i {
        let luma = lanes.map(|lanes| _mm_srai_epi32::<8>(weigh(lanes, weights, 128 + (16 << 8))));
        _mm_packus_epi16(
            _mm_packs_epi32(luma[0], luma[1]),
            _mm_packs_epi32(luma[2], luma[3]),
        )
    }

    /// The even 32-bit lanes of `a` then `b`, added to their odd lanes.
    #[target_feature(enable = "sse2")]
    fn add_pairs(a: __m128i, b: __m128i) -> __m128i {
        let (a, b) = (_mm_castsi128_ps(a), _mm_castsi128_ps(b));
        _mm_add_epi16(
            _mm_castps_si128(_mm_shuffle_ps::<0b10_00_10_00>(a, b)),
            _mm_castps_si128(_mm_shuffle_ps::<0b11_01_11_01>(a, b)),
        )
    }

    /// The weighted sum of each 32-bit lane's channels, plus `offset`.
    #[target_feature(enable = "sse2")]
    fn weigh(lanes: (__m128i, __m128i), weights: Weights, offset: i32) -> __m128i {
        _mm_add_epi32(
            _mm_add_epi32(
                _mm_madd_epi16(lanes.0, weights.0),
                _mm_madd_epi16(lanes.1, weights.1),
            ),
            _mm_set1_epi32(offset),
        )
    }

    #[target_feature(enable = "sse2")]
    fn load(pixels: &[u8]) -> [__m128i; 4] {
        assert_eq!(pixels.len(), STEP * 4);
        // Safety: each unaligned load reads 16 of the 64 bytes.
        [0, 1, 2, 3].map(|index| unsafe { _mm_loadu_si128(pixels.as_ptr().add(index * 16).cast()) })
    }

    #[target_feature(enable = "sse2")]
    fn store(target: &mut [u8], value: __m128i) {
        assert_eq!(target.len(), 16);
        // Safety: the unaligned store writes exactly the 16 bytes.
        unsafe { _mm_storeu_si128(target.as_mut_ptr().cast(), value) };
    }
}

/// The hardware converter and encoder, or their software replacements.
pub(crate) enum Converter {
    Gpu(converter::BgraToYuvConverter),
    Software(StagingPool),
}

impl Converter {
    pub(crate) fn convert(
        &mut self,
        bgra: &ID3D11Texture2D,
    ) -> Result<&ID3D11Texture2D, converter::Error> {
        match self {
            Self::Gpu(converter) => converter.convert(bgra),
            Self::Software(pool) => pool.copy(bgra),
        }
    }
}

pub(crate) enum Encoder {
    Hardware(encoder::MediaFoundationVideoEncoder),
    Software(SoftwareH264Encoder),
}

impl VideoEncoder for Encoder {
    fn poll(&mut self) -> Result<Vec<EncodedAccessUnit>, Error> {
        match self {
            Self::Hardware(encoder) => encoder.poll(),
            Self::Software(encoder) => encoder.poll(),
        }
    }

    fn wants_input(&self) -> bool {
        match self {
            Self::Hardware(encoder) => encoder.wants_input(),
            Self::Software(encoder) => encoder.wants_input(),
        }
    }

    fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        capture_timestamp_us: u64,
    ) -> Result<Vec<EncodedAccessUnit>, Error> {
        match self {
            Self::Hardware(encoder) => encoder.submit(texture, capture_timestamp_us),
            Self::Software(encoder) => encoder.submit(texture, capture_timestamp_us),
        }
    }

    fn request_keyframe(&self) -> Result<(), Error> {
        match self {
            Self::Hardware(encoder) => encoder.request_keyframe(),
            Self::Software(encoder) => encoder.request_keyframe(),
        }
    }

    fn set_bitrate(&self, bits_per_second: u32) -> Result<(), Error> {
        match self {
            Self::Hardware(encoder) => encoder.set_bitrate(bits_per_second),
            Self::Software(encoder) => encoder.set_bitrate(bits_per_second),
        }
    }
}

/// Settings of one video stream, for [`pipeline`].
pub(crate) struct PipelineConfig {
    pub width: u32,
    pub height: u32,
    pub frames_per_second: u32,
    pub bitrate_bits_per_second: u32,
    pub codec: VideoCodec,
    pub pixel_format: VideoPixelFormat,
    pub grayscale: bool,
}

/// The converter and encoder for a stream.
pub(crate) struct Pipeline {
    pub converter: Converter,
    pub encoder: Encoder,
    /// The stream rate: the requested one, capped for the software encoder.
    pub frames_per_second: u32,
}

/// The converter and encoder for a stream. The GPU pipeline comes first.
/// Software H.264 4:2:0 is the final fallback, so other profiles fail like
/// a missing hardware encoder and the Agent moves on to that profile. Safe
/// Mode has no GPU pipeline and goes straight to software.
pub(crate) fn pipeline(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    config: &PipelineConfig,
) -> Result<Pipeline, crate::Error> {
    let software_profile =
        config.codec == VideoCodec::H264 && config.pixel_format == VideoPixelFormat::Yuv420;
    if safe_mode() {
        if !software_profile {
            return Err(Error::HardwareEncoderUnavailable {
                codec: config.codec,
                pixel_format: config.pixel_format,
            }
            .into());
        }
        return software_pipeline(device, context, config);
    }
    match hardware_pipeline(device, context, config) {
        Err(error) if software_profile => {
            tracing::warn!(
                %error,
                "no GPU video pipeline for H.264 4:2:0; falling back to software encoding"
            );
            software_pipeline(device, context, config)
        }
        result => result,
    }
}

fn hardware_pipeline(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    config: &PipelineConfig,
) -> Result<Pipeline, crate::Error> {
    // The encoder first: without a GPU, its absence is the error to report,
    // not the missing video processor.
    let encoder = encoder::MediaFoundationVideoEncoder::new(
        device,
        config.width,
        config.height,
        config.frames_per_second,
        config.bitrate_bits_per_second,
        config.codec,
        config.pixel_format,
    )?;
    let converter = converter::BgraToYuvConverter::new(
        device,
        context,
        config.width,
        config.height,
        config.frames_per_second,
        config.pixel_format,
        config.grayscale,
    )?;
    Ok(Pipeline {
        converter: Converter::Gpu(converter),
        encoder: Encoder::Hardware(encoder),
        frames_per_second: config.frames_per_second,
    })
}

fn software_pipeline(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    config: &PipelineConfig,
) -> Result<Pipeline, crate::Error> {
    let frames_per_second = config.frames_per_second.min(MAX_FRAMES_PER_SECOND);
    let encoder = SoftwareH264Encoder::new(
        device,
        config.width,
        config.height,
        frames_per_second,
        config.bitrate_bits_per_second,
        config.grayscale,
    )?;
    let pool = StagingPool::new(device, context, config.width, config.height);
    Ok(Pipeline {
        converter: Converter::Software(pool),
        encoder: Encoder::Software(encoder),
        frames_per_second,
    })
}

#[cfg(test)]
mod tests {
    use super::bgra_to_nv12;

    /// The conversion as it was first written, one 2x2 block at a time: the
    /// reference the faster [`bgra_to_nv12`] must match exactly.
    fn reference_bgra_to_nv12(
        source: &[u8],
        pitch: usize,
        width: usize,
        height: usize,
        rgba: bool,
        grayscale: bool,
        target: &mut [u8],
    ) {
        let (red, blue) = if rgba { (0, 2) } else { (2, 0) };
        let (luma, chroma) = target.split_at_mut(width * height);
        for y in (0..height).step_by(2) {
            for x in (0..width).step_by(2) {
                let (mut sum_r, mut sum_g, mut sum_b) = (0_i32, 0_i32, 0_i32);
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let pixel = (y + dy) * pitch + (x + dx) * 4;
                    let r = i32::from(source[pixel + red]);
                    let g = i32::from(source[pixel + 1]);
                    let b = i32::from(source[pixel + blue]);
                    luma[(y + dy) * width + x + dx] =
                        ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
                    sum_r += r;
                    sum_g += g;
                    sum_b += b;
                }
                let (u, v) = if grayscale {
                    (128, 128)
                } else {
                    // The sums are four pixels: shift by two more bits.
                    (
                        (((-26 * sum_r - 87 * sum_g + 112 * sum_b + 512) >> 10) + 128) as u8,
                        (((112 * sum_r - 102 * sum_g - 10 * sum_b + 512) >> 10) + 128) as u8,
                    )
                };
                let index = y / 2 * width + x;
                chroma[index] = u;
                chroma[index + 1] = v;
            }
        }
    }

    /// Runs on a normal boot too: it uses the software path directly.
    #[test]
    #[ignore = "requires Windows Media Foundation and a D3D11 device"]
    fn software_encoder_produces_request_driven_keyframes() {
        use super::*;
        use std::time::{Duration, Instant};

        let (width, height) = (1920_u32, 1080_u32);
        let (device, context) = windows_capture::d3d11::create_d3d_device().unwrap();
        let pixels: Vec<u32> = (0..width * height)
            .map(|i| {
                if (i / width + i % width) % 8 < 4 {
                    0xffffffff
                } else {
                    0xff1060a0
                }
            })
            .collect();
        let desc = D3D11_TEXTURE2D_DESC {
            // One extra column and row, like an odd-sized display.
            Width: width + 1,
            Height: height + 1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut padded = vec![0_u32; ((width + 1) * (height + 1)) as usize];
        for row in 0..height as usize {
            let source = &pixels[row * width as usize..(row + 1) * width as usize];
            padded[row * (width as usize + 1)..][..width as usize].copy_from_slice(source);
        }
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: padded.as_ptr().cast(),
            SysMemPitch: (width + 1) * 4,
            ..Default::default()
        };
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
                .unwrap();
        }
        let texture = texture.unwrap();
        let mut pool = StagingPool::new(&device, &context, width, height);
        let mut encoder =
            SoftwareH264Encoder::new(&device, width, height, 30, 6_000_000, false).unwrap();
        let mut encode_time = Duration::ZERO;
        for index in 0..120_u64 {
            if index == 100 {
                encoder.request_keyframe().unwrap();
            }
            let started = Instant::now();
            let copy = pool.copy(&texture).unwrap().clone();
            let output = encoder.submit(&copy, index * 33_333 + 1).unwrap();
            encode_time += started.elapsed();
            assert_eq!(output.len(), 1, "frame {index}");
            let unit = &output[0];
            assert_eq!(unit.capture_timestamp_us, index * 33_333 + 1);
            assert!(unit.data.starts_with(&[0, 0, 0, 1]) || unit.data.starts_with(&[0, 0, 1]));
            assert_eq!(unit.keyframe, matches!(index, 0 | 100), "frame {index}");
            if index == 0 {
                assert!(unit.codec_config.is_some() || unit.data.len() > 1000);
            }
        }
        println!(
            "mean software copy+convert+encode: {:?} per 1080p frame",
            encode_time / 120
        );
    }

    /// Process page faults so far: a fresh multi-megabyte buffer faults in
    /// every page it touches, so faults per frame show allocation churn.
    fn page_faults() -> u32 {
        #[repr(C)]
        #[derive(Default)]
        struct Counters {
            cb: u32,
            page_fault_count: u32,
            rest: [usize; 8],
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> isize;
            fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
        }
        let mut counters = Counters {
            cb: size_of::<Counters>() as u32,
            ..Default::default()
        };
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
        counters.page_fault_count
    }

    /// A desktop-like picture: flat areas, gradients, text-like stripes and
    /// a square at `offset`, so successive pictures differ like motion.
    fn picture(width: usize, height: usize, offset: usize) -> Vec<u8> {
        let mut pixels = vec![0_u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                let pixel = &mut pixels[(y * width + x) * 4..][..4];
                let value = if (x + offset) % 400 < 120 && y % 300 < 120 {
                    [30, 200, 240, 255]
                } else if y < height / 3 {
                    [(x * 255 / width) as u8, (y * 255 / height) as u8, 90, 255]
                } else if (x / 7 + y / 11) % 5 == 0 {
                    [20, 20, 20, 255]
                } else {
                    [235, 235, 235, 255]
                };
                pixel.copy_from_slice(&value);
            }
        }
        pixels
    }

    type Conversion = fn(&[u8], usize, usize, usize, bool, bool, &mut [u8]);

    #[test]
    #[ignore = "requires Windows Media Foundation and a D3D11 device; benchmarks 1080p and 1440p, run with --release"]
    fn software_path_timings() {
        use super::*;
        use std::time::{Duration, Instant};

        for (width, height) in [(1920_usize, 1080_usize), (2560, 1440)] {
            let pictures = [picture(width, height, 0), picture(width, height, 37)];
            // The conversion alone, into one reused target, in color and in
            // grayscale, and the reference conversion it replaced.
            let mut target = vec![0_u8; width * height * 3 / 2];
            let mut time = |conversion: Conversion, grayscale| {
                let frames = 200;
                let started = Instant::now();
                for index in 0..frames {
                    let picture = &pictures[index % 2];
                    conversion(
                        picture,
                        width * 4,
                        width,
                        height,
                        false,
                        grayscale,
                        &mut target,
                    );
                }
                started.elapsed() / frames as u32
            };
            let [color, grayscale] = [false, true].map(|grayscale| {
                [bgra_to_nv12, reference_bgra_to_nv12].map(|c| time(c, grayscale))
            });

            let (device, context) = windows_capture::d3d11::create_d3d_device().unwrap();
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width as u32,
                Height: height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let textures: Vec<_> = pictures
                .iter()
                .map(|pixels| {
                    let data = D3D11_SUBRESOURCE_DATA {
                        pSysMem: pixels.as_ptr().cast(),
                        SysMemPitch: (width * 4) as u32,
                        ..Default::default()
                    };
                    let mut texture = None;
                    unsafe {
                        device
                            .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
                            .unwrap();
                    }
                    texture.unwrap()
                })
                .collect();
            let mut pool = StagingPool::new(&device, &context, width as u32, height as u32);
            let mut encoder = SoftwareH264Encoder::new(
                &device,
                width as u32,
                height as u32,
                30,
                6_000_000,
                false,
            )
            .unwrap();
            // New frames, then one frame encoded again and again, as an idle
            // desktop is refined.
            let (warmup, frames) = (10_u64, 120_u64);
            let mut timestamp = 0;
            let [(new, new_faults), (again, again_faults)] = [true, false].map(|new| {
                let mut copy = pool.copy(&textures[0]).unwrap().clone();
                let (mut total, mut faults) = (Duration::ZERO, 0);
                for index in 0..warmup + frames {
                    let faults_before = page_faults();
                    let started = Instant::now();
                    if new {
                        copy = pool.copy(&textures[index as usize % 2]).unwrap().clone();
                    }
                    timestamp += 33_333;
                    let output = encoder.submit(&copy, timestamp).unwrap();
                    if index >= warmup {
                        total += started.elapsed();
                        faults += page_faults() - faults_before;
                    }
                    assert_eq!(output.len(), 1, "frame {index}");
                }
                (total / frames as u32, faults / frames as u32)
            });
            let [[color, color_reference], [gray, gray_reference]] = [color, grayscale];
            println!("{width}x{height}: conversion {color:?} (reference {color_reference:?})");
            println!("  in grayscale {gray:?} (reference {gray_reference:?})");
            println!("  copy+convert+encode {new:?}, {new_faults} page faults per frame");
            println!("  same copy encoded again {again:?}, {again_faults} page faults per frame");
        }
    }

    fn convert(pixels: &[[u8; 4]], width: usize, rgba: bool, grayscale: bool) -> Vec<u8> {
        let height = pixels.len() / width;
        // A padded row pitch, as a mapped texture can have.
        let pitch = width * 4 + 8;
        let mut source = vec![0xEE; pitch * height];
        for (index, pixel) in pixels.iter().enumerate() {
            let offset = index / width * pitch + index % width * 4;
            source[offset..offset + 4].copy_from_slice(pixel);
        }
        let mut target = vec![0; width * height * 3 / 2];
        bgra_to_nv12(&source, pitch, width, height, rgba, grayscale, &mut target);
        target
    }

    #[test]
    fn converts_to_studio_range_bt709() {
        // BGRA white, black, red and blue in one 2x2 block.
        let block = [
            [255, 255, 255, 255],
            [0, 0, 0, 255],
            [0, 0, 255, 255],
            [255, 0, 0, 255],
        ];
        let nv12 = convert(&block, 2, false, false);
        assert_eq!(&nv12[..4], &[235, 16, 63, 32]);
        // The block averages to R 127.5, G 63.75, B 127.5: a dull magenta.
        assert_eq!(&nv12[4..], &[149, 153]);
        assert_eq!(&convert(&block, 2, false, true)[4..], &[128, 128]);
        // The same colors in RGBA order.
        let swapped: Vec<_> = block.iter().map(|p| [p[2], p[1], p[0], p[3]]).collect();
        assert_eq!(convert(&swapped, 2, true, false), nv12);
    }

    #[test]
    fn gray_has_neutral_chroma_and_planes_follow_rows() {
        let gray = vec![[128, 128, 128, 255]; 4 * 4];
        let nv12 = convert(&gray, 4, false, false);
        assert!(nv12[..16].iter().all(|&y| y == 126));
        assert!(nv12[16..].iter().all(|&c| c == 128));
    }

    /// The pictures [`matches_the_reference_conversion`] converts: noise,
    /// then pure colors, each channel at its extremes, and sharp edges.
    fn test_pictures(width: usize, height: usize, seed: u64) -> Vec<Vec<[u8; 4]>> {
        let mut state = seed | 1;
        let mut random = move || {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()
        };
        let count = width * height;
        let mut pictures = vec![
            (0..count)
                .map(|_| {
                    let bytes = random();
                    [bytes[0], bytes[1], bytes[2], bytes[3]]
                })
                .collect::<Vec<_>>(),
        ];
        for corner in 0..8_u8 {
            let level = |bit: u8| if corner & bit != 0 { 255 } else { 0 };
            pictures.push(vec![[level(1), level(2), level(4), 255]; count]);
        }
        for channel in 0..4 {
            // The channel alternates between 0 and 255 from pixel to pixel
            // and row to row, with the other channels at the opposite extreme.
            pictures.push(
                (0..count)
                    .map(|index| {
                        let on = (index % width + index / width).is_multiple_of(2);
                        let mut pixel = if on { [0; 4] } else { [255; 4] };
                        pixel[channel] = if on { 255 } else { 0 };
                        pixel
                    })
                    .collect(),
            );
        }
        // Columns and rows of white on black, which put whole blocks at the
        // extremes next to blocks half white.
        pictures.push(
            (0..count)
                .map(|index| {
                    let (x, y) = (index % width, index / width);
                    if x % 3 == 0 || y % 5 == 0 {
                        [255; 4]
                    } else {
                        [0, 0, 0, 255]
                    }
                })
                .collect(),
        );
        pictures
    }

    /// A reference comparison: the vectorized and the scalar conversion
    /// against the original, on widths that are and are not multiples of a
    /// SIMD step, row pitches wider than a row, and random padding bytes.
    #[test]
    fn matches_the_reference_conversion() {
        let sizes = [
            (2, 2),
            (4, 2),
            (14, 2),
            (16, 2),
            (18, 4),
            (30, 6),
            (32, 2),
            (34, 8),
            (64, 4),
            (130, 6),
            (1922, 4),
            (256, 64),
        ];
        for (seed, (width, height)) in sizes.into_iter().enumerate() {
            for padding in [0, 4, 60, 256] {
                let pitch = width * 4 + padding;
                for picture in test_pictures(width, height, seed as u64 + 0x9e37_79b9) {
                    // Padding bytes vary too: they must not reach the output.
                    let mut source: Vec<u8> = (0..pitch * height)
                        .map(|index| (index * 151 + seed * 7) as u8)
                        .collect();
                    for (index, pixel) in picture.iter().enumerate() {
                        let offset = index / width * pitch + index % width * 4;
                        source[offset..offset + 4].copy_from_slice(pixel);
                    }
                    let length = width * height * 3 / 2;
                    for rgba in [false, true] {
                        for grayscale in [false, true] {
                            let mut expected = vec![0xA5; length];
                            reference_bgra_to_nv12(
                                &source,
                                pitch,
                                width,
                                height,
                                rgba,
                                grayscale,
                                &mut expected,
                            );
                            let mut vectorized = vec![0; length];
                            bgra_to_nv12(
                                &source,
                                pitch,
                                width,
                                height,
                                rgba,
                                grayscale,
                                &mut vectorized,
                            );
                            let mut scalar = vec![0xFF; length];
                            super::convert_frame::<false>(
                                &source,
                                pitch,
                                width,
                                height,
                                rgba,
                                grayscale,
                                &mut scalar,
                            );
                            let case = format!(
                                "{width}x{height}, pitch {pitch}, rgba {rgba}, grayscale {grayscale}"
                            );
                            assert!(vectorized == expected, "vectorized, {case}");
                            assert!(scalar == expected, "scalar, {case}");
                        }
                    }
                }
            }
        }
    }

    /// A sample is written again only once no one else refers to it or its
    /// buffer, and the pool stops growing when every sample is held.
    #[test]
    fn input_samples_are_reused_unless_held() {
        use super::{InputSamples, MAX_INPUT_SAMPLES};
        use windows::core::Interface;

        let mut inputs = InputSamples::new(64);
        let first = inputs.writable().unwrap();
        let buffer = inputs.samples[first].buffer.as_raw();
        inputs.newest = Some((first, 7));
        // Unheld: the same copy is found again, and the next frame reuses
        // the sample and its buffer.
        assert_eq!(inputs.holding(Some(7)), Some(first));
        assert_eq!(inputs.holding(Some(8)), None);
        assert_eq!(inputs.holding(None), None);
        assert_eq!(inputs.writable().unwrap(), first);
        assert_eq!(inputs.samples[first].buffer.as_raw(), buffer);
        assert_eq!(inputs.samples.len(), 1);

        // Held, as by a transform after ProcessInput: neither found nor
        // written, whether the sample or only its buffer is held.
        inputs.newest = Some((first, 7));
        let held_sample = inputs.samples[first].sample.clone();
        assert_eq!(inputs.holding(Some(7)), None);
        let second = inputs.writable().unwrap();
        assert_ne!(second, first);
        let held_buffer = inputs.samples[second].buffer.clone();
        let third = inputs.writable().unwrap();
        assert!(third != first && third != second);
        drop((held_sample, held_buffer));
        assert_eq!(inputs.writable().unwrap(), first);
        assert_eq!(inputs.samples.len(), 3);

        // Every sample held: new ones up to the limit, then the held ones
        // are given up to their holders in turn.
        let mut held = Vec::new();
        for _ in 0..MAX_INPUT_SAMPLES + 3 {
            let index = inputs.writable().unwrap();
            held.push(inputs.samples[index].sample.clone());
            assert!(inputs.samples.len() <= MAX_INPUT_SAMPLES);
        }
        assert_eq!(inputs.samples.len(), MAX_INPUT_SAMPLES);
        // A given-up sample stays whole for its holder.
        assert_eq!(unsafe { held[0].GetTotalLength() }.unwrap(), 64);
        drop(held);
        assert!(inputs.samples.iter().all(super::InputSample::free));
    }

    /// The software encoder writes its input samples again from frame to
    /// frame, and encodes a staging copy submitted again without converting
    /// it again.
    #[test]
    #[ignore = "requires Windows Media Foundation and a D3D11 device"]
    fn software_encoder_reuses_its_input_samples() {
        use super::*;

        let (width, height) = (640_u32, 360_u32);
        let (device, context) = windows_capture::d3d11::create_d3d_device().unwrap();
        let textures: Vec<_> = [0xff10_60a0_u32, 0xffa0_6010]
            .into_iter()
            .map(|color| {
                let pixels = vec![color; (width * height) as usize];
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    ..Default::default()
                };
                let data = D3D11_SUBRESOURCE_DATA {
                    pSysMem: pixels.as_ptr().cast(),
                    SysMemPitch: width * 4,
                    ..Default::default()
                };
                let mut texture = None;
                unsafe {
                    device
                        .CreateTexture2D(&desc, Some(&data), Some(&mut texture))
                        .unwrap();
                }
                texture.unwrap()
            })
            .collect();
        let mut pool = StagingPool::new(&device, &context, width, height);
        let mut encoder =
            SoftwareH264Encoder::new(&device, width, height, 30, 2_000_000, false).unwrap();
        let mut buffers = std::collections::HashSet::new();
        let mut timestamp = 1;
        let mut submit = |encoder: &mut SoftwareH264Encoder, copy: &ID3D11Texture2D| {
            timestamp += 33_333;
            assert_eq!(encoder.submit(copy, timestamp).unwrap().len(), 1);
        };
        for index in 0..60 {
            let copy = pool.copy(&textures[index % 2]).unwrap().clone();
            submit(&mut encoder, &copy);
            let (newest, _) = encoder.inputs.newest.unwrap();
            buffers.insert(encoder.inputs.samples[newest].buffer.as_raw() as usize);
        }
        assert!(
            buffers.len() <= MAX_INPUT_SAMPLES,
            "{} input buffers for 60 frames",
            buffers.len()
        );

        // Mark the newest sample's first luma byte: encoding its copy again
        // must leave the mark, and a new copy must overwrite it.
        let copy = pool.copy(&textures[0]).unwrap().clone();
        submit(&mut encoder, &copy);
        let first_byte = |encoder: &SoftwareH264Encoder, value: Option<u8>| unsafe {
            let (newest, _) = encoder.inputs.newest.unwrap();
            let buffer = &encoder.inputs.samples[newest].buffer;
            let mut data = ptr::null_mut();
            buffer.Lock(&mut data, None, None).unwrap();
            if let Some(value) = value {
                *data = value;
            }
            let byte = *data;
            buffer.Unlock().unwrap();
            byte
        };
        let converted = first_byte(&encoder, None);
        first_byte(&encoder, Some(converted ^ 0xff));
        submit(&mut encoder, &copy);
        assert_eq!(first_byte(&encoder, None), converted ^ 0xff);
        let copy = pool.copy(&textures[0]).unwrap().clone();
        submit(&mut encoder, &copy);
        assert_eq!(first_byte(&encoder, None), converted);
    }
}
