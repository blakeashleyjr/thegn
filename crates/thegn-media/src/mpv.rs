//! mpv backend over its JSON IPC socket (`--input-ipc-server=<path>`). The one
//! non-MPRIS backend in v1. Newline-delimited JSON: each request carries a
//! `request_id`; the reply with the matching id carries `data` + `error`.
//! Interleaved event lines (no `request_id`) are skipped.
//!
//! The socket path is a Unix domain socket; on non-Unix targets the backend
//! compiles but is inert (mpv on Windows uses a named pipe — not wired yet).

use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};

use crate::model::{
    LoopMode, MAX_FIELD_BYTES, MAX_QUEUE_ITEMS, MediaKind, MediaState, PlaybackState, Playlist,
    QueueItem, QueuePage,
};
use crate::{MediaBackend, MediaCaps, MediaError};

pub struct MpvIpc {
    // Read only by the `#[cfg(unix)]` IPC path; inert (and unread) elsewhere.
    #[cfg_attr(not(unix), allow(dead_code))]
    socket: String,
}

/// Longest player-supplied reason kept in an error message.
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_REASON: usize = 120;

/// Per-request ceiling: a wedged mpv must not hang the poller.
#[cfg(unix)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// One playlist reply can be large, but a single line must remain bounded.
#[allow(dead_code)]
const MAX_REPLY_LINE_BYTES: usize = 2 * 1024 * 1024;
/// Total bytes consumed while skipping events and garbage before the reply.
#[allow(dead_code)]
const MAX_EXCHANGE_BYTES: usize = 8 * 1024 * 1024;

/// mpv error strings that mean "this optional property has no value right
/// now" (no file loaded, no video track, ...), not a failed command.
fn is_absent_error(reason: &str) -> bool {
    matches!(reason, "property unavailable" | "property not found")
}

/// Strip control characters and bound the length of a player-supplied reason.
#[cfg_attr(not(unix), allow(dead_code))]
fn sanitize_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON)
        .collect()
}

/// The decoded reply to one IPC request.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug)]
enum Reply {
    /// `error == "success"` with a `data` field.
    Data(Value),
    /// `error == "success"` and no `data` (a command that returns nothing).
    NoData,
    /// mpv rejected the request; carries the sanitized, bounded error string.
    Rejected(String),
}

/// Decode a reply line already matched to our request id.
#[cfg_attr(not(unix), allow(dead_code))]
fn decode_reply(msg: &Value) -> Result<Reply, MediaError> {
    match msg.get("error").and_then(Value::as_str) {
        Some("success") => Ok(match msg.get("data") {
            Some(d) => Reply::Data(d.clone()),
            None => Reply::NoData,
        }),
        Some(other) => Ok(Reply::Rejected(sanitize_reason(other))),
        None => Err(MediaError::Backend(
            "mpv reply carried no error field".into(),
        )),
    }
}

/// Parse a loop property value without treating unrecognized values as on/off.
fn loop_on(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "" | "no" | "false" | "off" | "0" => Some(false),
            "inf" | "force" | "yes" | "true" | "on" => Some(true),
            number => number.parse::<i64>().ok().map(|n| n != 0),
        },
        Value::Number(n) => n.as_i64().map(|n| n != 0),
        _ => None,
    }
}

/// Fold `loop-file` and `loop-playlist` into one mode (track repeat wins,
/// as in mpv). `None` when neither property could be read.
fn decode_loop(file: Option<&Value>, playlist: Option<&Value>) -> Option<LoopMode> {
    if file.and_then(loop_on) == Some(true) {
        return Some(LoopMode::Track);
    }
    match (file, playlist) {
        (Some(file), Some(playlist))
            if loop_on(file) == Some(false) && loop_on(playlist) == Some(true) =>
        {
            Some(LoopMode::Playlist)
        }
        (Some(file), Some(playlist))
            if loop_on(file) == Some(false) && loop_on(playlist) == Some(false) =>
        {
            Some(LoopMode::None)
        }
        _ => None,
    }
}

