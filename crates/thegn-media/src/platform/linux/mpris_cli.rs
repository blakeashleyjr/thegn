//! `playerctl` CLI fallback for MPRIS — used when the native `zbus` path can't
//! open a session bus. Drives the same `org.mpris.MediaPlayer2` players through
//! the `playerctl` binary. No playlist support (playerctl exposes none); the UI
//! gates that off [`MediaCaps::playlists`].

use std::time::Duration;

use futures::future::BoxFuture;
use tokio::process::Command;

use crate::helper::{self, Limits};
use crate::model::{LoopMode, MediaKind, MediaState, PlaybackState, Playlist};
use crate::{MediaBackend, MediaCaps, MediaError};

/// Field separator for the `metadata --format` template. Since metadata may
/// contain this byte, the parser validates the complete field count and rejects
/// records whose framing is ambiguous.
const SEP: char = '\u{1f}';
const FIELDS: usize = 13;
const FORMAT: &str = "{{playerName}}\u{1f}{{status}}\u{1f}{{title}}\u{1f}{{artist}}\u{1f}{{album}}\u{1f}{{mpris:length}}\u{1f}{{position}}\u{1f}{{shuffle}}\u{1f}{{loopStatus}}\u{1f}{{volume}}\u{1f}{{mpris:artUrl}}\u{1f}{{xesam:url}}\u{1f}{{mpris:trackid}}";

pub struct MprisCli {
    priority: Vec<String>,
}

impl MprisCli {
    pub fn new(priority: Vec<String>) -> Self {
        Self { priority }
    }

    /// Is the `playerctl` binary on `PATH`? One-shot probe, async and bounded so
    /// a wedged binary can neither block a worker thread nor stall resolution.
    pub async fn available() -> bool {
        let mut cmd = Command::new("playerctl");
        cmd.arg("--version");
        matches!(helper::output(cmd, Limits::PROBE).await, Ok(o) if o.status.success())
    }

    /// `--player=a,b,c` priority list, when configured.
    fn player_arg(&self) -> Option<String> {
        if self.priority.is_empty() {
            None
        } else {
            Some(format!("--player={}", self.priority.join(",")))
        }
    }

    /// Run `playerctl <args>`, returning trimmed stdout.
    async fn run(&self, args: &[&str]) -> Result<String, MediaError> {
        let mut cmd = Command::new("playerctl");
        if let Some(p) = self.player_arg() {
            cmd.arg(p);
        }
        cmd.args(args);
        let out = helper::output(cmd, Limits::OP)
            .await
            .map_err(|e| e.into_media("playerctl"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            tracing::debug!(
                target: "thegn::media",
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "playerctl {args:?} failed"
            );
            Err(classify_failure(&out.stderr))
        }
    }

    pub async fn list_players(&self) -> Result<Vec<String>, MediaError> {
        // `--list-all` ignores the --player filter, so call playerctl directly.
        let mut cmd = Command::new("playerctl");
        cmd.arg("--list-all");
        let out = helper::output(cmd, Limits::OP)
            .await
            .map_err(|e| e.into_media("playerctl"))?;
        if !out.status.success() {
            return match classify_failure(&out.stderr) {
                MediaError::NoPlayer => Ok(Vec::new()),
                error => Err(error),
            };
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }
}

impl MediaBackend for MprisCli {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            let line = match self.run(&["metadata", "--format", FORMAT]).await {
                Ok(l) => l,
                Err(MediaError::NoPlayer) => return Ok(None),
                Err(e) => return Err(e),
            };
            Ok(Some(parse_line(&line)?))
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["play-pause"]).await.map(|_| ()) })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["next"]).await.map(|_| ()) })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["previous"]).await.map(|_| ()) })
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            self.run(&["shuffle", if on { "On" } else { "Off" }])
                .await
                .map(|_| ())
        })
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.run(&["loop", mode.as_mpris()]).await.map(|_| ()) })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // playerctl relative-volume syntax: "0.05+" / "0.05-".
            let arg = format!("{:.3}{}", delta.abs(), if delta < 0.0 { "-" } else { "+" });
            self.run(&["volume", &arg]).await.map(|_| ())
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>> {
        Box::pin(async {
            Err(MediaError::Unsupported(
                "playerctl playlists are not implemented".into(),
            ))
        })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async {
            Err(MediaError::Unsupported(
                "playerctl playlist activation is not implemented".into(),
            ))
        })
    }

    fn seek(&self, offset: Duration, forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // `playerctl position <secs>+` / `<secs>-` seeks relative.
            let arg = format!(
                "{:.3}{}",
                offset.as_secs_f64(),
                if forward { "+" } else { "-" }
            );
            self.run(&["position", &arg]).await.map(|_| ())
        })
    }
    fn set_position<'a>(
        &'a self,
        pos: Duration,
        _track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            // Bare `playerctl position <secs>` sets an absolute position.
            let arg = format!("{:.3}", pos.as_secs_f64());
            self.run(&["position", &arg]).await.map(|_| ())
        })
    }
    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let arg = format!("{:.3}", (level.min(100) as f64) / 100.0);
            self.run(&["volume", &arg]).await.map(|_| ())
        })
    }

    /// `playerctl --list-all`; no push watcher (the host polls).
    fn players(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async move { self.list_players().await.unwrap_or_default() })
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: true,
            playlists: false,
            signals: false, // host polls on [media] poll_interval_secs
            seek: true,
            art: true,
            queue: false,
            abs_volume: true,
            chapters: false,
            fullscreen: false,
        }
    }
}

