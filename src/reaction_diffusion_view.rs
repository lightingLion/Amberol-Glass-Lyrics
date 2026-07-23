// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Audio-seeded Gray-Scott reaction-diffusion with chemical lyric cavities.
//! The text texture only modulates the U/V chemistry for a short envelope;
//! appearance and disappearance remain entirely inside the continuous field.

use gtk::{cairo, gdk, glib, pango, prelude::*, subclass::prelude::*};
use std::{
    cell::Cell,
    collections::hash_map::DefaultHasher,
    ffi::CString,
    hash::{Hash, Hasher},
    ptr,
    sync::OnceLock,
};

// Recipe adapted from ph-200711/Turing-Patterns-Music-Video-Generator mode 4.
const SIM_WIDTH: i32 = 768;
const SIM_HEIGHT: i32 = 1024;
const INITIALIZATION_PASSES: u32 = 3_600;
const INITIALIZATION_STEPS_PER_FRAME: u32 = 48;
// The GTK canvas renders at 30 fps; 48 steps matches the reference's
// 24 steps at a typical 60 Hz browser requestAnimationFrame loop.
const PLAYBACK_STEPS_PER_FRAME: u32 = 48;
const FEED_RATE: f32 = 0.0480;
const KILL_RATE: f32 = 0.0615;
const GLYPH_EMERGE_SECONDS: f32 = 0.30;
const GLYPH_MIN_RELEASE_SECONDS: f32 = 1.20;
const GLYPH_MAX_RELEASE_SECONDS: f32 = 4.00;
const GLYPH_RELEASE_CUE_RATIO: f32 = 0.72;
const GLYPH_CHEMISTRY_STRENGTH: f32 = 0.13;

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
    cue_start_seconds: f32,
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
    use glib::subclass::Signal;
    use once_cell::sync::Lazy;
    use std::cell::RefCell;

    pub struct ReactionDiffusionView {
        renderer: RefCell<Option<Renderer>>,
        pub(super) pending_mask: RefCell<Option<MaskUpdate>>,
        pub(super) last_bounds: Cell<Option<MaskRect>>,
        pub(super) palette: Cell<[[f32; 3]; 3]>,
        pub(super) pending_position: Cell<Option<f32>>,
        pub(super) reset_pattern: Cell<bool>,
        pub(super) pattern_seed: Cell<u64>,
        pub(super) playing: Cell<bool>,
        pub(super) pattern_ready_emitted: Cell<bool>,
        last_frame_us: Cell<i64>,
        frame_delta: Cell<f32>,
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
                reset_pattern: Cell::new(true),
                pattern_seed: Cell::new(0xA6D1_35C7_94E2_8B01),
                playing: Cell::new(false),
                pattern_ready_emitted: Cell::new(false),
                last_frame_us: Cell::new(0),
                frame_delta: Cell::new(1.0 / 30.0),
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
                    let previous = imp.last_frame_us.get();
                    if now - previous >= 33_000 {
                        let delta = if previous > 0 {
                            ((now - previous) as f32 / 1_000_000.0).clamp(0.001, 0.05)
                        } else {
                            1.0 / 30.0
                        };
                        imp.frame_delta.set(delta);
                        imp.last_frame_us.set(now);
                        obj.queue_render();
                    }
                    glib::ControlFlow::Continue
                }
            ));
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("pattern-ready").build()]);
            SIGNALS.as_ref()
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
                            "Audio-seeded Turing canvas ready: {}x{} with timed lyric cavities",
                            SIM_WIDTH,
                            SIM_HEIGHT
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
            let mut emit_pattern_ready = false;
            if let Some(renderer) = self.renderer.borrow_mut().as_mut() {
                unsafe {
                    renderer.pattern_seed = self.pattern_seed.get();
                    if self.reset_pattern.replace(false) {
                        self.pattern_ready_emitted.set(false);
                        renderer.reset_pattern();
                    }
                    if let Some(update) = self.pending_mask.borrow_mut().take() {
                        renderer.set_text_mask(
                            &update.pixels,
                            update.cue_start_seconds,
                            update.duration_seconds,
                        );
                    }
                    if let Some(position) = self.pending_position.take() {
                        renderer.song_seconds = position.max(0.0);
                    }
                    renderer.playback_running = self.playing.get();
                    renderer.palette = self.palette.get();
                    renderer.render(
                        obj.width().saturating_mul(scale),
                        obj.height().saturating_mul(scale),
                        self.frame_delta.get(),
                    );
                    emit_pattern_ready =
                        renderer.pattern_ready() && !self.pattern_ready_emitted.replace(true);
                }
            }
            if emit_pattern_ready {
                log::debug!("Gray-Scott initialization completed before playback");
                obj.emit_by_name::<()>("pattern-ready", &[]);
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
    pub fn set_lyric(&self, text: &str, cue_start_ms: u64, duration_ms: u64) {
        self.upcast_ref::<gtk::Accessible>()
            .update_property(&[gtk::accessible::Property::Label(text)]);
        if text.trim().is_empty() {
            self.imp().last_bounds.set(None);
            self.imp().pending_mask.replace(Some(MaskUpdate {
                pixels: vec![0_u8; (SIM_WIDTH * SIM_HEIGHT * 4) as usize],
                bounds: MaskRect::default(),
                cue_start_seconds: cue_start_ms as f32 / 1_000.0,
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
                    cue_start_seconds: cue_start_ms as f32 / 1_000.0,
                    duration_seconds: duration_ms.max(50) as f32 / 1_000.0,
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
        let mut ordered = colors
            .iter()
            .map(|color| [color.red(), color.green(), color.blue()])
            .collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            relative_luminance(*left).total_cmp(&relative_luminance(*right))
        });

        let darkest = ordered[0];
        let brightest = ordered[ordered.len() - 1];
        let middle = ordered[ordered.len() / 2];
        let album_mix = ordered.iter().fold([0.0; 3], |mut sum, color| {
            for channel in 0..3 {
                sum[channel] += color[channel] / ordered.len() as f32;
            }
            sum
        });

        // Blend the extracted cover colors into each stop rather than forcing
        // a fixed cyan tint. Black/white are only used to preserve enough
        // contrast for the chemical strands and lyric cavities.
        let dark = mix_rgb(mix_rgb(darkest, album_mix, 0.18), [0.0, 0.0, 0.0], 0.40);
        let mid = mix_rgb(middle, album_mix, 0.52);
        let mut light = mix_rgb(mix_rgb(brightest, album_mix, 0.20), [1.0, 1.0, 1.0], 0.10);
        if contrast_ratio(dark, light) < 3.0 {
            light = mix_rgb(light, [1.0, 1.0, 1.0], 0.34);
        }
        self.imp().palette.set([dark, mid, light]);
    }

    pub fn begin_song(&self, _intro_duration: f32, pattern_seed: u64) {
        let imp = self.imp();
        imp.last_bounds.set(None);
        imp.pattern_seed.set(pattern_seed);
        imp.pending_position.set(Some(0.0));
        imp.reset_pattern.set(true);
        imp.pattern_ready_emitted.set(false);
        self.set_lyric("", 0, 0);
        self.queue_render();
    }

    pub fn set_playback_position(&self, seconds: f32) {
        self.imp().pending_position.set(Some(seconds.max(0.0)));
    }

    pub fn set_playing(&self, playing: bool) {
        self.imp().playing.set(playing);
        self.queue_render();
    }
}

fn render_text_mask(text: &str, previous: Option<MaskRect>) -> Result<MaskUpdate, String> {
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, SIM_WIDTH, SIM_HEIGHT)
        .map_err(|error| error.to_string())?;
    let cr = cairo::Context::new(&surface).map_err(|error| error.to_string())?;
    cr.set_source_rgb(0.0, 0.0, 0.0);
    cr.paint().map_err(|error| error.to_string())?;

    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_text(text);
    layout.set_width((SIM_WIDTH - 128) * pango::SCALE);
    layout.set_wrap(pango::WrapMode::WordChar);
    layout.set_alignment(pango::Alignment::Center);
    // The reference demo uses Comfortaa around weight 375. Regular Sans keeps
    // a comparable stroke-to-pattern ratio and has complete CJK fallback.
    let character_count = text.chars().filter(|c| !c.is_whitespace()).count();
    let mut font_size = if character_count > 34 {
        62
    } else if character_count > 20 {
        74
    } else {
        84
    };
    let logical = loop {
        layout.set_font_description(Some(&pango::FontDescription::from_string(&format!(
            "Sans {font_size}"
        ))));
        let (_, logical) = layout.pixel_extents();
        if (logical.width() <= SIM_WIDTH - 108 && logical.height() <= 300) || font_size <= 42 {
            break logical;
        }
        font_size -= 2;
    };

    let width = logical.width().clamp(1, SIM_WIDTH - 108);
    let height = logical.height().clamp(1, SIM_HEIGHT - 240);
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let hash = hasher.finish() as usize;
    let center_x = (SIM_WIDTH - width) / 2;
    let center_y = (SIM_HEIGHT - height) / 2;
    let right_x = (SIM_WIDTH - width - 54).max(54);
    let lower_y = (SIM_HEIGHT - height - 132).max(132);
    let upper_y = 132.min(SIM_HEIGHT - height - 132).max(54);
    let upper_middle_y = 228.min(SIM_HEIGHT - height - 132).max(54);
    let lower_middle_y = (SIM_HEIGHT - height - 228).max(54);
    let candidates = [
        (center_x, center_y),
        (54, upper_y),
        (right_x, lower_y),
        (right_x, upper_middle_y),
        (54, lower_middle_y),
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
    let mut coverage = vec![0_u8; (SIM_WIDTH * SIM_HEIGHT) as usize];
    for y in 0..SIM_HEIGHT as usize {
        for x in 0..SIM_WIDTH as usize {
            // Cairo rows start at the visual top; OpenGL texture row zero is bottom.
            let source = (SIM_HEIGHT as usize - 1 - y) * stride + x * 4;
            coverage[y * SIM_WIDTH as usize + x] = data[source + 2];
        }
    }
    let pixels = build_mask_field(&coverage, SIM_WIDTH as usize, SIM_HEIGHT as usize);
    Ok(MaskUpdate {
        pixels,
        bounds: MaskRect {
            x: selected.0,
            y: selected.1,
            width,
            height,
        },
        cue_start_seconds: 0.0,
        duration_seconds: 0.0,
    })
}

/// RGBA chemical mask. Only R is populated with antialiased glyph coverage;
/// the display shader never samples this texture.
fn build_mask_field(coverage: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut field = vec![0_u8; width * height * 4];
    for (index, coverage) in coverage.iter().enumerate() {
        field[index * 4] = *coverage;
    }
    field
}

struct Renderer {
    simulation_program: u32,
    display_program: u32,
    vao: u32,
    state_textures: [u32; 2],
    framebuffers: [u32; 2],
    mask_texture: u32,
    front: usize,
    pattern_iterations: u32,
    queued_mask: Option<(Vec<u8>, f32, f32)>,
    has_current_mask: bool,
    song_seconds: f32,
    lyric_age: f32,
    lyric_start_seconds: f32,
    lyric_duration_seconds: f32,
    playback_running: bool,
    pattern_seed: u64,
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

        let mut mask_texture = 0;
        gl::GenTextures(1, &mut mask_texture);
        let empty = vec![0_u8; (SIM_WIDTH * SIM_HEIGHT * 4) as usize];
        gl::BindTexture(gl::TEXTURE_2D, mask_texture);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
        gl::TexImage2D(
            gl::TEXTURE_2D,
            0,
            gl::RGBA8 as i32,
            SIM_WIDTH,
            SIM_HEIGHT,
            0,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            empty.as_ptr().cast(),
        );
        gl::BindFramebuffer(gl::FRAMEBUFFER, 0);

        Ok(Self {
            simulation_program,
            display_program,
            vao,
            state_textures,
            framebuffers,
            mask_texture,
            front: 0,
            pattern_iterations: 0,
            queued_mask: None,
            has_current_mask: false,
            song_seconds: 0.0,
            lyric_age: 30.0,
            lyric_start_seconds: 0.0,
            lyric_duration_seconds: 0.0,
            playback_running: false,
            pattern_seed: 0xA6D1_35C7_94E2_8B01,
            palette: [
                [0.004, 0.012, 0.018],
                [0.000, 0.680, 0.760],
                [0.050, 0.960, 1.000],
            ],
        })
    }

    unsafe fn set_text_mask(
        &mut self,
        pixels: &[u8],
        cue_start_seconds: f32,
        duration_seconds: f32,
    ) {
        self.queued_mask = Some((
            pixels.to_vec(),
            cue_start_seconds.max(0.0),
            duration_seconds.max(0.0),
        ));
    }

    unsafe fn reset_pattern(&mut self) {
        let seed = make_seed(self.pattern_seed);
        for texture in self.state_textures {
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                0,
                0,
                SIM_WIDTH,
                SIM_HEIGHT,
                gl::RG,
                gl::FLOAT,
                seed.as_ptr().cast(),
            );
        }
        self.front = 0;
        self.pattern_iterations = 0;
        self.has_current_mask = false;
        self.song_seconds = 0.0;
        self.lyric_start_seconds = 0.0;
        self.lyric_duration_seconds = 0.0;
        self.lyric_age = 30.0;
    }

    fn pattern_ready(&self) -> bool {
        self.pattern_iterations >= INITIALIZATION_PASSES
    }

    unsafe fn render(&mut self, width: i32, height: i32, delta_seconds: f32) {
        self.lyric_age = (self.song_seconds - self.lyric_start_seconds)
            .max(0.0)
            .min(60.0);

        let mut gtk_framebuffer = 0;
        gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut gtk_framebuffer);
        gl::Disable(gl::BLEND);
        gl::Viewport(0, 0, SIM_WIDTH, SIM_HEIGHT);
        gl::BindVertexArray(self.vao);

        // Cue replacement is immediate and never waits for an older cavity.
        if let Some((pixels, cue_start, duration)) = self.queued_mask.take() {
            upload_mask(self.mask_texture, &pixels);
            self.has_current_mask = duration > 0.0;
            self.lyric_start_seconds = cue_start;
            self.lyric_duration_seconds = duration;
            self.lyric_age = (self.song_seconds - cue_start).max(0.0).min(60.0);
        }

        if !self.pattern_ready() {
            let steps = INITIALIZATION_PASSES
                .saturating_sub(self.pattern_iterations)
                .min(INITIALIZATION_STEPS_PER_FRAME);
            for _ in 0..steps {
                self.run_simulation_pass(0.0);
                self.pattern_iterations += 1;
            }
        } else if self.playback_running {
            let strength = if self.has_current_mask {
                glyph_chemistry_envelope(self.lyric_age, self.lyric_duration_seconds)
            } else {
                0.0
            };
            for _ in 0..PLAYBACK_STEPS_PER_FRAME {
                self.run_simulation_pass(strength);
            }
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

        if self.playback_running && self.pattern_ready() {
            self.song_seconds += delta_seconds;
        }
        self.lyric_age = (self.song_seconds - self.lyric_start_seconds)
            .max(0.0)
            .min(60.0);
    }

    unsafe fn run_simulation_pass(&mut self, carve_strength: f32) {
        let back = 1 - self.front;
        gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffers[back]);
        gl::UseProgram(self.simulation_program);
        bind_texture(
            self.simulation_program,
            b"state\0",
            0,
            self.state_textures[self.front],
        );
        bind_texture(self.simulation_program, b"textMask\0", 1, self.mask_texture);
        uniform_1f(self.simulation_program, b"feedRate\0", FEED_RATE);
        uniform_1f(self.simulation_program, b"killRate\0", KILL_RATE);
        uniform_1f(
            self.simulation_program,
            b"carveStrength\0",
            carve_strength.clamp(0.0, GLYPH_CHEMISTRY_STRENGTH),
        );
        gl::DrawArrays(gl::TRIANGLES, 0, 3);
        self.front = back;
    }
}

