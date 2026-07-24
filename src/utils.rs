// SPDX-FileCopyrightText: 2022  Emmanuele Bassi
// SPDX-License-Identifier: GPL-3.0-or-later

use core::cmp::Ordering;
use std::{collections::HashSet, path::PathBuf};

use color_thief::{get_palette, ColorFormat};
use gtk::{gdk, gio, glib, prelude::*};
use log::{debug, warn};

use crate::{
    audio::{Queue, Song},
    config::APPLICATION_ID,
};

pub fn settings_manager() -> gio::Settings {
    // We ship a single schema for both default and development profiles
    let app_id = APPLICATION_ID.trim_end_matches(".Devel");
    gio::Settings::new(app_id)
}

pub fn format_remaining_time(t: i64) -> String {
    // We use an explicit LRM character so the MINUS SIGN character
    // stays in front of the remaining time even in RTL locales, instead
    // of getting flipped at the end of the text
    format!("\u{200e}\u{2212}{}:{:02}", (t - (t % 60)) / 60, t % 60)
}

pub fn format_time(t: i64) -> String {
    format!("{}:{:02}", (t - (t % 60)) / 60, t % 60)
}

// The base cover size is 192px, but we need to account for HiDPI;
// better to scale down when rendering on displays with a scaling
// factor of 1 than having to scale up on displays with a scaling
// factor of 2.
const COVER_SIZE: i32 = 192 * 2;

pub fn load_cover_texture(buffer: &glib::Bytes) -> Option<gdk_pixbuf::Pixbuf> {
    let stream = gio::MemoryInputStream::from_bytes(buffer);

    if let Ok(pixbuf) = gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE) {
        let width = pixbuf.width();
        let height = pixbuf.height();
        let ratio = width as f32 / height as f32;

        let w: i32;
        let h: i32;

        if ratio > 1.0 {
            w = COVER_SIZE;
            h = (COVER_SIZE as f32 / ratio) as i32;
        } else {
            w = (COVER_SIZE as f32 * ratio) as i32;
            h = COVER_SIZE;
        }

        debug!("Cover size {width} x {height} (ratio: {ratio}), scaled: {w} x {h}");

        pixbuf.scale_simple(w, h, gdk_pixbuf::InterpType::Nearest)
    } else {
        warn!("Unable to load cover art");
        None
    }
}

pub fn cache_cover_art(uuid: &str, pixbuf: &gdk_pixbuf::Pixbuf) -> Option<PathBuf> {
    let mut cache_dir = glib::user_cache_dir();
    cache_dir.push("amberol");
    cache_dir.push("covers");
    glib::mkdir_with_parents(&cache_dir, 0o755);

    cache_dir.push(format!("{}.png", &uuid));
    let file = gio::File::for_path(&cache_dir);
    match file.create(gio::FileCreateFlags::NONE, gio::Cancellable::NONE) {
        Ok(stream) => {
            debug!("Creating cover data cache at {:?}", &cache_dir);
            pixbuf.save_to_streamv_async(
                &stream,
                "png",
                &[("tEXt::Software", "amberol")],
                gio::Cancellable::NONE,
                move |res| {
                    if let Err(e) = res {
                        warn!("Unable to cache cover data: {}", e);
                    }
                },
            );
        }
        Err(e) => {
            if let Some(file_error) = e.kind::<glib::FileError>() {
                match file_error {
                    glib::FileError::Exist => (),
                    _ => {
                        warn!("Unable to create cache file: {}", e);
                        return None;
                    }
                };
            }
        }
    };

    Some(cache_dir)
}

fn color_format(has_alpha: bool) -> ColorFormat {
    if has_alpha {
        ColorFormat::Rgba
    } else {
        ColorFormat::Rgb
    }
}

