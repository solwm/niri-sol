// Default sol-wallpaper shader: slowly morphing color blobs.
//
// Each fragment computes a smooth blend between several moving "blobs"
// (gaussian-ish falloffs centered at parameter-driven positions). The
// blobs drift in low-frequency Lissajous patterns and slowly hue-shift,
// so the wallpaper has the soft, ever-changing feel of a lava lamp at
// rest. Cheap: just a few sin/cos and length() per fragment.
//
// Available uniforms (kept stable so user-supplied shaders can reuse):
//   uniforms.resolution : vec2<f32>  framebuffer size in physical pixels
//   uniforms.time       : f32        seconds since daemon start

struct Uniforms {
    resolution: vec2<f32>,
    time: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Fullscreen triangle: three vertices spanning clip space, the GPU
// clips away the off-screen corner. Avoids an index/vertex buffer
// allocation.
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn palette(t: f32) -> vec3<f32> {
    // IQ-style cosine palette — warm purples and teals.
    let a = vec3<f32>(0.55, 0.45, 0.55);
    let b = vec3<f32>(0.45, 0.35, 0.45);
    let c = vec3<f32>(1.0, 1.0, 0.8);
    let d = vec3<f32>(0.10, 0.35, 0.65);
    return a + b * cos(6.2831853 * (c * t + d));
}

fn blob(uv: vec2<f32>, center: vec2<f32>, radius: f32) -> f32 {
    let d = length(uv - center);
    return exp(-(d * d) / (radius * radius));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Aspect-corrected coords: [-aspect/2, aspect/2] × [-0.5, 0.5].
    let aspect = uniforms.resolution.x / uniforms.resolution.y;
    let uv = vec2<f32>((in.uv.x - 0.5) * aspect, in.uv.y - 0.5);

    let t = uniforms.time * 0.07;

    // Four blobs in Lissajous paths. Different frequency ratios so they
    // never re-align — gives an aperiodic, organic drift.
    let c0 = vec2<f32>(cos(t * 0.7) * 0.35 * aspect, sin(t * 1.1) * 0.30);
    let c1 = vec2<f32>(sin(t * 0.9) * 0.40 * aspect, cos(t * 0.6) * 0.25);
    let c2 = vec2<f32>(cos(t * 1.3 + 1.7) * 0.30 * aspect, sin(t * 0.8 + 0.5) * 0.35);
    let c3 = vec2<f32>(sin(t * 1.1 + 3.0) * 0.45 * aspect, cos(t * 1.4 + 2.1) * 0.30);

    let r = 0.45;
    let f0 = blob(uv, c0, r);
    let f1 = blob(uv, c1, r);
    let f2 = blob(uv, c2, r);
    let f3 = blob(uv, c3, r);

    let total = f0 + f1 + f2 + f3 + 0.001;
    let w0 = f0 / total;
    let w1 = f1 / total;
    let w2 = f2 / total;
    let w3 = f3 / total;

    // Each blob picks its color from the palette at a slightly different
    // phase, and the phase itself drifts with time so the palette cycles.
    let col0 = palette(t * 0.5 + 0.00);
    let col1 = palette(t * 0.5 + 0.25);
    let col2 = palette(t * 0.5 + 0.50);
    let col3 = palette(t * 0.5 + 0.75);

    var col = col0 * w0 + col1 * w1 + col2 * w2 + col3 * w3;

    // Subtle vignette for a softer feel.
    let vd = length(uv * vec2<f32>(1.0 / aspect, 1.0)) * 1.4;
    col *= 1.0 - smoothstep(0.5, 1.1, vd) * 0.35;

    return vec4<f32>(col, 1.0);
}
