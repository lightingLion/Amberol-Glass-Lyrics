// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Low-resolution Gray-Scott reaction-diffusion background for the lyrics pane.
//! The simulation and colour mapping shaders are original implementations based
//! on the published Gray-Scott equations; no code is copied from the visual
//! reference project.

use gtk::{gdk, glib, prelude::*, subclass::prelude::*};
use std::{cell::Cell, ffi::CString, ptr, sync::OnceLock};

const SIM_SIZE: i32 = 192;

extern "C" {
    fn dlopen(
        filename: *const std::os::raw::c_char,
        flags: std::os::raw::c_int,
    ) -> *mut std::ffi::c_void;
    fn dlsym(
        handle: *mut std::ffi::c_void,
        symbol: *const std::os::raw::c_char,
    ) -> *mut std::ffi::c_void;
}

static LIB_GL: OnceLock<usize> = OnceLock::new();

unsafe fn load_gl_symbol(symbol: &str) -> *const std::ffi::c_void {
    let handle = *LIB_GL.get_or_init(|| {
        let library = CString::new("libGL.so.1").unwrap();
        dlopen(library.as_ptr(), 1) as usize // RTLD_LAZY
    }) as *mut std::ffi::c_void;
    if handle.is_null() {
        return ptr::null();
    }

    type GetProcAddress =
        unsafe extern "C" fn(*const std::os::raw::c_uchar) -> *const std::ffi::c_void;
    let getter_name = CString::new("glXGetProcAddressARB").unwrap();
    let getter = dlsym(handle, getter_name.as_ptr());
    let symbol = CString::new(symbol).expect("OpenGL symbol contains NUL");
    if !getter.is_null() {
        let getter: GetProcAddress = std::mem::transmute(getter);
        let address = getter(symbol.as_ptr().cast());
        if !address.is_null() {
            return address;
        }
    }
    dlsym(handle, symbol.as_ptr()).cast()
}

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct ReactionDiffusionView {
        renderer: RefCell<Option<Renderer>>,
        last_frame_us: Cell<i64>,
        init_failed: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ReactionDiffusionView {
        const NAME: &'static str = "AmberolReactionDiffusionView";
        type Type = super::ReactionDiffusionView;
        type ParentType = gtk::GLArea;
    }

    impl ObjectImpl for ReactionDiffusionView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_has_depth_buffer(false);
            obj.set_has_stencil_buffer(false);
            obj.set_auto_render(false);
            obj.set_allowed_apis(gdk::GLAPI::GL);
            obj.set_required_version(3, 3);

            obj.add_tick_callback(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move |_, clock| {
                    let now = clock.frame_time();
                    let imp = obj.imp();
                    // Balanced preset: fixed 192x192 simulation at at most 30 FPS.
                    if now - imp.last_frame_us.get() >= 33_000 {
                        imp.last_frame_us.set(now);
                        obj.queue_render();
                    }
                    glib::ControlFlow::Continue
                }
            ));
        }
    }

    impl WidgetImpl for ReactionDiffusionView {
        fn unrealize(&self) {
            let obj = self.obj();
            obj.make_current();
            self.renderer.borrow_mut().take();
            self.parent_unrealize();
        }
    }

    impl GLAreaImpl for ReactionDiffusionView {
        fn render(&self, _context: &gdk::GLContext) -> glib::Propagation {
            if self.init_failed.get() {
                return glib::Propagation::Proceed;
            }

            if self.renderer.borrow().is_none() {
                unsafe {
                    gl::load_with(|symbol| load_gl_symbol(symbol));
                }
                match unsafe { Renderer::new() } {
                    Ok(renderer) => {
                        log::debug!(
                            "Reaction-diffusion background ready: {}x{} at 30 FPS",
                            SIM_SIZE,
                            SIM_SIZE
                        );
                        *self.renderer.borrow_mut() = Some(renderer);
                    }
                    Err(error) => {
                        log::warn!("Reaction-diffusion background disabled: {error}");
                        self.init_failed.set(true);
                        return glib::Propagation::Proceed;
                    }
                }
            }

            let obj = self.obj();
            let scale = obj.scale_factor();
            if let Some(renderer) = self.renderer.borrow_mut().as_mut() {
                unsafe {
                    renderer.render(
                        obj.width().saturating_mul(scale),
                        obj.height().saturating_mul(scale),
                    );
                }
            }
            glib::Propagation::Stop
        }
    }
}

