// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! The visible lyrics are produced by a continuous Gray-Scott simulation.
//! Pango/Cairo only rasterises an invisible seed/constraint mask; the display
//! shader never paints that mask directly.

use gtk::{cairo, gdk, glib, pango, prelude::*, subclass::prelude::*};
use std::{
    cell::Cell,
    collections::hash_map::DefaultHasher,
    ffi::CString,
    hash::{Hash, Hasher},
    ptr,
    sync::OnceLock,
};

const SIM_SIZE: i32 = 256;

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
        dlopen(library.as_ptr(), 1) as usize
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

#[derive(Clone)]
struct MaskUpdate {
    pixels: Vec<u8>,
    bounds: MaskRect,
}

#[derive(Clone, Copy, Default)]
struct MaskRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl MaskRect {
    fn overlap_ratio(self, other: Self) -> f32 {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let w = (self.x + self.width).min(other.x + other.width) - x;
        let h = (self.y + self.height).min(other.y + other.height) - y;
        if w <= 0 || h <= 0 {
            0.0
        } else {
            (w * h) as f32 / (self.width * self.height).max(1) as f32
        }
    }
}

mod imp {
    use super::*;
    use std::cell::RefCell;

    pub struct ReactionDiffusionView {
        renderer: RefCell<Option<Renderer>>,
        pub(super) pending_mask: RefCell<Option<MaskUpdate>>,
        pub(super) last_bounds: Cell<Option<MaskRect>>,
        pub(super) palette: Cell<[[f32; 3]; 3]>,
        last_frame_us: Cell<i64>,
        init_failed: Cell<bool>,
    }

    impl Default for ReactionDiffusionView {
        fn default() -> Self {
            Self {
                renderer: RefCell::new(None),
                pending_mask: RefCell::new(None),
                last_bounds: Cell::new(None),
                palette: Cell::new([[0.20, 0.035, 0.11], [0.78, 0.22, 0.42], [1.00, 0.63, 0.30]]),
                last_frame_us: Cell::new(0),
                init_failed: Cell::new(false),
            }
        }
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
            obj.set_overflow(gtk::Overflow::Hidden);

