//! EGL/GLES presentation, replacing the earlier `wl_shm`-based path. See PLAN.md's stutter
//! investigation: a Mutter bug tied to fullscreen/maximized window transitions
//! (`GNOME/mutter#1647`) disrupts `wl_shm` buffer-release scheduling specifically (observed
//! stalls up to 170 seconds) but not GL/EGL swap-chain scheduling — confirmed via a controlled
//! A/B test against `mpv --vo=gpu --fullscreen` on this same machine/compositor, which hit the
//! identical Mutter assertion but played back with zero stalls.
//!
//! Renders onto the *same* `wl_surface` already used for input capture (see `input_surface.rs`)
//! rather than a separate GStreamer-owned window — the whole reason `wl_shm` was used in the
//! first place instead of just re-adopting `waylandsink` was to keep video and input on one
//! surface/coordinate-space (a real bug this project already fixed once, see that file's module
//! doc). `wp_viewporter` scaling is no longer needed either: the EGL window is sized to the
//! output's real physical resolution, and GL's own texture sampling (bilinear-filtered) handles
//! scaling the decoded frame's native resolution up to fill it.
//!
//! **Not yet live-tested** — written from verified API signatures (checked directly against the
//! vendored `khronos-egl`/`glow`/`wayland-egl` crate sources rather than assumed from memory) but
//! this hasn't run against real hardware yet.

use anyhow::{anyhow, Context, Result};
use glow::HasContext;
use khronos_egl as egl;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy};

const VERTEX_SHADER_SRC: &str = r#"
attribute vec2 a_pos;
attribute vec2 a_texcoord;
varying vec2 v_texcoord;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_texcoord = a_texcoord;
}
"#;

const FRAGMENT_SHADER_SRC: &str = r#"
precision mediump float;
varying vec2 v_texcoord;
uniform sampler2D u_tex;
void main() {
    // decode.rs's frames are tightly-packed BGRx8888 (see its module doc), uploaded here as if
    // they were RGBA (tex_image_2d with format=RGBA) to avoid depending on the
    // EXT_texture_format_BGRA8888 extension — swap R/B back here instead.
    vec4 c = texture2D(u_tex, v_texcoord);
    gl_FragColor = vec4(c.b, c.g, c.r, 1.0);
}
"#;

/// Full-viewport triangle strip: (x, y, u, v) per vertex. `v` is *not* flipped — vertex (-1, +1)
/// (top of the on-screen quad, since GL clip space has +Y up) maps to texcoord (0, 0), matching
/// `decode.rs`'s frame data where row 0 is the top of the image, uploaded to texture row 0.
#[rustfmt::skip]
const QUAD_VERTICES: [f32; 16] = [
    -1.0, -1.0, 0.0, 1.0, // bottom-left
     1.0, -1.0, 1.0, 1.0, // bottom-right
    -1.0,  1.0, 0.0, 0.0, // top-left
     1.0,  1.0, 1.0, 0.0, // top-right
];

/// Owns the EGL context/surface and the GL objects needed to render one texture as a full-screen
/// quad. Must be created and used entirely from a single thread — EGL contexts are thread-bound
/// (`eglMakeCurrent` binds to the calling thread), which matches how this is used: created and
/// driven only from `input_surface.rs`'s presentation-loop thread, never the Wayland dispatch
/// thread.
pub struct GlPresenter {
    // Order matters for Drop: the EGL surface/context must be destroyed before the
    // wl_egl_window, which must be destroyed before the underlying wl_surface goes away.
    _egl_window: wayland_egl::WlEglSurface,
    display: egl::Display,
    egl_surface: egl::Surface,
    context: egl::Context,
    gl: glow::Context,
    program: glow::Program,
    texture: glow::Texture,
    tex_size: Option<(i32, i32)>,
}

impl GlPresenter {
    /// `width`/`height` are the *output's* physical size (the surface/window size) — separate
    /// from whatever resolution decoded frames actually arrive at; `present_frame` uploads each
    /// frame at its own size and lets GL scale it to fill this viewport.
    pub fn new(conn: &Connection, surface: &WlSurface, width: i32, height: i32) -> Result<Self> {
        let wl_display_ptr = conn.backend().display_ptr() as *mut std::ffi::c_void;
        let egl_window = wayland_egl::WlEglSurface::new(surface.id(), width, height)
            .map_err(|e| anyhow!("creating wl_egl_window: {e}"))?;

        let egl = &egl::API;
        let display = unsafe { egl.get_display(wl_display_ptr) }.ok_or_else(|| anyhow!("eglGetDisplay failed"))?;
        egl.initialize(display).context("eglInitialize")?;
        egl.bind_api(egl::OPENGL_ES_API).context("eglBindAPI(OPENGL_ES_API)")?;

        let config_attribs = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::NONE,
        ];
        let mut configs = Vec::with_capacity(1);
        egl.choose_config(display, &config_attribs, &mut configs).context("eglChooseConfig")?;
        let config = *configs.first().ok_or_else(|| anyhow!("no matching EGL config found"))?;

