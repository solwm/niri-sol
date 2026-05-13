#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;

// `tex` is the tile-content offscreen, rendered upstream at full opacity
// (alpha = 1, premultiplied). smithay binds it to texture unit 0 like for
// the default texture shader.
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

// `backdrop_tex` is the wallpaper-layer offscreen (sol's xray.background),
// bound by the caller to texture unit 1. We sample it to obtain "what is
// underneath the tile" without round-tripping through a GL BLEND read-
// modify-write — which is where the NVIDIA bottom-right glitch lives.
uniform sampler2D backdrop_tex;

// `tex_alpha` is the window opacity factor (e.g. inactive_alpha or
// alpha-animation progress). We apply it manually in the shader instead
// of via smithay's per-draw `alpha` uniform so the final write can be
// opaque and skip GL BLEND entirely.
uniform float tex_alpha;

// Backdrop texture size in framebuffer pixels (same as `gl_FragCoord`'s
// pixel space — the wallpaper offscreen is sized to the output physical
// resolution). We sample by output-pixel position rather than mapping
// through `v_coords` because `v_coords` depends on the tile-texture src
// rect and isn't a reliable [0,1] interpolant.
uniform vec2 backdrop_tex_size;

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec4 tile = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
    tile = vec4(tile.rgb, 1.0);
#endif

    // Sample the wallpaper offscreen at the same output pixel position
    // we're writing to. The offscreen is sized to output physical pixels
    // and shares its Y origin with `gl_FragCoord`, so no flip is needed.
    vec2 bg_uv = gl_FragCoord.xy / backdrop_tex_size;
    vec4 bg = texture2D(backdrop_tex, bg_uv);

    // `tile` is premultiplied. Apply `tex_alpha` (still premultiplied).
    vec4 src = tile * tex_alpha;

    // Standard "over" composite onto opaque backdrop:
    //   result = src + bg * (1 - src.a)
    // bg.a is expected to be 1.0 (wallpaper is opaque); we still combine
    // through the equation rather than overwriting so a translucent
    // backdrop would compose correctly.
    vec4 result = src + bg * (1.0 - src.a);

    // Final write is OPAQUE. smithay sees opaque_regions covering the full
    // element, alpha = 1, and disables GL BLEND for this draw.
    gl_FragColor = vec4(result.rgb, 1.0) * alpha;

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        gl_FragColor = vec4(0.0, 0.2, 0.0, 0.2) + gl_FragColor * 0.8;
#endif
}
