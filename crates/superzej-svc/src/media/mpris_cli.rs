//! `playerctl` CLI fallback for MPRIS — used when the native `zbus` path can't
//! open a session bus. Drives the same `org.mpris.MediaPlayer2` players through
//! the `playerctl` binary. No playlist support (playerctl exposes none); the UI
//! gates that off [`MediaCaps::playlists`].

use std::time::Duration;

use tokio::process::Command;

use super::{MediaBackend, MediaCaps, MediaError};
use superzej_core::media::{LoopMode, MediaState, PlaybackState, Playlist};

/// Field separator for the `metadata --format` template — a unit-separator byte,
/// which never appears in track text.
const SEP: char = '\u{1f}';
const FORMAT: &str = "{{playerName}}\u{1f}{{status}}\u{1f}{{title}}\u{1f}{{artist}}\u{1f}{{album}}\u{1f}{{mpris:length}}\u{1f}{{position}}\u{1f}{{shuffle}}\u{1f}{{loopStatus}}\u{1f}{{volume}}";

pub struct MprisCli {
    priority: Vec<String>,
}

impl MprisCli {
    pub fn new(priority: Vec<String>) -> Self {
        Self { priority }
    }

    /// Is the `playerctl` binary on `PATH`? Cheap one-shot probe.
    pub fn available() -> bool {
        std::process::Command::new("playerctl")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// `--player=a,b,c` priority list, when configured.
    fn player_arg(&self) -> Option<String> {
        if self.priority.is_empty() {
            None
        } else {
            Some(format!("--player={}", self.priority.join(",")))
        }
    }

    /// Run `playerctl <args>`, returning trimmed stdout. A non-zero exit (no
    /// player, unsupported op) maps to [`MediaError::NoPlayer`].
    async fn run(&self, args: &[&str]) -> Result<String, MediaError> {
        let mut cmd = Command::new("playerctl");
        if let Some(p) = self.player_arg() {
            cmd.arg(p);
        }
        cmd.args(args);
        let out = cmd
            .output()
            .await
            .map_err(|e| MediaError::Unavailable(e.to_string()))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(MediaError::NoPlayer)
        }
    }

    pub async fn list_players(&self) -> Result<Vec<String>, MediaError> {
        // `--list-all` ignores the --player filter, so call playerctl directly.
        let out = Command::new("playerctl")
            .arg("--list-all")
            .output()
            .await
            .map_err(|e| MediaError::Unavailable(e.to_string()))?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }
}

impl MediaBackend for MprisCli {
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        let line = match self.run(&["metadata", "--format", FORMAT]).await {
            Ok(l) => l,
            Err(MediaError::NoPlayer) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(parse_line(&line)))
    }

    async fn play_pause(&self) -> Result<(), MediaError> {
        self.run(&["play-pause"]).await.map(|_| ())
    }
    async fn next(&self) -> Result<(), MediaError> {
        self.run(&["next"]).await.map(|_| ())
    }
    async fn previous(&self) -> Result<(), MediaError> {
        self.run(&["previous"]).await.map(|_| ())
    }
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        self.run(&["shuffle", if on { "On" } else { "Off" }])
            .await
            .map(|_| ())
    }
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        self.run(&["loop", mode.as_mpris()]).await.map(|_| ())
    }
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        // playerctl relative-volume syntax: "0.05+" / "0.05-".
        let arg = format!("{:.3}{}", delta.abs(), if delta < 0.0 { "-" } else { "+" });
        self.run(&["volume", &arg]).await.map(|_| ())
    }

    async fn playlists(&self) -> Result<Vec<Playlist>, MediaError> {
        Ok(Vec::new()) // playerctl exposes no Playlists interface
    }
    async fn activate_playlist(&self, _id: &str) -> Result<(), MediaError> {
        Ok(()) // no-op; caps().playlists == false so the UI never calls this
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

/// Parse the `\x1f`-delimited `metadata --format` line into a [`MediaState`].
/// Missing/blank fields fall back to defaults — players vary in what they expose.
fn parse_line(line: &str) -> MediaState {
    let f: Vec<&str> = line.split(SEP).collect();
    let get = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");

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

    MediaState {
        player: get(0).to_string(),
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
            .map(|v| (v * 100.0).round().clamp(0.0, 100.0) as u8),
        can_go_next: true,
        can_go_previous: true,
    }
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
        let line = "spotify\u{1f}Playing\u{1f}Get Lucky\u{1f}Daft Punk\u{1f}Random Access Memories\u{1f}248000000\u{1f}1000000\u{1f}true\u{1f}Playlist\u{1f}0.80";
        let s = parse_line(line);
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
    fn tolerates_blank_fields() {
        // A player that exposes only status + title.
        let line = "mpv\u{1f}Paused\u{1f}Some Song\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}\u{1f}";
        let s = parse_line(line);
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
    fn volume_step_arg_format() {
        assert_eq!(format!("{:.3}{}", 0.05f64.abs(), "+"), "0.050+");
        assert_eq!(format!("{:.3}{}", (-0.05f64).abs(), "-"), "0.050-");
    }
}