/// Opaque queue id: `<index>:<mpv entry id>` (or just `<index>` when mpv does
/// not report entry ids), so a later play can detect a stale index.
fn queue_id(idx: usize, entry: &Value) -> String {
    match entry.get("id").and_then(Value::as_i64) {
        Some(eid) => format!("{idx}:{eid}"),
        None => idx.to_string(),
    }
}

fn clip_field(value: &str) -> String {
    let mut boundary = value.len().min(MAX_FIELD_BYTES);
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_string()
}

fn queue_rows(entries: &[Value], limit: usize) -> Vec<QueueItem> {
    let mut rows = Vec::with_capacity(limit.min(entries.len()));
    for (i, entry) in entries.iter().enumerate().take(limit) {
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                entry
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f))
            })
            .map(clip_field)
            .unwrap_or_default();
        let is_current = entry
            .get("current")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        rows.push(QueueItem {
            id: queue_id(i, entry),
            title,
            artist: String::new(),
            duration: None,
            is_current,
        });
    }
    rows
}

impl MpvIpc {
    pub fn new(socket: String) -> Self {
        Self { socket }
    }

    async fn playlist_entries(&self) -> Result<Value, MediaError> {
        let list = self
            .get("playlist")
            .await?
            .ok_or_else(|| MediaError::Backend("mpv playlist unavailable".into()))?;
        if list.as_array().is_none() {
            return Err(MediaError::Backend(
                "mpv playlist has unexpected shape".into(),
            ));
        }
        Ok(list)
    }

    /// Send one `{"command": ..., "request_id": 1}` and decode its reply.
    /// Opens a fresh connection per call — mpv's IPC is stateless and
    /// connections are cheap. Transport, EOF, timeout, and malformed-reply
    /// failures are `Err`; a player rejection is `Ok(Reply::Rejected)`.
    #[cfg(unix)]
    async fn exchange(&self, command: Value) -> Result<Reply, MediaError> {
        match tokio::time::timeout(REQUEST_TIMEOUT, self.exchange_inner(command)).await {
            Ok(r) => r,
            Err(_) => Err(MediaError::Backend("mpv request timed out".into())),
        }
    }

    #[cfg(unix)]
    async fn exchange_inner(&self, command: Value) -> Result<Reply, MediaError> {
        use crate::mpd::read_bounded_line;
        use tokio::io::{AsyncWriteExt, BufReader};
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
        let mut total_bytes = 0usize;
        loop {
            let line = read_bounded_line(&mut reader, MAX_REPLY_LINE_BYTES)
                .await
                .map_err(|message| MediaError::Backend(format!("mpv reply {message}")))?
                .ok_or_else(|| MediaError::Backend("mpv closed the socket".into()))?;
            total_bytes = total_bytes.saturating_add(line.len().saturating_add(1));
            if total_bytes > MAX_EXCHANGE_BYTES {
                return Err(MediaError::Backend(format!(
                    "mpv exchange exceeds {MAX_EXCHANGE_BYTES} bytes"
                )));
            }
            let msg = match serde_json::from_str::<Value>(line.trim()) {
                Ok(msg) => msg,
                Err(_) if line.contains("request_id") => {
                    return Err(MediaError::Backend(
                        "malformed mpv reply for request".into(),
                    ));
                }
                Err(_) => continue,
            };
            // Skip async event lines; wait for our reply.
            if msg.get("request_id") == Some(&json!(1)) {
                return decode_reply(&msg);
            }
        }
    }

    #[cfg(not(unix))]
    async fn exchange(&self, _command: Value) -> Result<Reply, MediaError> {
        Err(MediaError::Unavailable(
            "mpv IPC requires a Unix socket".into(),
        ))
    }

