//! `AppState` ties together: sctk handlers, the EGL/GL renderer, the per-output
//! map, and the cycling timer.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use calloop::timer::{TimeoutAction, Timer};
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

use crate::config::Source;
use crate::egl::Egl;
use crate::img::DecodedImage;
use crate::output::PerOutput;
use crate::render::Renderer;
use crate::Runtime;

pub struct AppState {
    pub registry: RegistryState,
    pub compositor: CompositorState,
    pub output_state: OutputState,
    pub layer_shell: LayerShell,

    pub runtime: Runtime,
    /// All eligible images discovered under `runtime.source` if it's a Dir,
    /// or the single static path wrapped in a Vec if it's an Image.
    pub pool: Vec<PathBuf>,
    /// Index into `pool` for the currently-shown cycled wallpaper. None if
    /// pool is empty.
    pub current: Option<usize>,
    /// Image cache keyed on the file path.
    pub images: HashMap<PathBuf, DecodedImage>,

    pub egl: Egl,
    pub renderer: Renderer,

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

    let egl = Egl::new(&conn).context("init EGL")?;
    let renderer = Renderer::new(&egl).context("init renderer")?;

    let pool = build_pool(&runtime)?;
    let current = if pool.is_empty() {
        None
    } else {
        Some(pick_random(pool.len(), None))
    };

    let mut state = AppState {
        registry,
        compositor,
        output_state,
        layer_shell,
        runtime,
        pool,
        current,
        images: HashMap::new(),
        egl,
        renderer,
        outputs: HashMap::new(),
        running: true,
    };

    let mut event_loop: EventLoop<'static, AppState> =
        EventLoop::try_new().context("create event loop")?;
    let handle = event_loop.handle();
    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("insert wayland source: {e}"))?;

    // Cycling timer. Only meaningful when we have >1 image in the pool.
    if state.runtime.interval_secs > 0 && state.pool.len() > 1 {
        let interval = Duration::from_secs(state.runtime.interval_secs);
        let timer = Timer::from_duration(interval);
        handle
            .insert_source(timer, move |_deadline, _, app: &mut AppState| {
                app.cycle();
                TimeoutAction::ToDuration(interval)
            })
            .map_err(|e| anyhow::anyhow!("insert cycle timer: {e}"))?;
    }

    while state.running {
        event_loop
            .dispatch(None, &mut state)
            .context("event loop dispatch")?;
    }
    Ok(())
}

/// Scan `runtime.source` (and per-output overrides) into a single pool.
/// For Image source: one entry. For Dir source: every recognized image
/// inside the dir (non-recursive). Per-output overrides are NOT included
/// in the pool — they are static and handled separately.
fn build_pool(runtime: &Runtime) -> Result<Vec<PathBuf>> {
    let Some(src) = &runtime.source else {
        return Ok(Vec::new());
    };
    match src {
        Source::Image(p) => Ok(vec![p.clone()]),
        Source::Dir(d) => {
            let mut out = Vec::new();
            let read = std::fs::read_dir(d)
                .with_context(|| format!("read_dir {}", d.display()))?;
            for entry in read {
                let entry = match entry {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::warn!("readdir error: {err}");
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let ok = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| {
                        matches!(
                            e.to_ascii_lowercase().as_str(),
                            "png" | "jpg" | "jpeg"
                        )
                    })
                    .unwrap_or(false);
                if ok {
                    out.push(path);
                }
            }
            if out.is_empty() {
                tracing::warn!("no PNG/JPG/JPEG images found in {}", d.display());
            }
            out.sort();
            Ok(out)
        }
    }
}

