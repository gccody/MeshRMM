//! GPU-only grayscale pass shared by WGC and Desktop Duplication capture.
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::converter::Error;

const SHADER: &str = r#"
Texture2D<float4> source : register(t0);
RWTexture2D<float4> destination : register(u0);
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint width, height;
    destination.GetDimensions(width, height);
    if (id.x >= width || id.y >= height) return;
    float4 pixel = source.Load(int3(id.xy, 0));
    float gray = dot(pixel.rgb, float3(0.2126, 0.7152, 0.0722));
    destination[id.xy] = float4(gray, gray, gray, pixel.a);
}
"#;

pub(crate) struct GrayscalePass {
    context: ID3D11DeviceContext,
    source: ID3D11Texture2D,
    source_view: ID3D11ShaderResourceView,
    output: ID3D11Texture2D,
    output_view: ID3D11UnorderedAccessView,
    shader: ID3D11ComputeShader,
    region: D3D11_BOX,
}

impl GrayscalePass {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self, Error> {
        // All resources belong to the capture device. Allocation and compilation
        // happen once per stream, never in the per-frame path.
        unsafe {
            let mut bytecode = None;
            D3DCompile(
                SHADER.as_ptr().cast(),
                SHADER.len(),
                None,
                None,
                None,
                s!("main"),
                s!("cs_5_0"),
                0,
                0,
                &mut bytecode,
                None,
            )
            .map_err(Error::Processor)?;
            let bytecode = bytecode.ok_or(Error::MissingGrayscaleResource)?;
            let bytes = std::slice::from_raw_parts(
                bytecode.GetBufferPointer().cast(),
                bytecode.GetBufferSize(),
            );
            let mut shader = None;
            device
                .CreateComputeShader(bytes, None, Some(&mut shader))
                .map_err(Error::Processor)?;
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
            let mut source = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut source))
                .map_err(Error::Processor)?;
            let source = source.ok_or(Error::MissingGrayscaleResource)?;
            let mut source_view = None;
            device
                .CreateShaderResourceView(&source, None, Some(&mut source_view))
                .map_err(Error::Processor)?;
            // RGBA supports typed UAV writes, unlike BGRA on some D3D11 devices.
            let output_desc = D3D11_TEXTURE2D_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                BindFlags: (D3D11_BIND_UNORDERED_ACCESS.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                ..desc
            };
            let mut output = None;
            device
                .CreateTexture2D(&output_desc, None, Some(&mut output))
                .map_err(Error::Processor)?;
            let output = output.ok_or(Error::MissingGrayscaleResource)?;
            let mut output_view = None;
            device
                .CreateUnorderedAccessView(&output, None, Some(&mut output_view))
                .map_err(Error::Processor)?;
            Ok(Self {
                context: context.clone(),
                source,
                output,
                source_view: source_view.ok_or(Error::MissingGrayscaleResource)?,
                output_view: output_view.ok_or(Error::MissingGrayscaleResource)?,
                shader: shader.ok_or(Error::MissingGrayscaleResource)?,
                region: D3D11_BOX {
                    right: width,
                    bottom: height,
                    back: 1,
                    ..Default::default()
                },
            })
        }
    }

    pub fn convert(&self, input: &ID3D11Texture2D) -> &ID3D11Texture2D {
        // Capture surfaces need not support shader binding. Copy only the encoded
        // region (odd desktop dimensions are cropped by the capture backend).
        unsafe {
            self.context.CopySubresourceRegion(
                &self.source,
                0,
                0,
                0,
                0,
                input,
                0,
                Some(&self.region),
            );
            self.context.CSSetShader(&self.shader, None);
            self.context
                .CSSetShaderResources(0, Some(&[Some(self.source_view.clone())]));
            self.context.CSSetUnorderedAccessViews(
                0,
                1,
                Some([Some(self.output_view.clone())].as_ptr()),
                None,
            );
            self.context.Dispatch(
                self.region.right.div_ceil(8),
                self.region.bottom.div_ceil(8),
                1,
            );
            // Unbind before the video processor reads the output surface.
            self.context
                .CSSetUnorderedAccessViews(0, 1, Some([None].as_ptr()), None);
            self.context.CSSetShaderResources(0, Some(&[None]));
            self.context.CSSetShader(None, None);
        }
        &self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a D3D11 device; run explicitly on the Windows endpoint"]
    fn gpu_grayscale_preserves_luminance_and_handles_partial_workgroups() {
        let (device, context) = windows_capture::d3d11::create_d3d_device().unwrap();
        let pass = GrayscalePass::new(&device, &context, 3, 1).unwrap();
        // BGRA red, green, blue. Output is RGBA with BT.709 luminance.
        let pixels: [u8; 12] = [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255];
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            pass.source.GetDesc(&mut desc);
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: pixels.as_ptr().cast(),
                SysMemPitch: 12,
                ..Default::default()
            };
            let mut input = None;
            device
                .CreateTexture2D(&desc, Some(&data), Some(&mut input))
                .unwrap();
            let output = pass.convert(input.as_ref().unwrap());
            output.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            context.CopyResource(&staging, output);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let actual = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), 12).to_vec();
            context.Unmap(&staging, 0);
            for (pixel, expected) in actual.chunks_exact(4).zip([54u8, 182, 18]) {
                assert_eq!(pixel[0], pixel[1]);
                assert_eq!(pixel[1], pixel[2]);
                assert!(pixel[0].abs_diff(expected) <= 1, "{pixel:?} vs {expected}");
                assert_eq!(pixel[3], 255);
            }
        }
    }
}
