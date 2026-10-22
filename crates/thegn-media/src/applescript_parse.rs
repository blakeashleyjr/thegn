//! Pure parser and read-script builder for the escaped, unit-separated line
//! the macOS `osascript` snippet emits — no process/OS deps, so it compiles and
//! unit-tests on a Linux CI box.
//! [`applescript`](crate::applescript) (macOS only) shells out and folds the
//! result here.
//!
//! Field layout (unit-separator `\x1f` delimited, with backslashes and embedded
//! separators escaped inside each field), produced identically for Music.app
//! and Spotify:
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
    let encoded_fields: Vec<&str> = line.split(SEP).collect();
    if encoded_fields.len() != FIELDS {
        return Err(MediaError::Backend(format!(
            "malformed AppleScript metadata record ({} fields)",
            encoded_fields.len()
        )));
    }
    let f = encoded_fields
        .into_iter()
        .map(unescape_field)
        .collect::<Result<Vec<_>, _>>()?;
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

fn unescape_field(s: &str) -> Result<String, MediaError> {
    let mut unescaped = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            unescaped.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => unescaped.push('\\'),
            Some('u') => unescaped.push(SEP),
            Some(_) | None => {
                return Err(MediaError::Backend(
                    "malformed AppleScript field escape".into(),
                ));
            }
        }
    }
    Ok(unescaped)
}

pub(crate) fn loop_body(app: &str, mode: LoopMode) -> Result<&'static str, MediaError> {
    if app == "Spotify" {
        return match mode {
            LoopMode::None => Ok("set repeating to false"),
            LoopMode::Playlist => Ok("set repeating to true"),
            LoopMode::Track => Err(MediaError::Unsupported(
                "Spotify has no track repeat".into(),
            )),
        };
    }
    match mode {
        LoopMode::None => Ok("set song repeat to off"),
        LoopMode::Track => Ok("set song repeat to one"),
        LoopMode::Playlist => Ok("set song repeat to all"),
    }
}

pub(crate) fn read_script(app: &str) -> String {
    // Spotify reports `duration` in ms and uses `shuffling`/`repeating`; Music
    // reports `duration` in seconds and uses `shuffle enabled`/`song repeat`.
    let (duration_expr, shuffle_expr, repeat_expr) = if app == "Spotify" {
        (
            "((duration of t) / 1000)",
            "(shuffling as text)",
            "(repeating as text)",
        )
    } else {
        (
            "(duration of t)",
            "(shuffle enabled as text)",
            "(song repeat as text)",
        )
    };
    format!(
        r#"on esc(v)
  set s to v as text
  set AppleScript's text item delimiters to "\\"
  set parts to text items of s
  set AppleScript's text item delimiters to "\\\\"
  set s to parts as text
  set AppleScript's text item delimiters to (ASCII character 31)
  set parts to text items of s
  set AppleScript's text item delimiters to "\\u"
  set s to parts as text
  set AppleScript's text item delimiters to ""
  return s
end esc
if application "{app}" is running then
tell application "{app}"
if player state is stopped then
return ""
end if
set sep to (ASCII character 31)
set t to current track
return my esc("{app}") & sep & my esc(player state as text) & sep & my esc(name of t) & sep & my esc(artist of t) & sep & my esc(album of t) & sep & my esc({duration_expr}) & sep & my esc(player position) & sep & my esc({shuffle_expr}) & sep & my esc({repeat_expr}) & sep & my esc(sound volume as text)
end tell
end if
return """#
    )
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

    fn escape_fixture(value: &str) -> String {
        value.replace('\\', "\\\\").replace(SEP, "\\u")
    }

    fn encoded_record(fields: [&str; FIELDS]) -> String {
        let separator = SEP.to_string();
        fields
            .iter()
            .map(|field| escape_fixture(field))
            .collect::<Vec<_>>()
            .join(&separator)
    }

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
    fn escaped_separator_and_backslash_round_trip_in_every_field() {
        let player = "Music\u{1f}\\client";
        let playback = "playing\u{1f}\\state";
        let title = "title\u{1f}\\value";
        let artist = "artist\u{1f}\\value";
        let album = "album\u{1f}\\value";
        let line = encoded_record([
            player,
            playback,
            title,
            artist,
            album,
            "248.0\u{1f}\\length",
            "12.5\u{1f}\\position",
            "true\u{1f}\\shuffle",
            "one\u{1f}\\loop",
            "80\u{1f}\\volume",
        ]);
        let state = parse_line(&line).unwrap();
        assert_eq!(state.player, player);
        assert_eq!(state.title, title);
        assert_eq!(state.artist, artist);
        assert_eq!(state.album, album);
        assert_eq!(state.length, None);
        assert_eq!(state.position, None);
        assert_eq!(state.shuffle, None);
        assert_eq!(state.loop_mode, None);
        assert_eq!(state.volume, None);
    }

    #[test]
    fn malformed_escape_rejects_the_record() {
        for bad_field in ["bad\\", "bad\\q"] {
            let line = format!(
                "Music{SEP}playing{SEP}{bad_field}{SEP}artist{SEP}album{SEP}248{SEP}12{SEP}true{SEP}one{SEP}80"
            );
            assert!(matches!(parse_line(&line), Err(MediaError::Backend(_))));
        }
    }

    #[test]
    fn read_script_wraps_every_field_in_esc() {
        for app in ["Spotify", "Music"] {
            let script = read_script(app);
            assert!(script.contains("on esc(v)"));
            assert_eq!(script.matches("my esc(").count(), FIELDS);
        }
    }

    #[test]
    fn spotify_track_repeat_is_unsupported() {
        assert!(matches!(
            loop_body("Spotify", LoopMode::Track),
            Err(MediaError::Unsupported(_))
        ));
        assert_eq!(
            loop_body("Spotify", LoopMode::Playlist),
            Ok("set repeating to true")
        );
        assert_eq!(
            loop_body("Music", LoopMode::Track),
            Ok("set song repeat to one")
        );
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
