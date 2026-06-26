//! Provider-agnostic media-player model — the substrate for the optional media
//! control layer. Mirrors how [`crate::ci`] keeps the pure normalized data +
//! formatters in core while the async backend trait lives in `superzej-svc`:
//! this module holds the normalized now-playing snapshot, the playback-state /
//! loop-mode axes, and the display formatters (the statusbar badge text + a
//! position/length stamp). All pure and testable — no tokio, no D-Bus.
//!
//! The control standard on Linux is **MPRIS** (`org.mpris.MediaPlayer2`, a
//! D-Bus interface) which nearly every player implements (Spotify, mpv, ncspot,
//! spotify-player, musikcube, moc, VLC, cmus, …). The async `MediaBackend`
//! trait, the native `zbus` MPRIS impl, the `playerctl` CLI fallback, and the
//! mpv JSON-IPC backend all live in `superzej-svc`, which carries the
//! tokio/D-Bus deps this crate forbids.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Transport state, normalized across backends (maps MPRIS `PlaybackStatus` and
/// mpv's `pause`/`idle` onto one axis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    /// Actively playing.
    Playing,
    /// Loaded but paused.
    Paused,
    /// Nothing loaded / stopped.
    #[default]
    Stopped,
}

impl PlaybackState {
    /// Parse an MPRIS `PlaybackStatus` property value.
    pub fn from_mpris(s: &str) -> PlaybackState {
        match s.trim().to_ascii_lowercase().as_str() {
            "playing" => PlaybackState::Playing,
            "paused" => PlaybackState::Paused,
            _ => PlaybackState::Stopped, // "Stopped" or unknown
        }
    }

    /// Is anything loaded (playing or paused) — i.e. worth showing in the badge?
    pub fn is_active(self) -> bool {
        matches!(self, PlaybackState::Playing | PlaybackState::Paused)
    }

    /// A compact transport glyph: ▶ playing, ❚❚ paused, ■ stopped.
    pub fn glyph(self) -> &'static str {
        match self {
            PlaybackState::Playing => "\u{25b6}",        // ▶
            PlaybackState::Paused => "\u{275a}\u{275a}", // ❚❚
            PlaybackState::Stopped => "\u{25a0}",        // ■
        }
    }
}

/// Repeat mode, normalized across backends (MPRIS `LoopStatus`: None/Track/
/// Playlist).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopMode {
    /// No repeat.
    #[default]
    None,
    /// Repeat the current track.
    Track,
    /// Repeat the whole playlist.
    Playlist,
}

impl LoopMode {
    /// Parse an MPRIS `LoopStatus` property value.
    pub fn from_mpris(s: &str) -> LoopMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "track" => LoopMode::Track,
            "playlist" => LoopMode::Playlist,
            _ => LoopMode::None,
        }
    }

    /// The canonical MPRIS `LoopStatus` string this maps to.
    pub fn as_mpris(self) -> &'static str {
        match self {
            LoopMode::None => "None",
            LoopMode::Track => "Track",
            LoopMode::Playlist => "Playlist",
        }
    }

    /// The next mode when cycling (None → Playlist → Track → None), matching the
    /// usual player UX where the first press enables whole-list repeat.
    pub fn cycle(self) -> LoopMode {
        match self {
            LoopMode::None => LoopMode::Playlist,
            LoopMode::Playlist => LoopMode::Track,
            LoopMode::Track => LoopMode::None,
        }
    }
}

/// A normalized now-playing snapshot. Every backend folds its native metadata
/// onto this; the host renders one badge/panel regardless of source. All fields
/// are best-effort — a backend leaves what it can't supply at its default.
///
/// Fully `Eq` (volume is an integer percent, not a float) so the host can hold
/// it inside the `Eq`-deriving `PanelData` and dirty-diff with `==`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaState {
    /// Which player this came from (the bus-name tail, e.g. "spotify", "mpv",
    /// "vlc"). Drives the player picker and `players_priority`.
    pub player: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub state: PlaybackState,
    /// Current playback position, when the backend exposes it.
    pub position: Option<Duration>,
    /// Track length, when known.
    pub length: Option<Duration>,
    /// Shuffle on/off, when the backend exposes it.
    pub shuffle: Option<bool>,
    /// Repeat mode, when the backend exposes it.
    pub loop_mode: Option<LoopMode>,
    /// Volume as an integer percent `0..=100`, when the backend exposes it.
    pub volume: Option<u8>,
    /// MPRIS `CanGoNext` — whether "next" is meaningful (drives UI gating).
    pub can_go_next: bool,
    /// MPRIS `CanGoPrevious`.
    pub can_go_previous: bool,
}

