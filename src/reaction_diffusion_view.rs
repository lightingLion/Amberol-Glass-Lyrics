// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Prelude-built Turing texture plus hand-drawn lyric cavities.
//! The texture follows a repeated blur + 300% unsharp process; once built it
//! stays topologically stable. Pango/Cairo masks only carve and refill local
//! cavities, so a lyric never causes the whole field to reorganise.

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
const TURING_ITERATIONS: u32 = 20;

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
    duration_seconds: f32,
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
        pub(super) pending_position: Cell<Option<f32>>,
        pub(super) pending_intro_duration: Cell<Option<f32>>,
        pub(super) reset_pattern: Cell<bool>,
        last_frame_us: Cell<i64>,
        init_failed: Cell<bool>,
    }

    impl Default for ReactionDiffusionView {
        fn default() -> Self {
            Self {
                renderer: RefCell::new(None),
                pending_mask: RefCell::new(None),
                last_bounds: Cell::new(None),
                palette: Cell::new([
                    [0.004, 0.012, 0.018],
                    [0.000, 0.680, 0.760],
                    [0.050, 0.960, 1.000],
                ]),
                pending_position: Cell::new(Some(0.0)),
                pending_intro_duration: Cell::new(Some(4.0)),
                reset_pattern: Cell::new(true),
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
                            "Blur-unsharp Turing canvas ready: {}x{} at 30 FPS",
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
                        renderer.set_text_mask(&update.pixels, update.duration_seconds);
                    }
                    if self.reset_pattern.replace(false) {
                        renderer.reset_pattern();
                    }
                    if let Some(duration) = self.pending_intro_duration.take() {
                        renderer.intro_duration = duration.max(0.5);
                    }
                    if let Some(position) = self.pending_position.take() {
                        renderer.song_seconds = position.max(0.0);
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
    pub fn set_lyric(&self, text: &str, duration_seconds: f32) {
        self.upcast_ref::<gtk::Accessible>()
            .update_property(&[gtk::accessible::Property::Label(text)]);
        if text.trim().is_empty() {
            self.imp().last_bounds.set(None);
            self.imp().pending_mask.replace(Some(MaskUpdate {
                pixels: vec![0_u8; (SIM_SIZE * SIM_SIZE) as usize],
                bounds: MaskRect::default(),
                duration_seconds: 0.0,
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
                self.imp().pending_mask.replace(Some(MaskUpdate {
                    duration_seconds: duration_seconds.max(1.0),
                    ..update
                }));
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

    pub fn begin_song(&self, intro_duration: f32) {
        let imp = self.imp();
        imp.last_bounds.set(None);
        imp.pending_intro_duration
            .set(Some(intro_duration.clamp(0.5, 30.0)));
        imp.pending_position.set(Some(0.0));
        imp.reset_pattern.set(true);
        self.set_lyric("", 0.0);
        self.queue_render();
    }

    pub fn set_playback_position(&self, seconds: f32) {
        self.imp().pending_position.set(Some(seconds.max(0.0)));
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
        duration_seconds: 0.0,
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
    pattern_iterations: u32,
    intro_duration: f32,
    song_seconds: f32,
    lyric_age: f32,
    lyric_duration: f32,
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
            pattern_iterations: 0,
            intro_duration: 4.0,
            song_seconds: 0.0,
            lyric_age: 30.0,
            lyric_duration: 0.0,
            frame: 0,
            palette: [
                [0.004, 0.012, 0.018],
                [0.000, 0.680, 0.760],
                [0.050, 0.960, 1.000],
            ],
        })
    }

    unsafe fn set_text_mask(&mut self, pixels: &[u8], duration_seconds: f32) {
        upload_mask(self.mask_textures[1], &self.current_mask);
        upload_mask(self.mask_textures[0], pixels);
        self.current_mask.clear();
        self.current_mask.extend_from_slice(pixels);
        self.lyric_age = 0.0;
        self.lyric_duration = duration_seconds.max(0.0);
    }

    unsafe fn reset_pattern(&mut self) {
        let seed = make_seed();
        for texture in self.state_textures {
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                0,
                0,
                SIM_SIZE,
                SIM_SIZE,
                gl::RG,
                gl::FLOAT,
                seed.as_ptr().cast(),
            );
        }
        self.front = 0;
        self.pattern_iterations = 0;
        self.song_seconds = 0.0;
    }

    unsafe fn render(&mut self, width: i32, height: i32) {
        let mut gtk_framebuffer = 0;
        gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut gtk_framebuffer);
        gl::Disable(gl::BLEND);
        gl::Viewport(0, 0, SIM_SIZE, SIM_SIZE);
        gl::UseProgram(self.simulation_program);
        gl::BindVertexArray(self.vao);

        // Match the notebook's 20 repeated blur + unsharp iterations, but
        // schedule them across the prelude instead of evolving forever.
        let intro_progress = (self.song_seconds / self.intro_duration).clamp(0.0, 1.0);
        let target_iterations = (intro_progress * TURING_ITERATIONS as f32).floor() as u32;
        let steps = target_iterations
            .saturating_sub(self.pattern_iterations)
            .min(4);
        for _ in 0..steps {
            let back = 1 - self.front;
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffers[back]);
            bind_texture(
                self.simulation_program,
                b"state\0",
                0,
                self.state_textures[self.front],
            );
            gl::DrawArrays(gl::TRIANGLES, 0, 3);
            self.front = back;
            self.pattern_iterations += 1;
        }

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
        uniform_1f(self.display_program, b"time\0", self.frame as f32 / 30.0);
        uniform_1f(self.display_program, b"songTime\0", self.song_seconds);
        uniform_1f(
            self.display_program,
            b"introDuration\0",
            self.intro_duration,
        );
        uniform_1f(self.display_program, b"lyricAge\0", self.lyric_age);
        uniform_1f(
            self.display_program,
            b"lyricDuration\0",
            self.lyric_duration,
        );
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

        self.song_seconds += 1.0 / 30.0;
        self.lyric_age = (self.lyric_age + 1.0 / 30.0).min(60.0);
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
            let mut value = (x as u64)
                .wrapping_mul(0x9E37_79B1)
                .wrapping_add((y as u64).wrapping_mul(0x85EB_CA77))
                .wrapping_add(0xC2B2_AE3D);
            value ^= value >> 16;
            value = value.wrapping_mul(0x7FEB_352D);
            value ^= value >> 15;
            let gray = (value & 0xffff) as f32 / 65_535.0;
            pixels[i] = gray;
            pixels[i + 1] = gray;
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
    // One oversized triangle: (-1,-1), (3,-1), (-1,3).
    // The previous p-1 mapping ended at (+1,+1), covering only half the card.
    uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const SIMULATION_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
in vec2 uv;
layout(location = 0) out vec2 nextState;

void main() {
    vec2 px = 1.0 / vec2(textureSize(state, 0));
    float boxBlur = 0.0;
    for (int y = -2; y <= 2; ++y) {
        for (int x = -2; x <= 2; ++x) {
            boxBlur += texture(state, fract(uv + vec2(x, y) * px)).r;
        }
    }
    boxBlur /= 25.0;

    // A second radius-2 box blur is equivalent to this separable triangular
    // 9x9 kernel. This lets one fragment pass reproduce blur + 300% unsharp.
    float twiceBlurred = 0.0;
    float totalWeight = 0.0;
    for (int y = -4; y <= 4; ++y) {
        for (int x = -4; x <= 4; ++x) {
            float weight = float(5 - abs(x)) * float(5 - abs(y));
            twiceBlurred += texture(state, fract(uv + vec2(x, y) * px)).r * weight;
            totalWeight += weight;
        }
    }
    twiceBlurred /= totalWeight;
    float sharpened = clamp(boxBlur + 3.0 * (boxBlur - twiceBlurred), 0.0, 1.0);
    nextState = vec2(sharpened, sharpened);
}
"#;

const DISPLAY_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
uniform sampler2D textMask;
uniform float time;
uniform float songTime;
uniform float introDuration;
uniform float lyricAge;
uniform float lyricDuration;
uniform vec3 paletteDark;
uniform vec3 paletteMid;
uniform vec3 paletteLight;
in vec2 uv;
out vec4 color;

float hash21(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

void main() {
    // Quantised sub-pixel motion gives a restrained frame-by-frame hand-drawn
    // wobble without changing the established pattern topology.
    float handFrame = floor(time * 9.0);
    vec2 jitter = vec2(hash21(vec2(handFrame, 2.0)),
                       hash21(vec2(7.0, handFrame))) - 0.5;
    vec2 sampleUv = fract(uv + jitter / 384.0);
    float field = texture(state, sampleUv).r;
    float ink = smoothstep(0.42, 0.58, field);
    float edge = smoothstep(0.012, 0.055, fwidth(field));

    // During the prelude, a noisy wavefront progressively lays the texture
    // across the card. At completion the global pattern is visually frozen.
    float intro = clamp(songTime / max(introDuration, 0.5), 0.0, 1.0);
    float spreadNoise = hash21(floor(uv * 22.0));
    float spreadDistance = distance(uv, vec2(0.18, 0.72));
    float spread = smoothstep(-0.06, 0.06,
                              intro * 1.58 - spreadDistance + spreadNoise * 0.16);

    // A lyric is a cavity cut out of the stable texture. Entry and refill use
    // noisy stepped thresholds, producing the requested hand-drawn cadence.
    float enterDuration = min(0.75, max(0.28, lyricDuration * 0.24));
    float refillDuration = min(1.05, max(0.38, lyricDuration * 0.28));
    float refillStart = max(enterDuration, lyricDuration - refillDuration);
    float enter = smoothstep(0.0, enterDuration, lyricAge);
    float refill = 1.0 - smoothstep(refillStart, max(lyricDuration, refillStart + 0.01), lyricAge);
    float cavityStrength = min(enter, refill) * step(0.01, lyricDuration);
    float paperNoise = hash21(floor(uv * 150.0) + handFrame * 0.37);
    float cavityDraw = smoothstep(1.0 - cavityStrength,
                                  1.10 - cavityStrength, paperNoise);
    float mask = texture(textMask, sampleUv).r;
    float cavity = mask * cavityDraw;
    float cavityEdge = fwidth(mask * cavityDraw) * 9.0;

    vec3 patternColor = mix(paletteMid, paletteLight, edge * 0.55 + ink * 0.18);
    vec3 base = mix(paletteDark, patternColor, ink * spread);
    base = mix(base, paletteDark * 0.38, cavity * spread);
    base += paletteLight * cavityEdge * 0.16 * spread;
    float vignette = 1.0 - 0.20 * dot(uv - 0.5, uv - 0.5);
    color = vec4(base * vignette, 0.88);
}
"#;