            obj.add_tick_callback(glib::clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::ControlFlow::Break,
                move |_, clock| {
                    let now = clock.frame_time();
                    let imp = obj.imp();
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
                unsafe { gl::load_with(|symbol| load_gl_symbol(symbol)) };
                match unsafe { Renderer::new() } {
                    Ok(renderer) => {
                        log::debug!(
                            "Text-seeded reaction diffusion ready: {}x{} at 30 FPS",
                            SIM_SIZE,
                            SIM_SIZE
                        );
                        *self.renderer.borrow_mut() = Some(renderer);
                    }
                    Err(error) => {
                        log::warn!("Reaction-diffusion canvas disabled: {error}");
                        self.init_failed.set(true);
                        return glib::Propagation::Proceed;
                    }
                }
            }

            let obj = self.obj();
            let scale = obj.scale_factor();
            if let Some(renderer) = self.renderer.borrow_mut().as_mut() {
                unsafe {
                    if let Some(update) = self.pending_mask.borrow_mut().take() {
                        renderer.set_text_mask(&update.pixels);
                    }
                    renderer.palette = self.palette.get();
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

impl ReactionDiffusionView {
    pub fn set_lyric(&self, text: &str) {
        self.upcast_ref::<gtk::Accessible>()
            .update_property(&[gtk::accessible::Property::Label(text)]);
        if text.trim().is_empty() {
            self.imp().last_bounds.set(None);
            self.imp().pending_mask.replace(Some(MaskUpdate {
                pixels: vec![0_u8; (SIM_SIZE * SIM_SIZE) as usize],
                bounds: MaskRect::default(),
            }));
            self.queue_render();
            return;
        }
        match render_text_mask(text, self.imp().last_bounds.get()) {
            Ok(update) => {
                log::debug!(
                    "Lyric seed mask queued at {},{} ({}x{})",
                    update.bounds.x,
                    update.bounds.y,
                    update.bounds.width,
                    update.bounds.height
                );
                self.imp().last_bounds.set(Some(update.bounds));
                self.imp().pending_mask.replace(Some(update));
                self.queue_render();
            }
            Err(error) => log::warn!("Unable to build lyric text mask: {error}"),
        }
    }

    pub fn set_palette(&self, colors: &[gdk::RGBA]) {
        if colors.is_empty() {
            return;
        }
        let fallback = self.imp().palette.get();
        let pick = |index: usize, old: [f32; 3]| {
            colors
                .get(index)
                .map_or(old, |c| [c.red(), c.green(), c.blue()])
        };
        self.imp().palette.set([
            pick(0, fallback[0]),
            pick(1, fallback[1]),
            pick(2, fallback[2]),
        ]);
    }
}

fn render_text_mask(text: &str, previous: Option<MaskRect>) -> Result<MaskUpdate, String> {
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, SIM_SIZE, SIM_SIZE)
        .map_err(|error| error.to_string())?;
    let cr = cairo::Context::new(&surface).map_err(|error| error.to_string())?;
    cr.set_source_rgb(0.0, 0.0, 0.0);
    cr.paint().map_err(|error| error.to_string())?;

    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_text(text);
    layout.set_width((SIM_SIZE - 32) * pango::SCALE);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_alignment(pango::Alignment::Center);
    let font_size = if text.chars().count() > 34 { 17 } else { 22 };
    layout.set_font_description(Some(&pango::FontDescription::from_string(&format!(
        "Sans Bold {font_size}"
    ))));

    let (_, logical) = layout.pixel_extents();
    let width = logical.width().min(SIM_SIZE - 28).max(1);
    let height = logical.height().min(92).max(1);
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let hash = hasher.finish() as usize;
    let candidates = [
        ((SIM_SIZE - width) / 2, (SIM_SIZE - height) / 2),
        (18, 42),
        (SIM_SIZE - width - 18, SIM_SIZE - height - 42),
        (SIM_SIZE - width - 22, 54),
        (22, SIM_SIZE - height - 54),
    ];
    let mut selected = candidates[hash % candidates.len()];
    for offset in 0..candidates.len() {
        let candidate = candidates[(hash + offset) % candidates.len()];
        let bounds = MaskRect {
            x: candidate.0,
            y: candidate.1,
            width,
            height,
        };
        if previous.map_or(true, |old| bounds.overlap_ratio(old) < 0.32) {
            selected = candidate;
            break;
        }
    }

    cr.move_to(
        (selected.0 - logical.x()) as f64,
        (selected.1 - logical.y()) as f64,
    );
    cr.set_source_rgb(1.0, 1.0, 1.0);
    pangocairo::functions::show_layout(&cr, &layout);
    drop(layout);
    drop(cr);
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().map_err(|error| error.to_string())?;
    let mut pixels = vec![0_u8; (SIM_SIZE * SIM_SIZE) as usize];
    for y in 0..SIM_SIZE as usize {
        for x in 0..SIM_SIZE as usize {
            // Cairo rows start at the visual top; OpenGL texture row zero is bottom.
            let source = (SIM_SIZE as usize - 1 - y) * stride + x * 4;
            pixels[y * SIM_SIZE as usize + x] = data[source + 2];
        }
    }
    Ok(MaskUpdate {
        pixels,
        bounds: MaskRect {
            x: selected.0,
            y: selected.1,
            width,
            height,
        },
    })
}

struct Renderer {
    simulation_program: u32,
    display_program: u32,
    vao: u32,
    state_textures: [u32; 2],
    framebuffers: [u32; 2],
    mask_textures: [u32; 2],
    current_mask: Vec<u8>,
    front: usize,
    phase_seconds: f32,
    frame: u32,
    palette: [[f32; 3]; 3],
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
        let mut state_textures = [0; 2];
        let mut framebuffers = [0; 2];
        gl::GenTextures(2, state_textures.as_mut_ptr());
        gl::GenFramebuffers(2, framebuffers.as_mut_ptr());
        allocate_state_textures(&state_textures, &framebuffers)?;

        let mut mask_textures = [0; 2];
        gl::GenTextures(2, mask_textures.as_mut_ptr());
        let empty = vec![0_u8; (SIM_SIZE * SIM_SIZE) as usize];
        for texture in mask_textures {
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexImage2D(
                gl::TEXTURE_2D,
                0,
                gl::R8 as i32,
                SIM_SIZE,
                SIM_SIZE,
                0,
                gl::RED,
                gl::UNSIGNED_BYTE,
                empty.as_ptr().cast(),
            );
        }
        gl::BindFramebuffer(gl::FRAMEBUFFER, 0);

        Ok(Self {
            simulation_program,
            display_program,
            vao,
            state_textures,
            framebuffers,
            mask_textures,
            current_mask: empty,
            front: 0,
            phase_seconds: 4.0,
            frame: 0,
            palette: [[0.20, 0.035, 0.11], [0.78, 0.22, 0.42], [1.00, 0.63, 0.30]],
        })
    }

    unsafe fn set_text_mask(&mut self, pixels: &[u8]) {
        upload_mask(self.mask_textures[1], &self.current_mask);
        upload_mask(self.mask_textures[0], pixels);
        self.current_mask.clear();
        self.current_mask.extend_from_slice(pixels);
        self.phase_seconds = 0.0;
    }

    unsafe fn render(&mut self, width: i32, height: i32) {
        let mut gtk_framebuffer = 0;
        gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut gtk_framebuffer);
        let back = 1 - self.front;

        gl::Disable(gl::BLEND);
        gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffers[back]);
        gl::Viewport(0, 0, SIM_SIZE, SIM_SIZE);
        gl::UseProgram(self.simulation_program);
        bind_texture(
            self.simulation_program,
            b"state\0",
            0,
            self.state_textures[self.front],
        );
        bind_texture(
            self.simulation_program,
            b"textMask\0",
            1,
            self.mask_textures[0],
        );
        bind_texture(
            self.simulation_program,
            b"previousTextMask\0",
            2,
            self.mask_textures[1],
        );
        uniform_1f(self.simulation_program, b"phase\0", self.phase_seconds);
        uniform_1f(self.simulation_program, b"time\0", self.frame as f32 / 30.0);
        // Audio slots are wired as stable defaults in this first text-seed prototype.
        uniform_1f(self.simulation_program, b"audioEnergy\0", 0.12);
        uniform_1f(self.simulation_program, b"beatPulse\0", 0.0);
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
        bind_texture(
            self.display_program,
            b"state\0",
            0,
            self.state_textures[self.front],
        );
        bind_texture(
            self.display_program,
            b"textMask\0",
            1,
            self.mask_textures[0],
        );
        bind_texture(
            self.display_program,
            b"previousTextMask\0",
            2,
            self.mask_textures[1],
        );
        uniform_1f(self.display_program, b"phase\0", self.phase_seconds);
        uniform_1f(self.display_program, b"time\0", self.frame as f32 / 30.0);
        for (index, color) in self.palette.iter().enumerate() {
            let name = match index {
                0 => b"paletteDark\0".as_slice(),
                1 => b"paletteMid\0".as_slice(),
                _ => b"paletteLight\0".as_slice(),
            };
            gl::Uniform3f(
                gl::GetUniformLocation(self.display_program, name.as_ptr().cast()),
                color[0],
                color[1],
                color[2],
            );
        }
        gl::DrawArrays(gl::TRIANGLES, 0, 3);
        gl::BindVertexArray(0);
        gl::UseProgram(0);

        self.phase_seconds = (self.phase_seconds + 1.0 / 30.0).min(30.0);
        self.frame = self.frame.wrapping_add(1);
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteProgram(self.simulation_program);
            gl::DeleteProgram(self.display_program);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteTextures(2, self.state_textures.as_ptr());
            gl::DeleteTextures(2, self.mask_textures.as_ptr());
            gl::DeleteFramebuffers(2, self.framebuffers.as_ptr());
        }
    }
}