/// Pick a random index in `[0, len)`, biased to avoid re-picking `avoid` if
/// possible. Trivial Lehmer-style RNG seeded from the system clock — we
/// don't pull `fastrand` to keep the crate dep-light.
fn pick_random(len: usize, avoid: Option<usize>) -> usize {
    if len == 0 {
        return 0;
    }
    if len == 1 {
        return 0;
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    // xorshift64
    let mut x = seed.wrapping_mul(2685821657736338717);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let mut idx = (x as usize) % len;
    if let Some(a) = avoid {
        if idx == a {
            idx = (idx + 1) % len;
        }
    }
    idx
}

impl AppState {
    /// Lazy-decode + cache.
    fn ensure_image(&mut self, path: &PathBuf) -> bool {
        if self.images.contains_key(path) {
            return true;
        }
        match DecodedImage::load(path) {
            Ok(img) => {
                tracing::info!(
                    "loaded {} ({}×{})",
                    path.display(),
                    img.width,
                    img.height
                );
                self.images.insert(path.clone(), img);
                true
            }
            Err(err) => {
                tracing::error!("failed to load {}: {err:?}", path.display());
                false
            }
        }
    }

    /// Image path for an output: per-output override wins, else the current
    /// cycled image.
    fn path_for(&self, name: &str) -> Option<PathBuf> {
        if let Some(p) = self.runtime.per_output.get(name) {
            return Some(p.clone());
        }
        let idx = self.current?;
        self.pool.get(idx).cloned()
    }

    /// Apply the image-for-output rule to `outputs[oid]` (decoding lazily),
    /// then redraw it if it's already configured.
    fn assign_image(&mut self, oid: u32) {
        let name = self
            .outputs
            .get(&oid)
            .and_then(|p| p.name.clone());
        let Some(name) = name else { return };
        let Some(path) = self.path_for(&name) else {
            tracing::info!("no wallpaper for output `{name}` — leaving transparent");
            return;
        };
        if !self.ensure_image(&path) {
            return;
        }
        if let Some(p) = self.outputs.get_mut(&oid) {
            p.image_path = Some(path);
        }
    }

    fn draw_output(&mut self, oid: u32) {
        let Some(per_output) = self.outputs.get(&oid) else {
            return;
        };
        let Some(path) = per_output.image_path.clone() else {
            return;
        };
        let Some(decoded) = self.images.get(&path) else {
            return;
        };
        if let Err(err) =
            per_output.draw(&self.egl, &mut self.renderer, decoded, self.runtime.fit)
        {
            tracing::error!("draw failed: {err:?}");
        }
    }

    /// Pick the next cycled image, reassign + redraw every output that
    /// doesn't have a per-output override.
    fn cycle(&mut self) {
        if self.pool.len() <= 1 {
            return;
        }
        let next = pick_random(self.pool.len(), self.current);
        self.current = Some(next);
        let new_path = self.pool[next].clone();
        tracing::info!("cycle → {}", new_path.display());
        if !self.ensure_image(&new_path) {
            return;
        }
        let oids: Vec<u32> = self.outputs.keys().copied().collect();
        for oid in oids {
            let has_override = self
                .outputs
                .get(&oid)
                .and_then(|p| p.name.as_deref())
                .map(|n| self.runtime.per_output.contains_key(n))
                .unwrap_or(false);
            if has_override {
                continue;
            }
            if let Some(p) = self.outputs.get_mut(&oid) {
                p.image_path = Some(new_path.clone());
            }
            self.draw_output(oid);
        }
    }
}

// ---------- sctk handler implementations ----------

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let id = surface.id().protocol_id();
        let oid_opt = self.outputs.iter().find_map(|(oid, p)| {
            (p.layer.wl_surface().id().protocol_id() == id).then_some(*oid)
        });
        let Some(oid) = oid_opt else { return };

        let scale = new_factor.max(1);
        let Some(p) = self.outputs.get_mut(&oid) else {
            return;
        };
        if p.scale == scale {
            return;
        }
        p.scale = scale;
        let (w, h) = p.size_px;
        let lw = (w / p.scale.max(1)).max(1);
        let lh = (h / p.scale.max(1)).max(1);
        if let Err(err) = p.configure_size(&self.egl, lw, lh) {
            tracing::error!("configure_size after scale change: {err:?}");
            return;
        }
        self.draw_output(oid);
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
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Static wallpaper between cycles — no per-frame work.
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

        // Mark the surface as fully opaque so the compositor can skip
        // drawing whatever is behind us. We don't know the configured
        // surface size yet at this point, so use a region larger than any
        // realistic output; the compositor intersects with the surface
        // bounds.
        if let Ok(region) = Region::new(&self.compositor) {
            region.add(0, 0, i32::MAX, i32::MAX);
            surface.set_opaque_region(Some(region.wl_region()));
            // `region` drops here, destroying the wl_region (the surface
            // copies the region snapshot at the next commit, so destroying
            // it now is safe).
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

        if name.is_some() {
            self.assign_image(oid);
        }

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

        let mut name_just_set = false;
        if let Some(p) = self.outputs.get_mut(&oid) {
            if p.name.is_none() && name.is_some() {
                p.name = name.clone();
                name_just_set = true;
            }
            if p.scale != scale {
                p.scale = scale;
            }
        }
        if name_just_set {
            self.assign_image(oid);
            self.draw_output(oid);
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
            if let Some(s) = p.egl_surface {
                self.egl.destroy_surface(s);
            }
            tracing::info!(
                "output destroyed: id={oid} name={:?}",
                p.name.as_deref().unwrap_or("")
            );
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
            if let Some(p) = self.outputs.remove(&oid) {
                if let Some(s) = p.egl_surface {
                    self.egl.destroy_surface(s);
                }
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
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
        let w = w as i32;
        let h = h as i32;

        if let Some(p) = self.outputs.get_mut(&oid) {
            if let Err(err) = p.configure_size(&self.egl, w, h) {
                tracing::error!("configure_size: {err:?}");
                return;
            }
        }
        self.draw_output(oid);
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