impl MediaState {
    /// `"Artist — Title"`, falling back gracefully when fields are missing.
    /// Used by both the statusbar badge and the panel header.
    pub fn now_playing(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", "") => self.player.clone(),
            ("", t) => t.to_string(),
            (a, "") => a.to_string(),
            (a, t) => format!("{a} \u{2014} {t}"), // em dash
        }
    }

    /// The full badge text — transport glyph + now-playing — e.g. `▶ Daft Punk
    /// — Get Lucky`. Returns `None` when nothing is loaded so the caller hides
    /// the badge entirely.
    pub fn badge(&self) -> Option<String> {
        if !self.state.is_active() {
            return None;
        }
        Some(format!("{} {}", self.state.glyph(), self.now_playing()))
    }

    /// `"m:ss / m:ss"` position-of-length stamp for the panel, or `None` when no
    /// position is known.
    pub fn position_stamp(&self) -> Option<String> {
        let pos = self.position?;
        match self.length {
            Some(len) => Some(format!("{} / {}", fmt_mmss(pos), fmt_mmss(len))),
            None => Some(fmt_mmss(pos)),
        }
    }
}

/// A playable list exposed via the MPRIS `Playlists` interface (drives the
/// "select playlist" picker). `id` is the opaque object path the backend hands
/// back to [activate]; `name` is the human label.
///
/// [activate]: # "MediaBackend::activate_playlist in superzej-svc"
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
}

/// Format a duration as `m:ss` (or `h:mm:ss` past an hour).
fn fmt_mmss(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_state_from_mpris() {
        assert_eq!(PlaybackState::from_mpris("Playing"), PlaybackState::Playing);
        assert_eq!(PlaybackState::from_mpris("paused"), PlaybackState::Paused);
        assert_eq!(PlaybackState::from_mpris("Stopped"), PlaybackState::Stopped);
        assert_eq!(PlaybackState::from_mpris("garbage"), PlaybackState::Stopped);
        assert!(PlaybackState::Playing.is_active());
        assert!(PlaybackState::Paused.is_active());
        assert!(!PlaybackState::Stopped.is_active());
    }

    #[test]
    fn loop_mode_roundtrip_and_cycle() {
        assert_eq!(LoopMode::from_mpris("Track"), LoopMode::Track);
        assert_eq!(LoopMode::from_mpris("Playlist"), LoopMode::Playlist);
        assert_eq!(LoopMode::from_mpris("None"), LoopMode::None);
        assert_eq!(LoopMode::from_mpris("none"), LoopMode::None);
        assert_eq!(LoopMode::Track.as_mpris(), "Track");
        // None → Playlist → Track → None
        assert_eq!(LoopMode::None.cycle(), LoopMode::Playlist);
        assert_eq!(LoopMode::Playlist.cycle(), LoopMode::Track);
        assert_eq!(LoopMode::Track.cycle(), LoopMode::None);
    }

    #[test]
    fn now_playing_fallbacks() {
        let mut s = MediaState {
            artist: "Daft Punk".into(),
            title: "Get Lucky".into(),
            ..Default::default()
        };
        assert_eq!(s.now_playing(), "Daft Punk \u{2014} Get Lucky");
        s.artist.clear();
        assert_eq!(s.now_playing(), "Get Lucky");
        s.title.clear();
        s.player = "spotify".into();
        assert_eq!(s.now_playing(), "spotify");
    }

    #[test]
    fn badge_hidden_when_stopped() {
        let mut s = MediaState {
            title: "X".into(),
            state: PlaybackState::Stopped,
            ..Default::default()
        };
        assert_eq!(s.badge(), None);
        s.state = PlaybackState::Playing;
        assert_eq!(s.badge().as_deref(), Some("\u{25b6} X"));
        s.state = PlaybackState::Paused;
        assert_eq!(s.badge().as_deref(), Some("\u{275a}\u{275a} X"));
    }

    #[test]
    fn position_stamp_formats() {
        let mut s = MediaState::default();
        assert_eq!(s.position_stamp(), None);
        s.position = Some(Duration::from_secs(75));
        assert_eq!(s.position_stamp().as_deref(), Some("1:15"));
        s.length = Some(Duration::from_secs(200));
        assert_eq!(s.position_stamp().as_deref(), Some("1:15 / 3:20"));
        s.position = Some(Duration::from_secs(3661));
        s.length = Some(Duration::from_secs(7200));
        assert_eq!(s.position_stamp().as_deref(), Some("1:01:01 / 2:00:00"));
    }
}
