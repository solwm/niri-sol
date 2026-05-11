//! GLES renderer: one shader program + fullscreen quad, with a per-image
//! texture cache keyed on the image file path.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use glow::HasContext as _;

use crate::img::DecodedImage;
use crate::Fit;

const VERT_SRC: &str = r#"#version 300 es
precision mediump float;
layout(location = 0) in vec2 a_pos;
out vec2 v_uv;
uniform vec2 u_uv_scale;
uniform vec2 u_uv_offset;
void main() {
    // a_pos is in clip-space (-1..1). Map (-1,-1) -> uv(0,0), (1,1) -> uv(1,1).
    vec2 base_uv = a_pos * 0.5 + 0.5;
    // Flip Y to match image rows (image crate gives top-row first).
    base_uv.y = 1.0 - base_uv.y;
    v_uv = base_uv * u_uv_scale + u_uv_offset;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const FRAG_SRC: &str = r#"#version 300 es
precision mediump float;
in vec2 v_uv;
out vec4 frag;
uniform sampler2D u_tex;
uniform vec4 u_letterbox_color;
void main() {
    // Letterbox region (outside 0..1 in either axis) → solid color.
    if (v_uv.x < 0.0 || v_uv.x > 1.0 || v_uv.y < 0.0 || v_uv.y > 1.0) {
        frag = u_letterbox_color;
    } else {
        frag = texture(u_tex, v_uv);
    }
}
"#;

pub struct Renderer {
    pub gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
    u_uv_scale: glow::UniformLocation,
    u_uv_offset: glow::UniformLocation,
    u_letterbox: glow::UniformLocation,

    /// Cache: one GL texture per image file we've ever loaded.
    textures: HashMap<PathBuf, glow::Texture>,
}

impl Renderer {
    pub fn new(egl: &crate::egl::Egl) -> Result<Self> {
        // We need a current context to compile shaders / create buffers.
        // Bind the EGL context against EGL_NO_SURFACE so we can call GL.
        egl.egl
            .make_current(egl.display, None, None, Some(egl.context))
            .context("eglMakeCurrent(no_surface) for renderer setup failed")?;

        let gl = crate::egl::load_glow(&egl.egl)?;

        unsafe {
            let program = compile_program(&gl, VERT_SRC, FRAG_SRC)?;
            gl.use_program(Some(program));

            // Fullscreen quad as two tris.
            #[rustfmt::skip]
            let verts: [f32; 12] = [
                -1.0, -1.0,
                 1.0, -1.0,
                -1.0,  1.0,
                -1.0,  1.0,
                 1.0, -1.0,
                 1.0,  1.0,
            ];
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            gl.bind_vertex_array(Some(vao));

            let vbo = gl.create_buffer().map_err(|e| anyhow!(e))?;
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(
                glow::ARRAY_BUFFER,
                bytemuck_cast(&verts),
                glow::STATIC_DRAW,
            );
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);

            let u_uv_scale = gl
                .get_uniform_location(program, "u_uv_scale")
                .ok_or_else(|| anyhow!("missing uniform u_uv_scale"))?;
            let u_uv_offset = gl
                .get_uniform_location(program, "u_uv_offset")
                .ok_or_else(|| anyhow!("missing uniform u_uv_offset"))?;
            let u_letterbox = gl
                .get_uniform_location(program, "u_letterbox_color")
                .ok_or_else(|| anyhow!("missing uniform u_letterbox_color"))?;
            let u_tex = gl
                .get_uniform_location(program, "u_tex")
                .ok_or_else(|| anyhow!("missing uniform u_tex"))?;
            gl.uniform_1_i32(Some(&u_tex), 0);

            // Done with setup; release the context so the calling code's
            // draw path can `make_current` against a real surface.
            egl.release_current()?;

            Ok(Self {
                gl,
                program,
                vao,
                _vbo: vbo,
                u_uv_scale,
                u_uv_offset,
                u_letterbox,
                textures: HashMap::new(),
            })
        }
    }

    /// Upload `image` as a texture if it's not already cached. Returns the
    /// GL texture handle and the image's pixel dimensions.
    pub fn upload(&mut self, path: &PathBuf, image: &DecodedImage) -> Result<(glow::Texture, u32, u32)> {
        if let Some(&tex) = self.textures.get(path) {
            return Ok((tex, image.width, image.height));
        }
        unsafe {
            let tex = self.gl.create_texture().map_err(|e| anyhow!(e))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                image.width as i32,
                image.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&image.rgba)),
            );
            self.textures.insert(path.clone(), tex);
            Ok((tex, image.width, image.height))
        }
    }

    /// Draw the cached texture onto the currently-bound surface, sized
    /// `output_w × output_h` (in pixels), using `fit`.
    pub fn draw(
        &self,
        texture: glow::Texture,
        img_w: u32,
        img_h: u32,
        output_w: i32,
        output_h: i32,
        fit: Fit,
    ) {
        unsafe {
            self.gl.viewport(0, 0, output_w, output_h);
            self.gl.disable(glow::DEPTH_TEST);
            self.gl.disable(glow::BLEND);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);

            self.gl.use_program(Some(self.program));
            self.gl.bind_vertex_array(Some(self.vao));

            let (uv_scale, uv_offset) =
                fit_uv(fit, img_w as f32, img_h as f32, output_w as f32, output_h as f32);
            self.gl.uniform_2_f32(Some(&self.u_uv_scale), uv_scale.0, uv_scale.1);
            self.gl.uniform_2_f32(Some(&self.u_uv_offset), uv_offset.0, uv_offset.1);
            self.gl.uniform_4_f32(Some(&self.u_letterbox), 0.0, 0.0, 0.0, 1.0);

            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));

            self.gl.draw_arrays(glow::TRIANGLES, 0, 6);
        }
    }
}

