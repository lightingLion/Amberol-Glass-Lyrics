// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{audio::Song, lyrics::LyricsTrack, reaction_diffusion_view::ReactionDiffusionView};
use adw::subclass::prelude::*;
use gtk::{glib, prelude::*, CompositeTemplate};
use std::cell::{Cell, RefCell};

mod imp {
    use super::*;

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
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
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
        let track = song
            .file()
            .path()
            .map(|p| LyricsTrack::load_for_audio(&p))
            .unwrap_or_default();
        let intro_seconds = track
            .lines
            .first()
            .map(|line| line.start_ms as f32 / 1_000.0)
            .filter(|seconds| *seconds >= 0.5)
            .unwrap_or(3.0);
        imp.reaction_background.begin_song(intro_seconds);
        if track.lines.is_empty() {
            imp.track.replace(track);
            self.show_empty("No synced lyrics found");
        } else {
            imp.status_label.set_visible(false);
            imp.track.replace(track);
            self.update_position(0);
        }
    }

    pub fn update_position(&self, seconds: u64) {
        let imp = self.imp();
        imp.reaction_background
            .set_playback_position(seconds as f32);
        let track = imp.track.borrow();
        if track.lines.is_empty() {
            return;
        }
        let Some(index) = track.current_index(seconds.saturating_mul(1000)) else {
            if imp.current_index.take().is_some() {
                imp.reaction_background.set_lyric("", 0.0);
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
        let duration_seconds = track
            .lines
            .get(index + 1)
            .map(|next| next.start_ms.saturating_sub(track.lines[index].start_ms) as f32 / 1_000.0)
            .unwrap_or(5.0)
            .max(1.0);
        imp.reaction_background.set_lyric(text(0), duration_seconds);
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
        imp.reaction_background.set_lyric("", 0.0);
        imp.status_label.set_label(message);
        imp.status_label.set_visible(true);
    }
}
