//! macOS universal now-playing via the **mediaremote-adapter**. Apple gated the
//! private `MRMediaRemoteGetNowPlayingInfo` read path for unsigned binaries on
//! 15.4+, so a system-signed helper (the `mediaremote-adapter` project) is the
//! supported way to read the system Now-Playing session — which covers *every*
//! app (browsers, Spotify, Music, VLC, …), unlike the per-app AppleScript floor.
//!
//! We shell out to the adapter: `get` for a one-shot snapshot, `stream` for a
//! push watcher that emits a JSON line per change (the ~0%-idle contract). The
//! adapter command is discovered from `$SUPERZEJ_MEDIAREMOTE_ADAPTER` (a
//! space-separated argv prefix) or the `mediaremote-adapter` binary on `PATH`.
//! When it isn't installed, [`MediaRemote::connect`] returns `None` and the
//! caller falls back to [`crate::applescript`].
//!
//! Pure JSON decoding lives in [`crate::mediaremote_parse`] (Linux-testable).

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::mediaremote_parse;
use crate::model::MediaState;
use crate::{MediaBackend, MediaCaps, MediaError, MediaWatch};

/// The adapter-backed macOS backend. Holds the resolved argv prefix; each read
/// spawns the adapter (cheap, and nothing COM-like to keep alive).
pub struct MediaRemote {
    /// argv prefix, e.g. `["mediaremote-adapter"]` — subcommand is appended.
    argv: Vec<String>,
}

impl MediaRemote {
    /// Discover the adapter and probe it. `None` when it isn't installed/usable,
    /// so `auto` falls back to AppleScript.
    pub async fn connect() -> Option<MediaRemote> {
        let argv = discover_argv();
        let mr = MediaRemote { argv };
        // A successful `get` (even with an empty payload) proves the adapter runs.
        match mr.run(&["get"]).await {
            Ok(_) => Some(mr),
            Err(_) => None,
        }
    }

    /// Run the adapter with `args` appended, returning trimmed stdout.
    async fn run(&self, args: &[&str]) -> Result<String, MediaError> {
        let (prog, base) = self
            .argv
            .split_first()
            .ok_or_else(|| MediaError::Unavailable("no mediaremote adapter".into()))?;
        let out = Command::new(prog)
            .args(base)
            .args(args)
            .output()
            .await
            .map_err(|e| MediaError::Unavailable(format!("mediaremote adapter: {e}")))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(MediaError::Backend(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }

    pub async fn list_players(&self) -> Vec<String> {
        match self.snapshot().await {
            Ok(Some(s)) if !s.player.is_empty() => vec![s.player],
            _ => Vec::new(),
        }
    }

    /// Spawn the streaming watcher (`stream`): one JSON line per change.
    pub async fn watch(&self) -> Result<MediaRemoteWatch, MediaError> {
        let (prog, base) = self
            .argv
            .split_first()
            .ok_or_else(|| MediaError::Unavailable("no mediaremote adapter".into()))?;
        let mut child = Command::new(prog)
            .args(base)
            .arg("stream")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| MediaError::Unavailable(format!("mediaremote stream: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MediaError::Backend("mediaremote stream: no stdout".into()))?;
        Ok(MediaRemoteWatch {
            child,
            lines: BufReader::new(stdout).lines(),
        })
    }
}

impl MediaBackend for MediaRemote {
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        let out = match self.run(&["get"]).await {
            Ok(o) => o,
            Err(MediaError::Unavailable(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if out.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(&out)
            .map_err(|e| MediaError::Backend(format!("mediaremote json: {e}")))?;
        Ok(mediaremote_parse::to_state(&v))
    }

    // The adapter's control verbs (`play`, `pause`, `next`, `previous`) map onto
    // the shared transport; where the adapter build lacks a verb it errors and
    // the UI simply reports it.
    async fn play_pause(&self) -> Result<(), MediaError> {
        self.run(&["toggle"]).await.map(|_| ())
    }
    async fn next(&self) -> Result<(), MediaError> {
        self.run(&["next"]).await.map(|_| ())
    }
    async fn previous(&self) -> Result<(), MediaError> {
        self.run(&["previous"]).await.map(|_| ())
    }
    async fn set_shuffle(&self, _on: bool) -> Result<(), MediaError> {
        Err(MediaError::Backend("shuffle unsupported".into()))
    }
    async fn set_loop(&self, _mode: crate::model::LoopMode) -> Result<(), MediaError> {
        Err(MediaError::Backend("loop unsupported".into()))
    }
    async fn volume_step(&self, _delta: f64) -> Result<(), MediaError> {
        Ok(()) // system Now-Playing exposes no volume; caps().volume == false
    }
    async fn playlists(&self) -> Result<Vec<crate::model::Playlist>, MediaError> {
        Ok(Vec::new())
    }
    async fn activate_playlist(&self, _id: &str) -> Result<(), MediaError> {
        Ok(())
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            shuffle: false,
            loop_mode: false,
            volume: false,
            playlists: false,
            signals: true, // push via `stream`
            seek: false,
            art: false,
            queue: false,
            abs_volume: false,
            chapters: false,
            fullscreen: false,
        }
    }
}

/// The streaming watcher: each `stream` line is one now-playing change.
pub struct MediaRemoteWatch {
    child: Child,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl MediaWatch for MediaRemoteWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            // A line ⇒ a change; EOF/error ⇒ stream ended.
            matches!(self.lines.next_line().await, Ok(Some(_)))
        })
    }
}

impl Drop for MediaRemoteWatch {
    fn drop(&mut self) {
        // Best-effort: don't leave the streaming adapter process behind.
        let _ = self.child.start_kill();
    }
}

/// The adapter argv: `$SUPERZEJ_MEDIAREMOTE_ADAPTER` (space-split) overrides;
/// otherwise the `mediaremote-adapter` binary on `PATH`.
fn discover_argv() -> Vec<String> {
    if let Ok(cmd) = std::env::var("SUPERZEJ_MEDIAREMOTE_ADAPTER") {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if !argv.is_empty() {
            return argv;
        }
    }
    vec!["mediaremote-adapter".to_string()]
}