pub fn load_palette(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<gdk::RGBA>> {
    if let Ok(palette) = get_palette(
        pixbuf.pixel_bytes().unwrap().as_ref(),
        color_format(pixbuf.has_alpha()),
        5,
        4,
    ) {
        let colors: Vec<gdk::RGBA> = palette
            .iter()
            .map(|c| {
                gdk::RGBA::new(
                    c.r as f32 / 255.0,
                    c.g as f32 / 255.0,
                    c.b as f32 / 255.0,
                    1.0,
                )
            })
            .collect();

        return Some(colors);
    }

    None
}

fn load_files_from_folder_internal(
    base: &gio::File,
    folder: &gio::File,
    recursive: bool,
    visited_folders: &mut HashSet<String>,
) -> Vec<gio::File> {
    // Follow directory links, but identify their target before descending so
    // linked music libraries work without allowing symlink cycles.
    let folder_id = folder
        .query_info(
            "id::filesystem,id::file",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok()
        .and_then(|info| {
            let filesystem = info.attribute_string("id::filesystem")?;
            let file = info.attribute_string("id::file")?;
            Some(format!("{filesystem}:{file}"))
        })
        .unwrap_or_else(|| folder.uri().to_string());
    if !visited_folders.insert(folder_id) {
        debug!("Skipping already visited folder '{}'", folder.uri());
        return Vec::new();
    }

    let mut enumerator = if let Ok(enumerator) = folder.enumerate_children(
        "standard::name,standard::type",
        gio::FileQueryInfoFlags::NONE,
        None::<&gio::Cancellable>,
    ) {
        enumerator
    } else {
        warn!("Unable to enumerate {}", folder.uri());
        return Vec::new();
    };

    let mut files = Vec::new();
    loop {
        match enumerator.next() {
            Some(Ok(info)) => {
                let child = enumerator.child(&info);
                if recursive && info.file_type() == gio::FileType::Directory {
                    let mut res =
                        load_files_from_folder_internal(base, &child, recursive, visited_folders);
                    files.append(&mut res);
                } else if info.file_type() == gio::FileType::Regular {
                    files.push(child);
                }
            }
            Some(Err(err)) => {
                // One unreadable entry must not terminate the rest of the
                // directory scan, otherwise valid songs in later subfolders
                // silently disappear from the import result.
                warn!("Skipping an entry in '{}': {err}", folder.uri());
            }
            None => break,
        }
    }

    // gio::FileEnumerator has no guaranteed order, so we should
    // rely on the basename being formatted in a way that gives us an
    // implicit order; if anything, this will queue songs in the same
    // order in which they appear in the directory when browsing its
    // contents
    files.sort_by(|a, b| cmp_two_files(Some(base), a, b));

    files
}

pub fn cmp_two_files(base: Option<&gio::File>, a: &gio::File, b: &gio::File) -> Ordering {
    let parent_a = a.parent().unwrap();
    let parent_b = b.parent().unwrap();
    let parent_basename_a = if let Some(base) = base {
        if let Some(path) = base.relative_path(&parent_a) {
            path
        } else {
            parent_a.basename().unwrap()
        }
    } else {
        parent_a.basename().unwrap()
    };
    let parent_basename_b = if let Some(base) = base {
        if let Some(path) = base.relative_path(&parent_b) {
            path
        } else {
            parent_b.basename().unwrap()
        }
    } else {
        parent_b.basename().unwrap()
    };
    let basename_a = a.basename().unwrap();
    let basename_b = b.basename().unwrap();

    let mut order = cmp_like_nautilus(
        &parent_basename_a.to_string_lossy(),
        &parent_basename_b.to_string_lossy(),
    );

    if order.is_eq() {
        order = cmp_like_nautilus(&basename_a.to_string_lossy(), &basename_b.to_string_lossy());
    }

    order
}

fn cmp_like_nautilus(filename_a: &str, filename_b: &str) -> Ordering {
    let order;

    let sort_last_a = filename_a.as_bytes()[0] == b'.' || filename_a.as_bytes()[0] == b'#';
    let sort_last_b = filename_b.as_bytes()[0] == b'.' || filename_b.as_bytes()[0] == b'#';

    if !sort_last_a && sort_last_b {
        order = Ordering::Less;
    } else if sort_last_a && !sort_last_b {
        order = Ordering::Greater;
    } else {
        let key_a = glib::FilenameCollationKey::from(filename_a);
        let key_b = glib::FilenameCollationKey::from(filename_b);
        order = key_a.partial_cmp(&key_b).unwrap();
    }

    order
}

pub fn load_files_from_folder(folder: &gio::File, recursive: bool) -> Vec<gio::File> {
    use std::time::Instant;

    let now = Instant::now();
    let mut visited_folders = HashSet::new();
    let res = load_files_from_folder_internal(folder, folder, recursive, &mut visited_folders);
    debug!(
        "Folder enumeration: {} us (recursive: {}), total files: {}",
        now.elapsed().as_micros(),
        recursive,
        res.len()
    );

    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn recursively_loads_files_from_nested_folders() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "amberol-glass-lyrics-folder-scan-{}-{unique}",
            std::process::id()
        ));
        let nested = root.join("album").join("disc-2");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("root.flac"), b"fixture").unwrap();
        fs::write(root.join("album").join("track-1.ogg"), b"fixture").unwrap();
        fs::write(nested.join("track-2.mp3"), b"fixture").unwrap();

        let files = load_files_from_folder(&gio::File::for_path(&root), true);
        let relative_paths: Vec<_> = files
            .iter()
            .map(|file| {
                file.path()
                    .unwrap()
                    .strip_prefix(&root)
                    .unwrap()
                    .to_path_buf()
            })
            .collect();

        assert_eq!(relative_paths.len(), 3);
        assert!(relative_paths.contains(&PathBuf::from("root.flac")));
        assert!(relative_paths.contains(&PathBuf::from("album/track-1.ogg")));
        assert!(relative_paths.contains(&PathBuf::from("album/disc-2/track-2.mp3")));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn follows_linked_subfolders_without_revisiting_cycles() {
        use std::os::unix::fs::symlink;

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "amberol-glass-lyrics-linked-scan-{}-{unique}",
            std::process::id()
        ));
        let library = root.join("library");
        let album = library.join("album");
        let selection = root.join("selection");
        fs::create_dir_all(&album).unwrap();
        fs::create_dir_all(&selection).unwrap();
        fs::write(album.join("linked-track.flac"), b"fixture").unwrap();
        symlink(&album, selection.join("linked-album")).unwrap();
        symlink(&selection, album.join("cycle")).unwrap();

        let files = load_files_from_folder(&gio::File::for_path(&selection), true);

        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].basename().unwrap().to_string_lossy(),
            "linked-track.flac"
        );

        fs::remove_dir_all(root).unwrap();
    }
}