fn smooth_step(progress: f32) -> f32 {
    let progress = progress.clamp(0.0, 1.0);
    progress * progress * (3.0 - 2.0 * progress)
}

fn glyph_release_seconds(cue_duration_seconds: f32) -> f32 {
    (cue_duration_seconds * GLYPH_RELEASE_CUE_RATIO)
        .clamp(GLYPH_MIN_RELEASE_SECONDS, GLYPH_MAX_RELEASE_SECONDS)
}

fn glyph_chemistry_envelope(age_seconds: f32, cue_duration_seconds: f32) -> f32 {
    let release_seconds = glyph_release_seconds(cue_duration_seconds);
    let release_start = cue_duration_seconds.max(GLYPH_EMERGE_SECONDS);
    if age_seconds < 0.0 {
        0.0
    } else if age_seconds < GLYPH_EMERGE_SECONDS {
        smooth_step(age_seconds / GLYPH_EMERGE_SECONDS) * GLYPH_CHEMISTRY_STRENGTH
    } else if age_seconds < release_start {
        GLYPH_CHEMISTRY_STRENGTH
    } else if age_seconds < release_start + release_seconds {
        let release = (age_seconds - release_start) / release_seconds;
        (1.0 - smooth_step(release)) * GLYPH_CHEMISTRY_STRENGTH
    } else {
        0.0
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteProgram(self.simulation_program);
            gl::DeleteProgram(self.display_program);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteTextures(2, self.state_textures.as_ptr());
            gl::DeleteTextures(1, &self.mask_texture);
            gl::DeleteFramebuffers(2, self.framebuffers.as_ptr());
        }
    }
}

