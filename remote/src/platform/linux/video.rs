//! Draws the decoded picture: its three planes are textures, and a shader
//! converts them to RGB and scales them to the window. The conversion is
//! the Windows viewer's, studio-range BT.709 to full-range RGB.

use super::decoder::Picture;
use super::gpu::Gpu;
use crate::video_layout::VideoRect;

const SHADER: &str = r"
struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle that covers the viewport, which is the video's rectangle.
@vertex
fn vertex_main(@builtin(vertex_index) index: u32) -> Vertex {
    var vertex: Vertex;
    vertex.uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    vertex.position = vec4<f32>(vertex.uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    return vertex;
}

@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var blue_difference: texture_2d<f32>;
@group(0) @binding(2) var red_difference: texture_2d<f32>;
@group(0) @binding(3) var bilinear: sampler;

fn rgb(uv: vec2<f32>) -> vec3<f32> {
    let y = 1.164383 * (textureSample(luma, bilinear, uv).r - 16.0 / 255.0);
    let cb = textureSample(blue_difference, bilinear, uv).r - 128.0 / 255.0;
    let cr = textureSample(red_difference, bilinear, uv).r - 128.0 / 255.0;
    return clamp(
        vec3<f32>(
            y + 1.792741 * cr,
            y - 0.213249 * cb - 0.532909 * cr,
            y + 2.112402 * cb),
        vec3<f32>(0.0),
        vec3<f32>(1.0));
}

// For a surface that takes gamma-encoded colors as they are.
@fragment
fn fragment_encoded(vertex: Vertex) -> @location(0) vec4<f32> {
    return vec4<f32>(rgb(vertex.uv), 1.0);
}

// For an sRGB surface, which encodes what it is given.
@fragment
fn fragment_linear(vertex: Vertex) -> @location(0) vec4<f32> {
    let encoded = rgb(vertex.uv);
    let low = encoded / 12.92;
    let high = pow((encoded + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return vec4<f32>(select(high, low, encoded <= vec3<f32>(0.04045)), 1.0);
}
";

struct Planes {
    size: (u32, u32),
    chroma_size: (u32, u32),
    textures: [wgpu::Texture; 3],
    bind_group: wgpu::BindGroup,
}

pub(super) struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    planes: Option<Planes>,
}

impl VideoRenderer {
    pub(super) fn new(gpu: &Gpu, target: wgpu::TextureFormat) -> Self {
        let device = &gpu.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video planes"),
            entries: &[
                texture(0),
                texture(1),
                texture(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(if target.is_srgb() {
                    "fragment_linear"
                } else {
                    "fragment_encoded"
                }),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            planes: None,
        }
    }

    /// The size of the picture on screen, if one has been uploaded.
    pub(super) fn picture_size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(|planes| planes.size)
    }

    /// Whether the GPU can hold a picture of this size.
    pub(super) fn fits(gpu: &Gpu, width: u32, height: u32) -> bool {
        let limit = gpu.max_texture_side();
        width <= limit && height <= limit
    }

    fn create_planes(&self, gpu: &Gpu, picture: &Picture) -> Planes {
        let texture = |label, (width, height): (u32, u32)| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let size = (picture.width, picture.height);
        let chroma_size = (picture.chroma_width, picture.chroma_height);
        let textures = [
            texture("video luma", size),
            texture("video blue difference", chroma_size),
            texture("video red difference", chroma_size),
        ];
        let views = textures
            .each_ref()
            .map(|texture| texture.create_view(&wgpu::TextureViewDescriptor::default()));
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video planes"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&views[1]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&views[2]),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        Planes {
            size,
            chroma_size,
            textures,
            bind_group,
        }
    }

    /// Makes `picture` the one drawn. The caller has checked that it fits.
    pub(super) fn upload(&mut self, gpu: &Gpu, picture: &Picture) {
        let size = (picture.width, picture.height);
        let chroma_size = (picture.chroma_width, picture.chroma_height);
        if self
            .planes
            .as_ref()
            .is_none_or(|planes| planes.size != size || planes.chroma_size != chroma_size)
        {
            self.planes = Some(self.create_planes(gpu, picture));
        }
        let Some(planes) = &self.planes else {
            return;
        };
        for (texture, data, (width, height)) in [
            (&planes.textures[0], &picture.y, size),
            (&planes.textures[1], &picture.u, chroma_size),
            (&planes.textures[2], &picture.v, chroma_size),
        ] {
            gpu.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    /// Draws the picture into `area`, in the surface's physical pixels.
    pub(super) fn draw(&self, pass: &mut wgpu::RenderPass<'_>, area: VideoRect) {
        let Some(planes) = &self.planes else {
            return;
        };
        if area.width <= 0 || area.height <= 0 {
            return;
        }
        pass.set_viewport(
            area.left as f32,
            area.top as f32,
            area.width as f32,
            area.height as f32,
            0.0,
            1.0,
        );
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &planes.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
