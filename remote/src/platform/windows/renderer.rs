use super::*;

pub(super) struct D3d11Renderer {
    window: HWND,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
    swap_chain: IDXGISwapChain2,
}

impl D3d11Renderer {
    pub(super) fn window(&self) -> HWND {
        self.window
    }

    pub(super) unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        format: VideoFormat,
        active_display: Display,
        displays: Vec<Display>,
        control: ControlSink,
        debug: DebugInfo,
    ) -> anyhow::Result<Self> {
        let window = unsafe { create_window(format, active_display, displays, control, debug)? };
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .context("DXGI factory creation failed")?;
        let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: format.width,
            Height: format.height.saturating_add(VIEWER_TOOLBAR_HEIGHT),
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
            OutputHeight: format.height.saturating_add(VIEWER_TOOLBAR_HEIGHT),
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("D3D11 presentation video processor enumeration failed")?;
        let input_format = match format.pixel_format {
            meshrmm_protocol::PixelFormat::Nv12 => DXGI_FORMAT_NV12,
            meshrmm_protocol::PixelFormat::Ayuv => DXGI_FORMAT_AYUV,
        };
        let input_support = unsafe { enumerator.CheckVideoProcessorFormat(input_format) }?;
        let output_support =
            unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }?;
        if input_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0
            || output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0
        {
            bail!("GPU cannot convert the decoded YUV surfaces to BGRA presentation surfaces");
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
            top: VIEWER_TOOLBAR_HEIGHT as i32,
            right: format.width as i32,
            bottom: format.height.saturating_add(VIEWER_TOOLBAR_HEIGHT) as i32,
        };
        unsafe { video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect)) };
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            )
        };
        let source_rect = RECT {
            left: 0,
            top: 0,
            right: format.width as i32,
            bottom: format.height as i32,
        };
        unsafe {
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source_rect))
        };
        unsafe { video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect)) };
        if let Ok(video_context1) = video_context.cast::<ID3D11VideoContext1>() {
            unsafe {
                video_context1.VideoProcessorSetStreamColorSpace1(
                    &processor,
                    0,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                )
            };
            unsafe {
                video_context1.VideoProcessorSetOutputColorSpace1(
                    &processor,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                )
            };
        }
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

    pub(super) unsafe fn present(
        &self,
        texture: &ID3D11Texture2D,
        subresource: u32,
    ) -> anyhow::Result<()> {
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
        result.context("GPU YUV-to-BGRA presentation blit failed")?;
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
