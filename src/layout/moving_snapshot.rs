//! Snapshot of a tile at its pre-move position, used for the crossfade
//! transition: the snapshot stays drawn at the old slot with declining alpha
//! while the live tile fades in at its new slot via `Tile::alpha_animation`.

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Scale, Transform};

use crate::animation::Animation;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::render_to_encompassing_texture;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

/// A frozen capture of a tile at its old position, fading out over the
/// configured crossfade duration. Created at the moment of a layout
/// mutation (e.g. master↔stack swap) and dropped once the fade is done.
#[derive(Debug)]
pub struct MovingSnapshot {
    /// Pre-rendered texture of the tile's contents at the moment of the move.
    buffer: TextureBuffer<GlesTexture>,
    /// Offset of the texture content relative to `pos` (the texture's
    /// encompassing rect may extend slightly beyond the tile's geometry to
    /// accommodate shadow / focus-ring).
    buffer_offset: Point<f64, Logical>,
    /// Where the snapshot is rendered in workspace-local coordinates (the
    /// tile's *old* slot position).
    pos: Point<f64, Logical>,
    /// The fade animation: value `0.0 → 1.0`; the snapshot's alpha is
    /// `1.0 - value`. When `is_done()`, the snapshot is removed.
    anim: Animation,
}

niri_render_elements! {
    MovingSnapshotRenderElement => {
        Texture = RelocateRenderElement<PrimaryGpuTextureRenderElement>,
    }
}

impl MovingSnapshot {
    /// Bake `snapshot` into a texture and attach a fade-out animation.
    /// `pos` is the tile's old slot position; `scale` is the output scale.
    pub fn new<E: RenderElement<GlesRenderer>>(
        renderer: &mut GlesRenderer,
        snapshot: RenderSnapshot<E, E>,
        scale: Scale<f64>,
        pos: Point<f64, Logical>,
        anim: Animation,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("MovingSnapshot::new");

        let (texture, _sync_point, geo) = render_to_encompassing_texture(
            renderer,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            &snapshot.contents,
        )
        .context("error rendering snapshot to texture")?;

        let buffer = TextureBuffer::from_texture(
            renderer,
            texture,
            scale,
            Transform::Normal,
            Vec::new(),
        );
        let buffer_offset = geo.loc.to_f64().to_logical(scale);

        Ok(Self {
            buffer,
            buffer_offset,
            pos,
            anim,
        })
    }

    pub fn advance_animations(&mut self) {
        // The Animation ticks itself off the Clock; nothing else to do.
    }

    pub fn is_done(&self) -> bool {
        self.anim.is_done()
    }

    /// Build the render element for this frame. Returns `None` if the
    /// fade has reached zero (caller should drop the snapshot).
    pub fn render(&self, scale: Scale<f64>) -> Option<MovingSnapshotRenderElement> {
        let progress = self.anim.clamped_value().clamp(0., 1.) as f32;
        let alpha = (1.0 - progress).max(0.0);
        if alpha <= 0.001 {
            return None;
        }

        let elem = TextureRenderElement::from_texture_buffer(
            self.buffer.clone(),
            Point::from((0., 0.)),
            alpha,
            None,
            None,
            Kind::Unspecified,
        );
        let elem = PrimaryGpuTextureRenderElement(elem);

        // Position the texture at `pos + buffer_offset` in physical pixels.
        let render_pos = (self.pos + self.buffer_offset)
            .to_physical_precise_round(scale);
        let elem = RelocateRenderElement::from_element(
            elem,
            render_pos,
            Relocate::Relative,
        );
        Some(MovingSnapshotRenderElement::Texture(elem))
    }
}
