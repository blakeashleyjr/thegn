//! macOS **fallback** backend driving Music.app + Spotify via `osascript`
//! (AppleScript). Universal now-playing (any app, incl. browser tabs) comes from
//! [`crate::mediaremote`], which `auto` prefers; this per-app floor runs only
//! when the MediaRemote adapter isn't installed. No entitlement, every macOS
//! version — it scripts the two reliably-scriptable players directly (broader
//! apps like VLC/browsers aren't uniformly scriptable, so they're covered by the
//! MediaRemote path rather than faked here).
//!
//! Poll mode (`caps.signals = false`): the host re-snapshots on the
//! `[media] poll_interval_secs` ticker. The unit-separated read output is folded
//! by the Linux-testable `applescript_parse`.

use futures::future::BoxFuture;

use crate::model::{LoopMode, MediaState, Playlist};
use crate::{MediaBackend, MediaCaps, MediaError};

/// Players probed, in priority order. The first running + non-stopped one wins.
const APPS: &[&str] = &["Spotify", "Music"];

/// Stateless `osascript` controller.
pub struct AppleScript;

impl AppleScript {
    pub fn new() -> Self {
        AppleScript
    }

    /// Read one app; `Ok(None)` when it isn't running or is stopped.
    async fn read_one(&self, app: &str) -> Result<Option<String>, MediaError> {
        let line = osascript(&read_script(app)).await?;
        Ok(if line.is_empty() { None } else { Some(line) })
    }

    /// The first running + non-stopped player (the control target).
    async fn active_app(&self) -> Option<(&'static str, String)> {
        for app in APPS {
            if let Ok(Some(line)) = self.read_one(app).await {
                return Some((app, line));
            }
        }
        None
    }

    pub async fn list_players(&self) -> Vec<String> {
        match self.active_app().await {
            Some((app, _)) => vec![app.to_string()],
            None => Vec::new(),
        }
    }

    /// Send a control body to the active app (Spotify and Music share most verbs;
    /// the differing ones pass distinct bodies).
    async fn control(&self, spotify_body: &str, music_body: &str) -> Result<(), MediaError> {
        let (app, _) = self.active_app().await.ok_or(MediaError::NoPlayer)?;
        let body = if app == "Spotify" {
            spotify_body
        } else {
            music_body
        };
        osascript(&format!(
            "if application \"{app}\" is running then\ntell application \"{app}\"\n{body}\nend tell\nend if"
        ))
        .await
        .map(|_| ())
    }
}

impl Default for AppleScript {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaBackend for AppleScript {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            match self.active_app().await {
                Some((_, line)) => Ok(Some(crate::applescript_parse::parse_line(&line))),
                None => Ok(None),
            }
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.control("playpause", "playpause").await })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.control("next track", "next track").await })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.control("previous track", "previous track").await })
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let v = if on { "true" } else { "false" };
            self.control(
                &format!("set shuffling to {v}"),
                &format!("set shuffle enabled to {v}"),
            )
            .await
        })
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // Spotify only has on/off; Music has off/one/all.
            let spotify = match mode {
                LoopMode::None => "set repeating to false",
                _ => "set repeating to true",
            };
            let music = match mode {
                LoopMode::None => "set song repeat to off",
                LoopMode::Track => "set song repeat to one",
                LoopMode::Playlist => "set song repeat to all",
            };
            self.control(spotify, music).await
        })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // `sound volume` is 0..=100 on both apps; clamp in-script.
            let step = (delta * 100.0).round() as i64;
            let body = format!(
                "set v to (sound volume) + {step}\nif v > 100 then set v to 100\nif v < 0 then set v to 0\nset sound volume to v"
            );
            self.control(&body, &body).await
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>> {
        Box::pin(async move {
            Ok(Vec::new()) // not exposed via this scripting floor
        })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { Ok(()) })
    }

    fn seek(
        &self,
        offset: std::time::Duration,
        forward: bool,
    ) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // `player position` is in seconds on both apps; nudge relative to it.
            let step = offset.as_secs_f64() * if forward { 1.0 } else { -1.0 };
            let body = format!(
                "set p to (player position) + {step}\nif p < 0 then set p to 0\nset player position to p"
            );
            self.control(&body, &body).await
        })
    }
    fn set_position<'a>(
        &'a self,
        pos: std::time::Duration,
        _track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            let body = format!("set player position to {}", pos.as_secs_f64());
            self.control(&body, &body).await
        })
    }
    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let body = format!("set sound volume to {}", level.min(100));
            self.control(&body, &body).await
        })
    }

    /// The running scriptable apps; no push watcher (the host polls).
    fn players(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move { self.list_players().await })
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: true,
            playlists: false,
            signals: false, // host polls on [media] poll_interval_secs
            seek: true,
            art: false,
            queue: false,
            abs_volume: true,
            chapters: false,
            fullscreen: false,
        }
    }
}

/// Run an AppleScript via `osascript -e`, returning trimmed stdout.
async fn osascript(script: &str) -> Result<String, MediaError> {
    let out = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output()
        .await
        .map_err(|e| MediaError::Unavailable(e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(MediaError::Backend(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

/// The read snippet for one app: emits the unit-separated line consumed by
/// [`crate::applescript_parse`], or `""` when not running / stopped.
fn read_script(app: &str) -> String {
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
        "if application \"{app}\" is running then\n\
         tell application \"{app}\"\n\
         if player state is stopped then\n\
         return \"\"\n\
         end if\n\
         set sep to (ASCII character 31)\n\
         set t to current track\n\
         return \"{app}\" & sep & (player state as text) & sep & (name of t) & sep & (artist of t) & sep & (album of t) & sep & {duration_expr} & sep & (player position) & sep & {shuffle_expr} & sep & {repeat_expr} & sep & (sound volume as text)\n\
         end tell\n\
         end if\n\
         return \"\""
    )
}
