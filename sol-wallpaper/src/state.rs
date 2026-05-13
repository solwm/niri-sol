//! `AppState` ties together: sctk handlers, the shared wgpu device,
//! per-output wgpu surfaces, and the frame-callback driven render loop.

use std::collections::HashMap;

use anyhow::{Context, Result};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_surface},
    Connection, Proxy, QueueHandle,
};

use crate::gpu::Gpu;
use crate::output::PerOutput;
use crate::Runtime;

/// Baked-in default shader. Used when the user hasn't supplied a path
/// in `wallpaper.conf` (or that path fails to read).
const DEFAULT_SHADER: &str = include_str!("../shaders/default.wgsl");

pub struct AppState {
    pub registry: RegistryState,
    pub compositor: CompositorState,
    pub output_state: OutputState,
    pub layer_shell: LayerShell,

    /// Held for raw wl_display pointer extraction (wgpu surface
    /// creation needs it for every output).
    pub conn: Connection,

    /// Shared wgpu state. Initialized lazily on the first output's
    /// configure event: we need a Wayland surface to probe an adapter.
    pub gpu: Option<Gpu>,
    /// Shader source actually in use. Either the user-supplied file or
    /// `DEFAULT_SHADER`. Stored so we can rebuild the pipeline on hot
    /// reload (not implemented yet).
    pub shader_source: String,

    /// Keyed on the `wl_output`'s ObjectId protocol id (u32).
    pub outputs: HashMap<u32, PerOutput>,

    pub running: bool,
}

pub fn run(runtime: Runtime) -> Result<()> {
    let conn = Connection::connect_to_env().context("connect to wayland")?;
    let (globals, event_queue) = registry_queue_init(&conn).context("registry init")?;
    let qh: QueueHandle<AppState> = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("bind wl_compositor")?;
    let output_state = OutputState::new(&globals, &qh);
    let layer_shell = LayerShell::bind(&globals, &qh).context("bind zwlr_layer_shell_v1")?;
    let registry = RegistryState::new(&globals);

    let shader_source = crate::gpu::Gpu::load_shader_source(runtime.shader_path.as_deref())
        .unwrap_or_else(|| DEFAULT_SHADER.to_string());

    let mut state = AppState {
        registry,
        compositor,
        output_state,
        layer_shell,
        conn: conn.clone(),
        gpu: None,
        shader_source,
        outputs: HashMap::new(),
        running: true,
    };

    let mut event_loop: EventLoop<'static, AppState> =
        EventLoop::try_new().context("create event loop")?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;

    while state.running {
        event_loop
            .dispatch(None, &mut state)
            .context("event loop dispatch")?;
    }
    Ok(())
}

impl AppState {
    /// Initialize the shared GPU using `surface_id` as the probe
    /// surface for adapter selection. Becomes a no-op once gpu is set.
    /// On success, the probe surface is bound to the corresponding
    /// PerOutput so we don't waste it.
    fn ensure_gpu(&mut self, oid: u32) {
        if self.gpu.is_some() {
            return;
        }
        let Some(per_output) = self.outputs.get(&oid) else {
            return;
        };
        let surface_id = per_output.layer.wl_surface().id();
        match Gpu::new(&self.conn, &surface_id, &self.shader_source) {
            Ok((gpu, probe_surface)) => {
                self.gpu = Some(gpu);
                // The probe surface is for this output; keep it.
                if let Some(p) = self.outputs.get_mut(&oid) {
                    p.surface = Some(probe_surface);
                }
            }
            Err(err) => {
                tracing::error!("wgpu init failed: {err:?}");
            }
        }
    }

    fn configure_and_draw(&mut self, oid: u32, logical_w: i32, logical_h: i32, qh: &QueueHandle<Self>) {
        self.ensure_gpu(oid);
        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        if let Some(p) = self.outputs.get_mut(&oid) {
            if let Err(err) = p.configure_size(&self.conn, gpu, logical_w, logical_h) {
                tracing::error!("configure_size: {err:?}");
                return;
            }
            if let Err(err) = p.draw(gpu) {
                tracing::error!("draw: {err:?}");
                return;
            }
            p.request_frame(qh);
        }
    }
}

