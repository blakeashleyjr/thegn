//! Provider-agnostic media-player model — the pure normalized data + display
//! formatters shared by every backend. Holds the now-playing snapshot, the
//! playback-state / loop-mode axes, and the formatters (the statusbar badge text
//! and a position/length stamp). All pure and testable — no tokio, no D-Bus and
//! no OS deps — so it cross-compiles cleanly and `thegn-core` re-exports it.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Maximum normalized display-label characters retained for media metadata.
/// Host-panel collection row limits are defined alongside this display cap.
pub const MAX_LABEL_CHARS: usize = 256;
/// Maximum combining marks kept after one base character in a display label: the Unicode
/// Stream-Safe Text Format limit (UAX #15). Further marks in the same run are dropped.
pub const MAX_COMBINING_RUN: usize = 30;
/// Maximum UTF-8 bytes retained for each provider supplied display field.
/// Fields are cut only at character boundaries by the canonical ingress
/// admission step.
pub const MAX_FIELD_BYTES: usize = 4096;
/// Maximum UTF-8 bytes accepted for an opaque provider identifier.
/// Identifiers are rejected above this limit and are never truncated.
pub const MAX_ID_BYTES: usize = 1024;
/// Maximum queue rows retained at backend ingress and by the host media panel.
pub const MAX_QUEUE_ITEMS: usize = 500;
/// Maximum source rows retained by the host media panel.
pub const MAX_SOURCES: usize = 64;
/// Maximum playlist rows retained at backend ingress; matches the count native
/// MPRIS requests from `GetPlaylists`.
pub const MAX_PLAYLISTS: usize = 100;
/// Maximum player names a consumer displays; ingress keeps one more
/// ([`PLAYER_LIST_LIMIT`]).
pub const MAX_PLAYERS: usize = MAX_SOURCES;
/// Player names retained at ingress: the cap plus one overflow name, so a
/// consumer that sees more than [`MAX_PLAYERS`] names knows the list was cut.
/// Consumers display at most [`MAX_PLAYERS`].
pub const PLAYER_LIST_LIMIT: usize = MAX_PLAYERS + 1;

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
            const VIDEO_EXT: &[&str] = &[
                "mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "wmv", "mpg", "mpeg", "ts",
            ];
            if url_extension(u)
                .is_some_and(|ext| VIDEO_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)))
            {
                return MediaKind::Video;
            }
            const AUDIO_EXT: &[&str] = &[
                "mp3", "flac", "ogg", "oga", "opus", "m4a", "wav", "aac", "wma",
            ];
            if url_extension(u)
                .is_some_and(|ext| AUDIO_EXT.iter().any(|e| ext.eq_ignore_ascii_case(e)))
            {
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

/// Return the final path segment's extension without consulting a URL's host,
/// query, or fragment. Bare paths are kept intact because providers such as MPD
/// commonly supply relative filenames rather than URLs.
fn url_extension(value: &str) -> Option<String> {
    let path = if let Some((_, after_scheme)) = value.split_once("://") {
        let boundary = after_scheme.find(['/', '?', '#'])?;
        if after_scheme.as_bytes()[boundary] != b'/' {
            return None;
        }
        let path = after_scheme[boundary..]
            .split(['?', '#'])
            .next()
            .unwrap_or(&after_scheme[boundary..]);
        percent_decode(path).unwrap_or_else(|| path.to_string())
    } else {
        value.to_string()
    };
    let filename = path.rsplit('/').next()?;
    let (_, extension) = filename.rsplit_once('.')?;
    if extension.is_empty() {
        return None;
    }
    Some(extension.to_string())
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Normalize provider-controlled text before it is displayed in terminal UI.
/// C0/C1 controls and U+2028/U+2029 line separators become spaces; bidi and invisible format
/// characters drop. At most `MAX_COMBINING_RUN` combining marks are kept per base character.
/// Output is capped at `MAX_LABEL_CHARS` normalized characters, with `...` when visible text is
/// omitted.
pub fn display_text(value: &str) -> String {
    display_text_parts(&[value])
}

fn normalize_display_char(ch: char) -> Option<char> {
    match ch {
        '\u{00ad}'
        | '\u{034f}'
        | '\u{061c}'
        | '\u{180e}'
        | '\u{200e}'
        | '\u{200f}'
        | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{2069}'
        | '\u{206a}'..='\u{206f}'
        | '\u{200b}'..='\u{200d}'
        | '\u{feff}' => None,
        '\u{2028}' | '\u{2029}' => Some(' '),
        ch if ch.is_control() => Some(' '),
        ch => Some(ch),
    }
}

/// Match the generic combining-diacritic blocks; not the full Mn category, which needs Unicode
/// data this leaf crate does not carry.
fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036f}'
            | '\u{0483}'..='\u{0489}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20ff}'
            | '\u{fe20}'..='\u{fe2f}'
    )
}