unsafe fn allocate_state_textures(textures: &[u32; 2], fbos: &[u32; 2]) -> Result<(), String> {
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
            gl::BindFramebuffer(gl::FRAMEBUFFER, fbos[index]);
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
            return Ok(());
        }
    }
    Err("no supported reaction-diffusion framebuffer format".into())
}

unsafe fn upload_mask(texture: u32, pixels: &[u8]) {
    gl::BindTexture(gl::TEXTURE_2D, texture);
    gl::TexSubImage2D(
        gl::TEXTURE_2D,
        0,
        0,
        0,
        SIM_SIZE,
        SIM_SIZE,
        gl::RED,
        gl::UNSIGNED_BYTE,
        pixels.as_ptr().cast(),
    );
}

unsafe fn bind_texture(program: u32, name: &[u8], unit: u32, texture: u32) {
    gl::ActiveTexture(gl::TEXTURE0 + unit);
    gl::BindTexture(gl::TEXTURE_2D, texture);
    gl::Uniform1i(
        gl::GetUniformLocation(program, name.as_ptr().cast()),
        unit as i32,
    );
}

unsafe fn uniform_1f(program: u32, name: &[u8], value: f32) {
    gl::Uniform1f(gl::GetUniformLocation(program, name.as_ptr().cast()), value);
}