// ---------- sctk handler implementations ----------

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let id = surface.id().protocol_id();
        let oid_opt = self.outputs.iter().find_map(|(oid, p)| {
            (p.layer.wl_surface().id().protocol_id() == id).then_some(*oid)
        });
        let Some(oid) = oid_opt else { return };

        let scale = new_factor.max(1);
        let (lw, lh) = {
            let Some(p) = self.outputs.get_mut(&oid) else {
                return;
            };
            if p.scale == scale {
                return;
            }
            p.scale = scale;
            let (w, h) = p.size_px;
            (
                (w / p.scale.max(1)).max(1),
                (h / p.scale.max(1)).max(1),
            )
        };
        self.configure_and_draw(oid, lw, lh, qh);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Find which output's surface this callback was for, draw the
        // next frame, and immediately request another callback.
        let id = surface.id().protocol_id();
        let oid_opt = self.outputs.iter().find_map(|(oid, p)| {
            (p.layer.wl_surface().id().protocol_id() == id).then_some(*oid)
        });
        let Some(oid) = oid_opt else { return };

        let Some(gpu) = self.gpu.as_ref() else { return };
        if let Some(p) = self.outputs.get_mut(&oid) {
            p.frame_requested = false;
            if let Err(err) = p.draw(gpu) {
                tracing::error!("frame draw: {err:?}");
                return;
            }
            p.request_frame(qh);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let info = self.output_state.info(&output);
        let name = info.as_ref().and_then(|i| i.name.clone());
        let scale = info.as_ref().map(|i| i.scale_factor).unwrap_or(1).max(1);

        let surface = self.compositor.create_surface(qh);

        // Mark the surface as fully opaque — sol's compositor uses this
        // to skip drawing whatever is "behind" the wallpaper layer.
        if let Ok(region) = Region::new(&self.compositor) {
            region.add(0, 0, i32::MAX, i32::MAX);
            surface.set_opaque_region(Some(region.wl_region()));
        }

        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("sol-wallpaper"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();

        let oid = output.id().protocol_id();
        let mut per_output = PerOutput::new(output, layer);
        per_output.scale = scale;
        per_output.name = name.clone();
        self.outputs.insert(oid, per_output);

        tracing::info!(
            "new output: id={oid} name={:?} scale={scale}",
            name.unwrap_or_default()
        );
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let oid = output.id().protocol_id();
        let info = self.output_state.info(&output);
        let name = info.as_ref().and_then(|i| i.name.clone());
        let scale = info.as_ref().map(|i| i.scale_factor).unwrap_or(1).max(1);

        if let Some(p) = self.outputs.get_mut(&oid) {
            if p.name.is_none() && name.is_some() {
                p.name = name;
            }
            if p.scale != scale {
                p.scale = scale;
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let oid = output.id().protocol_id();
        if let Some(p) = self.outputs.remove(&oid) {
            tracing::info!(
                "output destroyed: id={oid} name={:?}",
                p.name.as_deref().unwrap_or("")
            );
            // Surface, uniforms, bind group drop here.
        }
    }
}

impl LayerShellHandler for AppState {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
    ) {
        let surface_id = layer.wl_surface().id().protocol_id();
        let oid = self.outputs.iter().find_map(|(id, p)| {
            (p.layer.wl_surface().id().protocol_id() == surface_id).then_some(*id)
        });
        if let Some(oid) = oid {
            self.outputs.remove(&oid);
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let surface_id = layer.wl_surface().id().protocol_id();
        let oid_opt = self.outputs.iter().find_map(|(id, p)| {
            (p.layer.wl_surface().id().protocol_id() == surface_id).then_some(*id)
        });
        let Some(oid) = oid_opt else { return };

        let (w, h) = configure.new_size;
        self.configure_and_draw(oid, w as i32, h as i32, qh);
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(AppState);
delegate_output!(AppState);
delegate_layer!(AppState);
delegate_registry!(AppState);
