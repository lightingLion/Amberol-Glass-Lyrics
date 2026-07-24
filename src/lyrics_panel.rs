// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{audio::Song, lyrics::LyricsTrack, reaction_diffusion_view::ReactionDiffusionView};
use adw::subclass::prelude::*;
use gtk::{glib, prelude::*, CompositeTemplate};
use std::{
    cell::{Cell, RefCell},
    fs::File,
    io::{Read, Seek, SeekFrom},
};

mod imp {
    use super::*;
    use glib::subclass::Signal;
    use once_cell::sync::Lazy;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/io/bassi/Amberol/lyrics-panel.ui")]
    pub struct LyricsPanel {
        #[template_child]
        pub title_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub older_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub previous_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub current_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub next_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub later_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub status_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub reaction_background: TemplateChild<ReactionDiffusionView>,
        pub track: RefCell<LyricsTrack>,
        pub current_index: Cell<Option<usize>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LyricsPanel {
        const NAME: &'static str = "AmberolLyricsPanel";
        type Type = super::LyricsPanel;
        type ParentType = gtk::Widget;
        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);
            klass.set_layout_manager_type::<gtk::BinLayout>();
            klass.set_css_name("lyricspanel");
            klass.set_accessible_role(gtk::AccessibleRole::Group);
        }
        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            ReactionDiffusionView::static_type();
            obj.init_template();
        }
    }
    impl ObjectImpl for LyricsPanel {
        fn constructed(&self) {
            self.parent_constructed();
            let panel = self.obj().downgrade();
            self.reaction_background
                .connect_local("pattern-ready", false, move |_| {
                    if let Some(panel) = panel.upgrade() {
                        panel.emit_by_name::<()>("pattern-ready", &[]);
                    }
                    None
                });
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }

        fn signals() -> &'static [Signal] {
            static SIGNALS: Lazy<Vec<Signal>> =
                Lazy::new(|| vec![Signal::builder("pattern-ready").build()]);
            SIGNALS.as_ref()
        }
    }
    impl WidgetImpl for LyricsPanel {}
}

glib::wrapper! {
    pub struct LyricsPanel(ObjectSubclass<imp::LyricsPanel>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for LyricsPanel {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl LyricsPanel {
    pub fn load_song(&self, song: Option<&Song>) {
        let imp = self.imp();
        imp.current_index.set(None);
        let Some(song) = song else {
            imp.title_label.set_label("Lyrics");
            imp.track.replace(LyricsTrack::default());
            self.show_empty("No synced lyrics found");
            return;
        };
        imp.title_label.set_label(&song.title());
        if let Some(palette) = song.cover_palette() {
            imp.reaction_background.set_palette(&palette);
        }
        let embedded_lyrics = song.lyrics();
        let track = song
            .file()
            .path()
            .map(|p| LyricsTrack::load_for_audio_with_embedded(&p, embedded_lyrics.as_deref()))
            .unwrap_or_default();
        let intro_seconds = track
            .lines
            .first()
            .map(|line| line.start_ms as f32 / 1_000.0)
            .filter(|seconds| *seconds >= 0.5)
            .unwrap_or(3.0);
        imp.reaction_background
            .begin_song(intro_seconds, audio_pattern_seed(song));
        if track.lines.is_empty() {
            imp.track.replace(track);
            self.show_empty("No synced lyrics found");
        } else {
            imp.status_label.set_visible(false);
            imp.track.replace(track);
            self.update_position_ms(0);
        }
    }

    pub fn set_playing(&self, playing: bool) {
        self.imp().reaction_background.set_playing(playing);
    }

    pub fn update_position_ms(&self, position_ms: u64) {
        let imp = self.imp();
        imp.reaction_background
            .set_playback_position(position_ms as f32 / 1_000.0);
        let track = imp.track.borrow();
        if track.lines.is_empty() {
            return;
        }
        let Some(index) = track.current_index(position_ms) else {
            if imp.current_index.take().is_some() {
                imp.reaction_background.set_lyric("", 0, 0);
            }
            return;
        };
        if imp.current_index.replace(Some(index)) == Some(index) {
            return;
        }
        let text = |offset: isize| -> &str {
            let pos = index as isize + offset;
            if pos < 0 {
                ""
            } else {
                track
                    .lines
                    .get(pos as usize)
                    .map(|l| l.text.as_str())
                    .unwrap_or("")
            }
        };
        imp.older_label.set_label(text(-2));
        imp.previous_label.set_label(text(-1));
        imp.current_label.set_label(text(0));
        let duration_ms = track
            .lines
            .get(index + 1)
            .map(|next| next.start_ms.saturating_sub(track.lines[index].start_ms))
            .unwrap_or(5_000)
            .max(50);
        log::debug!(
            "LRC cue {} ms activated at {} ms (visual duration {} ms)",
            track.lines[index].start_ms,
            position_ms,
            duration_ms
        );
        imp.reaction_background
            .set_lyric(text(0), track.lines[index].start_ms, duration_ms);
        imp.next_label.set_label(text(1));
        imp.later_label.set_label(text(2));
    }

    fn show_empty(&self, message: &str) {
        let imp = self.imp();
        imp.older_label.set_label("");
        imp.previous_label.set_label("");
        imp.current_label.set_label(message);
        imp.next_label.set_label("");
        imp.later_label.set_label("");
        imp.reaction_background.set_lyric("", 0, 0);
        imp.status_label.set_label(message);
        imp.status_label.set_visible(true);
    }
}

/// Samples small, evenly distributed chunks of the real audio file. This is
/// intentionally cheap (a few KiB total) but content-dependent: the same song
/// recreates the same opening composition, while another file gets different
/// seed coordinates even when its metadata is similar.
fn audio_pattern_seed(song: &Song) -> u64 {
    const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut hash = FNV_OFFSET;
    let update = |hash: &mut u64, bytes: &[u8]| {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(FNV_PRIME);
        }
    };

    if let Some(path) = song.file().path() {
        update(&mut hash, path.to_string_lossy().as_bytes());
        if let Ok(mut file) = File::open(&path) {
            if let Ok(metadata) = file.metadata() {
                let length = metadata.len();
                update(&mut hash, &length.to_le_bytes());
                let mut sample = [0_u8; 768];
                for index in 0..8_u64 {
                    let last_start = length.saturating_sub(sample.len() as u64);
                    let offset = last_start.saturating_mul(index) / 7;
                    if file.seek(SeekFrom::Start(offset)).is_ok() {
                        if let Ok(read) = file.read(&mut sample) {
                            update(&mut hash, &offset.to_le_bytes());
                            update(&mut hash, &sample[..read]);
                        }
                    }
                }
                return hash;
            }
        }
    }
    if let Some(uuid) = song.uuid() {
        update(&mut hash, uuid.as_bytes());
    }
    hash
}
