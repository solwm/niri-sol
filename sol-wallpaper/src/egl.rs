//! EGL display + context bootstrapped from a Wayland connection.
//!
//! One shared GLES 3.0 context is reused across every output. Each output
//! gets its own `EGLSurface` via `wl_egl_window`; we switch the active draw
//! surface with `eglMakeCurrent` before each frame.

use anyhow::{anyhow, bail, Context, Result};
use khronos_egl as egl;
use wayland_client::Connection;

pub type EglApi = egl::Instance<egl::Static>;

pub struct Egl {
    pub egl: EglApi,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
}

impl Egl {
    pub fn new(conn: &Connection) -> Result<Self> {
        let egl = egl::Instance::new(egl::Static);

        // Bind the display backed by the Wayland connection.
        let wl_display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
        let display = unsafe {
            egl.get_display(wl_display_ptr)
                .ok_or_else(|| anyhow!("eglGetDisplay returned NULL"))?
        };

        let (major, minor) = egl
            .initialize(display)
            .context("eglInitialize failed")?;
        tracing::info!("EGL {major}.{minor} initialized");

        egl.bind_api(egl::OPENGL_ES_API)
            .context("eglBindAPI(GLES) failed")?;

        let attribs = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attribs)
            .context("eglChooseConfig failed")?
            .ok_or_else(|| anyhow!("no matching EGL config"))?;

        let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
        let context = egl
            .create_context(display, config, None, &context_attribs)
            .context("eglCreateContext failed")?;

        Ok(Self { egl, display, config, context })
    }

    /// Create an EGLSurface for a `wl_egl_window` (a render target tied to a
    /// `wl_surface`).
    pub fn create_window_surface(
        &self,
        wl_egl_window: &wayland_egl::WlEglSurface,
    ) -> Result<egl::Surface> {
        let surface = unsafe {
            self.egl
                .create_window_surface(
                    self.display,
                    self.config,
                    wl_egl_window.ptr() as _,
                    None,
                )
                .context("eglCreateWindowSurface failed")?
        };
        Ok(surface)
    }

    pub fn make_current(&self, surface: egl::Surface) -> Result<()> {
        self.egl
            .make_current(self.display, Some(surface), Some(surface), Some(self.context))
            .context("eglMakeCurrent failed")?;
        Ok(())
    }

    pub fn release_current(&self) -> Result<()> {
        self.egl
            .make_current(self.display, None, None, None)
            .context("eglMakeCurrent(NULL) failed")?;
        Ok(())
    }

    pub fn swap_buffers(&self, surface: egl::Surface) -> Result<()> {
        self.egl
            .swap_buffers(self.display, surface)
            .context("eglSwapBuffers failed")?;
        Ok(())
    }

    pub fn destroy_surface(&self, surface: egl::Surface) {
        if let Err(err) = self.egl.destroy_surface(self.display, surface) {
            tracing::warn!("eglDestroySurface failed: {err}");
        }
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

/// Build a `glow::Context` by loading GLES function pointers via `eglGetProcAddress`.
///
/// Must be called with an EGL context already made current (any context will
/// do — `glow::Context` is just a function-pointer table).
pub fn load_glow(egl: &EglApi) -> Result<glow::Context> {
    let ctx = unsafe {
        glow::Context::from_loader_function(|symbol| {
            egl.get_proc_address(symbol)
                .map(|p| p as *const _)
                .unwrap_or(std::ptr::null())
        })
    };
    // Sanity-check the loader by querying the GL version.
    use glow::HasContext as _;
    let version = unsafe { ctx.get_parameter_string(glow::VERSION) };
    if version.is_empty() {
        bail!("glow loaded but glGetString(GL_VERSION) returned empty");
    }
    tracing::info!("GL version: {version}");
    Ok(ctx)
}