unsafe fn allocate_state_textures(textures: &[u32; 2], fbos: &[u32; 2]) -> Result<(), String> {
    let seed = make_seed(0xA6D1_35C7_94E2_8B01);
    let formats = [gl::RG32F, gl::RG16F, gl::RG8];
    for internal in formats {
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
                SIM_WIDTH,
                SIM_HEIGHT,
                0,
                gl::RG,
                gl::FLOAT,
                seed.as_ptr().cast(),
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
    Err("no supported two-channel reaction-diffusion framebuffer format".into())
}

unsafe fn upload_mask(texture: u32, pixels: &[u8]) {
    gl::BindTexture(gl::TEXTURE_2D, texture);
    gl::TexSubImage2D(
        gl::TEXTURE_2D,
        0,
        0,
        0,
        SIM_WIDTH,
        SIM_HEIGHT,
        gl::RGBA,
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

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn random_unit(state: &mut u64) -> f32 {
    (splitmix64(state) >> 40) as f32 / ((1_u32 << 24) - 1) as f32
}

fn mix_rgb(from: [f32; 3], to: [f32; 3], amount: f32) -> [f32; 3] {
    let amount = amount.clamp(0.0, 1.0);
    [
        from[0] + (to[0] - from[0]) * amount,
        from[1] + (to[1] - from[1]) * amount,
        from[2] + (to[2] - from[2]) * amount,
    ]
}

fn relative_luminance(color: [f32; 3]) -> f32 {
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
}

fn contrast_ratio(first: [f32; 3], second: [f32; 3]) -> f32 {
    let a = relative_luminance(first);
    let b = relative_luminance(second);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Gray-Scott U/V state seeded by audio-derived circular disturbances. The
/// density and radius match the reference generator's 12 px cells and 3 px
/// seed dots, while the hash makes every audio file deterministic.
fn make_seed(pattern_seed: u64) -> Vec<f32> {
    let mut pixels = vec![0.0_f32; (SIM_WIDTH * SIM_HEIGHT * 2) as usize];
    for pair in pixels.chunks_exact_mut(2) {
        pair[0] = 1.0; // Chemical U
        pair[1] = 0.0; // Chemical V
    }

    let mut random = pattern_seed ^ 0x6A09_E667_F3BC_C909;
    const CELL: i32 = 12;
    let cells_x = (SIM_WIDTH + CELL - 1) / CELL;
    let cells_y = (SIM_HEIGHT + CELL - 1) / CELL;
    for cell_y in 0..cells_y {
        for cell_x in 0..cells_x {
            if random_unit(&mut random) < 0.965 {
                continue;
            }
            let center_x = (cell_x * CELL + CELL / 2).min(SIM_WIDTH - 1) as f32;
            let center_y = (cell_y * CELL + CELL / 2).min(SIM_HEIGHT - 1) as f32;
            let radius = 2.6 + random_unit(&mut random) * 0.8;
            let min_x = (center_x - radius).floor().max(0.0) as i32;
            let max_x = (center_x + radius).ceil().min((SIM_WIDTH - 1) as f32) as i32;
            let min_y = (center_y - radius).floor().max(0.0) as i32;
            let max_y = (center_y + radius).ceil().min((SIM_HEIGHT - 1) as f32) as i32;
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    let dx = x as f32 + 0.5 - center_x;
                    let dy = y as f32 + 0.5 - center_y;
                    if dx * dx + dy * dy <= radius * radius {
                        let index = ((y * SIM_WIDTH + x) * 2) as usize;
                        pixels[index] = 0.45;
                        pixels[index + 1] = 0.95;
                    }
                }
            }
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
    uv = p;
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const SIMULATION_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
uniform sampler2D textMask;
uniform float feedRate;
uniform float killRate;
uniform float carveStrength;
in vec2 uv;
layout(location = 0) out vec2 nextState;

ivec2 wrapCoord(ivec2 coordinate) {
    ivec2 size = textureSize(state, 0);
    return ivec2((coordinate.x % size.x + size.x) % size.x,
                 (coordinate.y % size.y + size.y) % size.y);
}

vec2 stateAt(ivec2 coordinate, ivec2 offset) {
    return texelFetch(state, wrapCoord(coordinate + offset), 0).rg;
}

float maskAt(ivec2 coordinate, ivec2 offset) {
    ivec2 size = textureSize(textMask, 0);
    ivec2 samplePoint = clamp(coordinate + offset, ivec2(0), size - ivec2(1));
    return texelFetch(textMask, samplePoint, 0).r;
}

void main() {
    ivec2 coordinate = ivec2(gl_FragCoord.xy);
    vec2 chemistry = stateAt(coordinate, ivec2(0));

    // Gray-Scott Laplacian from the reference repository: cardinal 0.2,
    // diagonal 0.05 and centre -1.0.
    vec2 laplacian = -chemistry;
    laplacian += 0.20 * stateAt(coordinate, ivec2( 1,  0));
    laplacian += 0.20 * stateAt(coordinate, ivec2(-1,  0));
    laplacian += 0.20 * stateAt(coordinate, ivec2( 0,  1));
    laplacian += 0.20 * stateAt(coordinate, ivec2( 0, -1));
    laplacian += 0.05 * stateAt(coordinate, ivec2( 1,  1));
    laplacian += 0.05 * stateAt(coordinate, ivec2(-1,  1));
    laplacian += 0.05 * stateAt(coordinate, ivec2( 1, -1));
    laplacian += 0.05 * stateAt(coordinate, ivec2(-1, -1));

    float u = chemistry.r;
    float v = chemistry.g;
    float uvv = u * v * v;
    u += laplacian.r - uvv + feedRate * (1.0 - u);
    v += 0.5 * laplacian.g + uvv - (feedRate + killRate) * v;

    // Mode-4 hazy void: the glyph holds the actual chemistry toward empty U/V
    // for its LRC interval. Once the sung line ends, only Gray-Scott evolution
    // fills the cavity; the display shader has no text mask or opacity animation.
    float rawMask = maskAt(coordinate, ivec2(0));
    float mask = rawMask * carveStrength;
    float chemicalWeight = smoothstep(0.04, 0.72, mask);
    u = mix(u, 1.0, chemicalWeight);
    v = mix(v, 0.0, chemicalWeight);

    // A one-texel outer ring is injected into the same chemistry. Its V peak
    // is above the free pattern's normal range, so the display shader can
    // resolve it as a very fine white rim without compositing a text layer.
    float nearbyMask = max(
        max(maskAt(coordinate, ivec2( 1,  0)), maskAt(coordinate, ivec2(-1,  0))),
        max(maskAt(coordinate, ivec2( 0,  1)), maskAt(coordinate, ivec2( 0, -1)))
    );
    float outsideGlyph = 1.0 - smoothstep(0.02, 0.24, rawMask);
    float ring = smoothstep(0.08, 0.62, nearbyMask)
               * outsideGlyph
               * clamp(carveStrength / 0.13, 0.0, 1.0);
    u = mix(u, 0.34, ring * 0.30);
    v = mix(v, 0.86, ring * 0.30);

    nextState = clamp(vec2(u, v), 0.0, 1.0);
}
"#;

const DISPLAY_SHADER: &str = r#"#version 330 core
uniform sampler2D state;
uniform vec3 paletteDark;
uniform vec3 paletteMid;
uniform vec3 paletteLight;
in vec2 uv;
out vec4 color;

void main() {
    float v = texture(state, uv).g;
    float concentration = clamp((v - 0.02) / 0.40, 0.0, 1.0);

    vec3 background = paletteDark * 0.18;
    vec3 albumPattern = mix(
        paletteMid,
        paletteLight,
        smoothstep(0.14, 0.88, concentration)
    );
    vec3 base = mix(
        background,
        albumPattern,
        smoothstep(0.04, 0.66, concentration)
    );

    vec2 texel = 1.0 / vec2(textureSize(state, 0));
    float gx = texture(state, uv + vec2(texel.x, 0.0)).g
             - texture(state, uv - vec2(texel.x, 0.0)).g;
    float gy = texture(state, uv + vec2(0.0, texel.y)).g
             - texture(state, uv - vec2(0.0, texel.y)).g;
    float edge = clamp(length(vec2(gx, gy)) * 2.2, 0.0, 1.0);
    vec3 coverHighlight = mix(paletteMid, paletteLight, 0.72);
    base += coverHighlight * pow(edge, 1.5) * 0.52;
    float lyricRim = smoothstep(0.74, 0.90, v);
    base = mix(base, vec3(1.0), lyricRim * 0.76);

    float vignette = smoothstep(1.25, 0.35, length(uv - 0.5) * 1.4);
    base *= mix(0.65, 1.0, vignette);
    color = vec4(base, 0.90);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_four_envelope_matches_reference_recipe() {
        assert_eq!(glyph_chemistry_envelope(-0.1, 3.0), 0.0);
        assert_eq!(glyph_chemistry_envelope(0.0, 3.0), 0.0);
        assert!((glyph_chemistry_envelope(0.30, 3.0) - 0.13).abs() < 0.0001);
        assert_eq!(
            glyph_chemistry_envelope(2.90, 3.0),
            GLYPH_CHEMISTRY_STRENGTH
        );
        assert_eq!(glyph_release_seconds(0.5), GLYPH_MIN_RELEASE_SECONDS);
        assert_eq!(glyph_release_seconds(10.0), GLYPH_MAX_RELEASE_SECONDS);
        assert_eq!(
            glyph_chemistry_envelope(3.0 + glyph_release_seconds(3.0), 3.0),
            0.0
        );
    }

    #[test]
    fn audio_seed_is_deterministic_two_channel_gray_scott_state() {
        let first = make_seed(0x1234_5678);
        let again = make_seed(0x1234_5678);
        let other = make_seed(0x8765_4321);
        assert_eq!(first, again);
        assert_ne!(first, other);
        assert_eq!(first.len(), (SIM_WIDTH * SIM_HEIGHT * 2) as usize);
        let disturbed = first
            .chunks_exact(2)
            .filter(|pair| pair[1] > 0.5 && pair[0] < 0.9)
            .count();
        assert!(disturbed > 1_000);
    }
}
