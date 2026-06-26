//! mpv backend over its JSON IPC socket (`--input-ipc-server=<path>`). The one
//! non-MPRIS backend in v1. Newline-delimited JSON: each request carries a
//! `request_id`; the reply with the matching id carries `data` + `error`.
//! Interleaved event lines (no `request_id`) are skipped.

use std::time::Duration;

use serde_json::{Value, json};

use super::{MediaBackend, MediaCaps, MediaError};
use superzej_core::media::{LoopMode, MediaState, PlaybackState, Playlist};

pub struct MpvIpc {
    socket: String,
}

impl MpvIpc {
    pub fn new(socket: String) -> Self {
        Self { socket }
    }

    /// Send one `{"command": ..., "request_id": 1}` and return its `data` (or
    /// `None` when the property is unavailable / errored). Opens a fresh
    /// connection per call — mpv's IPC is stateless and connections are cheap.
    #[cfg(unix)]
    async fn request(&self, command: Value) -> Result<Option<Value>, MediaError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| MediaError::Unavailable(format!("mpv socket: {e}")))?;
        let (rd, mut wr) = stream.into_split();
        let req = json!({ "command": command, "request_id": 1 });
        let mut line =
            serde_json::to_string(&req).map_err(|e| MediaError::Backend(e.to_string()))?;
        line.push('\n');
        wr.write_all(line.as_bytes())
            .await
            .map_err(|e| MediaError::Backend(e.to_string()))?;

        let mut reader = BufReader::new(rd);
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = reader
                .read_line(&mut buf)
                .await
                .map_err(|e| MediaError::Backend(e.to_string()))?;
            if n == 0 {
                return Err(MediaError::Backend("mpv closed the socket".into()));
            }
            let Ok(msg) = serde_json::from_str::<Value>(buf.trim()) else {
                continue;
            };
            // Skip async event lines; wait for our reply.
            if msg.get("request_id") == Some(&json!(1)) {
                if msg.get("error").and_then(|e| e.as_str()) == Some("success") {
                    return Ok(msg.get("data").cloned());
                }
                return Ok(None); // property unavailable / command failed
            }
        }
    }

    #[cfg(not(unix))]
    async fn request(&self, _command: Value) -> Result<Option<Value>, MediaError> {
        Err(MediaError::Unavailable(
            "mpv IPC requires a Unix socket".into(),
        ))
    }

    async fn get(&self, prop: &str) -> Option<Value> {
        self.request(json!(["get_property", prop]))
            .await
            .ok()
            .flatten()
    }
}

impl MediaBackend for MpvIpc {
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        // Probe one property to confirm mpv is reachable; absence ⇒ no player.
        let paused = match self.request(json!(["get_property", "pause"])).await {
            Ok(Some(v)) => v.as_bool().unwrap_or(false),
            Ok(None) => false,
            Err(MediaError::Unavailable(_)) => return Ok(None), // socket not there
            Err(e) => return Err(e),
        };
        let idle = self
            .get("idle-active")
            .await
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let state = if idle {
            PlaybackState::Stopped
        } else if paused {
            PlaybackState::Paused
        } else {
            PlaybackState::Playing
        };

        let meta = self.get("metadata").await;
        let title = self
            .get("media-title")
            .await
            .and_then(|v| v.as_str().map(str::to_string))
            .or_else(|| meta_field(&meta, &["title", "TITLE"]))
            .unwrap_or_default();
        let artist = meta_field(&meta, &["artist", "ARTIST"]).unwrap_or_default();
        let album = meta_field(&meta, &["album", "ALBUM"]).unwrap_or_default();

        let length = self.get("duration").await.and_then(secs);
        let position = self.get("time-pos").await.and_then(secs);
        let volume = self
            .get("volume")
            .await
            .and_then(|v| v.as_f64())
            .map(|v| v.round().clamp(0.0, 100.0) as u8);
        let loop_mode = self.get("loop-playlist").await.map(|v| {
            if v.as_bool() == Some(false) {
                LoopMode::None
            } else {
                LoopMode::Playlist
            }
        });

        Ok(Some(MediaState {
            player: "mpv".to_string(),
            title,
            artist,
            album,
            state,
            position,
            length,
            shuffle: None, // mpv exposes no persistent shuffle state
            loop_mode,
            volume,
            can_go_next: true,
            can_go_previous: true,
        }))
    }

    async fn play_pause(&self) -> Result<(), MediaError> {
        self.request(json!(["cycle", "pause"])).await.map(|_| ())
    }
    async fn next(&self) -> Result<(), MediaError> {
        self.request(json!(["playlist-next"])).await.map(|_| ())
    }
    async fn previous(&self) -> Result<(), MediaError> {
        self.request(json!(["playlist-prev"])).await.map(|_| ())
    }
    async fn set_shuffle(&self, _on: bool) -> Result<(), MediaError> {
        // mpv has no shuffle toggle — `playlist-shuffle` reorders once.
        self.request(json!(["playlist-shuffle"])).await.map(|_| ())
    }
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        let val = match mode {
            LoopMode::None => json!("no"),
            LoopMode::Track => json!("inf"), // loop-file
            LoopMode::Playlist => json!("inf"),
        };
        let prop = if matches!(mode, LoopMode::Track) {
            "loop-file"
        } else {
            "loop-playlist"
        };
        self.request(json!(["set_property", prop, val]))
            .await
            .map(|_| ())
    }
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        let cur = self
            .get("volume")
            .await
            .and_then(|v| v.as_f64())
            .unwrap_or(100.0);
        let next = (cur + delta * 100.0).clamp(0.0, 130.0);
        self.request(json!(["set_property", "volume", next]))
            .await
            .map(|_| ())
    }

    async fn playlists(&self) -> Result<Vec<Playlist>, MediaError> {
        Ok(Vec::new()) // mpv has no MPRIS-style named playlists
    }
    async fn activate_playlist(&self, _id: &str) -> Result<(), MediaError> {
        Ok(())
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: true,
            playlists: false,
            signals: false, // host polls on [media] poll_interval_secs
        }
    }
}

/// Read a metadata field, trying each candidate key (mpv casing varies).
fn meta_field(meta: &Option<Value>, keys: &[&str]) -> Option<String> {
    let obj = meta.as_ref()?.as_object()?;
    for k in keys {
        if let Some(v) = obj.get(*k).and_then(|v| v.as_str())
            && !v.is_empty()
        {
            return Some(v.to_string());
        }
    }
    None
}

/// mpv reports durations as float seconds.
fn secs(v: Value) -> Option<Duration> {
    let s = v.as_f64()?;
    if s.is_finite() && s >= 0.0 {
        Some(Duration::from_secs_f64(s))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn meta_field_tries_casings() {
        let m = Some(json!({"ARTIST": "Boards of Canada", "title": "Roygbiv"}));
        assert_eq!(
            meta_field(&m, &["artist", "ARTIST"]).as_deref(),
            Some("Boards of Canada")
        );
        assert_eq!(
            meta_field(&m, &["title", "TITLE"]).as_deref(),
            Some("Roygbiv")
        );
        assert_eq!(meta_field(&m, &["album"]), None);
        assert_eq!(meta_field(&None, &["artist"]), None);
    }

    #[test]
    fn secs_rejects_garbage() {
        assert_eq!(secs(json!(248.5)), Some(Duration::from_secs_f64(248.5)));
        assert_eq!(secs(json!(-1.0)), None);
        assert_eq!(secs(json!("nope")), None);
    }
}
