//! wgpu instance + device + render pipeline shared across all outputs.
//!
//! Each `PerOutput` owns its own `wgpu::Surface` and recreates the
//! swap-chain configuration on resize, but the heavyweight Vulkan/GLES
//! device + shader pipeline are created once at daemon startup and
//! reused.

use std::ffi::c_void;
use std::path::Path;
use std::ptr::NonNull;
use std::time::Instant;

use anyhow::{Context, Result};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::backend::ObjectId;
use wayland_client::Connection;

/// Uniforms uploaded per frame to the fragment shader.
///
/// `repr(C)` + bytemuck Pod: the layout matches `struct Uniforms` in the
/// WGSL file exactly. Field order, alignment, and padding all have to
/// stay in sync with `shaders/default.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Uniforms {
    pub resolution: [f32; 2],
    pub time: f32,
    pub _pad: f32,
}

/// Shared GPU state owned by `AppState`. One adapter / device / queue /
/// pipeline for the whole daemon; surfaces live per output.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,

    /// Render pipeline driving the wallpaper shader. Format-agnostic at
    /// the pipeline level — we set the surface's preferred format on
    /// each `PerOutput`, and the pipeline is configured with that
    /// format at construction time.
    pub pipeline: wgpu::RenderPipeline,
    /// Bind-group layout for the uniform buffer; reused by every
    /// `PerOutput`'s bind group.
    pub bind_group_layout: wgpu::BindGroupLayout,
    /// Surface format the pipeline was built for. All outputs are
    /// configured to use this format.
    pub surface_format: wgpu::TextureFormat,

    /// Daemon-start wall-clock for the `time` uniform; monotonic and
    /// shared across outputs so the animation looks coherent on
    /// multi-monitor setups.
    pub start: Instant,
}

impl Gpu {
    /// Bootstrap wgpu using one of the daemon's already-created Wayland
    /// surfaces to pick a compatible adapter. The `probe_surface` is
    /// only used during adapter selection; afterwards it can be dropped
    /// (or, more typically, reused as the surface for that output).
    pub fn new(
        conn: &Connection,
        probe_surface_id: &ObjectId,
        shader_source: &str,
    ) -> Result<(Self, wgpu::Surface<'static>)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            // wgpu picks Vulkan on Wayland by default; GLES is a
            // fallback for systems without VK_KHR_wayland_surface.
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..Default::default()
        });

        let probe = unsafe { create_surface(&instance, conn, probe_surface_id)? };

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&probe),
            force_fallback_adapter: false,
        }))
        .context("no compatible wgpu adapter")?;

        let info = adapter.get_info();
        tracing::info!(
            "wgpu adapter: {} ({:?}, {:?})",
            info.name,
            info.backend,
            info.device_type
        );

        // Pick limits that fit the actual adapter's capabilities. The
        // default downlevel limits cap texture dimensions at 2048, which
        // fails immediately on a 4K monitor whose Wayland integer scale
        // is 2 (logical 3072×1728 × 2 = 6144×3456). Falling back to the
        // adapter's reported limits gives us up to whatever the GPU
        // actually supports (16k+ on modern hardware).
        let limits = adapter.limits();
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("sol-wallpaper-device"),
                required_features: wgpu::Features::empty(),
                required_limits: limits,
                memory_hints: wgpu::MemoryHints::Performance,
            },
            None,
        ))
        .context("request_device")?;

        // Use the surface's preferred format so we avoid extra blits
        // between RGBA/BGRA. All outputs use the same format because
        // they're driven by the same compositor and adapter.
        let caps = probe.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sol-wallpaper-shader"),
            source: wgpu::ShaderSource::Wgsl(shader_source.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("sol-wallpaper-bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sol-wallpaper-pl"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sol-wallpaper-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok((
            Self {
                instance,
                adapter,
                device,
                queue,
                pipeline,
                bind_group_layout,
                surface_format,
                start: Instant::now(),
            },
            probe,
        ))
    }

    /// Read a user-supplied WGSL file. Returns `None` if `path` is
    /// `None` or the file fails to read; callers should fall back to
    /// the baked-in default.
    pub fn load_shader_source(path: Option<&Path>) -> Option<String> {
        let p = path?;
        match std::fs::read_to_string(p) {
            Ok(s) => Some(s),
            Err(err) => {
                tracing::error!("failed to read shader {}: {err}; using default", p.display());
                None
            }
        }
    }
}

/// Construct a wgpu::Surface<'static> for a Wayland `wl_surface`.
///
/// # Safety
/// The returned surface must NOT outlive the underlying `wl_display` /
/// `wl_surface`. We rely on the fact that `Gpu` owns the `Connection`
/// (via `AppState`) for the lifetime of the daemon, and that each
/// `PerOutput` drops its surface before its `LayerSurface`.
pub unsafe fn create_surface(
    instance: &wgpu::Instance,
    conn: &Connection,
    surface_id: &ObjectId,
) -> Result<wgpu::Surface<'static>> {
    let display_ptr = conn.backend().display_ptr() as *mut c_void;
    let surface_ptr = surface_id.as_ptr() as *mut c_void;

    let display_nn = NonNull::new(display_ptr).context("null wl_display")?;
    let surface_nn = NonNull::new(surface_ptr).context("null wl_surface")?;

    let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display_nn));
    let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(surface_nn));

    let surface = instance
        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: raw_display,
            raw_window_handle: raw_window,
        })
        .context("create_surface_unsafe")?;
    Ok(surface)
}
