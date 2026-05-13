//! Per-output wgpu surface + per-frame draw.

use anyhow::{Context, Result};
use smithay_client_toolkit::shell::{wlr_layer::LayerSurface, WaylandSurface};
use wayland_client::protocol::wl_output;
use wayland_client::{Connection, QueueHandle, Proxy};

use crate::gpu::{self, Gpu, Uniforms};

pub struct PerOutput {
    /// Held to keep the wl_output binding alive for the layer surface; not
    /// otherwise read after construction.
    #[allow(dead_code)]
    pub wl_output: wl_output::WlOutput,
    pub name: Option<String>,
    pub layer: LayerSurface,
    pub scale: i32,

    /// Wgpu surface bound to `layer.wl_surface()`. Created lazily on the
    /// first `configure_size` once we know the physical pixel size.
    pub surface: Option<wgpu::Surface<'static>>,
    /// Last `configure()` we applied to the surface. We re-apply only on
    /// resize since `present()` doesn't care about identical configs.
    pub config: Option<wgpu::SurfaceConfiguration>,

    /// Per-output uniform buffer + bind group. Holds (resolution, time).
    /// Recreated alongside the surface.
    pub uniforms: Option<wgpu::Buffer>,
    pub bind_group: Option<wgpu::BindGroup>,

    /// Whether we've already requested a frame callback that hasn't
    /// fired yet — used so we don't pile up callbacks.
    pub frame_requested: bool,

    /// Current buffer size in physical pixels (logical size × scale).
    pub size_px: (i32, i32),
}

impl PerOutput {
    pub fn new(wl_output: wl_output::WlOutput, layer: LayerSurface) -> Self {
        Self {
            wl_output,
            name: None,
            layer,
            scale: 1,
            surface: None,
            config: None,
            uniforms: None,
            bind_group: None,
            frame_requested: false,
            size_px: (0, 0),
        }
    }

    /// (Re)configure the wgpu surface for the current physical size.
    /// Creates the surface lazily on first call.
    pub fn configure_size(
        &mut self,
        conn: &Connection,
        gpu: &Gpu,
        logical_w: i32,
        logical_h: i32,
    ) -> Result<()> {
        let w = (logical_w.max(1)) * self.scale.max(1);
        let h = (logical_h.max(1)) * self.scale.max(1);
        if (w, h) == self.size_px && self.surface.is_some() {
            return Ok(());
        }
        self.size_px = (w, h);
        self.layer
            .wl_surface()
            .set_buffer_scale(self.scale.max(1));

        // Create the surface if we don't have one. (It may already have
        // been pre-populated by `AppState::ensure_gpu` if this output
        // was the one used to probe the wgpu adapter.)
        if self.surface.is_none() {
            let surface_id = self.layer.wl_surface().id();
            let surface = unsafe { gpu::create_surface(&gpu.instance, conn, &surface_id)? };
            self.surface = Some(surface);
        }

        // Uniforms + bind group are always allocated alongside the
        // surface; check independently because the probe-surface case
        // gives us a surface without the uniform plumbing.
        if self.uniforms.is_none() {
            let uniforms = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("sol-wallpaper-uniforms"),
                size: std::mem::size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("sol-wallpaper-bg"),
                layout: &gpu.bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                }],
            });
            self.uniforms = Some(uniforms);
            self.bind_group = Some(bind_group);
        }

        let surface = self.surface.as_ref().unwrap();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: gpu.surface_format,
            width: w as u32,
            height: h as u32,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&gpu.device, &config);
        self.config = Some(config);
        Ok(())
    }

    /// Draw a frame and present. Quietly bails out if the surface
    /// hasn't been configured yet (we'll redraw on the configure
    /// event).
    pub fn draw(&mut self, gpu: &Gpu) -> Result<()> {
        let (Some(surface), Some(uniforms), Some(bind_group)) =
            (self.surface.as_ref(), self.uniforms.as_ref(), self.bind_group.as_ref())
        else {
            return Ok(());
        };

        let elapsed = gpu.start.elapsed().as_secs_f32();
        let u = Uniforms {
            resolution: [self.size_px.0 as f32, self.size_px.1 as f32],
            time: elapsed,
            _pad: 0.0,
        };
        gpu.queue.write_buffer(uniforms, 0, bytemuck::bytes_of(&u));

        let frame = match surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                // Surface needs reconfigure. The next `configure()` from
                // the compositor will trigger that; for now drop this
                // frame.
                if let Some(cfg) = &self.config {
                    surface.configure(&gpu.device, cfg);
                }
                return Ok(());
            }
            Err(err) => {
                return Err(anyhow::anyhow!("get_current_texture: {err:?}"));
            }
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sol-wallpaper-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sol-wallpaper-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&gpu.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        gpu.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// Request a frame callback so we get woken up for the next frame.
    /// Idempotent: if a callback is already pending, do nothing.
    pub fn request_frame<S>(&mut self, qh: &QueueHandle<S>)
    where
        S: 'static
            + wayland_client::Dispatch<
                wayland_client::protocol::wl_callback::WlCallback,
                wayland_client::protocol::wl_surface::WlSurface,
            >,
    {
        if self.frame_requested {
            return;
        }
        self.frame_requested = true;
        let surface = self.layer.wl_surface().clone();
        self.layer.wl_surface().frame(qh, surface);
        self.layer.wl_surface().commit();
    }
}