#[derive(Default)]
struct DisplayFilter {
    combining_run: usize,
}

impl DisplayFilter {
    fn admit(&mut self, ch: char) -> Option<char> {
        let out = normalize_display_char(ch)?;
        let is_mark = is_combining_mark(out);
        if is_mark && self.combining_run >= MAX_COMBINING_RUN {
            return None;
        }
        if is_mark {
            self.combining_run += 1;
        } else {
            self.combining_run = 0;
        }
        Some(out)
    }
}

fn display_text_parts(parts: &[&str]) -> String {
    let mut chars = parts.iter().flat_map(|part| part.chars());
    let mut output = String::with_capacity(MAX_LABEL_CHARS + 3);
    let mut filter = DisplayFilter::default();
    let mut count = 0;
    for ch in chars.by_ref() {
        let normalized = filter.admit(ch);
        if let Some(normalized) = normalized {
            output.push(normalized);
            count += 1;
            if count == MAX_LABEL_CHARS {
                break;
            }
        }
    }
    if count == MAX_LABEL_CHARS && chars.any(|ch| filter.admit(ch).is_some()) {
        output.push_str("...");
    }
    output
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
    /// Parse a known MPRIS `LoopStatus` property value.
    pub fn parse_mpris(s: &str) -> Option<LoopMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(LoopMode::None),
            "track" => Some(LoopMode::Track),
            "playlist" => Some(LoopMode::Playlist),
            _ => None,
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
    /// Apply the shared provider-ingress bounds while preserving exact opaque
    /// identifiers. Invalid identifiers and oversized art references are
    /// omitted rather than truncated.
    pub fn admitted(mut self) -> MediaState {
        self.player = bounded_field(self.player);
        self.title = bounded_field(self.title);
        self.artist = bounded_field(self.artist);
        self.album = bounded_field(self.album);
        self.art_url = self
            .art_url
            .filter(|url| url.len() <= MAX_FIELD_BYTES && !url.chars().any(char::is_control));
        self.track_id = self.track_id.filter(|id| is_valid_opaque_id(id));
        self
    }

    /// `"Artist — Title"`, falling back gracefully when fields are missing.
    /// Used by both the statusbar badge and the panel header.
    pub fn now_playing(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", "") => display_text(&self.player),
            ("", title) => display_text(title),
            (artist, "") => display_text(artist),
            (artist, title) => display_text_parts(&[artist, " \u{2014} ", title]),
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

/// Whether an opaque provider identifier is safe to retain and round-trip.
/// Accepted IDs remain byte-exact; callers must reject invalid values rather
/// than truncating them into a potentially colliding identifier.
pub fn is_valid_opaque_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

fn bounded_field(mut value: String) -> String {
    if value.len() > MAX_FIELD_BYTES {
        let mut boundary = MAX_FIELD_BYTES;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
    }
    value
}

/// A playable list exposed via the MPRIS `Playlists` interface (drives the
/// "select playlist" picker). `id` is the opaque object path the backend hands
/// back to `MediaBackend::activate_playlist`; `name` is the human label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
}

impl Playlist {
    /// Admit a playlist with a bounded display name and an exact valid ID.
    pub fn admitted(mut self) -> Option<Playlist> {
        if !is_valid_opaque_id(&self.id) {
            return None;
        }
        self.name = bounded_field(self.name);
        Some(self)
    }
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
    /// Admit a queue row with bounded display fields and an exact valid ID.
    pub fn admitted(mut self) -> Option<QueueItem> {
        if !is_valid_opaque_id(&self.id) {
            return None;
        }
        self.title = bounded_field(self.title);
        self.artist = bounded_field(self.artist);
        Some(self)
    }

    /// `"Artist — Title"`, falling back to the title/id when fields are missing.
    pub fn label(&self) -> String {
        match (self.artist.trim(), self.title.trim()) {
            ("", "") => display_text(&self.id),
            ("", title) => display_text(title),
            (artist, "") => display_text(artist),
            (artist, title) => display_text_parts(&[artist, " \u{2014} ", title]),
        }
    }
}

/// A bounded queue read with the total number of rows reported by the backend.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueuePage {
    pub items: Vec<QueueItem>,
    pub total: usize,
}