fn classify_failure(stderr: &[u8]) -> MediaError {
    let text = String::from_utf8_lossy(stderr);
    if text.contains("No players found") {
        return MediaError::NoPlayer;
    }
    let mut message: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(200)
        .collect();
    if message.trim().is_empty() {
        message = "playerctl exited unsuccessfully".into();
    }
    MediaError::Backend(message.trim().to_string())
}

/// Parse the `\x1f`-delimited `metadata --format` line into a [`MediaState`].
/// Every template field must be present; ambiguous framing is rejected atomically.
fn parse_line(line: &str) -> Result<MediaState, MediaError> {
    let f: Vec<&str> = line.split(SEP).collect();
    if f.len() != FIELDS {
        return Err(MediaError::Backend(format!(
            "malformed playerctl metadata record ({} fields)",
            f.len()
        )));
    }
    let get = |i: usize| f[i].trim();

    let length = get(5)
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|us| Duration::from_micros(us as u64));
    // `{{position}}` is microseconds in playerctl's templating.
    let position = get(6)
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .map(|us| Duration::from_micros(us as u64));

    let player = get(0).to_string();
    let art = get(10);
    let url = get(11);
    let trackid = get(12);
    Ok(MediaState {
        title: get(2).to_string(),
        artist: get(3).to_string(),
        album: get(4).to_string(),
        state: PlaybackState::from_mpris(get(1)),
        position,
        length,
        shuffle: parse_bool(get(7)),
        loop_mode: parse_loop(get(8)),
        volume: get(9)
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8),
        can_go_next: true,
        can_go_previous: true,
        art_url: (!art.is_empty()).then(|| art.to_string()),
        kind: MediaKind::from_hints(&player, None, (!url.is_empty()).then_some(url)),
        can_seek: true, // playerctl can always position-seek
        track_id: (!trackid.is_empty()).then(|| trackid.to_string()),
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

fn parse_loop(s: &str) -> Option<LoopMode> {
    if s.is_empty() {
        None
    } else {
        Some(LoopMode::from_mpris(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_line() {
        let line = "spotify\u{1f}Playing\u{1f}Get Lucky\u{1f}Daft Punk\u{1f}Random Access Memories\u{1f}248000000\u{1f}1000000\u{1f}true\u{1f}Playlist\u{1f}0.80\u{1f}art\u{1f}url\u{1f}track-id";
        let s = parse_line(line).unwrap();
        assert_eq!(s.player, "spotify");
        assert_eq!(s.state, PlaybackState::Playing);
        assert_eq!(s.title, "Get Lucky");
        assert_eq!(s.artist, "Daft Punk");
        assert_eq!(s.album, "Random Access Memories");
        assert_eq!(s.length, Some(Duration::from_secs(248)));
        assert_eq!(s.position, Some(Duration::from_secs(1)));
        assert_eq!(s.shuffle, Some(true));
        assert_eq!(s.loop_mode, Some(LoopMode::Playlist));
        assert_eq!(s.volume, Some(80));
    }

    #[test]
    fn nan_volume_is_unknown() {
        let line = "spotify\u{1f}Playing\u{1f}Title\u{1f}Artist\u{1f}Album\u{1f}1000000\u{1f}0\u{1f}true\u{1f}None\u{1f}nan\u{1f}art\u{1f}url\u{1f}track-id";
        let state = parse_line(line).unwrap();
        assert_eq!(state.volume, None);
    }

    #[test]
    fn tolerates_blank_fields() {
        // A player that exposes only status + title.
        let line = "mpv\u{1f}Paused\u{1f}Some Song\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}";
        let s = parse_line(line).unwrap();
        assert_eq!(s.player, "mpv");
        assert_eq!(s.state, PlaybackState::Paused);
        assert_eq!(s.title, "Some Song");
        assert_eq!(s.artist, "");
        assert_eq!(s.length, None);
        assert_eq!(s.shuffle, None);
        assert_eq!(s.loop_mode, None);
        assert_eq!(s.volume, None);
    }

    #[test]
    fn rejects_wrong_field_counts_and_delimiter_collisions() {
        let valid = "spotify\u{1f}Playing\u{1f}Title\u{1f}Artist\u{1f}Album\u{1f}1000000\u{1f}0\u{1f}true\u{1f}None\u{1f}0.8\u{1f}art\u{1f}url\u{1f}track-id";
        assert!(parse_line(valid).is_ok());

        let fields: Vec<&str> = valid.split(SEP).collect();
        let twelve_fields = fields[..12].join(&SEP.to_string());
        assert!(parse_line(&twelve_fields).is_err());
        let fourteen_fields = format!("{valid}{SEP}extra");
        assert!(parse_line(&fourteen_fields).is_err());

        let colliding_title = valid.replacen("Title", "Title\u{1f}Injected", 1);
        assert!(parse_line(&colliding_title).is_err());
    }

    #[test]
    fn volume_step_arg_format() {
        assert_eq!(format!("{:.3}{}", 0.05f64.abs(), "+"), "0.050+");
        assert_eq!(format!("{:.3}{}", (-0.05f64).abs(), "-"), "0.050-");
    }

    #[test]
    fn classifies_playerctl_failures_without_hiding_provider_errors() {
        assert_eq!(classify_failure(b"No players found"), MediaError::NoPlayer);
        assert_eq!(
            classify_failure(b"Cannot play: not supported\n"),
            MediaError::Backend("Cannot play: not supported".into())
        );
        let long = vec![b'x'; 300];
        assert!(
            matches!(classify_failure(&long), MediaError::Backend(message) if message.len() <= 200)
        );
        assert!(matches!(
            classify_failure(b"bad\x1b[31merror"),
            MediaError::Backend(message) if !message.chars().any(char::is_control)
        ));
    }

    #[tokio::test]
    async fn playlist_methods_are_unsupported() {
        let cli = MprisCli::new(vec![]);
        assert!(matches!(
            cli.playlists().await,
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            cli.activate_playlist("x").await,
            Err(MediaError::Unsupported(_))
        ));
    }
}
