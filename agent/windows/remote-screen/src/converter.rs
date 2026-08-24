use std::mem::ManuallyDrop;

use thiserror::Error;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_FORMAT_AYUV, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::VideoPixelFormat;

pub(crate) const SURFACE_COUNT: usize = 3;

#[derive(Debug, Error)]
pub enum Error {
    #[error("D3D11 video processor creation failed: {0}")]
    Processor(#[source] windows::core::Error),
    #[error("GPU does not support BGRA input and {0} output video processing")]
    UnsupportedFormatConversion(VideoPixelFormat),
    #[error("D3D11 YUV texture allocation returned no texture")]
    MissingTexture,
    #[error("D3D11 video processor view creation returned no view")]
    MissingView,
}

pub struct BgraToYuvConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    surfaces: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next_surface: usize,
}

impl BgraToYuvConverter {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        frames_per_second: u32,
        pixel_format: VideoPixelFormat,
    ) -> Result<Self, Error> {
        // Safety: all COM interfaces originate from one D3D11 device and remain
        // owned by this converter on the capture worker thread.
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast().map_err(Error::Processor)?;
            let video_context: ID3D11VideoContext = context.cast().map_err(Error::Processor)?;
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: frames_per_second,
                    Denominator: 1,
                },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: frames_per_second,
                    Denominator: 1,
                },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&content)
                .map_err(Error::Processor)?;
            let bgra_support = enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(Error::Processor)?;
            let output_format = match pixel_format {
                VideoPixelFormat::Yuv420 => DXGI_FORMAT_NV12,
                VideoPixelFormat::Yuv444 => DXGI_FORMAT_AYUV,
            };
            let output_support = enumerator
                .CheckVideoProcessorFormat(output_format)
                .map_err(Error::Processor)?;
            if bgra_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
                || output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
            {
                return Err(Error::UnsupportedFormatConversion(pixel_format));
            }
            let processor = video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(Error::Processor)?;
            let rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect));
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect));
            // Desktop capture is full-range RGB. Make the RGB -> studio-range
            // BT.709 conversion explicit so drivers do not choose SD-video
            // defaults and the bitstream's color metadata matches its pixels.
            if let Ok(video_context1) = video_context.cast::<ID3D11VideoContext1>() {
                video_context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                );
                video_context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                );
            }

            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: output_format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut surfaces = Vec::with_capacity(SURFACE_COUNT);
            for _ in 0..SURFACE_COUNT {
                let mut texture = None;
                device
                    .CreateTexture2D(&texture_desc, None, Some(&mut texture))
                    .map_err(Error::Processor)?;
                let texture = texture.ok_or(Error::MissingTexture)?;
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(
                        &texture,
                        &enumerator,
                        &output_desc,
                        Some(&mut view),
                    )
                    .map_err(Error::Processor)?;
                surfaces.push((texture, view.ok_or(Error::MissingView)?));
            }
            Ok(Self {
                video_device,
                video_context,
                enumerator,
                processor,
                surfaces,
                next_surface: 0,
            })
        }
    }

    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<&ID3D11Texture2D, Error> {
        // Safety: the input texture belongs to the same device and the returned
        // YUV texture stays alive in the converter's fixed three-surface pool.
        unsafe {
            let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    bgra,
                    &self.enumerator,
                    &input_desc,
                    Some(&mut input_view),
                )
                .map_err(Error::Processor)?;
            let input_view = input_view.ok_or(Error::MissingView)?;
            let output_index = self.next_surface;
            self.next_surface = (self.next_surface + 1) % self.surfaces.len();
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(input_view.clone())),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.surfaces[output_index].1,
                0,
                std::slice::from_ref(&stream),
            );
            // Release the interface copy stored in the C-compatible ManuallyDrop field.
            let _ = ManuallyDrop::take(&mut stream.pInputSurface);
            result.map_err(Error::Processor)?;
            Ok(&self.surfaces[output_index].0)
        }
    }
}
