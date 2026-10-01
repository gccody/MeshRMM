//! Video for Safe Mode. Windows loads no GPU vendor driver there, so there is
//! no hardware encoder and the D3D11 video processor may be missing too.
//! Frames are copied to the CPU, converted to NV12, and encoded with
//! Microsoft's software H.264 encoder. Normal boots never use this path.

use std::ptr;

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
use windows::core::Interface;

use crate::converter::{self, SURFACE_COUNT};
use crate::encoder::{
    self, Error, MediaFoundationRuntime, OutputFramer, VideoEncoder, make_video_type,
    set_optional_initial_codec_value, set_required_runtime_codec_value,
};
use crate::{EncodedAccessUnit, VideoCodec, VideoPixelFormat};

/// Software encoding is only for Safe Mode.
pub(crate) fn required() -> bool {
    unsafe { GetSystemMetrics(SM_CLEANBOOT) != 0 }
}

/// Keeps the CPU encoder from starving the rest of a Safe Mode session.
pub(crate) const MAX_FRAMES_PER_SECOND: u32 = 30;

/// CPU-readable copies of captured frames. Like the GPU converter's pool, a
/// copy stays valid until two more frames are copied, so a capture loop can
/// encode the last frame again.
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
        unsafe {
            self.context
                .CopySubresourceRegion(target, 0, 0, 0, 0, source, 0, Some(&region))
        };
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

/// Microsoft's synchronous H.264 encoder, fed from [`StagingPool`] copies.
pub(crate) struct SoftwareH264Encoder {
    transform: IMFTransform,
    codec_api: ICodecAPI,
    output_info: MFT_OUTPUT_STREAM_INFO,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    grayscale: bool,
    frame_duration_100ns: i64,
    output: OutputFramer,
    // Dropped after the transform, in declaration order.
    _runtime: Option<MediaFoundationRuntime>,
    _com: ComApartment,
}

/// COM on the capture thread for the encoder's lifetime. Media Foundation
/// does not initialize it, and Safe Mode skips Media Foundation.
struct ComApartment(bool);

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
        // Safe Mode disables the Media Foundation platform: MFStartup fails,
        // and with it everything that needs its work queues. The software
        // encoder transform and the media types, samples and buffers that
        // feed it are plain COM objects that work without it.
        let runtime = match MediaFoundationRuntime::start() {
            Ok(runtime) => Some(runtime),
            Err(Error::Startup(error)) if error.code() == MF_E_DISABLED_IN_SAFEMODE => None,
            Err(error) => return Err(error),
        };
        let _com = ComApartment::enter()?;
        let context = unsafe { device.GetImmediateContext() }.map_err(Error::Configuration)?;
        // Safety: the transform and its interfaces stay on this capture thread.
        unsafe {
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|_| Error::SoftwareEncoderUnavailable)?;
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
                "Windows is in Safe Mode; encoding video in software"
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
                _runtime: runtime,
                _com,
            })
        }
    }

    /// Converts a staging copy into a new NV12 media buffer.
    fn nv12_buffer(&self, staging: &ID3D11Texture2D) -> Result<IMFMediaBuffer, Error> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { staging.GetDesc(&mut desc) };
        let rgba = desc.Format == DXGI_FORMAT_R8G8B8A8_UNORM;
        let (width, height) = (self.width as usize, self.height as usize);
        let length = width * height * 3 / 2;
        // Safety: the mapped texture and locked buffer are each released before
        // returning, and every row access stays within their reported sizes.
        unsafe {
            let buffer = MFCreateMemoryBuffer(length as u32).map_err(Error::Input)?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(Error::Input)?;
            let mut target = ptr::null_mut();
            let locked = buffer.Lock(&mut target, None, None);
            if let Err(error) = locked {
                self.context.Unmap(staging, 0);
                return Err(Error::Input(error));
            }
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
            buffer.Unlock().map_err(Error::Input)?;
            buffer
                .SetCurrentLength(length as u32)
                .map_err(Error::Input)?;
            Ok(buffer)
        }
    }

    fn drain(&mut self) -> Result<Vec<EncodedAccessUnit>, Error> {
        let mut outputs = Vec::new();
        while let Some(output) = self.output.take(&self.transform, &self.output_info)? {
            outputs.push(output);
        }
        Ok(outputs)
    }
}

impl VideoEncoder for SoftwareH264Encoder {
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
        let buffer = self.nv12_buffer(texture)?;
        let mut outputs = Vec::new();
        // Safety: the sample owns its buffer; the synchronous transform copies
        // or releases it before ProcessInput returns or on the next output.
        unsafe {
            let sample = MFCreateSample().map_err(Error::Input)?;
            sample.AddBuffer(&buffer).map_err(Error::Input)?;
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
                outputs.extend(self.drain()?);
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .map_err(Error::Input)?;
            }
        }
        self.output.submitted(capture_timestamp_us);
        outputs.extend(self.drain()?);
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

impl Drop for SoftwareH264Encoder {
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
    }
}

/// Full-range RGB to studio-range BT.709 NV12, matching the GPU converter and
/// the color metadata in the encoder's media types. Chroma is the average of
/// each 2x2 block. `width` and `height` are even.
fn bgra_to_nv12(
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

/// The hardware converter and encoder, or their Safe Mode replacements.
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

/// The converter and encoder for a stream. In Safe Mode only H.264 4:2:0 is
/// available, so other profiles fail like a missing hardware encoder and the
/// Agent falls back to it.
pub(crate) fn pipeline(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    config: &PipelineConfig,
) -> Result<(Converter, Encoder), crate::Error> {
    if required() {
        if config.codec != VideoCodec::H264 || config.pixel_format != VideoPixelFormat::Yuv420 {
            return Err(Error::HardwareEncoderUnavailable {
                codec: config.codec,
                pixel_format: config.pixel_format,
            }
            .into());
        }
        let encoder = SoftwareH264Encoder::new(
            device,
            config.width,
            config.height,
            config.frames_per_second,
            config.bitrate_bits_per_second,
            config.grayscale,
        )?;
        let pool = StagingPool::new(device, context, config.width, config.height);
        return Ok((Converter::Software(pool), Encoder::Software(encoder)));
    }
    let converter = converter::BgraToYuvConverter::new(
        device,
        context,
        config.width,
        config.height,
        config.frames_per_second,
        config.pixel_format,
        config.grayscale,
    )?;
    let encoder = encoder::MediaFoundationVideoEncoder::new(
        device,
        config.width,
        config.height,
        config.frames_per_second,
        config.bitrate_bits_per_second,
        config.codec,
        config.pixel_format,
    )?;
    Ok((Converter::Gpu(converter), Encoder::Hardware(encoder)))
}

/// The capture rate for a stream: Safe Mode caps it for the CPU encoder.
pub(crate) fn frames_per_second(requested: u32) -> u32 {
    if required() {
        requested.min(MAX_FRAMES_PER_SECOND)
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::bgra_to_nv12;

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
}