impl QueuePage {
    /// Retain at most [`MAX_QUEUE_ITEMS`] rows while recording the input count.
    pub fn from_rows(mut rows: Vec<QueueItem>) -> QueuePage {
        let total = rows.len();
        rows.truncate(MAX_QUEUE_ITEMS);
        QueuePage { items: rows, total }
    }

    /// Number of backend rows omitted from this page.
    pub fn hidden(&self) -> usize {
        self.total.saturating_sub(self.items.len())
    }
}

/// A bounded playlist read with the total number of playlists the backend reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaylistPage {
    pub items: Vec<Playlist>,
    pub total: usize,
}

impl PlaylistPage {
    /// Retain at most [`MAX_PLAYLISTS`] rows while recording the input count.
    pub fn from_rows(mut rows: Vec<Playlist>) -> PlaylistPage {
        let total = rows.len();
        rows.truncate(MAX_PLAYLISTS);
        PlaylistPage { items: rows, total }
    }

    /// Number of backend rows omitted from this page.
    pub fn hidden(&self) -> usize {
        self.total.saturating_sub(self.items.len())
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
    fn queue_page_from_rows_caps_and_counts() {
        let page = QueuePage::from_rows(vec![QueueItem::default(); MAX_QUEUE_ITEMS]);
        assert_eq!(page.items.len(), MAX_QUEUE_ITEMS);
        assert_eq!(page.total, MAX_QUEUE_ITEMS);
        assert_eq!(page.hidden(), 0);

        let page = QueuePage::from_rows(vec![QueueItem::default(); MAX_QUEUE_ITEMS + 1]);
        assert_eq!(page.items.len(), MAX_QUEUE_ITEMS);
        assert_eq!(page.total, MAX_QUEUE_ITEMS + 1);
        assert_eq!(page.hidden(), 1);

        let page = QueuePage::from_rows(vec![QueueItem::default(); MAX_QUEUE_ITEMS + 3]);
        assert_eq!(page.items.len(), MAX_QUEUE_ITEMS);
        assert_eq!(page.total, MAX_QUEUE_ITEMS + 3);
        assert_eq!(page.hidden(), 3);

        let page = QueuePage::from_rows(vec![QueueItem::default(); 2]);
        assert_eq!(page.total, 2);
        assert_eq!(page.hidden(), 0);
    }

    #[test]
    fn playlist_page_from_rows_caps_and_counts() {
        let row = |index| Playlist {
            id: format!("playlist-{index}"),
            name: format!("Playlist {index}"),
        };
        let page = PlaylistPage::from_rows((0..MAX_PLAYLISTS).map(row).collect());
        assert_eq!(page.items.len(), MAX_PLAYLISTS);
        assert_eq!(page.total, MAX_PLAYLISTS);
        assert_eq!(page.hidden(), 0);

        let page = PlaylistPage::from_rows((0..MAX_PLAYLISTS + 3).map(row).collect());
        assert_eq!(page.items.len(), MAX_PLAYLISTS);
        assert_eq!(page.total, MAX_PLAYLISTS + 3);
        assert_eq!(page.hidden(), 3);

        let page = PlaylistPage::from_rows((0..2).map(row).collect());
        assert_eq!(page.total, 2);
        assert_eq!(page.hidden(), 0);
    }

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
        assert_eq!(LoopMode::parse_mpris("Track"), Some(LoopMode::Track));
        assert_eq!(LoopMode::parse_mpris("Playlist"), Some(LoopMode::Playlist));
        assert_eq!(LoopMode::parse_mpris("None"), Some(LoopMode::None));
        assert_eq!(LoopMode::parse_mpris(" none "), Some(LoopMode::None));
        assert_eq!(LoopMode::parse_mpris("garbage"), None);
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
    fn display_text_removes_terminal_and_bidi_controls() {
        let normalized = display_text("a\x1b[31mb\nc\u{202e}d");
        assert_eq!(normalized, "a [31mb cd");
        assert!(
            !normalized
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '\u{202e}'))
        );
        let state = MediaState {
            title: "a\x1b\nb".into(),
            ..Default::default()
        };
        assert_eq!(state.now_playing(), "a  b");
    }

    #[test]
    fn display_text_bounds_long_labels() {
        let normalized = display_text(&"x".repeat(1024 * 1024));
        assert!(normalized.chars().count() <= MAX_LABEL_CHARS + 3);
        assert!(normalized.ends_with("..."));
    }

    #[test]
    fn display_text_does_not_mark_dropped_tail_as_truncated() {
        let value = format!("{}\u{200b}", "x".repeat(MAX_LABEL_CHARS));
        assert_eq!(display_text(&value), "x".repeat(MAX_LABEL_CHARS));
    }

    #[test]
    fn display_text_drops_invisible_format_characters() {
        assert_eq!(
            display_text("a\u{ad}b\u{2060}c\u{180e}d\u{34f}e\u{2063}f\u{206a}g"),
            "abcdefg"
        );
        assert_eq!(display_text("a\u{2028}b\u{2029}c"), "a b c");
    }

    #[test]
    fn display_text_caps_combining_mark_runs() {
        let zalgo = format!("e{}x", "\u{301}".repeat(200));
        let normalized = display_text(&zalgo);
        assert_eq!(
            normalized,
            format!("e{}x", "\u{301}".repeat(MAX_COMBINING_RUN))
        );
        assert_eq!(normalized.chars().count(), MAX_COMBINING_RUN + 2);
        assert!(!normalized.ends_with("..."));
        assert_eq!(MAX_COMBINING_RUN, 30);
    }

    #[test]
    fn display_text_combining_run_resets_at_each_base() {
        let marks = "\u{301}".repeat(MAX_COMBINING_RUN);
        let separate_runs = format!("a{marks}b{marks}");
        // Positive control: each base starts a new combining run; this passes without the fix.
        assert_eq!(display_text(&separate_runs), separate_runs);

        let joined_marks = format!("a{}\u{200d}{}", marks, "\u{301}");
        assert_eq!(display_text(&joined_marks), format!("a{marks}"));
    }

    #[test]
    fn display_text_does_not_mark_capped_combining_tail_as_truncated() {
        let head = format!(
            "{}e{}",
            "x".repeat(MAX_LABEL_CHARS - MAX_COMBINING_RUN - 1),
            "\u{301}".repeat(MAX_COMBINING_RUN)
        );
        let value = format!("{head}{}", "\u{301}".repeat(5));
        assert_eq!(display_text(&value), head);
    }

    #[test]
    fn queue_item_with_only_artist_has_no_dangling_separator() {
        let item = QueueItem {
            artist: "Artist".into(),
            ..Default::default()
        };
        assert_eq!(item.label(), "Artist");
        assert!(!item.label().contains('\u{2014}'));
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
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h/song.mp3?redirect=.mp4")),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://video.mp4.example/stream")),
            MediaKind::Unknown
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h/x.mp3#t=.mkv")),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("music/a.flac")),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("100% Hits.flac")),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h/watch?v=abc")),
            MediaKind::Unknown
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h/a%2Emp4")),
            MediaKind::Video
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h/a%ZZ.mp3")),
            MediaKind::Audio
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h?x=/a.mp4")),
            MediaKind::Unknown
        );
        assert_eq!(
            MediaKind::from_hints("x", None, Some("https://h#/a.mkv")),
            MediaKind::Unknown
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
        let q = QueueItem {
            artist: "A\u{202e}rtist".into(),
            title: "Track\nName".into(),
            ..Default::default()
        };
        assert_eq!(q.label(), "Artist \u{2014} Track Name");
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

    #[test]
    fn admitted_metadata_is_byte_bounded_at_utf8_boundaries() {
        let state = MediaState {
            title: format!("{}é", "a".repeat(MAX_FIELD_BYTES - 1)),
            ..Default::default()
        }
        .admitted();
        assert!(state.title.len() <= MAX_FIELD_BYTES);
        assert!(state.title.is_char_boundary(state.title.len()));
        assert_eq!(state.title, "a".repeat(MAX_FIELD_BYTES - 1));

        let large = MediaState {
            title: "x".repeat(1024 * 1024),
            ..Default::default()
        }
        .admitted();
        assert_eq!(large.title.len(), MAX_FIELD_BYTES);

        let oversized_art_url = MediaState {
            art_url: Some("u".repeat(MAX_FIELD_BYTES + 1)),
            ..Default::default()
        }
        .admitted();
        assert!(oversized_art_url.art_url.is_none());
    }

    #[test]
    fn opaque_ids_are_exact_or_rejected() {
        let valid = "i".repeat(MAX_ID_BYTES);
        let state = MediaState {
            track_id: Some(valid.clone()),
            ..Default::default()
        }
        .admitted();
        assert_eq!(state.track_id.as_deref(), Some(valid.as_str()));

        let oversized = MediaState {
            track_id: Some("i".repeat(MAX_ID_BYTES + 1)),
            ..Default::default()
        }
        .admitted();
        assert!(oversized.track_id.is_none());
        assert!(!is_valid_opaque_id(""));
        assert!(!is_valid_opaque_id("a\nb"));
        assert!(!is_valid_opaque_id("a\u{1b}b"));
    }
}
