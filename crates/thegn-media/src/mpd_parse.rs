//! Pure MPD-protocol decoders — folding `status` + `currentsong` key/value pairs
//! onto the shared [`MediaState`]. Split out of [`crate::mpd`] so it unit-tests on
//! every platform without a running daemon (mirrors `smtc_decode`/
//! `applescript_parse`). No I/O, no tokio.

use std::time::Duration;

use crate::model::{LoopMode, MediaKind, MediaState, PlaybackState};

/// A parsed `key: value` line from an MPD response.
pub type Pair = (String, String);

/// Split one MPD response line into a `(key, value)` pair on the first `": "`.
/// Returns `None` for status lines (`OK`, `ACK …`) or anything without a colon.
pub fn parse_line(line: &str) -> Option<Pair> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line == "OK" || line.starts_with("OK ") || line.starts_with("ACK") {
        return None;
    }
    let (k, v) = line.split_once(':')?;
    Some((k.trim().to_string(), v.trim_start().to_string()))
}

/// The value for `key` (exact match; MPD keys have fixed casing), if present.
pub fn field<'a>(pairs: &'a [Pair], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// MPD `state` (`play`/`pause`/`stop`) → [`PlaybackState`].
pub fn parse_state(s: &str) -> PlaybackState {
    match s.trim().to_ascii_lowercase().as_str() {
        "play" => PlaybackState::Playing,
        "pause" => PlaybackState::Paused,
        _ => PlaybackState::Stopped,
    }
}

/// MPD repeat/single flags → [`LoopMode`]: `single` means repeat-one, `repeat`
/// alone means repeat-all.
pub fn parse_loop(repeat: bool, single: bool) -> LoopMode {
    match (repeat, single) {
        (_, true) => LoopMode::Track,
        (true, false) => LoopMode::Playlist,
        (false, false) => LoopMode::None,
    }
}

/// Parse an MPD boolean flag (`"0"`/`"1"`).
fn flag(pairs: &[Pair], key: &str) -> Option<bool> {
    field(pairs, key).map(|v| v.trim() == "1")
}

/// Parse a float-seconds field (`elapsed`, `duration`) into a `Duration`.
fn secs(pairs: &[Pair], key: &str) -> Option<Duration> {
    let s: f64 = field(pairs, key)?.trim().parse().ok()?;
    (s.is_finite() && s >= 0.0).then(|| Duration::from_secs_f64(s))
}

/// Fold an MPD `status` + `currentsong` response into a [`MediaState`]. Pure so
/// the wire format is fully unit-tested without a daemon.
pub fn to_state(status: &[Pair], song: &[Pair]) -> MediaState {
    let state = parse_state(field(status, "state").unwrap_or("stop"));

    let title = field(song, "Title").unwrap_or_default().to_string();
    let artist = field(song, "Artist")
        .or_else(|| field(song, "AlbumArtist"))
        .unwrap_or_default()
        .to_string();
    let album = field(song, "Album").unwrap_or_default().to_string();
    let file = field(song, "file");

    // Length: prefer `status.duration` (float), fall back to the song's integer
    // `Time`, then `duration`.
    let length = secs(status, "duration")
        .or_else(|| secs(song, "duration"))
        .or_else(|| {
            field(song, "Time")
                .and_then(|t| t.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
        });
    let position = secs(status, "elapsed");

    // MPD volume is `0..=100`, or `-1` when it has no mixer.
    let volume = field(status, "volume")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&v| v >= 0)
        .map(|v| v.clamp(0, 100) as u8);

    let shuffle = flag(status, "random");
    let loop_mode = Some(parse_loop(
        flag(status, "repeat").unwrap_or(false),
        flag(status, "single").unwrap_or(false),
    ));

    // MPD is audio in practice; classify off the file extension, defaulting audio.
    let kind = match MediaKind::from_hints("mpd", None, file) {
        MediaKind::Video => MediaKind::Video,
        _ => MediaKind::Audio,
    };

    MediaState {
        player: "mpd".to_string(),
        title,
        artist,
        album,
        state,
        position,
        length,
        shuffle,
        loop_mode,
        volume,
        can_go_next: true,
        can_go_previous: true,
        art_url: None, // readpicture/albumart deferred
        kind,
        can_seek: true,
        track_id: field(status, "songid").map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(lines: &[&str]) -> Vec<Pair> {
        lines.iter().filter_map(|l| parse_line(l)).collect()
    }

    #[test]
    fn parse_line_splits_and_skips_status() {
        assert_eq!(
            parse_line("Title: Ezio's Family"),
            Some(("Title".into(), "Ezio's Family".into()))
        );
        // Values may themselves contain colons (file paths, URLs).
        assert_eq!(
            parse_line("file: http://host:8000/stream"),
            Some(("file".into(), "http://host:8000/stream".into()))
        );
        assert_eq!(parse_line("OK"), None);
        assert_eq!(parse_line("OK MPD 0.24.0"), None);
        assert_eq!(parse_line("ACK [50@0] {play} No such song"), None);
    }

    #[test]
    fn folds_status_and_song() {
        let status = pairs(&[
            "volume: 80",
            "repeat: 0",
            "random: 1",
            "single: 0",
            "state: play",
            "songid: 12",
            "elapsed: 12.500",
            "duration: 248.000",
        ]);
        let song = pairs(&[
            "file: music/ezio.flac",
            "Title: Ezio's Family",
            "Artist: Jesper Kyd",
            "Album: Assassin's Creed 2",
            "Time: 248",
        ]);
        let s = to_state(&status, &song);
        assert_eq!(s.player, "mpd");
        assert_eq!(s.title, "Ezio's Family");
        assert_eq!(s.artist, "Jesper Kyd");
        assert_eq!(s.album, "Assassin's Creed 2");
        assert_eq!(s.state, PlaybackState::Playing);
        assert_eq!(s.position, Some(Duration::from_secs_f64(12.5)));
        assert_eq!(s.length, Some(Duration::from_secs_f64(248.0)));
        assert_eq!(s.volume, Some(80));
        assert_eq!(s.shuffle, Some(true));
        assert_eq!(s.loop_mode, Some(LoopMode::None));
        assert_eq!(s.kind, MediaKind::Audio);
        assert!(s.can_seek);
    }

    #[test]
    fn no_mixer_volume_is_none_and_stopped() {
        let status = pairs(&["volume: -1", "state: stop", "repeat: 1", "single: 1"]);
        let s = to_state(&status, &[]);
        assert_eq!(s.volume, None);
        assert_eq!(s.state, PlaybackState::Stopped);
        // single overrides repeat → repeat-one.
        assert_eq!(s.loop_mode, Some(LoopMode::Track));
        assert!(!s.state.is_active());
    }

    #[test]
    fn time_fallback_when_no_float_duration() {
        let status = pairs(&["state: pause"]);
        let song = pairs(&["Title: X", "Time: 200"]);
        let s = to_state(&status, &song);
        assert_eq!(s.length, Some(Duration::from_secs(200)));
        assert_eq!(s.state, PlaybackState::Paused);
    }
}
