//! macOS universal now-playing via the **mediaremote-adapter**. Apple gated the
//! private `MRMediaRemoteGetNowPlayingInfo` read path for unsigned binaries on
//! 15.4+, so a system-signed helper (the `mediaremote-adapter` project) is the
//! supported way to read the system Now-Playing session — which covers *every*
//! app (browsers, Spotify, Music, VLC, …), unlike the per-app AppleScript floor.
//!
//! We shell out to the adapter: `get` for a one-shot snapshot, `stream` for a
//! push watcher that emits a JSON line per change (the ~0%-idle contract). The
//! adapter command is discovered from `$THEGN_MEDIAREMOTE_ADAPTER` (a
//! space-separated argv prefix) or the `mediaremote-adapter` binary on `PATH`.
//! When it isn't installed, [`MediaRemote::connect`] returns `None` and the
//! caller falls back to [`crate::applescript`].
//!
//! Pure JSON decoding lives in `mediaremote_parse` (Linux-testable).

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::helper::{self, Limits};
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
        let mut cmd = Command::new(prog);
        cmd.args(base).args(args);
        let out = helper::output(cmd, Limits::OP)
            .await
            .map_err(|e| e.into_media("mediaremote adapter"))?;
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
        let mut cmd = Command::new(prog);
        cmd.args(base)
            .arg("stream")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Own process group so Drop can take down adapter descendants too.
        crate::platform::prepare_group(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| MediaError::Unavailable(format!("mediaremote stream: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| MediaError::Backend("mediaremote stream: no stdout".into()))?;
        Ok(MediaRemoteWatch {
            child,
            lines: BufReader::new(stdout),
            rec: Vec::new(),
        })
    }
}

impl MediaBackend for MediaRemote {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
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
        })
    }

    // The adapter's control verbs (`play`, `pause`, `next`, `previous`) map onto
    // the shared transport; where the adapter build lacks a verb it errors and
    // the UI simply reports it.
    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["toggle"]).await.map(|_| ()) })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["next"]).await.map(|_| ()) })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["previous"]).await.map(|_| ()) })
    }
    fn set_shuffle(&self, _on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { Err(MediaError::Backend("shuffle unsupported".into())) })
    }
    fn set_loop(&self, _mode: crate::model::LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { Err(MediaError::Backend("loop unsupported".into())) })
    }
    fn volume_step(&self, _delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            Ok(()) // system Now-Playing exposes no volume; caps().volume == false
        })
    }
    fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { Ok(()) })
    }

    fn players(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move { self.list_players().await })
    }

    /// The adapter's `watch` stream push watcher.
    fn watch(&self) -> BoxFuture<'_, Option<Box<dyn MediaWatch + Send>>> {
        Box::pin(async move {
            MediaRemote::watch(self)
                .await
                .ok()
                .map(|w| Box::new(w) as Box<dyn MediaWatch + Send>)
        })
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
    lines: BufReader<tokio::process::ChildStdout>,
    /// Partial record carried across a cancelled `changed()` call.
    rec: Vec<u8>,
}

/// Longest stream record accepted; a longer line is treated as a broken stream.
const MAX_RECORD: u64 = 64 * 1024;

impl MediaWatch for MediaRemoteWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            // A newline-terminated record ⇒ a change; EOF, error, or an
            // oversize record ⇒ stream ended (Drop then kills the tree). There
            // is deliberately no read-idle deadline: a push stream is silent
            // between changes, and a timer would break the 0%-idle contract.
            let room = MAX_RECORD.saturating_sub(self.rec.len() as u64);
            let n = (&mut self.lines)
                .take(room)
                .read_until(b'\n', &mut self.rec)
                .await;
            let whole = matches!(n, Ok(n) if n > 0) && self.rec.last() == Some(&b'\n');
            if whole {
                self.rec.clear();
            }
            whole
        })
    }
}

impl Drop for MediaRemoteWatch {
    fn drop(&mut self) {
        // Kill the whole adapter tree while the leader is unreaped (`id()` is
        // None once reaped, so we never killpg a recycled pgid); `kill_on_drop`
        // then reaps the leader in the background.
        if let Some(pid) = self.child.id() {
            crate::platform::kill_group(pid);
        }
        let _ = self.child.start_kill(); // best-effort: child may already have exited
    }
}

/// The adapter argv: `$THEGN_MEDIAREMOTE_ADAPTER` (space-split) overrides;
/// otherwise the `mediaremote-adapter` binary on `PATH`.
fn discover_argv() -> Vec<String> {
    if let Ok(cmd) = std::env::var("THEGN_MEDIAREMOTE_ADAPTER") {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if !argv.is_empty() {
            return argv;
        }
    }
    vec!["mediaremote-adapter".to_string()]
}