    /// Run a mutation: success (with or without data) is `Ok`, a rejection is
    /// a typed `Err` carrying mpv's reason.
    async fn command(&self, command: Value) -> Result<(), MediaError> {
        match self.exchange(command).await? {
            Reply::Data(_) | Reply::NoData => Ok(()),
            Reply::Rejected(reason) => Err(MediaError::Backend(format!("mpv rejected: {reason}"))),
        }
    }

    /// Read an optional property. Only mpv's documented "no value" errors and
    /// an empty success become `Ok(None)`; transport, EOF, and every other
    /// rejection are `Err`.
    async fn get(&self, prop: &str) -> Result<Option<Value>, MediaError> {
        match self.exchange(json!(["get_property", prop])).await? {
            Reply::Data(v) => Ok(Some(v)),
            Reply::NoData => Ok(None),
            Reply::Rejected(r) if is_absent_error(&r) => Ok(None),
            Reply::Rejected(r) => Err(MediaError::Backend(format!(
                "mpv rejected get_property {prop}: {r}"
            ))),
        }
    }
}

impl MediaBackend for MpvIpc {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            // Probe one property to confirm mpv is reachable; absence ⇒ no player.
            let paused = match self.get("pause").await {
                Ok(v) => v.and_then(|v| v.as_bool()).unwrap_or(false),
                Err(MediaError::Unavailable(_)) => return Ok(None), // socket not there
                Err(e) => return Err(e),
            };
            let idle = self
                .get("idle-active")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let state = if idle {
                PlaybackState::Stopped
            } else if paused {
                PlaybackState::Paused
            } else {
                PlaybackState::Playing
            };

            let meta = self.get("metadata").await?;
            let title = self
                .get("media-title")
                .await?
                .and_then(|v| v.as_str().map(str::to_string))
                .or_else(|| meta_field(&meta, &["title", "TITLE"]))
                .unwrap_or_default();
            let artist = meta_field(&meta, &["artist", "ARTIST"]).unwrap_or_default();
            let album = meta_field(&meta, &["album", "ALBUM"]).unwrap_or_default();