        let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE];
        let context = egl
            .create_context(display, config, None, &context_attribs)
            .context("eglCreateContext (GLES2)")?;

        let egl_surface = unsafe {
            egl.create_window_surface(display, config, egl_window.ptr() as egl::NativeWindowType, None)
        }
        .context("eglCreateWindowSurface")?;

        egl.make_current(display, Some(egl_surface), Some(egl_surface), Some(context))
            .context("eglMakeCurrent")?;

        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name).map(|f| f as *const std::ffi::c_void).unwrap_or(std::ptr::null())
            })
        };

        let program = unsafe { build_program(&gl)? };
        let texture = unsafe { create_texture(&gl)? };
        unsafe { setup_geometry(&gl, program)? };

        tracing::info!(width, height, "EGL/GLES presenter ready");
        Ok(Self {
            _egl_window: egl_window,
            display,
            egl_surface,
            context,
            gl,
            program,
            texture,
            tex_size: None,
        })
    }

    pub fn present_frame(&mut self, frame: &crate::decode::DecodedFrame) -> Result<()> {
        let (w, h) = (frame.width as i32, frame.height as i32);
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            if self.tex_size == Some((w, h)) {
                self.gl
                    .tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w, h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(&frame.data));
            } else {
                self.gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, Some(&frame.data));
                self.tex_size = Some((w, h));
                tracing::info!(width = w, height = h, "(re)allocated GL texture storage");
            }

            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.use_program(Some(self.program));
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        egl::API
            .swap_buffers(self.display, self.egl_surface)
            .map_err(|e| anyhow!("eglSwapBuffers failed: {e:?}"))?;
        Ok(())
    }
}

impl Drop for GlPresenter {
    fn drop(&mut self) {
        let egl = &egl::API;
        let _ = egl.make_current(self.display, None, None, None);
        let _ = egl.destroy_surface(self.display, self.egl_surface);
        let _ = egl.destroy_context(self.display, self.context);
    }
}

unsafe fn compile_shader(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader> {
    let shader = gl.create_shader(kind).map_err(|e| anyhow!("glCreateShader: {e}"))?;
    gl.shader_source(shader, src);
    gl.compile_shader(shader);
    if !gl.get_shader_compile_status(shader) {
        let log = gl.get_shader_info_log(shader);
        return Err(anyhow!("shader compile failed: {log}"));
    }
    Ok(shader)
}

unsafe fn build_program(gl: &glow::Context) -> Result<glow::Program> {
    let vertex = compile_shader(gl, glow::VERTEX_SHADER, VERTEX_SHADER_SRC)?;
    let fragment = compile_shader(gl, glow::FRAGMENT_SHADER, FRAGMENT_SHADER_SRC)?;
    let program = gl.create_program().map_err(|e| anyhow!("glCreateProgram: {e}"))?;
    gl.attach_shader(program, vertex);
    gl.attach_shader(program, fragment);
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        return Err(anyhow!("program link failed: {log}"));
    }
    gl.delete_shader(vertex);
    gl.delete_shader(fragment);
    Ok(program)
}

unsafe fn create_texture(gl: &glow::Context) -> Result<glow::Texture> {
    let texture = gl.create_texture().map_err(|e| anyhow!("glCreateTexture: {e}"))?;
    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    Ok(texture)
}

/// Uploads the static full-screen-quad vertex buffer and wires up the vertex attributes. Called
/// once at setup — the geometry never changes, only the texture contents do.
unsafe fn setup_geometry(gl: &glow::Context, program: glow::Program) -> Result<()> {
    let buffer = gl.create_buffer().map_err(|e| anyhow!("glCreateBuffer: {e}"))?;
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(buffer));
    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast_f32_slice(&QUAD_VERTICES), glow::STATIC_DRAW);

    gl.use_program(Some(program));
    let stride = 4 * std::mem::size_of::<f32>() as i32;
    if let Some(loc) = gl.get_attrib_location(program, "a_pos") {
        gl.enable_vertex_attrib_array(loc);
        gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, stride, 0);
    }
    if let Some(loc) = gl.get_attrib_location(program, "a_texcoord") {
        gl.enable_vertex_attrib_array(loc);
        gl.vertex_attrib_pointer_f32(loc, 2, glow::FLOAT, false, stride, 2 * std::mem::size_of::<f32>() as i32);
    }
    if let Some(loc) = gl.get_uniform_location(program, "u_tex") {
        gl.uniform_1_i32(Some(&loc), 0);
    }
    Ok(())
}

/// `glow`'s `buffer_data_u8_slice` wants raw bytes; this is just a `f32` slice reinterpreted as
/// `u8` (safe: both are POD, no alignment issue since we're only ever reading, not casting back).
fn bytemuck_cast_f32_slice(data: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(data)) }
}
