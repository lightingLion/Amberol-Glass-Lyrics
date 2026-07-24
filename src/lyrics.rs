// SPDX-FileCopyrightText: 2026 Amberol Glass Lyrics contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use regex::Regex;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LyricsLine {
    pub start_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct LyricsTrack {
    pub lines: Vec<LyricsLine>,
    pub synced: bool,
    pub source: Option<PathBuf>,
}

impl LyricsTrack {
    pub fn load_for_audio(audio: &Path) -> Self {
        Self::load_for_audio_with_embedded(audio, None)
    }

    pub fn load_for_audio_with_embedded(audio: &Path, embedded: Option<&str>) -> Self {
        let lrc = audio.with_extension("lrc");
        if let Ok(content) = fs::read_to_string(&lrc) {
            let mut track = Self::parse_lrc(&content);
            track.source = Some(lrc);
            if !track.lines.is_empty() {
                return track;
            }
        }

        if let Some(content) = embedded {
            let mut track = Self::parse_lrc(content);
            if !track.lines.is_empty() {
                // No sidecar path: this track came from LYRICS/SYNCEDLYRICS
                // metadata embedded in the audio container.
                track.source = None;
                return track;
            }
        }

        let txt = audio.with_extension("txt");
        if let Ok(content) = fs::read_to_string(&txt) {
            let lines = content
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .enumerate()
                .map(|(i, text)| LyricsLine {
                    start_ms: i as u64 * 5_000,
                    text: text.to_string(),
                })
                .collect();
            return Self {
                lines,
                synced: false,
                source: Some(txt),
            };
        }
        Self::default()
    }

    pub fn parse_lrc(input: &str) -> Self {
        let tag = Regex::new(r"\[(\d{1,3}):(\d{2})(?:[\.:](\d{1,3}))?\]").unwrap();
        let offset_re = Regex::new(r"(?i)^\[offset:([+-]?\d+)\]").unwrap();
        let mut offset: i64 = 0;
        let mut lines = Vec::new();
        for raw in input.lines() {
            let raw = raw.trim().trim_start_matches('\u{feff}');
            if let Some(c) = offset_re.captures(raw) {
                offset = c[1].parse().unwrap_or(0);
                continue;
            }
            let text = tag.replace_all(raw, "").trim().to_string();
            if text.is_empty() {
                continue;
            }
            for c in tag.captures_iter(raw) {
                let minutes: u64 = c[1].parse().unwrap_or(0);
                let seconds: u64 = c[2].parse().unwrap_or(0);
                let fraction = c.get(3).map(|v| v.as_str()).unwrap_or("0");
                let fraction_ms = match fraction.len() {
                    1 => fraction.parse::<u64>().unwrap_or(0) * 100,
                    2 => fraction.parse::<u64>().unwrap_or(0) * 10,
                    _ => fraction[..fraction.len().min(3)].parse().unwrap_or(0),
                };
                let base = (minutes * 60 + seconds) * 1_000 + fraction_ms;
                lines.push(LyricsLine {
                    start_ms: (base as i64 + offset).max(0) as u64,
                    text: text.clone(),
                });
            }
        }
        lines.sort_by_key(|line| line.start_ms);
        Self {
            synced: !lines.is_empty(),
            lines,
            source: None,
        }
    }

    pub fn current_index(&self, position_ms: u64) -> Option<usize> {
        let next = self
            .lines
            .partition_point(|line| line.start_ms <= position_ms);
        next.checked_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_lrc_and_multiple_timestamps() {
        let t = LyricsTrack::parse_lrc("[00:01.20]One\n[00:02.50][00:03.00]Two");
        assert_eq!(t.lines.len(), 3);
        assert_eq!(t.lines[0].start_ms, 1200);
        assert_eq!(t.current_index(2600), Some(1));
    }
    #[test]
    fn applies_offset() {
        let t = LyricsTrack::parse_lrc("[offset:-500]\n[00:01.00]One");
        assert_eq!(t.lines[0].start_ms, 500);
    }

    #[test]
    fn switches_on_exact_subsecond_boundaries() {
        let t = LyricsTrack::parse_lrc("[00:00.500]First\n[00:01.001]Second\n[00:01.250]Third");
        assert_eq!(t.current_index(499), None);
        assert_eq!(t.current_index(500), Some(0));
        assert_eq!(t.current_index(1_000), Some(0));
        assert_eq!(t.current_index(1_001), Some(1));
        assert_eq!(t.current_index(1_249), Some(1));
        assert_eq!(t.current_index(1_250), Some(2));
    }

    #[test]
    fn loads_synced_embedded_lyrics_when_sidecar_is_absent() {
        let audio = std::env::temp_dir().join(format!(
            "amberol-glass-lyrics-metadata-{}.flac",
            std::process::id()
        ));
        let track = LyricsTrack::load_for_audio_with_embedded(
            &audio,
            Some("[00:00.500]Embedded first\n[00:02.250]Embedded second"),
        );

        assert!(track.synced);
        assert_eq!(track.lines.len(), 2);
        assert_eq!(track.lines[0].text, "Embedded first");
        assert_eq!(track.lines[1].start_ms, 2_250);
        assert!(track.source.is_none());
    }
}
