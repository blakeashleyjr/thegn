//! Pure decoder for the `mediaremote-adapter` now-playing JSON — folded onto
//! [`MediaState`]. Split out of [`crate::mediaremote`] so it unit-tests on Linux
//! without macOS/MediaRemote (mirrors `applescript_parse`/`smtc_decode`).
//!
//! The adapter emits a JSON object per now-playing update. Key names vary by
//! adapter version, so every field is looked up across a few candidates
//! (friendly names and the raw `kMRMediaRemoteNowPlayingInfo*` keys).

use std::time::Duration;

use serde_json::Value;

use crate::MediaError;
use crate::model::{MediaKind, MediaState, PlaybackState};

/// Decode one `get` response, preserving empty-session and malformed-response
/// outcomes separately.
pub fn decode_get(out: &str) -> Result<Option<MediaState>, MediaError> {
    let trimmed = out.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|error| MediaError::Backend(format!("mediaremote json: {error}")))?;
    let Some(object) = value.as_object() else {
        return Err(MediaError::Backend(
            "mediaremote response must be a JSON object or null".into(),
        ));
    };
    for key in ["payload", "info"] {
        if let Some(wrapper) = object.get(key)
            && !wrapper.is_object()
        {
            return Err(MediaError::Backend(format!(
                "mediaremote {key} wrapper must be an object"
            )));
        }
    }
    Ok(to_state(&value))
}

/// First string value present under any of `keys`.
fn s<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

/// First numeric (float) value present under any of `keys`.
fn f(obj: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_f64()))
}

/// First boolean value present under any of `keys`.
fn b(obj: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(|v| v.as_bool()))
}

/// Fold one adapter JSON object into a [`MediaState`]. `None` when there's no
/// track at all (empty payload between sessions).
pub fn to_state(v: &Value) -> Option<MediaState> {
    // Some adapter builds wrap the info in a `payload`/`info` object.
    let obj = v.get("payload").or_else(|| v.get("info")).unwrap_or(v);

    let title = s(obj, &["title", "kMRMediaRemoteNowPlayingInfoTitle"]).unwrap_or_default();
    let artist = s(obj, &["artist", "kMRMediaRemoteNowPlayingInfoArtist"]).unwrap_or_default();
    let album = s(obj, &["album", "kMRMediaRemoteNowPlayingInfoAlbum"]).unwrap_or_default();

    // Nothing playing between sessions → no badge.
    if title.is_empty() && artist.is_empty() && album.is_empty() {
        return None;
    }

    let length = f(obj, &["duration", "kMRMediaRemoteNowPlayingInfoDuration"])
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(Duration::from_secs_f64);
    let position = f(
        obj,
        &["elapsedTime", "kMRMediaRemoteNowPlayingInfoElapsedTime"],
    )
    .filter(|p| p.is_finite() && *p >= 0.0)
    .map(Duration::from_secs_f64);

    // `playing` when present, else derive from playback rate (>0 == playing).
    let playing = b(obj, &["playing"]).unwrap_or_else(|| {
        f(
            obj,
            &["playbackRate", "kMRMediaRemoteNowPlayingInfoPlaybackRate"],
        )
        .map(|r| r > 0.0)
        .unwrap_or(true)
    });
    let state = if playing {
        PlaybackState::Playing
    } else {
        PlaybackState::Paused
    };

    let player = s(
        obj,
        &["bundleIdentifier", "appBundleIdentifier", "app", "player"],
    )
    .map(short_app_name)
    .unwrap_or_default();

    let kind = MediaKind::from_hints(&player, None, None);

    Some(MediaState {
        player,
        title: title.to_string(),
        artist: artist.to_string(),
        album: album.to_string(),
        state,
        position,
        length,
        shuffle: None,
        loop_mode: None,
        volume: None,
        can_go_next: true,
        can_go_previous: true,
        art_url: None, // artwork is a base64 blob; deferred
        kind,
        // The MediaRemote adapter has no seek operation, even when duration is known.
        can_seek: false,
        track_id: None,
    })
}

/// `com.apple.Music` → `Music`, `com.spotify.client` → `spotify` — a short,
/// picker-friendly player name from a bundle id.
fn short_app_name(bundle: &str) -> String {
    let last = bundle.rsplit('.').next().unwrap_or(bundle);
    if last == "client" || last.is_empty() {
        // e.g. com.spotify.client → "spotify"
        bundle.split('.').rev().nth(1).unwrap_or(bundle).to_string()
    } else {
        last.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn friendly_keys() {
        let v = json!({
            "title": "Get Lucky",
            "artist": "Daft Punk",
            "album": "Random Access Memories",
            "duration": 248.0,
            "elapsedTime": 30.0,
            "playing": true,
            "bundleIdentifier": "com.spotify.client"
        });
        let s = to_state(&v).unwrap();
        assert_eq!(s.title, "Get Lucky");
        assert_eq!(s.artist, "Daft Punk");
        assert_eq!(s.state, PlaybackState::Playing);
        assert_eq!(s.length, Some(Duration::from_secs_f64(248.0)));
        assert!(!s.can_seek);
        assert_eq!(s.player, "spotify");
    }

    #[test]
    fn raw_mr_keys_and_paused_via_rate() {
        let v = json!({
            "kMRMediaRemoteNowPlayingInfoTitle": "Ezio's Family",
            "kMRMediaRemoteNowPlayingInfoArtist": "Jesper Kyd",
            "kMRMediaRemoteNowPlayingInfoPlaybackRate": 0.0,
            "bundleIdentifier": "com.apple.Music"
        });
        let s = to_state(&v).unwrap();
        assert_eq!(s.title, "Ezio's Family");
        assert_eq!(s.state, PlaybackState::Paused);
        assert_eq!(s.player, "Music");
    }

    #[test]
    fn payload_wrapper_and_empty() {
        let wrapped = json!({ "payload": { "title": "X", "playing": true } });
        assert_eq!(to_state(&wrapped).unwrap().title, "X");
        // No metadata at all → nothing to show.
        assert!(to_state(&json!({ "playing": false })).is_none());
    }

    #[test]
    fn duration_does_not_imply_seek_support() {
        let v = json!({ "title": "X", "duration": 90.0 });
        let s = to_state(&v).unwrap();
        assert_eq!(s.length, Some(Duration::from_secs(90)));
        assert!(!s.can_seek);
    }

    #[test]
    fn decode_get_rejects_malformed_or_non_object_responses() {
        for out in ["\"hi\"", "[1]", "{\"payload\":3}", "not json"] {
            assert!(decode_get(out).is_err(), "accepted {out:?}");
        }
    }

    #[test]
    fn decode_get_preserves_empty_and_valid_responses() {
        assert!(decode_get("").unwrap().is_none());
        assert!(decode_get("null").unwrap().is_none());
        let state = decode_get(r#"{"title":"Get Lucky","artist":"Daft Punk","playing":true}"#)
            .unwrap()
            .unwrap();
        assert_eq!(state.title, "Get Lucky");
        assert_eq!(state.artist, "Daft Punk");
    }
}