glib::wrapper! {
    pub struct ReactionDiffusionView(ObjectSubclass<imp::ReactionDiffusionView>)
        @extends gtk::Widget, gtk::GLArea,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for ReactionDiffusionView {
    fn default() -> Self {
        glib::Object::new()
    }
}

struct Renderer {
    simulation_program: u32,
    display_program: u32,
    vao: u32,
    textures: [u32; 2],
    framebuffers: [u32; 2],
    front: usize,
}

impl Renderer {
    unsafe fn new() -> Result<Self, String> {
        let vertex = compile_shader(gl::VERTEX_SHADER, VERTEX_SHADER)?;
        let simulation_fragment = compile_shader(gl::FRAGMENT_SHADER, SIMULATION_SHADER)?;
        let display_fragment = compile_shader(gl::FRAGMENT_SHADER, DISPLAY_SHADER)?;
        let simulation_program = link_program(vertex, simulation_fragment)?;
        let display_program = link_program(vertex, display_fragment)?;
        gl::DeleteShader(vertex);
        gl::DeleteShader(simulation_fragment);
        gl::DeleteShader(display_fragment);

        let mut vao = 0;
        gl::GenVertexArrays(1, &mut vao);

        let mut textures = [0; 2];
        let mut framebuffers = [0; 2];
        gl::GenTextures(2, textures.as_mut_ptr());
        gl::GenFramebuffers(2, framebuffers.as_mut_ptr());

        let seed_rg = make_seed();
        let mut seed_rgba = Vec::with_capacity((SIM_SIZE * SIM_SIZE * 4) as usize);
        for chemistry in seed_rg.chunks_exact(2) {
            seed_rgba.extend_from_slice(&[chemistry[0], chemistry[1], 0.0, 1.0]);
        }
        let formats = [
            (gl::RG16F, gl::RG, seed_rg.as_ptr()),
            (gl::RGBA16F, gl::RGBA, seed_rgba.as_ptr()),
            (gl::RGBA8, gl::RGBA, seed_rgba.as_ptr()),
        ];
        let mut texture_ready = false;
        for (internal, format, seed) in formats {
            let mut complete = true;
            while gl::GetError() != gl::NO_ERROR {}
            for index in 0..2 {
                gl::BindTexture(gl::TEXTURE_2D, textures[index]);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::REPEAT as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::REPEAT as i32);
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    internal as i32,
                    SIM_SIZE,
                    SIM_SIZE,
                    0,
                    format,
                    gl::FLOAT,
                    seed.cast(),
                );
                gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffers[index]);
                gl::FramebufferTexture2D(
                    gl::FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    textures[index],
                    0,
                );
                complete &= gl::GetError() == gl::NO_ERROR
                    && gl::CheckFramebufferStatus(gl::FRAMEBUFFER) == gl::FRAMEBUFFER_COMPLETE;
            }
            if complete {
                texture_ready = true;
                break;
            }
        }
        if !texture_ready {
            gl::DeleteProgram(simulation_program);
            gl::DeleteProgram(display_program);
            gl::DeleteVertexArrays(1, &vao);
            gl::DeleteTextures(2, textures.as_ptr());
            gl::DeleteFramebuffers(2, framebuffers.as_ptr());
            return Err("no supported reaction-diffusion framebuffer format".into());
        }
        gl::BindFramebuffer(gl::FRAMEBUFFER, 0);

        Ok(Self {
            simulation_program,
            display_program,
            vao,
            textures,
            framebuffers,
            front: 0,
        })
    }

    unsafe fn render(&mut self, width: i32, height: i32) {
        let mut gtk_framebuffer = 0;
        gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut gtk_framebuffer);

        let back = 1 - self.front;
        gl::Disable(gl::BLEND);
        gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffers[back]);
        gl::Viewport(0, 0, SIM_SIZE, SIM_SIZE);
        gl::UseProgram(self.simulation_program);
        gl::ActiveTexture(gl::TEXTURE0);
        gl::BindTexture(gl::TEXTURE_2D, self.textures[self.front]);
        gl::Uniform1i(
            gl::GetUniformLocation(self.simulation_program, b"state\0".as_ptr().cast()),
            0,
        );
        gl::BindVertexArray(self.vao);
        gl::DrawArrays(gl::TRIANGLES, 0, 3);
        self.front = back;

        gl::BindFramebuffer(gl::FRAMEBUFFER, gtk_framebuffer as u32);
        gl::Viewport(0, 0, width.max(1), height.max(1));
        gl::Enable(gl::BLEND);
        gl::BlendFunc(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA);
        gl::ClearColor(0.0, 0.0, 0.0, 0.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);
        gl::UseProgram(self.display_program);
        gl::BindTexture(gl::TEXTURE_2D, self.textures[self.front]);
        gl::Uniform1i(
            gl::GetUniformLocation(self.display_program, b"state\0".as_ptr().cast()),
            0,
        );
        gl::DrawArrays(gl::TRIANGLES, 0, 3);
        gl::BindVertexArray(0);
        gl::UseProgram(0);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteProgram(self.simulation_program);
            gl::DeleteProgram(self.display_program);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteTextures(2, self.textures.as_ptr());
            gl::DeleteFramebuffers(2, self.framebuffers.as_ptr());
        }
    }
}

