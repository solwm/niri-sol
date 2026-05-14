// Default sol-wallpaper shader: scattered small shapes (circles,
// squares, triangles) over a near-black background. Each shape pops in
// fast, holds at full brightness, then fades out while its soft edge
// expands — the "blur away" feel — over a few seconds. Shapes are
// arranged on a screen-sized grid, one per cell (with frequent skips
// and large jitter so the grid never reads), and each shape's type,
// color, position, rotation, size, and phase offset are seeded from a
// per-cell hash so the result is irregular and re-runnable.
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

// Fullscreen triangle.
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var out: VsOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

// Stable 2D hash → [0,1)^2.
fn hash22(p: vec2<f32>) -> vec2<f32> {
    var q = vec3<f32>(p.x, p.y, p.x + p.y);
    q = fract(q * vec3<f32>(0.1031, 0.1030, 0.0973));
    q = q + dot(q, q.yzx + 33.33);
    return fract(vec2<f32>(q.x + q.y, q.y + q.z));
}

// Stable scalar hash.
fn hash1(p: vec2<f32>) -> f32 {
    let q = fract(vec3<f32>(p.x, p.y, p.x * 31.7) * 0.1031);
    let r = q + dot(q, q.yzx + 33.33);
    return fract((r.x + r.y) * r.z);
}

// IQ-style cosine palette → pleasant pastels with a teal/magenta/amber
// rotation. `seed` (in [0,1)) shifts the phase so adjacent cells pick
// different colors.
fn nice_color(seed: f32) -> vec3<f32> {
    let a = vec3<f32>(0.55, 0.50, 0.55);
    let b = vec3<f32>(0.45, 0.45, 0.45);
    let c = vec3<f32>(1.0, 0.9, 0.8);
    let d = vec3<f32>(0.10, 0.35, 0.65);
    return a + b * cos(6.2831853 * (c * seed + d));
}

// 2×2 rotation matrix.
fn rot2(a: f32) -> mat2x2<f32> {
    let c = cos(a);
    let s = sin(a);
    return mat2x2<f32>(c, -s, s, c);
}

// Signed distance to an axis-aligned square (half-extent `r`). Positive
// outside, negative inside.
fn sd_square(p: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0);
}

// Signed distance to an equilateral triangle pointing up, "radius" `r`
// (distance from centroid to vertex). Standard IQ formulation.
fn sd_triangle(p: vec2<f32>, r: f32) -> f32 {
    let k = sqrt(3.0);
    var q = vec2<f32>(abs(p.x) - r, p.y + r / k);
    if (q.x + k * q.y > 0.0) {
        q = vec2<f32>(q.x - k * q.y, -k * q.x - q.y) / 2.0;
    }
    q.x = q.x - clamp(q.x, -2.0 * r, 0.0);
    return -length(q) * sign(q.y);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Pixel coords (physical). Smaller cell_px → denser shapes.
    let cell_px = 90.0;
    let p = in.uv * uniforms.resolution;
    let cell = floor(p / cell_px);
    let local = fract(p / cell_px) - 0.5; // [-0.5, 0.5] within cell

    // Lifecycle period (seconds).
    let lifecycle = 7.0;

    // Walk this cell + 8 neighbors so shapes near a cell boundary
    // contribute to the current pixel.
    var col = vec3<f32>(0.0);
    for (var dy: i32 = -1; dy <= 1; dy = dy + 1) {
        for (var dx: i32 = -1; dx <= 1; dx = dx + 1) {
            let n_cell = cell + vec2<f32>(f32(dx), f32(dy));

            // Skip ~18% of cells outright so the grid doesn't read
            // as a grid — gives the picture an irregular density.
            let skip_seed = hash1(n_cell + vec2<f32>(91.3, 17.7));
            if (skip_seed < 0.18) {
                continue;
            }

            // Strong jitter (full cell wander) for an organic scatter.
            let jitter = hash22(n_cell) - 0.5;
            let n_center = vec2<f32>(f32(dx), f32(dy)) + jitter;

            // Phase offset distributes lifecycles evenly across cells.
            let phase_off = hash1(n_cell + vec2<f32>(7.13, 41.7)) * lifecycle;
            let phase = fract((uniforms.time + phase_off) / lifecycle);

            //   [0.00, 0.05] — pop in
            //   [0.05, 0.30] — sharp, full brightness
            //   [0.30, 1.00] — fade out + edge expands
            let appear = smoothstep(0.00, 0.05, phase);
            let fade = 1.0 - smoothstep(0.30, 1.00, phase);
            let alpha = appear * fade;
            let blur = smoothstep(0.30, 1.00, phase);

            // Per-shape size multiplier (0.6..1.3) so shapes aren't
            // uniformly sized — gives a sense of distribution.
            let size_seed = hash1(n_cell + vec2<f32>(53.1, 7.7));
            let size_mul = 0.6 + size_seed * 0.7;

            // Base radius (in cell units). Small so the overall shape
            // footprint feels light against the dark background.
            let r0 = 0.040 * size_mul;
            let r = r0 + blur * 0.14 * size_mul;
            let edge = 0.012 + blur * 0.09;

            // Per-shape rotation so squares/triangles aren't aligned.
            let rot_seed = hash1(n_cell + vec2<f32>(13.7, 91.3));
            let q = rot2(rot_seed * 6.2831853) * (local - n_center);

            // Pick shape type from a third hash bucket. 3 types,
            // roughly equal probability.
            let type_seed = hash1(n_cell + vec2<f32>(29.1, 67.3));
            var d: f32;
            if (type_seed < 0.34) {
                // Circle
                d = length(q) - r;
            } else if (type_seed < 0.67) {
                // Square (axis-aligned in local frame; rotation gives
                // it a random screen-space orientation).
                d = sd_square(q, r);
            } else {
                // Triangle.
                d = sd_triangle(q, r);
            }

            let shape = (1.0 - smoothstep(-edge, edge, d)) * alpha;

            let color_seed = hash1(n_cell + vec2<f32>(3.7, 19.1));
            // Multiplier > 1 pushes peak shape intensity past what the
            // palette alone can produce, so shapes pop against the
            // near-black background. The hottest pixels clamp on the
            // surface format's sRGB encode.
            let shape_col = nice_color(color_seed) * 1.55;

            col = col + shape_col * shape;
        }
    }

    // Near-black base with a barely-there cool tint.
    let base = vec3<f32>(0.004, 0.004, 0.010);
    return vec4<f32>(base + col, 1.0);
}
