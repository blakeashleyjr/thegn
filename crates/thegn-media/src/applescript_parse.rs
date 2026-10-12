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

use crate::MediaError;
use crate::model::{LoopMode, MediaKind, MediaState, PlaybackState};

const SEP: char = '\u{1f}';
const FIELDS: usize = 10;

pub(crate) fn pick_active(
    reads: Vec<(&str, Result<Option<String>, MediaError>)>,
) -> Result<Option<(&str, String)>, MediaError> {
    let mut first_error = None;
    for (app, read) in reads {
        match read {
            Ok(Some(line)) => return Ok(Some((app, line))),
            Ok(None) => {}
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

pub(crate) fn parse_line(line: &str) -> Result<MediaState, MediaError> {
    let f: Vec<&str> = line.split(SEP).collect();
    if f.len() != FIELDS {
        return Err(MediaError::Backend(format!(
            "malformed AppleScript metadata record ({} fields)",
            f.len()
        )));
    }
    let get = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");

    // Music.app reports seconds (real); Spotify's ms are divided in the script.
    let length = get(5)
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(Duration::from_secs_f64);
    let position = get(6)
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64);

    let player = get(0).to_string();
    let kind = MediaKind::from_hints(&player, None, None);
    Ok(MediaState {
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
            .filter(|v| v.is_finite())
            .map(|v| v.round().clamp(0.0, 100.0) as u8),
        can_go_next: true,
        can_go_previous: true,
        art_url: None, // artwork is binary; not fetched via the scripting floor
        kind,
        can_seek: true, // `player position` is settable on both apps
        track_id: None,
        player,
    })
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
        "off" | "false" | "none" => Some(LoopMode::None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spotify_line() {
        // Spotify: state lowercase, ms→secs done in script, repeating=true.
        let line = "Spotify\u{1f}playing\u{1f}Get Lucky\u{1f}Daft Punk\u{1f}Random Access Memories\u{1f}248.0\u{1f}12.5\u{1f}true\u{1f}true\u{1f}80";
        let s = parse_line(line).unwrap();
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
        let s = parse_line(line).unwrap();
        assert_eq!(s.player, "Music");
        assert_eq!(s.state, PlaybackState::Paused);
        assert_eq!(s.loop_mode, Some(LoopMode::Track));
        assert_eq!(s.shuffle, Some(false));
        assert_eq!(s.volume, Some(55));
    }

    #[test]
    fn tolerates_blanks_and_off() {
        let line = "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}off\u{1f}";
        let s = parse_line(line).unwrap();
        assert_eq!(s.title, "X");
        assert_eq!(s.artist, "");
        assert_eq!(s.length, None);
        assert_eq!(s.loop_mode, Some(LoopMode::None));
        assert_eq!(s.volume, None);
    }

    #[test]
    fn unknown_loop_text_stays_unknown() {
        let line = "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}weird\u{1f}";
        assert_eq!(parse_line(line).unwrap().loop_mode, None);
    }

    #[test]
    fn rejects_wrong_field_counts_and_separator_collision() {
        let nine = "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}off";
        let eleven =
            "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}off\u{1f}\u{1f}extra";
        let collision = "Music\u{1f}playing\u{1f}X\u{1f}in-title\u{1f}Artist\u{1f}Album\u{1f}248\u{1f}12\u{1f}true\u{1f}false\u{1f}80";
        assert!(parse_line(nine).is_err());
        assert!(parse_line(eleven).is_err());
        assert!(parse_line(collision).is_err());
    }

    #[test]
    fn non_finite_volume_is_unknown() {
        let line = "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}off\u{1f}NaN";
        assert_eq!(parse_line(line).unwrap().volume, None);
    }

    #[test]
    fn active_app_does_not_hide_read_errors() {
        let error = MediaError::Backend("authorization denied".into());
        assert_eq!(
            pick_active(vec![("Spotify", Err(error.clone())), ("Music", Ok(None)),]),
            Err(error)
        );
    }

    #[test]
    fn active_app_uses_later_success_after_earlier_read_error() {
        let line = "Music\u{1f}playing\u{1f}X\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}off\u{1f}50";
        assert_eq!(
            pick_active(vec![
                ("Spotify", Err(MediaError::Backend("read failed".into()))),
                ("Music", Ok(Some(line.into()))),
            ])
            .unwrap(),
            Some(("Music", line.into()))
        );
    }
}
