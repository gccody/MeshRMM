use super::*;
use crate::video_layout::VideoRect;
use window::ClientLayout;

const SWAP_CHAIN_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;

pub(super) struct D3d11Renderer {
    window: HWND,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: Option<ID3D11VideoProcessorOutputView>,
    swap_chain: IDXGISwapChain2,
    /// Redrawn after a resize, since a static desktop sends no new frames.
    last_frame: Option<(ID3D11Texture2D, u32)>,
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
        let video_window = unsafe { window::video_window(window) }
            .context("remote video window is unavailable")?;
        let layout = unsafe { window::client_layout(window) }
            .context("remote window client area is unavailable")?;
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .context("DXGI factory creation failed")?;
        // The buffers track the video window; the video is letterboxed inside
        // them. Stretch scaling only shows while the user drags the border.
        let swap_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: layout.width.max(1),
            Height: layout.height.max(1),
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
            Flags: SWAP_CHAIN_FLAGS.0 as u32,
        };
        let swap_chain: IDXGISwapChain2 =
            unsafe { factory.CreateSwapChainForHwnd(device, video_window, &swap_desc, None, None) }
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
            OutputWidth: layout.width.max(1),
            OutputHeight: layout.height.max(1),
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
        // Letterbox bars stay black.
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        unsafe { video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black) };
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
        let mut renderer = Self {
            window,
            context: context.clone(),
            video_device,
            video_context,
            enumerator,
            processor,
            output_view: None,
            swap_chain,
            last_frame: None,
        };
        unsafe { renderer.configure_output(&layout)? };
        Ok(renderer)
    }

    /// Points the video processor at the current back buffer and places the
    /// video in the letterboxed rectangle that pointer mapping also uses.
    unsafe fn configure_output(&mut self, layout: &ClientLayout) -> anyhow::Result<()> {
        let back_buffer: ID3D11Texture2D = unsafe { self.swap_chain.GetBuffer(0) }
            .context("DXGI swap chain returned no back buffer")?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                &back_buffer,
                &self.enumerator,
                &output_desc,
                Some(&mut output_view),
            )
        }
        .context("swap-chain video output view creation failed")?;
        self.output_view = Some(output_view.context("D3D11 returned no video output view")?);
        let target = RECT {
            left: 0,
            top: 0,
            right: layout.width as i32,
            bottom: layout.height as i32,
        };
        let destination = rect(layout.video);
        unsafe {
            self.video_context.VideoProcessorSetOutputTargetRect(
                &self.processor,
                true,
                Some(&target),
            );
            self.video_context.VideoProcessorSetStreamDestRect(
                &self.processor,
                0,
                true,
                Some(&destination),
            );
        }
        Ok(())
    }

    /// Resizes the swap chain to the video window and redraws the last frame.
    pub(super) unsafe fn resize(&mut self, layout: &ClientLayout) -> anyhow::Result<()> {
        if layout.width == 0 || layout.height == 0 {
            // Minimized; keep the current buffers until the window returns.
            return Ok(());
        }
        // DXGI requires every reference to the old buffers to be released.
        self.output_view = None;
        unsafe { self.context.Flush() };
        unsafe {
            self.swap_chain.ResizeBuffers(
                0,
                layout.width,
                layout.height,
                DXGI_FORMAT_UNKNOWN,
                SWAP_CHAIN_FLAGS,
            )
        }
        .context("DXGI swap chain resize failed")?;
        unsafe { self.configure_output(layout)? };
        tracing::debug!(
            width = layout.width,
            height = layout.height,
            video_left = layout.video.left,
            video_top = layout.video.top,
            video_width = layout.video.width,
            video_height = layout.video.height,
            "resized the viewer swap chain"
        );
        if let Some((texture, subresource)) = self.last_frame.clone() {
            unsafe { self.present(&texture, subresource)? };
        }
        Ok(())
    }

    pub(super) unsafe fn present(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
    ) -> anyhow::Result<()> {
        let output_view = self
            .output_view
            .clone()
            .context("swap-chain video output view is unavailable")?;
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
                &output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        let _ = unsafe { ManuallyDrop::take(&mut stream.pInputSurface) };
        result.context("GPU YUV-to-BGRA presentation blit failed")?;
        self.last_frame = Some((texture.clone(), subresource));
        // One-interval presentation avoids tearing. Flip-discard plus maximum
        // frame latency 1 prevents an additional multi-frame swap-chain queue.
        unsafe { self.swap_chain.Present(1, DXGI_PRESENT(0)) }
            .ok()
            .context("DXGI presentation failed")
    }
}

fn rect(video: VideoRect) -> RECT {
    RECT {
        left: video.left,
        top: video.top,
        right: video.right(),
        bottom: video.bottom(),
    }
}

impl Drop for D3d11Renderer {
    fn drop(&mut self) {
        if !self.window.is_invalid() {
            let _ = unsafe { DestroyWindow(self.window) };
        }
    }
}