fn make_seed() -> Vec<f32> {
    let mut pixels = vec![0.0; (SIM_SIZE * SIM_SIZE * 2) as usize];
    for y in 0..SIM_SIZE {
        for x in 0..SIM_SIZE {
            let i = ((y * SIM_SIZE + x) * 2) as usize;
            let wave = ((x * 17 + y * 29 + (x * y) % 31) % 97) as f32 / 97.0;
            let dx = x as f32 - SIM_SIZE as f32 * 0.5;
            let dy = y as f32 - SIM_SIZE as f32 * 0.5;
            let ring = ((dx * dx + dy * dy).sqrt() * 0.17).sin();
            let b = if wave > 0.83 || ring > 0.93 {
                0.82
            } else {
                0.0
            };
            pixels[i] = 1.0 - b * 0.48;
            pixels[i + 1] = b;
        }
    }
    pixels
}

unsafe fn compile_shader(kind: u32, source: &str) -> Result<u32, String> {
    let shader = gl::CreateShader(kind);
    let source = CString::new(source).unwrap();
    gl::ShaderSource(shader, 1, &source.as_ptr(), ptr::null());
    gl::CompileShader(shader);
    let mut ok = 0;
    gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut ok);
    if ok == 0 {
        let log = shader_log(shader);
        gl::DeleteShader(shader);
        Err(log)
    } else {
        Ok(shader)
    }
}

unsafe fn shader_log(shader: u32) -> String {
    let mut length = 0;
    gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut length);
    let mut log = vec![0_u8; length.max(1) as usize];
    gl::GetShaderInfoLog(shader, length, ptr::null_mut(), log.as_mut_ptr().cast());
    String::from_utf8_lossy(&log)
        .trim_end_matches('\0')
        .to_owned()
}

unsafe fn link_program(vertex: u32, fragment: u32) -> Result<u32, String> {
    let program = gl::CreateProgram();
    gl::AttachShader(program, vertex);
    gl::AttachShader(program, fragment);
    gl::LinkProgram(program);
    let mut ok = 0;
    gl::GetProgramiv(program, gl::LINK_STATUS, &mut ok);
    if ok == 0 {
        let mut length = 0;
        gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut length);
        let mut log = vec![0_u8; length.max(1) as usize];
        gl::GetProgramInfoLog(program, length, ptr::null_mut(), log.as_mut_ptr().cast());
        gl::DeleteProgram(program);
        Err(String::from_utf8_lossy(&log)
            .trim_end_matches('\0')
            .to_owned())
    } else {
        Ok(program)
    }
}

const VERTEX_SHADER: &str = r#"#version 330 core
out vec2 uv;
void main() {
    vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2);
    uv = p * 0.5;
    gl_Position = vec4(p - 1.0, 0.0, 1.0);
}
"#;

const SIMULATION_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
in vec2 uv;
layout(location = 0) out vec2 next_state;

void main() {
    vec2 px = 1.0 / vec2(textureSize(state, 0));
    vec2 c = texture(state, uv).rg;
    vec2 lap = -c;
    lap += 0.20 * (texture(state, uv + vec2(px.x, 0)).rg +
                   texture(state, uv - vec2(px.x, 0)).rg +
                   texture(state, uv + vec2(0, px.y)).rg +
                   texture(state, uv - vec2(0, px.y)).rg);
    lap += 0.05 * (texture(state, uv + px).rg +
                   texture(state, uv - px).rg +
                   texture(state, uv + vec2(px.x, -px.y)).rg +
                   texture(state, uv + vec2(-px.x, px.y)).rg);
    float reaction = c.x * c.y * c.y;
    float feed = 0.035;
    float kill = 0.061;
    float a = c.x + 0.16 * lap.x - reaction + feed * (1.0 - c.x);
    float b = c.y + 0.08 * lap.y + reaction - (feed + kill) * c.y;
    next_state = clamp(vec2(a, b), 0.0, 1.0);
}
"#;

const DISPLAY_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
in vec2 uv;
out vec4 color;

void main() {
    vec2 chemistry = texture(state, uv).rg;
    float pattern = smoothstep(0.08, 0.58, chemistry.y);
    float edge = smoothstep(0.015, 0.12, abs(dFdx(pattern)) + abs(dFdy(pattern)));
    vec3 wine = vec3(0.23, 0.055, 0.13);
    vec3 rose = vec3(0.80, 0.26, 0.43);
    vec3 amber = vec3(1.00, 0.62, 0.31);
    vec3 mapped = mix(wine, rose, pattern);
    mapped = mix(mapped, amber, edge * 0.75);
    float vignette = 1.0 - 0.30 * dot(uv - 0.5, uv - 0.5);
    color = vec4(mapped * vignette, 0.52 + pattern * 0.18);
}
"#;