fn make_seed() -> Vec<f32> {
    let mut pixels = vec![0.0; (SIM_SIZE * SIM_SIZE * 2) as usize];
    for y in 0..SIM_SIZE {
        for x in 0..SIM_SIZE {
            let i = ((y * SIM_SIZE + x) * 2) as usize;
            let wave = ((x * 17 + y * 29 + (x * y) % 31) % 101) as f32 / 101.0;
            let b = if wave > 0.93 { 0.86 } else { 0.0 };
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
uniform sampler2D textMask;
uniform sampler2D previousTextMask;
uniform float phase;
uniform float time;
uniform float audioEnergy;
uniform float beatPulse;
in vec2 uv;
layout(location = 0) out vec2 nextState;

float noise(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

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

    float mask = texture(textMask, uv).r;
    float oldMask = texture(previousTextMask, uv).r;
    float n = noise(floor(uv / px) + floor(time * 17.0));
    float seedStage = 1.0 - smoothstep(0.10, 0.22, phase);
    float growthStage = smoothstep(0.12, 0.90, phase);
    float erosionStage = smoothstep(0.08, 1.35, phase);
    float sparseSeed = mask * step(0.72, n) * seedStage * 0.24;
    float softConstraint = mask * growthStage * (0.010 + audioEnergy * 0.006);
    float oldErosion = oldMask * erosionStage * (0.012 + 0.020 * n);

    float reaction = c.x * c.y * c.y;
    float feed = 0.0345 + audioEnergy * 0.0015;
    float kill = 0.061;
    float a = c.x + 0.16 * lap.x - reaction + feed * (1.0 - c.x);
    float b = c.y + 0.08 * lap.y + reaction - (feed + kill) * c.y;
    b += sparseSeed + softConstraint + beatPulse * mask * 0.035;
    b -= oldErosion;
    b += (n - 0.5) * mask * growthStage * 0.005;
    nextState = clamp(vec2(a, b), 0.0, 1.0);
}
"#;

const DISPLAY_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
uniform sampler2D textMask;
uniform sampler2D previousTextMask;
uniform float phase;
uniform float time;
uniform vec3 paletteDark;
uniform vec3 paletteMid;
uniform vec3 paletteLight;
in vec2 uv;
out vec4 color;

float noise(vec2 p) {
    return fract(sin(dot(p, vec2(41.7, 289.1))) * 45758.5453);
}

void main() {
    float b = texture(state, uv).g;
    float mask = texture(textMask, uv).r;
    float oldMask = texture(previousTextMask, uv).r;
    float chemistry = smoothstep(0.10, 0.58, b);
    float cellularEdge = smoothstep(0.018, 0.11, fwidth(chemistry));
    float n = noise(floor(uv * 256.0) + floor(time * 2.0));

    float formation = smoothstep(0.10, 0.95, phase);
    float threshold = mix(0.93, 0.16, formation);
    float currentInk = mask * smoothstep(threshold, threshold + 0.13,
                                          chemistry + n * 0.22);

    float erosion = smoothstep(0.05, 1.45, phase);
    float erosionThreshold = mix(0.12, 0.94, erosion);
    float previousInk = oldMask * chemistry *
        smoothstep(erosionThreshold, erosionThreshold + 0.10,
                   chemistry * 0.72 + n * 0.42);

    float glyphMatter = clamp(currentInk + previousInk * 0.68, 0.0, 1.0);
    float glyphEdge = cellularEdge * clamp(mask + oldMask * (1.0 - erosion), 0.0, 1.0);
    float freePattern = chemistry * (1.0 - mask * 0.45) * 0.22;

    vec3 mapped = mix(paletteDark, paletteMid, freePattern + glyphMatter * 0.72);
    mapped = mix(mapped, paletteLight, glyphEdge * 0.88 + glyphMatter * 0.20);
    float vignette = 1.0 - 0.30 * dot(uv - 0.5, uv - 0.5);
    float alpha = 0.30 + freePattern * 0.22 + glyphMatter * 0.62 + glyphEdge * 0.18;
    color = vec4(mapped * vignette, clamp(alpha, 0.0, 0.96));
}
"#;
