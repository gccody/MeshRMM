//! NV12-to-BGRA conversion with a pixel shader, for D3D11 devices without a
//! video processor, such as WARP on a machine with no GPU. It matches the
//! video processor's studio-range BT.709 to full-range RGB conversion.

use std::ffi::CStr;

use super::*;
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile,
};
use windows::Win32::Graphics::Direct3D::{
    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_SRV_DIMENSION_TEXTURE2D, ID3DBlob, ID3DInclude,
};
use windows::core::PCSTR;

/// A full-viewport triangle, sampling the luma plane and the half-size
/// interleaved chroma plane of an NV12 texture.
const SHADER: &str = r#"
struct Vertex {
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

Vertex vertex_main(uint id : SV_VertexID) {
    Vertex vertex;
    vertex.uv = float2((id << 1) & 2, id & 2);
    vertex.position = float4(vertex.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return vertex;
}

cbuffer Crop : register(b0) {
    // The stream's share of a texture a decoder padded.
    float2 scale;
    float2 unused;
};
Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
SamplerState bilinear : register(s0);

float4 pixel_main(Vertex vertex) : SV_Target {
    float2 uv = vertex.uv * scale;
    float y = 1.164383 * (luma.Sample(bilinear, uv) - 16.0 / 255.0);
    float2 c = chroma.Sample(bilinear, uv) - 128.0 / 255.0;
    float3 rgb = float3(
        y + 1.792741 * c.y,
        y - 0.213249 * c.x - 0.532909 * c.y,
        y + 2.112402 * c.x);
    return float4(saturate(rgb), 1);
}
"#;

pub(in crate::platform::windows) struct ShaderConversion {
    device: ID3D11Device,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    crop: ID3D11Buffer,
    target: Option<ID3D11RenderTargetView>,
    viewport: D3D11_VIEWPORT,
    /// The last texture's plane views and crop. A software decoder uploads
    /// every picture into the same texture.
    planes: Option<Planes>,
}

struct Planes {
    texture: ID3D11Texture2D,
    /// The stream size the crop was computed for.
    size: (u32, u32),
    views: [Option<ID3D11ShaderResourceView>; 2],
    scale: [f32; 2],
}

impl ShaderConversion {
    pub(in crate::platform::windows) fn check_format(format: VideoFormat) -> anyhow::Result<()> {
        if format.pixel_format != meshrmm_protocol::PixelFormat::Nv12 {
            bail!(
                "without a D3D11 video processor only NV12 video can be shown, not {:?}",
                format.pixel_format
            );
        }
        Ok(())
    }

    pub(in crate::platform::windows) unsafe fn new(
        device: &ID3D11Device,
        format: VideoFormat,
    ) -> anyhow::Result<Self> {
        Self::check_format(format)?;
        let vertex = unsafe { compile(c"vertex_main", c"vs_4_0")? };
        let pixel = unsafe { compile(c"pixel_main", c"ps_4_0")? };
        let mut vertex_shader = None;
        unsafe { device.CreateVertexShader(blob_bytes(&vertex), None, Some(&mut vertex_shader)) }
            .context("NV12 vertex shader creation failed")?;
        let mut pixel_shader = None;
        unsafe { device.CreatePixelShader(blob_bytes(&pixel), None, Some(&mut pixel_shader)) }
            .context("NV12 pixel shader creation failed")?;
        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .context("NV12 sampler creation failed")?;
        let crop_desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut crop = None;
        unsafe { device.CreateBuffer(&crop_desc, None, Some(&mut crop)) }
            .context("NV12 crop buffer creation failed")?;
        Ok(Self {
            device: device.clone(),
            vertex_shader: vertex_shader.context("D3D11 returned no vertex shader")?,
            pixel_shader: pixel_shader.context("D3D11 returned no pixel shader")?,
            sampler: sampler.context("D3D11 returned no sampler")?,
            crop: crop.context("D3D11 returned no crop buffer")?,
            target: None,
            viewport: D3D11_VIEWPORT::default(),
            planes: None,
        })
    }

    pub(in crate::platform::windows) unsafe fn configure_output(
        &mut self,
        back_buffer: &ID3D11Texture2D,
        layout: &ClientLayout,
    ) -> anyhow::Result<()> {
        let mut target = None;
        unsafe {
            self.device
                .CreateRenderTargetView(back_buffer, None, Some(&mut target))
        }
        .context("swap-chain render target view creation failed")?;
        self.target = Some(target.context("D3D11 returned no render target view")?);
        self.viewport = D3D11_VIEWPORT {
            TopLeftX: layout.video.left as f32,
            TopLeftY: layout.video.top as f32,
            Width: layout.video.width as f32,
            Height: layout.video.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        Ok(())
    }

    pub(in crate::platform::windows) fn release_output(&mut self) {
        self.target = None;
    }

    pub(in crate::platform::windows) unsafe fn convert(
        &mut self,
        context: &ID3D11DeviceContext,
        texture: &ID3D11Texture2D,
        subresource: u32,
        format: VideoFormat,
    ) -> anyhow::Result<()> {
        let target = self
            .target
            .clone()
            .context("swap-chain render target view is unavailable")?;
        if subresource != 0 {
            bail!("the NV12 shader cannot read texture array slice {subresource}");
        }
        let size = (format.width, format.height);
        if self
            .planes
            .as_ref()
            .is_none_or(|planes| planes.texture != *texture || planes.size != size)
        {
            self.planes = Some(unsafe { self.plane_views(texture, format)? });
            let scale = self.planes.as_ref().map_or([1.0; 2], |planes| planes.scale);
            let crop = [scale[0], scale[1], 0.0, 0.0];
            unsafe { context.UpdateSubresource(&self.crop, 0, None, crop.as_ptr().cast(), 0, 0) };
        }
        let planes = self
            .planes
            .as_ref()
            .context("NV12 plane views unavailable")?;
        unsafe {
            // Letterbox bars stay black.
            context.ClearRenderTargetView(&target, &[0.0, 0.0, 0.0, 1.0]);
            context.OMSetRenderTargets(Some(&[Some(target)]), None);
            context.RSSetViewports(Some(&[self.viewport]));
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetShaderResources(0, Some(&planes.views));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.crop.clone())]));
            context.Draw(3, 0);
            // Release the bindings: the decoder rewrites the texture, and a
            // resize needs every reference to the back buffer gone.
            context.PSSetShaderResources(0, Some(&[None, None]));
            context.OMSetRenderTargets(None, None);
        }
        Ok(())
    }

    unsafe fn plane_views(
        &self,
        texture: &ID3D11Texture2D,
        format: VideoFormat,
    ) -> anyhow::Result<Planes> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 {
            bail!("the NV12 shader cannot read {:?} textures", desc.Format);
        }
        let mut views = [None, None];
        for (view, plane_format) in views
            .iter_mut()
            .zip([DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM])
        {
            let view_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: plane_format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                    },
                },
            };
            unsafe {
                self.device
                    .CreateShaderResourceView(texture, Some(&view_desc), Some(view))
            }
            .with_context(|| format!("NV12 {plane_format:?} plane view creation failed"))?;
        }
        Ok(Planes {
            texture: texture.clone(),
            size: (format.width, format.height),
            views,
            scale: [
                format.width as f32 / desc.Width.max(1) as f32,
                format.height as f32 / desc.Height.max(1) as f32,
            ],
        })
    }
}

unsafe fn compile(entry_point: &CStr, target: &CStr) -> anyhow::Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            PCSTR::null(),
            None,
            None::<&ID3DInclude>,
            PCSTR(entry_point.as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3 | D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(error) = result {
        let detail = errors
            .as_ref()
            .map(|errors| String::from_utf8_lossy(blob_bytes(errors)).into_owned())
            .unwrap_or_default();
        return Err(error).with_context(|| format!("NV12 shader compilation failed: {detail}"));
    }
    code.context("the shader compiler returned no code")
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    // Safety: the blob owns this buffer for as long as it is borrowed.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}
