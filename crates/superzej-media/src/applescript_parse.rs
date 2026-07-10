//! Pure parser for the unit-separated line the macOS `osascript` read snippet
//! emits — no process/OS deps, so it compiles and unit-tests on a Linux CI box.
//! [`applescript`](crate::applescript) (macOS only) shells out and folds the
//! result here.
//!
//! Field layout (unit-separator `\x1f` delimited), produced identically for
//! Music.app and Spotify:
//! `player · state · title · artist · album · length_secs · position_secs ·
//! shuffle · loop · volume(0–100)`.

use std::time::Duration;

use crate::model::{LoopMode, MediaKind, MediaState, PlaybackState};

const SEP: char = '\u{1f}';

pub(crate) fn parse_line(line: &str) -> MediaState {
    let f: Vec<&str> = line.split(SEP).collect();
    let get = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");

    // Music.app reports seconds (real); Spotify's ms are divided in the script.
    let length = get(5)
        .parse::<f64>()
        .ok()
        .filter(|s| *s > 0.0)
        .map(Duration::from_secs_f64);
    let position = get(6)
        .parse::<f64>()
        .ok()
        .filter(|s| *s >= 0.0)
        .map(Duration::from_secs_f64);

    let player = get(0).to_string();
    let kind = MediaKind::from_hints(&player, None, None);
    MediaState {
        title: get(2).to_string(),
        artist: get(3).to_string(),
        album: get(4).to_string(),
        state: PlaybackState::from_mpris(get(1)), // playing/paused/stopped
        position,
        length,
        shuffle: parse_bool(get(7)),
        loop_mode: parse_loop(get(8)),
        // Both apps report `sound volume` as an integer percent 0..=100.
        volume: get(9)
            .parse::<f64>()
            .ok()
            .map(|v| v.round().clamp(0.0, 100.0) as u8),
        can_go_next: true,
        can_go_previous: true,
        art_url: None, // artwork is binary; not fetched via the scripting floor
        kind,
        can_seek: true, // `player position` is settable on both apps
        track_id: None,
        player,
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" => Some(true),
        "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

/// Music.app `song repeat` is `off`/`one`/`all`; Spotify `repeating` is a bare
/// boolean (`true` ⇒ whole-context repeat).
fn parse_loop(s: &str) -> Option<LoopMode> {
    match s.to_ascii_lowercase().as_str() {
        "" => None,
        "one" | "track" => Some(LoopMode::Track),
        "all" | "playlist" | "true" => Some(LoopMode::Playlist),
        _ => Some(LoopMode::None), // "off" / "false" / unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spotify_line() {
        // Spotify: state lowercase, ms→secs done in script, repeating=true.
        let line = "Spotify\u{1f}playing\u{1f}Get Lucky\u{1f}Daft Punk\u{1f}Random Access Memories\u{1f}248.0\u{1f}12.5\u{1f}true\u{1f}true\u{1f}80";
        let s = parse_line(line);
        assert_eq!(s.player, "Spotify");
        assert_eq!(s.state, PlaybackState::Playing);
        assert_eq!(s.title, "Get Lucky");
        assert_eq!(s.artist, "Daft Punk");
        assert_eq!(s.length, Some(Duration::from_secs(248)));
        assert_eq!(s.position, Some(Duration::from_secs_f64(12.5)));
        assert_eq!(s.shuffle, Some(true));
        assert_eq!(s.loop_mode, Some(LoopMode::Playlist));
        assert_eq!(s.volume, Some(80));
    }

    #[test]
    fn parses_music_repeat_one() {
        let line = "Music\u{1f}paused\u{1f}Roygbiv\u{1f}Boards of Canada\u{1f}MHTRTC\u{1f}210.0\u{1f}0\u{1f}false\u{1f}one\u{1f}55";
        let s = parse_line(line);
        assert_eq!(s.player, "Music");
        assert_eq!(s.state, PlaybackState::Paused);
        assert_eq!(s.loop_mode, Some(LoopMode::Track));
        assert_eq!(s.shuffle, Some(false));
        assert_eq!(s.volume, Some(55));
    }

    #[test]
    fn tolerates_blanks_and_off() {
        let line = "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}off\u{1f}";
        let s = parse_line(line);
        assert_eq!(s.title, "X");
        assert_eq!(s.artist, "");
        assert_eq!(s.length, None);
        assert_eq!(s.loop_mode, Some(LoopMode::None));
        assert_eq!(s.volume, None);
    }
}