/// Compute UV scale + offset to map a unit quad (base_uv in `[0,1]²`) onto a
/// region of the source image such that the image fills/fits/etc. the output.
///
/// `base_uv * scale + offset` should produce the sampling UV. Values outside
/// `[0,1]` are treated as letterbox by the fragment shader.
fn fit_uv(fit: Fit, iw: f32, ih: f32, ow: f32, oh: f32) -> ((f32, f32), (f32, f32)) {
    if iw <= 0.0 || ih <= 0.0 || ow <= 0.0 || oh <= 0.0 {
        return ((1.0, 1.0), (0.0, 0.0));
    }
    let img_ar = iw / ih;
    let out_ar = ow / oh;
    match fit {
        Fit::Stretch => ((1.0, 1.0), (0.0, 0.0)),
        Fit::Fill => {
            // Cover: sample a sub-rect of the image; quad still maps to [0,1].
            if out_ar > img_ar {
                // Output wider than image: crop top/bottom.
                let s = img_ar / out_ar;
                ((1.0, s), (0.0, (1.0 - s) * 0.5))
            } else {
                // Output taller than image: crop left/right.
                let s = out_ar / img_ar;
                ((s, 1.0), ((1.0 - s) * 0.5, 0.0))
            }
        }
        Fit::Fit => {
            // Contain: image fills one axis; the other axis goes outside [0,1].
            if out_ar > img_ar {
                // Output wider: letterbox left/right (UV.x goes outside).
                let s = out_ar / img_ar;
                ((s, 1.0), (-(s - 1.0) * 0.5, 0.0))
            } else {
                let s = img_ar / out_ar;
                ((1.0, s), (0.0, -(s - 1.0) * 0.5))
            }
        }
        Fit::Center => {
            // Native resolution centered. UV sampled at output px / image px.
            let sx = ow / iw;
            let sy = oh / ih;
            (
                (sx, sy),
                ((1.0 - sx) * 0.5, (1.0 - sy) * 0.5),
            )
        }
    }
}

unsafe fn compile_program(gl: &glow::Context, vs_src: &str, fs_src: &str) -> Result<glow::Program> {
    let vs = compile_shader(gl, glow::VERTEX_SHADER, vs_src)?;
    let fs = compile_shader(gl, glow::FRAGMENT_SHADER, fs_src)?;

    let program = gl.create_program().map_err(|e| anyhow!(e))?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        return Err(anyhow!("link failed: {log}"));
    }
    gl.detach_shader(program, vs);
    gl.detach_shader(program, fs);
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    Ok(program)
}

unsafe fn compile_shader(gl: &glow::Context, ty: u32, src: &str) -> Result<glow::Shader> {
    let sh = gl.create_shader(ty).map_err(|e| anyhow!(e))?;
    gl.shader_source(sh, src);
    gl.compile_shader(sh);
    if !gl.get_shader_compile_status(sh) {
        let log = gl.get_shader_info_log(sh);
        return Err(anyhow!("shader compile failed: {log}"));
    }
    Ok(sh)
}

/// `bytemuck`-style cast of `&[f32]` to `&[u8]` without pulling in the crate.
fn bytemuck_cast(slice: &[f32]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            slice.as_ptr() as *const u8,
            std::mem::size_of_val(slice),
        )
    }
}
