# Known Issues

## Bottom-right L-shape coverage hole at fractional output scale

**Symptom.** On outputs running at a *fractional* scale (e.g. 3840×2160 at scale 1.25, giving a 3072×1728 logical area), a fixed rectangular region in the bottom-right corner of the framebuffer can fail to be written by texture-rendering elements. The hole's dimensions are exactly the delta between physical and logical extents:

| Output physical | Scale | Logical | Right gap | Bottom gap |
|-----------------|-------|---------|-----------|------------|
| 3840 × 2160 | 1.25 | 3072 × 1728 | 768 px | 432 px |

The hole shows up as stale framebuffer content visible through any alpha-translucent thing drawn on top of it — most easily seen during a tile-movement crossfade animation: whatever is *behind* the alpha-fading tile bleeds through in the bottom-right corner. Wallpaper bleeds through windows; workspace background bled through the wallpaper before the workaround below.

**Cause.** Reproduces with two unrelated wallpaper daemons (`awww`, `sol-wallpaper`), and `awww` works correctly on Hyprland. The bug is on sol's compositor side — specifically in smithay's `gles::Frame::render_texture_from_to`. The signature condition: `BLEND` enabled + damage rect covering the bottom-right corner at fractional output scale. The white-rectangle test (a `SolidColorRenderElement` overlay) draws correctly in the same region, so the issue is texture-rendering-specific rather than viewport or scissor.

**Workarounds in place.**

1. **`sol-wallpaper` declares its surface fully opaque** (`set_opaque_region(huge)`) before binding the layer surface. When smithay sees an opaque-region claim with `alpha == 1`, it routes the texture draw through the *non-blending* path (`render_texture_from_to` at `lines 2766-2779`), which doesn't trip the bug. As a side effect the workspace background underneath is correctly skipped.

2. **Crossfade snapshot's xray position** (`scrolling.rs::swap_with_crossfade`) is now offset by the tile's slot, matching the live render path. Independent fix — was sampling the wrong wallpaper region for the snapshot's blur backdrop.

**What is *not* fixed.** Tile content during alpha animation (`inactive_alpha`, crossfade, etc.) goes through `OffscreenRenderElement` with `alpha < 1`, which forces the blending texture path. The L-shape bleed-through persists for translucent tiles. Declaring the offscreen element opaque does not help: by the time damage tracking sees the claim, the wallpaper has already drawn under the tile and the tile's own draw still fails to overwrite the L-shape.

**A proper fix** would likely require patching smithay's `gles` backend — either splitting damage rects that span the corner, or skipping the bug by tweaking the texture vertex transform / scissor at that specific size class. Out of scope for this branch.

**Related files.**
- `sol-wallpaper/src/state.rs` — opaque-region workaround.
- `src/layout/scrolling.rs::swap_with_crossfade` — tile-offset xray_pos fix.
- `~/.cargo/git/checkouts/smithay-*/src/backend/renderer/gles/mod.rs` — `render_texture_from_to`, lines 2693-2790 (relevant for further investigation).
