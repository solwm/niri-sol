//! Per-output rendering state.

use std::path::PathBuf;

use anyhow::{Context, Result};
use khronos_egl as egl;
use smithay_client_toolkit::shell::{wlr_layer::LayerSurface, WaylandSurface};
use wayland_client::protocol::wl_output;
use wayland_client::Proxy;

use crate::egl::Egl;
use crate::render::Renderer;
use crate::Fit;

pub struct PerOutput {
    /// Held to keep the wl_output binding alive for the layer surface; not
    /// otherwise read after construction.
    #[allow(dead_code)]
    pub wl_output: wl_output::WlOutput,
    pub name: Option<String>,
    pub layer: LayerSurface,
    pub scale: i32,

    /// Set once the user-provided image is loaded and uploaded; until then
    /// we draw nothing (the layer remains transparent).
    pub image_path: Option<PathBuf>,

    /// `wl_egl_window` lives as long as the EGLSurface. `None` until first
    /// configure event tells us the size.
    pub egl_window: Option<wayland_egl::WlEglSurface>,
    pub egl_surface: Option<egl::Surface>,

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
            image_path: None,
            egl_window: None,
            egl_surface: None,
            size_px: (0, 0),
        }
    }

    /// Recreate the `wl_egl_window` + `EGLSurface` for a new size, or resize
    /// in place if both already exist.
    pub fn configure_size(
        &mut self,
        egl: &Egl,
        logical_w: i32,
        logical_h: i32,
    ) -> Result<()> {
        let w = (logical_w.max(1)) * self.scale.max(1);
        let h = (logical_h.max(1)) * self.scale.max(1);
        if (w, h) == self.size_px && self.egl_surface.is_some() {
            return Ok(());
        }
        self.size_px = (w, h);
        self.layer
            .wl_surface()
            .set_buffer_scale(self.scale.max(1));

        if let Some(window) = self.egl_window.as_mut() {
            window.resize(w, h, 0, 0);
        } else {
            let surface = self.layer.wl_surface().clone();
            let window = wayland_egl::WlEglSurface::new(surface.id(), w, h)
                .context("create wl_egl_window")?;
            let egl_surface = egl.create_window_surface(&window)?;
            self.egl_window = Some(window);
            self.egl_surface = Some(egl_surface);
        }
        Ok(())
    }

    /// Draw the assigned image to this output. No-op if no image is set or
    /// the EGL surface hasn't been created yet.
    pub fn draw(
        &self,
        egl: &Egl,
        renderer: &mut Renderer,
        decoded: &crate::img::DecodedImage,
        fit: Fit,
    ) -> Result<()> {
        let Some(surface) = self.egl_surface else {
            return Ok(());
        };
        let Some(path) = &self.image_path else {
            return Ok(());
        };

        egl.make_current(surface)?;

        let (tex, iw, ih) = renderer.upload(path, decoded)?;
        renderer.draw(tex, iw, ih, self.size_px.0, self.size_px.1, fit);

        egl.swap_buffers(surface)?;
        // Don't release the context — keeping it current is cheaper if we
        // immediately render to the next output.
        Ok(())
    }
}

impl Drop for PerOutput {
    fn drop(&mut self) {
        // EGLSurface depends on wl_egl_window, which depends on wl_surface
        // (held by `layer`). Tear them down in that order.
        if let Some(_window) = self.egl_window.take() {
            // wl_egl_window's Drop destroys the underlying wl_egl_window.
        }
        // egl_surface is destroyed via Egl::destroy_surface; we can't do
        // that here without a reference to Egl. The Egl handle outlives all
        // PerOutput instances (it lives in AppState alongside this map and
        // is dropped after), so leaking the EGLSurface handle here is fine:
        // eglTerminate in Egl::drop destroys remaining surfaces.
    }
}