            let length = self.get("duration").await?.and_then(secs);
            let position = self.get("time-pos").await?.and_then(secs);
            let volume = self
                .get("volume")
                .await?
                .and_then(|v| v.as_f64())
                .map(|v| v.round().clamp(0.0, 100.0) as u8);
            let loop_file = self.get("loop-file").await?;
            let loop_playlist = self.get("loop-playlist").await?;
            let loop_mode = decode_loop(loop_file.as_ref(), loop_playlist.as_ref());
            // A present, non-empty `video-format` means an actual video track is
            // decoding (mpv playing music has none) — the reliable audio/video split.
            let kind = if self
                .get("video-format")
                .await?
                .and_then(|v| v.as_str().map(|s| !s.is_empty()))
                .unwrap_or(false)
            {
                MediaKind::Video
            } else {
                MediaKind::Audio
            };
            // Conservative: an unreadable `seekable` is not seekable.
            let can_seek = self
                .get("seekable")
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // Next/previous are offered only when the playlist position says
            // there is somewhere to go (or playlist repeat wraps around).
            let pos = self.get("playlist-pos").await?.and_then(|v| v.as_i64());
            let count = self.get("playlist-count").await?.and_then(|v| v.as_i64());
            let wraps = loop_playlist.as_ref().and_then(loop_on) == Some(true);
            let (can_go_next, can_go_previous) = match (pos, count) {
                (Some(p), Some(c)) if p >= 0 && c > 0 => (wraps || p + 1 < c, wraps || p > 0),
                _ => (false, false),
            };

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
                can_go_next,
                can_go_previous,
                art_url: None, // mpv doesn't surface cover art over IPC
                kind,
                can_seek,
                track_id: None, // mpv seeks by absolute time, no track id needed
            }))
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.command(json!(["cycle", "pause"])).await })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.command(json!(["playlist-next"])).await })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.command(json!(["playlist-prev"])).await })
    }
    fn set_shuffle(&self, _on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // mpv only has the one-shot `playlist-shuffle` reorder, which is
            // not an idempotent boolean setter, so shuffle is not offered.
            Err(MediaError::Unsupported("mpv shuffle".into()))
        })
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // Write both properties so exactly one repeat mode is active.
            let (file, playlist) = match mode {
                LoopMode::None => ("no", "no"),
                LoopMode::Track => ("inf", "no"),
                LoopMode::Playlist => ("no", "inf"),
            };
            self.command(json!(["set_property", "loop-file", file]))
                .await?;
            self.command(json!(["set_property", "loop-playlist", playlist]))
                .await
        })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let cur = self
                .get("volume")
                .await?
                .and_then(|v| v.as_f64())
                .ok_or_else(|| MediaError::Backend("mpv volume unavailable".into()))?;
            let next = (cur + delta * 100.0).clamp(0.0, 130.0);
            self.command(json!(["set_property", "volume", next])).await
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>> {
        Box::pin(async move { Err(MediaError::Unsupported("mpv playlists".into())) })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { Err(MediaError::Unsupported("mpv playlists".into())) })
    }

    fn seek(&self, offset: Duration, forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let secs = offset.as_secs_f64() * if forward { 1.0 } else { -1.0 };
            self.command(json!(["seek", secs, "relative"])).await
        })
    }
    fn set_position<'a>(
        &'a self,
        pos: Duration,
        _track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            self.command(json!(["seek", pos.as_secs_f64(), "absolute"]))
                .await
        })
    }
    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            self.command(json!(["set_property", "volume", level.min(100) as f64]))
                .await
        })
    }

    fn queue(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, MediaError>> {
        Box::pin(async move {
            let list = self.playlist_entries().await?;
            let arr = list
                .as_array()
                .ok_or_else(|| MediaError::Backend("mpv playlist has unexpected shape".into()))?;
            Ok(queue_rows(arr, MAX_QUEUE_ITEMS))
        })
    }
    fn queue_page(&self) -> BoxFuture<'_, Result<QueuePage, MediaError>> {
        Box::pin(async move {
            let list = self.playlist_entries().await?;
            let arr = list
                .as_array()
                .ok_or_else(|| MediaError::Backend("mpv playlist has unexpected shape".into()))?;
            Ok(QueuePage {
                total: arr.len(),
                items: queue_rows(arr, MAX_QUEUE_ITEMS),
            })
        })
    }
    fn play_queue_item<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            let bad = || MediaError::Backend(format!("bad mpv playlist index {id:?}"));
            let (idx_s, eid_s) = match id.split_once(':') {
                Some((i, e)) => (i, Some(e)),
                None => (id, None),
            };
            let idx: usize = idx_s.parse().map_err(|_| bad())?;
            if let Some(e) = eid_s {
                // Bound to an entry identity: refuse if the list moved on.
                let want: i64 = e.parse().map_err(|_| bad())?;
                let list = self
                    .get("playlist")
                    .await?
                    .ok_or_else(|| MediaError::Backend("mpv playlist unavailable".into()))?;
                let have = list
                    .as_array()
                    .and_then(|a| a.get(idx))
                    .and_then(|entry| entry.get("id"))
                    .and_then(Value::as_i64);
                if have != Some(want) {
                    return Err(MediaError::Backend(
                        "mpv playlist changed; stale queue selection".into(),
                    ));
                }
            } else {
                // Legacy ids carry only an index, so still reject stale or
                // out-of-range selections before writing playlist-pos.
                let count = self
                    .get("playlist-count")
                    .await?
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| MediaError::Backend("mpv playlist count unavailable".into()))?;
                if count <= 0 || idx >= count as usize {
                    return Err(MediaError::Backend(
                        "mpv playlist index out of range".into(),
                    ));
                }
            }
            self.command(json!(["set_property", "playlist-pos", idx as i64]))
                .await
        })
    }

    fn chapter_next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.command(json!(["add", "chapter", 1])).await })
    }
    fn chapter_prev(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.command(json!(["add", "chapter", -1])).await })
    }
    fn toggle_fullscreen(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.command(json!(["cycle", "fullscreen"])).await })
    }

    /// One mpv instance per socket; no push watcher (the host polls).
    fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
        Box::pin(async { Ok(vec!["mpv".to_string()]) })
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            // No idempotent boolean shuffle in mpv (only a one-shot reorder).
            shuffle: false,
            loop_mode: true,
            volume: true,
            playlists: false,
            signals: false, // host polls on [media] poll_interval_secs
            seek: true,
            art: false,
            queue: true,
            abs_volume: true,
            chapters: true,
            fullscreen: true,
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

    #[test]
    fn decode_loop_folds_both_properties() {
        let no = json!("no");
        let inf = json!("inf");
        assert_eq!(decode_loop(None, None), None);
        assert_eq!(decode_loop(Some(&no), Some(&no)), Some(LoopMode::None));
        assert_eq!(
            decode_loop(Some(&json!(false)), Some(&json!(false))),
            Some(LoopMode::None)
        );
        assert_eq!(decode_loop(Some(&inf), Some(&no)), Some(LoopMode::Track));
        assert_eq!(decode_loop(Some(&no), Some(&inf)), Some(LoopMode::Playlist));
        assert_eq!(decode_loop(Some(&inf), Some(&inf)), Some(LoopMode::Track));
        assert_eq!(decode_loop(None, Some(&json!(3))), None);
        assert_eq!(decode_loop(None, Some(&no)), None);
        assert_eq!(decode_loop(Some(&json!("unknown")), Some(&inf)), None);
    }

    #[test]
    fn sanitize_reason_bounds_and_strips() {
        let long = format!("bad\n\u{1b}[31m{}", "x".repeat(500));
        let s = sanitize_reason(&long);
        assert!(s.chars().count() <= MAX_REASON);
        assert!(!s.chars().any(char::is_control));
    }

    #[test]
    fn queue_rows_decode_at_most_the_limit() {
        let entries = (0..7)
            .map(|i| json!({"filename": format!("/m/{i}.mp3"), "id": i}))
            .collect::<Vec<_>>();
        let rows = queue_rows(&entries, 3);
        assert_eq!(rows.len(), 3);
        assert!(rows.capacity() <= 3);
        assert_eq!(rows[0].id, "0:0");
        assert_eq!(rows[1].id, "1:1");
        assert_eq!(rows[2].id, "2:2");
    }

    #[test]
    fn queue_rows_clip_titles_to_the_field_budget() {
        let expected = "a".repeat(MAX_FIELD_BYTES - 1);
        let title = format!("{expected}éb");
        let filename = format!("/m/{title}");
        let entries = vec![json!({"title": title}), json!({"filename": filename})];
        let rows = queue_rows(&entries, entries.len());
        assert!(rows[0].title.len() <= MAX_FIELD_BYTES);
        assert_eq!(rows[0].title, expected);
        assert!(rows[1].title.len() <= MAX_FIELD_BYTES);
        assert_eq!(rows[1].title, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn queue_page_reports_the_playlist_total_and_caps_the_rows() {
        use super::socket_tests::{err, ok, serve};

        let playlist = (0..MAX_QUEUE_ITEMS + 3)
            .map(|i| json!({"filename": format!("/m/{i}.mp3"), "id": i}))
            .collect::<Vec<_>>();
        let payload = json!(playlist);
        let (_dir, mpv, _seen) = serve(move |_| Some(vec![ok(payload.clone())]));
        let page = mpv.queue_page().await.unwrap();
        assert_eq!(page.items.len(), MAX_QUEUE_ITEMS);
        assert_eq!(page.total, MAX_QUEUE_ITEMS + 3);
        assert_eq!(
            page.items[MAX_QUEUE_ITEMS - 1].id,
            format!("{}:{}", MAX_QUEUE_ITEMS - 1, MAX_QUEUE_ITEMS - 1)
        );

        let (_dir, unavailable, _seen) = serve(|_| Some(vec![err("property unavailable")]));
        assert!(unavailable.queue_page().await.is_err());
        let (_dir, unexpected, _seen) = serve(|_| Some(vec![ok(json!("oops"))]));
        assert!(unexpected.queue_page().await.is_err());
    }
}

#[cfg(all(test, unix))]
pub(super) mod socket_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    pub(super) type Seen = Arc<Mutex<Vec<Value>>>;

    /// Fake mpv: one request per connection. `handler` maps the command array
    /// to the raw lines to send back, or `None` to drop the connection.
    pub(super) fn serve<F>(handler: F) -> (tempfile::TempDir, MpvIpc, Seen)
    where
        F: Fn(&Value) -> Option<Vec<String>> + Send + Sync + 'static,
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let seen: Seen = Arc::default();
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (rd, mut wr) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(rd).read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let Ok(req) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                let cmd = req["command"].clone();
                seen2.lock().unwrap().push(cmd.clone());
                if let Some(lines) = handler(&cmd) {
                    for l in lines {
                        if wr.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let mpv = MpvIpc::new(path.to_string_lossy().into_owned());
        (dir, mpv, seen)
    }

    pub(super) fn ok(data: Value) -> String {
        json!({"request_id": 1, "error": "success", "data": data}).to_string()
    }
    fn ok_nodata() -> String {
        json!({"request_id": 1, "error": "success"}).to_string()
    }
    pub(super) fn err(reason: &str) -> String {
        json!({"request_id": 1, "error": reason}).to_string()
    }

    #[tokio::test]
    async fn success_without_data_is_ok() {
        let (_d, mpv, seen) = serve(|_| Some(vec![ok_nodata()]));
        mpv.play_pause().await.unwrap();
        assert_eq!(seen.lock().unwrap()[0], json!(["cycle", "pause"]));
    }

    #[tokio::test]
    async fn every_control_surfaces_a_rejection() {
        let (_d, mpv, _s) = serve(|cmd| {
            // volume read must succeed so volume_step reaches the write.
            if cmd[0] == "get_property" {
                Some(vec![ok(json!(50.0))])
            } else {
                Some(vec![err("property unavailable\u{7}")])
            }
        });
        let one = Duration::from_secs(1);
        let results = [
            mpv.play_pause().await,
            mpv.next().await,
            mpv.previous().await,
            mpv.set_loop(LoopMode::Track).await,
            mpv.volume_step(0.05).await,
            mpv.set_volume(40).await,
            mpv.seek(one, true).await,
            mpv.set_position(one, None).await,
            mpv.chapter_next().await,
            mpv.chapter_prev().await,
            mpv.toggle_fullscreen().await,
        ];
        for r in results {
            match r {
                Err(MediaError::Backend(m)) => {
                    assert!(m.contains("property unavailable"), "{m}");
                    assert!(!m.chars().any(char::is_control));
                }
                other => panic!("expected Backend rejection, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn interleaved_events_and_garbage_are_skipped() {
        let (_d, mpv, _s) = serve(|_| {
            Some(vec![
                "not json".into(),
                json!({"event": "playback-restart"}).to_string(),
                json!({"request_id": 7, "error": "success"}).to_string(),
                ok_nodata(),
            ])
        });
        mpv.next().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_reply_line_is_rejected_before_json_decode() {
        let oversized = ok(json!("x".repeat(MAX_REPLY_LINE_BYTES + 1)));
        let (_dir, mpv, _seen) = serve(move |_| Some(vec![oversized.clone()]));

        assert!(
            matches!(mpv.get("media-title").await, Err(MediaError::Backend(message)) if message.contains("exceeds"))
        );
    }

    #[tokio::test]
    async fn skipped_lines_share_a_bounded_exchange_budget() {
        let event = json!({
            "event": "playback-restart",
            "padding": "x".repeat(1024 * 1024),
        })
        .to_string();
        let lines = vec![event; 9];
        let (_dir, mpv, _seen) = serve(move |_| Some(lines.clone()));

        assert!(
            matches!(mpv.next().await, Err(MediaError::Backend(message)) if message.contains("exceeds"))
        );
    }

    #[tokio::test]
    async fn malformed_reply_with_request_id_fails_immediately() {
        let (_d, mpv, _s) = serve(|_| Some(vec![r#"{"request_id": 1, broken"#.into()]));
        assert!(
            matches!(mpv.next().await, Err(MediaError::Backend(message)) if message.contains("malformed"))
        );
    }

    #[tokio::test]
    async fn reply_without_error_field_is_malformed() {
        let (_d, mpv, _s) = serve(|_| Some(vec![json!({"request_id": 1}).to_string()]));
        assert!(matches!(mpv.next().await, Err(MediaError::Backend(_))));
    }

    #[tokio::test]
    async fn disconnect_is_an_error_not_success() {
        let (_d, mpv, _s) = serve(|_| None);
        assert!(matches!(mpv.next().await, Err(MediaError::Backend(_))));
        assert!(mpv.queue().await.is_err());
    }

    #[tokio::test]
    async fn snapshot_without_socket_is_no_player() {
        let mpv = MpvIpc::new("/nonexistent/mpv.sock".into());
        assert!(mpv.snapshot().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn snapshot_disconnect_mid_read_is_error() {
        // First property answers, the rest drop the connection.
        let (_d, mpv, _s) = serve(|cmd| {
            if cmd[1] == "pause" {
                Some(vec![ok(json!(false))])
            } else {
                None
            }
        });
        assert!(mpv.snapshot().await.is_err());
    }

    fn snapshot_handler(
        loop_file: &'static str,
        loop_pl: &'static str,
    ) -> impl Fn(&Value) -> Option<Vec<String>> {
        move |cmd| {
            let prop = cmd[1].as_str().unwrap_or("");
            Some(vec![match prop {
                "pause" | "idle-active" => ok(json!(false)),
                "media-title" => ok(json!("Song")),
                "loop-file" => ok(json!(loop_file)),
                "loop-playlist" => ok(json!(loop_pl)),
                "playlist-pos" => ok(json!(1)),
                "playlist-count" => ok(json!(2)),
                "volume" => ok(json!(70.0)),
                // seekable absent; everything else absent too.
                _ => err("property unavailable"),
            }])
        }
    }

    #[tokio::test]
    async fn snapshot_decodes_loop_and_is_conservative() {
        let (_d, mpv, _s) = serve(snapshot_handler("inf", "no"));
        let s = mpv.snapshot().await.unwrap().unwrap();
        assert_eq!(s.loop_mode, Some(LoopMode::Track));
        assert_eq!(s.title, "Song");
        assert_eq!(s.volume, Some(70));
        assert!(!s.can_seek, "unreadable seekable must not default to true");
        assert!(!s.can_go_next, "last of two entries, no wrap");
        assert!(s.can_go_previous);

        let (_d2, mpv2, _s2) = serve(snapshot_handler("no", "inf"));
        let s2 = mpv2.snapshot().await.unwrap().unwrap();
        assert_eq!(s2.loop_mode, Some(LoopMode::Playlist));
        assert!(s2.can_go_next, "playlist repeat wraps");
    }

    #[tokio::test]
    async fn snapshot_hard_rejection_is_error() {
        let (_d, mpv, _s) = serve(|cmd| {
            if cmd[1] == "pause" {
                Some(vec![ok(json!(false))])
            } else {
                Some(vec![err("invalid parameter")])
            }
        });
        assert!(mpv.snapshot().await.is_err());
    }

    #[tokio::test]
    async fn set_loop_writes_both_properties() {
        let (_d, mpv, seen) = serve(|_| Some(vec![ok_nodata()]));
        mpv.set_loop(LoopMode::Track).await.unwrap();
        mpv.set_loop(LoopMode::Playlist).await.unwrap();
        mpv.set_loop(LoopMode::None).await.unwrap();
        let s = seen.lock().unwrap().clone();
        assert_eq!(s[0], json!(["set_property", "loop-file", "inf"]));
        assert_eq!(s[1], json!(["set_property", "loop-playlist", "no"]));
        assert_eq!(s[2], json!(["set_property", "loop-file", "no"]));
        assert_eq!(s[3], json!(["set_property", "loop-playlist", "inf"]));
        assert_eq!(s[4], json!(["set_property", "loop-file", "no"]));
        assert_eq!(s[5], json!(["set_property", "loop-playlist", "no"]));
    }

    #[tokio::test]
    async fn shuffle_and_playlists_are_unsupported_and_send_nothing() {
        let (_d, mpv, seen) = serve(|_| Some(vec![ok_nodata()]));
        assert!(!mpv.caps().shuffle);
        assert!(!mpv.caps().playlists);
        assert!(matches!(
            mpv.set_shuffle(true).await,
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            mpv.set_shuffle(false).await,
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            mpv.activate_playlist("x").await,
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            mpv.playlists().await,
            Err(MediaError::Unsupported(_))
        ));
        assert!(seen.lock().unwrap().is_empty());
    }

    fn playlist_json() -> Value {
        json!([
            {"filename": "/m/a.mp3", "id": 10},
            {"filename": "/m/b.mp3", "id": 11, "current": true, "title": "B"},
        ])
    }

    #[tokio::test]
    async fn queue_reads_entries_and_errors_do_not_become_empty() {
        let (_d, mpv, _s) = serve(|_| Some(vec![ok(playlist_json())]));
        let q = mpv.queue().await.unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].title, "a.mp3");
        assert_eq!(q[1].id, "1:11");
        assert!(q[1].is_current);

        let (_d2, mpv2, _s2) = serve(|_| Some(vec![err("property unavailable")]));
        assert!(mpv2.queue().await.is_err());
        let (_d3, mpv3, _s3) = serve(|_| Some(vec![ok(json!("oops"))]));
        assert!(mpv3.queue().await.is_err());
    }

    #[tokio::test]
    async fn stale_queue_selection_is_refused() {
        let (_d, mpv, seen) = serve(|cmd| {
            if cmd[0] == "get_property" {
                Some(vec![ok(playlist_json())])
            } else {
                Some(vec![ok_nodata()])
            }
        });
        // Entry 1 now has id 11, so "1:99" is stale and must not write.
        assert!(mpv.play_queue_item("1:99").await.is_err());
        assert!(seen.lock().unwrap().iter().all(|c| c[0] == "get_property"));
        mpv.play_queue_item("1:11").await.unwrap();
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            &json!(["set_property", "playlist-pos", 1])
        );
        assert!(mpv.play_queue_item("nope").await.is_err());
    }

    #[tokio::test]
    async fn legacy_queue_index_is_checked_against_playlist_count() {
        let (_d, mpv, seen) = serve(|cmd| {
            if cmd[0] == "get_property" && cmd[1] == "playlist-count" {
                Some(vec![ok(json!(3))])
            } else {
                Some(vec![ok_nodata()])
            }
        });
        assert!(matches!(
            mpv.play_queue_item("3").await,
            Err(MediaError::Backend(message)) if message.contains("out of range")
        ));
        assert!(
            seen.lock()
                .unwrap()
                .iter()
                .all(|command| command[0] != "set_property")
        );

        mpv.play_queue_item("2").await.unwrap();
        assert_eq!(
            seen.lock().unwrap().last().unwrap(),
            &json!(["set_property", "playlist-pos", 2])
        );
    }

    #[tokio::test]
    async fn rejected_queue_play_is_error() {
        let (_d, mpv, _s) = serve(|_| Some(vec![err("property unavailable")]));
        assert!(mpv.play_queue_item("3").await.is_err());
    }
}
