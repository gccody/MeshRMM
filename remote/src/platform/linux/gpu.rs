//! Drawing with the GPU through wgpu: Vulkan where the computer has it,
//! OpenGL otherwise, and Mesa's software drivers on a computer with neither,
//! such as a virtual machine. The viewer's windows share one device; each
//! has a [`Canvas`] with its surface and its controls.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use winit::dpi::PhysicalSize;
use winit::window::Window;

/// Runs a future that the GPU driver completes, parking the thread while it
/// is pending. wgpu's native futures are ready at once.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => return output,
            std::task::Poll::Pending => std::thread::park(),
        }
    }
}

/// The GPU device every window of the viewer draws with.
pub(super) struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    pub(super) device: wgpu::Device,
    pub(super) queue: wgpu::Queue,
}

impl Gpu {
    /// Opens the device that can draw to `window`, the viewer's first, and
    /// returns that window's surface with it.
    pub(super) fn new(
        display: winit::event_loop::OwnedDisplayHandle,
        window: Arc<Window>,
    ) -> anyhow::Result<(Self, wgpu::Surface<'static>)> {
        let mut descriptor = wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display));
        descriptor.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        let instance = wgpu::Instance::new(descriptor.with_env());
        let surface = instance
            .create_surface(window)
            .context("could not create a drawing surface for the viewer window")?;
        let request = |force_fallback_adapter| {
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                // The video is one textured rectangle; any GPU draws it.
                power_preference: wgpu::PowerPreference::LowPower,
                force_fallback_adapter,
                compatible_surface: Some(&surface),
                ..Default::default()
            }))
        };
        let adapter = request(false)
            .or_else(|_| request(true))
            .map_err(|error| anyhow::anyhow!("{error}"))
            .context(
                "this computer has no Vulkan or OpenGL driver the viewer can draw with; \
                 install your distribution's Mesa drivers",
            )?;
        let info = adapter.get_info();
        tracing::info!(
            adapter = %info.name,
            backend = ?info.backend,
            device_type = ?info.device_type,
            driver = %info.driver_info,
            "the viewer draws with this GPU adapter"
        );
        // The limits every backend has, with textures as large as the
        // adapter's so a 4K or 5K picture fits.
        let limits = wgpu::Limits {
            max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d,
            ..wgpu::Limits::downlevel_webgl2_defaults()
        };
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("MeshRMM viewer"),
            required_limits: limits,
            ..Default::default()
        }))
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("could not open the GPU device")?;
        Ok((
            Self {
                instance,
                adapter,
                device,
                queue,
            },
            surface,
        ))
    }

    pub(super) fn create_surface(
        &self,
        window: Arc<Window>,
    ) -> anyhow::Result<wgpu::Surface<'static>> {
        self.instance
            .create_surface(window)
            .context("could not create a drawing surface for the viewer window")
    }

    /// The largest texture side, which is the largest picture it can show.
    pub(super) fn max_texture_side(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
}

/// The least time between two paints of a window. The surface never waits
/// for the display, so nothing else holds a busy window to its refresh rate.
const FRAME: Duration = Duration::from_millis(8);
/// How often a window with nothing to draw looks for changes anyway.
const IDLE: Duration = Duration::from_secs(1);
/// How long a window that was asked to draw and did not is left alone, as
/// one the desktop has hidden is.
const UNANSWERED: Duration = Duration::from_millis(250);

/// What a paint did.
pub(super) struct Painted {
    /// Whether a frame reached the screen.
    pub(super) presented: bool,
}

/// A window's surface and its egui controls.
pub(super) struct Canvas {
    pub(super) window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    configuration: wgpu::SurfaceConfiguration,
    configured: bool,
    renderer: egui_wgpu::Renderer,
    pub(super) egui: egui::Context,
    pub(super) input: egui_winit::State,
    last_paint: Instant,
    next_paint: Instant,
}

