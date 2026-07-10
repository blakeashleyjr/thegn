//! Provider-agnostic media-player model — the pure normalized data + display
//! formatters shared by every backend. Holds the now-playing snapshot, the
//! playback-state / loop-mode axes, and the formatters (the statusbar badge text
//! and a position/length stamp). All pure and testable — no tokio, no D-Bus and
//! no OS deps — so it cross-compiles cleanly and `superzej-core` re-exports it.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Transport state, normalized across backends (maps MPRIS `PlaybackStatus`,
/// Windows SMTC `PlaybackStatus`, and mpv's `pause`/`idle` onto one axis).
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

/// What kind of media is loaded, so the UI can offer video-centric affordances
/// (larger seek steps, chapter next/prev, a fullscreen toggle) only when they
/// make sense. Derived best-effort from metadata / player heuristics; `Unknown`
/// is treated as audio by the UI (the safe default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// Music / audio-only.
    Audio,
    /// Video (a file with a video track, a movie player, a browser video).
    Video,
    /// Couldn't tell — the UI treats this as audio.
    #[default]
    Unknown,
}

impl MediaKind {
    /// Is this something we should offer video controls for?
    pub fn is_video(self) -> bool {
        matches!(self, MediaKind::Video)
    }

    /// Best-effort classification from MPRIS-ish metadata hints and the player
    /// name. `mime` is `mpris:mime`/content type when present; `url` is
    /// `xesam:url`; `player` is the bus-name tail. Pure so it's unit-tested on
    /// every platform.
    pub fn from_hints(player: &str, mime: Option<&str>, url: Option<&str>) -> MediaKind {
        if let Some(m) = mime {
            let m = m.to_ascii_lowercase();
            if m.starts_with("video/") {
                return MediaKind::Video;
            }
            if m.starts_with("audio/") {
                return MediaKind::Audio;
            }
        }
        if let Some(u) = url {
            let u = u.to_ascii_lowercase();
            const VIDEO_EXT: &[&str] = &[
                ".mp4", ".mkv", ".webm", ".mov", ".avi", ".m4v", ".flv", ".wmv", ".mpg", ".mpeg",
                ".ts",
            ];
            if VIDEO_EXT.iter().any(|e| u.contains(e)) {
                return MediaKind::Video;
            }
            const AUDIO_EXT: &[&str] = &[
                ".mp3", ".flac", ".ogg", ".oga", ".opus", ".m4a", ".wav", ".aac", ".wma",
            ];
            if AUDIO_EXT.iter().any(|e| u.contains(e)) {
                return MediaKind::Audio;
            }
        }
        // Player-name heuristic: dedicated video players / browsers lean video.
        let p = player.to_ascii_lowercase();
        const VIDEO_PLAYERS: &[&str] = &[
            "mpv",
            "vlc",
            "kodi",
            "smplayer",
            "totem",
            "celluloid",
            "haruna",
            "chromium",
            "chrome",
            "firefox",
            "brave",
            "youtube",
        ];
        if VIDEO_PLAYERS.iter().any(|n| p.contains(n)) {
            return MediaKind::Video;
        }
        const AUDIO_PLAYERS: &[&str] = &[
            "spotify",
            "mpd",
            "ncspot",
            "cmus",
            "moc",
            "rhythmbox",
            "clementine",
            "audacious",
            "musikcube",
            "spotifyd",
            "tidal",
        ];
        if AUDIO_PLAYERS.iter().any(|n| p.contains(n)) {
            return MediaKind::Audio;
        }
        MediaKind::Unknown
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
    /// Cover-art reference (`mpris:artUrl` — a `file://`/`http(s)` URL, an SMTC
    /// thumbnail token, or an AppleScript artwork path). The host fetches +
    /// renders it; the leaf only carries the reference. `None` ⇒ no art.
    pub art_url: Option<String>,
    /// Audio vs video, so the UI can gate video-only controls.
    pub kind: MediaKind,
    /// Whether the backend supports seeking within the current track (MPRIS
    /// `CanSeek`; true for mpv/playerctl). Gates the scrubber + skip keys.
    pub can_seek: bool,
    /// Opaque current-track id (MPRIS `mpris:trackid`), needed for absolute
    /// `SetPosition`. `None` when the backend doesn't expose one.
    pub track_id: Option<String>,
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
/// back to `MediaBackend::activate_playlist`; `name` is the human label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
}

/// One entry in the play queue / up-next list (MPRIS `TrackList`, mpv
/// `playlist`). `id` is the opaque handle handed back to
/// `MediaBackend::play_queue_item` (an MPRIS track object path, or a stringified
/// mpv playlist index). Fully `Eq` for dirty-diffing inside `PanelData`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration: Option<Duration>,
    /// Whether this is the currently-playing entry (drives the ▸ marker).
    pub is_current: bool,
}

impl QueueItem {
    /// `"Artist — Title"`, falling back to the title/id when fields are missing.
    pub fn label(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", "") => self.id.clone(),
            ("", t) => t.to_string(),
            (a, t) => format!("{a} \u{2014} {t}"),
        }
    }
}

/// Format a duration as `m:ss` (or `h:mm:ss` past an hour).
pub(crate) fn fmt_mmss(d: Duration) -> String {
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
    fn media_kind_from_hints() {
        // mime wins.
        assert_eq!(
            MediaKind::from_hints("spotify", Some("video/mp4"), None),
            MediaKind::Video
        );
        assert_eq!(
            MediaKind::from_hints("mpv", Some("audio/flac"), None),
            MediaKind::Audio
        );
        // url extension next.
        assert_eq!(
            MediaKind::from_hints("x", None, Some("file:///m/clip.MKV")),
            MediaKind::Video
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("file:///m/song.mp3")),
            MediaKind::Audio
        );
        // player-name heuristic last.
        assert_eq!(MediaKind::from_hints("mpv", None, None), MediaKind::Video);
        assert_eq!(
            MediaKind::from_hints("spotify", None, None),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::from_hints("mystery", None, None),
            MediaKind::Unknown
        );
        assert!(MediaKind::Video.is_video());
        assert!(!MediaKind::Unknown.is_video());
    }

    #[test]
    fn queue_item_label() {
        let q = QueueItem {
            artist: "Aphex Twin".into(),
            title: "Xtal".into(),
            ..Default::default()
        };
        assert_eq!(q.label(), "Aphex Twin \u{2014} Xtal");
        let q = QueueItem {
            title: "Untitled".into(),
            ..Default::default()
        };
        assert_eq!(q.label(), "Untitled");
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
