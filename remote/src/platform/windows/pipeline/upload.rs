use super::*;

/// Copies NV12 pictures from system memory into a texture the renderer's
/// video processor can read.
pub(super) struct CpuUpload {
    device: ID3D11Device,
    texture: Option<(ID3D11Texture2D, (u32, u32))>,
}

impl CpuUpload {
    pub(super) fn new(device: &ID3D11Device) -> Self {
        Self {
            device: device.clone(),
            texture: None,
        }
    }

    /// `size` is the decoder's padded picture size. The texture has the same
    /// size; the renderer crops it to the stream's, as it does GPU surfaces.
    pub(super) unsafe fn upload(
        &mut self,
        buffer: &IMFMediaBuffer,
        size: (u32, u32),
    ) -> anyhow::Result<ID3D11Texture2D> {
        let (width, height) = size;
        let texture = match &self.texture {
            Some((texture, current)) if *current == size => texture.clone(),
            _ => {
                let texture = unsafe { nv12_texture(&self.device, width, height)? };
                self.texture = Some((texture.clone(), size));
                texture
            }
        };
        // Lock returns the picture contiguously: the chroma plane follows
        // the padded luma plane, with the same pitch.
        let mut data = ptr::null_mut();
        let mut length = 0;
        unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
            .context("decoded picture lock failed")?;
        let required = width as usize * height as usize * 3 / 2;
        if data.is_null() || (length as usize) < required {
            let _ = unsafe { buffer.Unlock() };
            bail!("decoded picture holds {length} bytes; {width}x{height} NV12 needs {required}");
        }
        let context = unsafe { self.device.GetImmediateContext() };
        let result = context.map(|context| unsafe {
            context.UpdateSubresource(&texture, 0, None, data.cast(), width, 0)
        });
        let _ = unsafe { buffer.Unlock() };
        result.context("D3D11 immediate context unavailable")?;
        Ok(texture)
    }
}

unsafe fn nv12_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> anyhow::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // The shader conversion reads its planes. NVIDIA's video processor
        // rejects a shader-resource-only NV12 input, as it does BGRA.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .with_context(|| format!("{width}x{height} NV12 upload texture creation failed"))?;
    texture.context("D3D11 returned no NV12 upload texture")
}