impl Canvas {
    pub(super) fn new(
        gpu: &Gpu,
        window: Arc<Window>,
        surface: wgpu::Surface<'static>,
    ) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let capabilities = surface.get_capabilities(&gpu.adapter);
        // The video and the controls are drawn already gamma-encoded.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| {
                matches!(
                    format,
                    wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                )
            })
            .or_else(|| capabilities.formats.first().copied())
            .context("the GPU offers no format to draw the viewer window in")?;
        // Never wait for the display: a hidden Wayland window gets no frame
        // to wait for, and the wait would hold up the technician's input.
        let present_mode = [
            wgpu::PresentMode::Mailbox,
            wgpu::PresentMode::Immediate,
            wgpu::PresentMode::Fifo,
        ]
        .into_iter()
        .find(|mode| capabilities.present_modes.contains(mode))
        .unwrap_or(wgpu::PresentMode::Fifo);
        let mut configuration = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .context("the GPU cannot draw to the viewer window")?;
        configuration.format = format;
        configuration.present_mode = present_mode;
        configuration.view_formats = vec![format];
        configuration.desired_maximum_frame_latency = 1;
        tracing::info!(?format, ?present_mode, "configured a viewer window surface");

        let egui = egui::Context::default();
        super::style::apply(&egui);
        let input = egui_winit::State::new(
            egui.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(gpu.max_texture_side() as usize),
        );
        let renderer =
            egui_wgpu::Renderer::new(&gpu.device, format, egui_wgpu::RendererOptions::default());
        let mut canvas = Self {
            window,
            surface,
            configuration,
            configured: false,
            renderer,
            egui,
            input,
            last_paint: Instant::now(),
            next_paint: Instant::now(),
        };
        canvas.configure(gpu);
        Ok(canvas)
    }

    pub(super) fn format(&self) -> wgpu::TextureFormat {
        self.configuration.format
    }

    /// The surface's size in physical pixels.
    pub(super) fn size(&self) -> PhysicalSize<u32> {
        PhysicalSize::new(self.configuration.width, self.configuration.height)
    }

    fn configure(&mut self, gpu: &Gpu) {
        let size = self.window.inner_size();
        // A minimised window has no size; it keeps its last configuration.
        if size.width == 0 || size.height == 0 {
            return;
        }
        let limit = gpu.max_texture_side();
        self.configuration.width = size.width.min(limit);
        self.configuration.height = size.height.min(limit);
        self.surface.configure(&gpu.device, &self.configuration);
        self.configured = true;
    }

    /// The window changed size.
    pub(super) fn resized(&mut self, gpu: &Gpu) {
        self.configure(gpu);
    }

    /// Something the window shows changed: it draws soon, though no sooner
    /// than a frame after it last did.
    pub(super) fn invalidate(&mut self) {
        self.next_paint = self.next_paint.min(self.last_paint + FRAME);
    }

    /// Draws again within `limit` even if nothing changes, as for a clock.
    pub(super) fn paint_within(&mut self, limit: Duration) {
        self.next_paint = self.next_paint.min(self.last_paint + limit.max(FRAME));
    }

    /// Asks the desktop for a redraw when one is due. Returns when to ask
    /// again.
    pub(super) fn poll(&mut self, now: Instant) -> Instant {
        if self.next_paint <= now {
            self.window.request_redraw();
            self.next_paint = now + UNANSWERED;
        }
        self.next_paint
    }

    /// Runs the controls and draws the window: `under` first, beneath the
    /// controls, then the controls.
    pub(super) fn paint(
        &mut self,
        gpu: &Gpu,
        clear: wgpu::Color,
        controls: impl FnMut(&mut egui::Ui),
        under: impl FnOnce(&mut wgpu::RenderPass<'_>, PhysicalSize<u32>),
    ) -> Painted {
        let now = Instant::now();
        self.last_paint = now;
        let raw_input = self.input.take_egui_input(&self.window);
        let mut output = self.egui.run_ui(raw_input, controls);
        self.input
            .handle_platform_output(&self.window, output.platform_output);
        // When the controls want to be drawn again without an event, such
        // as for an animation or a tooltip's delay.
        let repaint_after = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(IDLE, |viewport| viewport.repaint_delay);
        self.next_paint = now + repaint_after.clamp(FRAME, IDLE);
        // The controls' textures change whether or not this frame is shown.
        for (id, deltas) in output.textures_delta.set.drain() {
            for delta in &deltas {
                self.renderer
                    .update_texture(&gpu.device, &gpu.queue, id, delta);
            }
        }
        let primitives = self.egui.tessellate(output.shapes, output.pixels_per_point);
        let presented = self.draw(gpu, clear, &primitives, output.pixels_per_point, under);
        for id in output.textures_delta.free.drain() {
            self.renderer.free_texture(&id);
        }
        Painted { presented }
    }

    /// Draws a frame and shows it. Returns whether it reached the screen.
    fn draw(
        &mut self,
        gpu: &Gpu,
        clear: wgpu::Color,
        primitives: &[egui::ClippedPrimitive],
        pixels_per_point: f32,
        under: impl FnOnce(&mut wgpu::RenderPass<'_>, PhysicalSize<u32>),
    ) -> bool {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return false;
        }
        if !self.configured
            || size.width.min(gpu.max_texture_side()) != self.configuration.width
            || size.height.min(gpu.max_texture_side()) != self.configuration.height
        {
            self.configure(gpu);
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // Drawn as it is; the next paint reconfigures.
                self.configured = false;
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configure(gpu);
                self.next_paint = self.last_paint + FRAME;
                return false;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                match gpu.create_surface(Arc::clone(&self.window)) {
                    Ok(surface) => {
                        self.surface = surface;
                        self.configure(gpu);
                        self.next_paint = self.last_paint + FRAME;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not replace a lost window surface");
                    }
                }
                return false;
            }
            // Hidden, or the display took too long: a later paint draws.
            other => {
                tracing::debug!(status = ?other, "the window surface gave no frame to draw");
                return false;
            }
        };
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.configuration.width, self.configuration.height],
            pixels_per_point,
        };
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("viewer window"),
            });
        let user_commands = self.renderer.update_buffers(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            primitives,
            &screen,
        );
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("viewer window"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                        depth_slice: None,
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            under(&mut pass, self.size());
            self.renderer.render(&mut pass, primitives, &screen);
        }
        gpu.queue
            .submit(user_commands.into_iter().chain([encoder.finish()]));
        self.window.pre_present_notify();
        gpu.queue.present(frame);
        true
    }
}
