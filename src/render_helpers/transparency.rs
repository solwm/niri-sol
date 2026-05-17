//! Manual-alpha-composite render element for alpha-animated tiles.
//!
//! Replaces the BLEND-enabled draw of `OffscreenRenderElement` for the case
//! where we want to render a tile at `alpha < 1`. The shader samples the
//! tile-offscreen (`tex`) and the wallpaper backdrop (`backdrop_tex`,
//! sourced from `xray.background`), computes the alpha blend itself, and
//! writes opaque pixels. The element reports `alpha = 1` + opaque-regions
//! covering its full geometry so smithay routes the draw through the
//! BLEND-disabled path — avoiding the NVIDIA bottom-right read-modify-write
//! glitch that motivated this code.
//!
//! Only valid as a drop-in for the *top* layer of a tile; the assumption
//! that the wallpaper is what's "behind" the tile is master-stack-specific
//! (tiles never overlap each other or other surfaces).

use std::cell::RefCell;
use std::rc::Rc;

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    ffi, GlesError, GlesFrame, GlesRenderer, Uniform, UniformValue,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture as _;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::effect_buffer::EffectBuffer;
use crate::render_helpers::offscreen::OffscreenRenderElement;
use crate::render_helpers::renderer::AsGlesFrame as _;
use crate::render_helpers::shaders::Shaders;

#[derive(Debug, Clone)]
pub struct TransparencyRenderElement {
    /// Inner offscreen wraps everything we don't override (id, geometry,
    /// damage tracking). Its own `.alpha` and `.draw` are bypassed.
    inner: OffscreenRenderElement,

    /// Wallpaper offscreen, sampled as `backdrop_tex` for manual blending.
    /// The buffer is prepared by the xray pipeline earlier in the same
    /// frame; we just need its texture handle.
    backdrop_buffer: Rc<RefCell<EffectBuffer>>,

    /// The transparency factor we want the tile rendered at. Smithay still
    /// sees `alpha = 1` on the element (so it picks the BLEND-OFF path);
    /// this is fed to the shader's `tex_alpha` uniform.
    tex_alpha: f32,

    /// When true, sample the blurred version of the wallpaper offscreen
    /// (frosted-glass effect for inactive tiles). The caller must have
    /// called `backdrop_buffer.prepare(renderer, true)` in the same frame
    /// so the blurred texture is populated.
    blur: bool,
}

impl TransparencyRenderElement {
    pub fn new(
        inner: OffscreenRenderElement,
        backdrop_buffer: Rc<RefCell<EffectBuffer>>,
        tex_alpha: f32,
        blur: bool,
    ) -> Self {
        Self {
            inner,
            backdrop_buffer,
            tex_alpha,
            blur,
        }
    }
}

impl Element for TransparencyRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    /// Force smithay to see this element as fully opaque. Combined with
    /// `alpha() = 1.0` below it puts the draw on smithay's BLEND-OFF path.
    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let geo = self.geometry(scale);
        let local = Rectangle::from_size(geo.size);
        OpaqueRegions::from_slice(&[local])
    }

    fn alpha(&self) -> f32 {
        1.0
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for TransparencyRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let backdrop_texture = match self.backdrop_buffer.borrow_mut().render(frame, self.blur) {
            Ok(t) => t,
            Err(_) => {
                return <OffscreenRenderElement as RenderElement<GlesRenderer>>::draw(
                    &self.inner,
                    frame,
                    src,
                    dst,
                    damage,
                    opaque_regions,
                    _cache,
                );
            }
        };

        let program = match Shaders::get_from_frame(frame).transparency.clone() {
            Some(p) => p,
            None => {
                return <OffscreenRenderElement as RenderElement<GlesRenderer>>::draw(
                    &self.inner,
                    frame,
                    src,
                    dst,
                    damage,
                    opaque_regions,
                    _cache,
                );
            }
        };

        let bg_size = backdrop_texture.size();
        let bg_w = bg_size.w as f32;
        let bg_h = bg_size.h as f32;

        // Bind the backdrop to texture unit 1 and force GL_BLEND off. We
        // claim `alpha = 1` and full opaque_regions so smithay should pick
        // its BLEND-off draw path — but if it doesn't for any reason, the
        // shader's opaque output would still be read-modify-written and the
        // NVIDIA bottom-right L-glitch could recur. Forcing BLEND off here
        // is a belt-and-braces guarantee.
        //
        // The wallpaper offscreen is allocated by EffectBuffer via
        // `renderer.create_buffer(...)`, which uses TEXTURE_2D — not an
        // external EGLImage. So we don't need samplerExternalOES handling.
        let bg_target = ffi::TEXTURE_2D;
        let bg_tex_id = backdrop_texture.tex_id();
        let blend_was_on = frame
            .with_context(|gl| unsafe {
                let was_on = gl.IsEnabled(ffi::BLEND) != 0;

                gl.ActiveTexture(ffi::TEXTURE1);
                gl.BindTexture(bg_target, bg_tex_id);
                gl.TexParameteri(bg_target, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(bg_target, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(bg_target, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
                gl.TexParameteri(bg_target, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);
                // Restore TEXTURE0 active so render_texture's later
                // BindTexture(TEXTURE0) hits the right slot.
                gl.ActiveTexture(ffi::TEXTURE0);

                gl.Disable(ffi::BLEND);

                was_on
            })
            .unwrap_or(false);

        let uniforms = [
            Uniform::new("backdrop_tex", UniformValue::_1i(1)),
            Uniform::new("tex_alpha", UniformValue::_1f(self.tex_alpha)),
            Uniform::new("backdrop_tex_size", UniformValue::_2f(bg_w, bg_h)),
        ];

        let result = frame.render_texture_from_to(
            self.inner.texture(),
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            1.0,
            Some(&program),
            &uniforms,
        );

        // Clear the TEXTURE1 binding and restore the prior GL_BLEND state
        // so later draws aren't affected.
        frame
            .with_context(|gl| unsafe {
                gl.ActiveTexture(ffi::TEXTURE1);
                gl.BindTexture(bg_target, 0);
                gl.ActiveTexture(ffi::TEXTURE0);
                if blend_was_on {
                    gl.Enable(ffi::BLEND);
                }
            })
            .ok();

        result
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for TransparencyRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
