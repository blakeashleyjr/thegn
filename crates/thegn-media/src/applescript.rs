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
//! by the Linux-testable [`crate::applescript_parse`].

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
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        match self.active_app().await {
            Some((_, line)) => Ok(Some(crate::applescript_parse::parse_line(&line))),
            None => Ok(None),
        }
    }

    async fn play_pause(&self) -> Result<(), MediaError> {
        self.control("playpause", "playpause").await
    }
    async fn next(&self) -> Result<(), MediaError> {
        self.control("next track", "next track").await
    }
    async fn previous(&self) -> Result<(), MediaError> {
        self.control("previous track", "previous track").await
    }
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        let v = if on { "true" } else { "false" };
        self.control(
            &format!("set shuffling to {v}"),
            &format!("set shuffle enabled to {v}"),
        )
        .await
    }
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
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
    }
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        // `sound volume` is 0..=100 on both apps; clamp in-script.
        let step = (delta * 100.0).round() as i64;
        let body = format!(
            "set v to (sound volume) + {step}\nif v > 100 then set v to 100\nif v < 0 then set v to 0\nset sound volume to v"
        );
        self.control(&body, &body).await
    }

    async fn playlists(&self) -> Result<Vec<Playlist>, MediaError> {
        Ok(Vec::new()) // not exposed via this scripting floor
    }
    async fn activate_playlist(&self, _id: &str) -> Result<(), MediaError> {
        Ok(())
    }

    async fn seek(&self, offset: std::time::Duration, forward: bool) -> Result<(), MediaError> {
        // `player position` is in seconds on both apps; nudge relative to it.
        let step = offset.as_secs_f64() * if forward { 1.0 } else { -1.0 };
        let body = format!(
            "set p to (player position) + {step}\nif p < 0 then set p to 0\nset player position to p"
        );
        self.control(&body, &body).await
    }
    async fn set_position(
        &self,
        pos: std::time::Duration,
        _track_id: Option<&str>,
    ) -> Result<(), MediaError> {
        let body = format!("set player position to {}", pos.as_secs_f64());
        self.control(&body, &body).await
    }
    async fn set_volume(&self, level: u8) -> Result<(), MediaError> {
        let body = format!("set sound volume to {}", level.min(100));
        self.control(&body, &body).await
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