pub fn store_playlist(queue: &Queue) {
    let pls = glib::KeyFile::new();
    pls.set_string("playlist", "X-GNOME-Title", "Amberol's current playlist");

    pls.set_int64("playlist", "NumberOfEntries", queue.n_songs() as i64);

    let model = queue.model();
    for i in 0..model.n_items() {
        let item = model.item(i).unwrap();
        let song = item.downcast_ref::<Song>().unwrap();
        let path = song.file().path().expect("Unknown file");
        let path_str = path.to_string_lossy();
        pls.set_value("playlist", &format!("File{i}"), &path_str);
    }

    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    glib::mkdir_with_parents(&pls_cache, 0o755);

    pls_cache.push("current.pls");
    match pls.save_to_file(&pls_cache) {
        Ok(_) => debug!("Current playlist updated to: {:?}", &pls_cache),
        Err(e) => debug!("Unable to save current playlist: {e}"),
    }
}

pub fn load_cached_songs() -> Option<Vec<gio::File>> {
    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    pls_cache.push("current.pls");

    let pls = glib::KeyFile::new();
    if let Err(e) = pls.load_from_file(&pls_cache, glib::KeyFileFlags::NONE) {
        debug!("Unable to load current playlist: {e}");
        return None;
    }

    let n_entries: usize = match pls.int64("playlist", "NumberOfEntries") {
        Ok(n) => n as usize,
        Err(_) => 0,
    };

    let mut res = Vec::with_capacity(n_entries);

    for i in 0..n_entries {
        match pls.value("playlist", &format!("File{i}")) {
            Ok(p) => res.push(gio::File::for_path(p)),
            Err(e) => debug!("Skipping File{i} from playlist: {e}"),
        }
    }

    Some(res)
}

pub fn has_cached_playlist() -> bool {
    let mut pls_cache = glib::user_cache_dir();
    pls_cache.push("amberol");
    pls_cache.push("playlists");
    pls_cache.push("current.pls");

    pls_cache.exists()
}
