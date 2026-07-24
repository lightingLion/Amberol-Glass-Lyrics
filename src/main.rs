// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

mod application;
mod audio;
mod config;
mod cover_picture;
mod drag_overlay;
mod i18n;
mod lyrics;
mod lyrics_panel;
mod marquee;
mod playback_control;
mod playlist_view;
mod queue_row;
mod reaction_diffusion_view;
mod search;
mod song_cover;
mod song_details;
mod sort;
mod utils;
mod volume_control;
mod waveform_view;
mod window;

use std::{env, path::PathBuf};

use config::{APPLICATION_ID, GETTEXT_PACKAGE, LOCALEDIR, PKGDATADIR, PROFILE};
use gettextrs::{bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory};
use gtk::{gio, glib, prelude::*};
use log::{debug, error, LevelFilter};

use self::application::Application;

fn main() -> glib::ExitCode {
    let mut builder = pretty_env_logger::formatted_builder();
    if APPLICATION_ID.ends_with("Devel") {
        builder.filter(Some("amberol"), LevelFilter::Debug);
    } else {
        builder.filter(Some("amberol"), LevelFilter::Info);
    }
    builder.init();

    // Set up gettext translations
    debug!("Setting up locale data");
    setlocale(LocaleCategory::LcAll, "");

    let locale_dir = runtime_locale_dir();
    bindtextdomain(GETTEXT_PACKAGE, &locale_dir).expect("Unable to bind the text domain");
    bind_textdomain_codeset(GETTEXT_PACKAGE, "UTF-8")
        .expect("Unable to set the text domain encoding");
    textdomain(GETTEXT_PACKAGE).expect("Unable to switch to the text domain");

    debug!("Setting up pulseaudio environment");
    let app_id = APPLICATION_ID.trim_end_matches(".Devel");
    env::set_var("PULSE_PROP_application.icon_name", app_id);
    env::set_var(
        "PULSE_PROP_application.metadata().name",
        "Amberol Glass Lyrics",
    );
    env::set_var("PULSE_PROP_media.role", "music");

    debug!("Loading resources");
    configure_windows_runtime();
    let resources = match env::var("MESON_DEVENV") {
        Err(_) => gio::Resource::load(runtime_resource_path())
            .expect("Unable to find amberol-glass-lyrics.gresource"),
        Ok(_) => match env::current_exe() {
            Ok(path) => {
                let mut resource_path = path;
                resource_path.pop();
                resource_path.push("amberol-glass-lyrics.gresource");
                gio::Resource::load(&resource_path)
                    .expect("Unable to find amberol-glass-lyrics.gresource in devenv")
            }
            Err(err) => {
                error!("Unable to find the current path: {}", err);
                return glib::ExitCode::FAILURE;
            }
        },
    };
    gio::resources_register(&resources);

    debug!("Setting up application (profile: {})", &PROFILE);
    glib::set_application_name("Amberol Glass Lyrics");
    glib::set_program_name(Some("amberol-glass-lyrics"));

    gst::init().expect("Failed to initialize gstreamer");

    let ctx = glib::MainContext::default();
    let _guard = ctx.acquire().unwrap();

    Application::new().run()
}

#[cfg(target_os = "windows")]
fn executable_dir() -> Option<PathBuf> {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
}

fn runtime_locale_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(dir) = executable_dir() {
        return dir.join("share").join("locale");
    }

    PathBuf::from(LOCALEDIR)
}

fn runtime_resource_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(dir) = executable_dir() {
        return dir.join("amberol-glass-lyrics.gresource");
    }

    PathBuf::from(PKGDATADIR).join("amberol-glass-lyrics.gresource")
}

#[cfg(target_os = "windows")]
fn configure_windows_runtime() {
    if let Some(dir) = executable_dir() {
        let share = dir.join("share");
        let lib = dir.join("lib");
        env::set_var(
            "GSETTINGS_SCHEMA_DIR",
            share.join("glib-2.0").join("schemas"),
        );
        env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", lib.join("gstreamer-1.0"));
        let pixbuf_cache = lib
            .join("gdk-pixbuf-2.0")
            .join("2.10.0")
            .join("loaders.cache");
        if pixbuf_cache.exists() {
            env::set_var("GDK_PIXBUF_MODULE_FILE", pixbuf_cache);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn configure_windows_runtime() {}
