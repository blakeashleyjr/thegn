//! Media-player control seam (optional `[media]` feature). The async sibling of
//! [`crate::ci`]: a [`MediaBackend`] trait normalizing now-playing snapshots +
//! transport control + playlist selection onto superzej-core's
//! [`superzej_core::media`] model, with per-backend impls that degrade
//! native→CLI just like the git/gh seams ("a gap is slower or unavailable,
//! never broken").
//!
//! - [`mpris`] — the Linux D-Bus standard (`org.mpris.MediaPlayer2`), native via
//!   `zbus`. Covers Spotify desktop, mpv (mpris plugin), ncspot, spotify-player,
//!   musikcube, moc, VLC, cmus, … and exposes a push **signal watcher** so the
//!   host updates on `PropertiesChanged` without polling (the ~0%-idle contract).
//! - [`mpris_cli`] — the `playerctl` CLI fallback, used when the native D-Bus
//!   path can't connect. `playerctl --follow` gives a child-stdout push stream.
//! - [`mpv`] — a single mpv instance over its JSON IPC socket.
//!
//! Like [`crate::ci`], the object-unsafe `async fn` trait is driven through a
//! static-dispatch router ([`MediaClient`]), never made into a `dyn`.

pub mod mpris;
pub mod mpris_cli;
pub mod mpv;

use superzej_core::config::{MediaBackendKind, MediaConfig};
use superzej_core::media::{LoopMode, MediaState, Playlist};

pub use mpris::{MprisWatch, MprisZbus};
pub use mpris_cli::MprisCli;
pub use mpv::MpvIpc;

/// What went wrong talking to a player. Callers treat every variant as "show
/// nothing / no-op" — a missing player or absent tool is never a hard error.
/// Hand-rolled `Display` (no `thiserror` dep), mirroring [`superzej_core::ci::CiError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// No player is currently present on the bus / socket.
    NoPlayer,
    /// The backend's transport (D-Bus, the mpv socket, the `playerctl` binary)
    /// could not be reached.
    Unavailable(String),
    /// The player rejected the request or returned something unparseable.
    Backend(String),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::NoPlayer => f.write_str("no media player available"),
            MediaError::Unavailable(m) => write!(f, "media backend unavailable: {m}"),
            MediaError::Backend(m) => write!(f, "media backend error: {m}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// Per-backend capabilities — lets the UI hide controls a backend can't do
/// (e.g. `playerctl`/mpv have no MPRIS Playlists). Mirrors `ci::CiCaps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaCaps {
    pub shuffle: bool,
    pub loop_mode: bool,
    pub volume: bool,
    pub playlists: bool,
    /// Whether the backend offers a push-signal stream (no polling needed).
    pub signals: bool,
}

/// A media-control backend for one player protocol. Read (`snapshot`) first;
/// the mutations mirror MPRIS's `Player` interface. Methods take `&self` so the
/// router can hold one connection.
#[allow(async_fn_in_trait)]
pub trait MediaBackend: Send + Sync {
    /// The current now-playing snapshot, or `None` when nothing is loaded.
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError>;

    /// Toggle play/pause.
    async fn play_pause(&self) -> Result<(), MediaError>;
    /// Skip to the next track.
    async fn next(&self) -> Result<(), MediaError>;
    /// Return to the previous track.
    async fn previous(&self) -> Result<(), MediaError>;
    /// Set shuffle on/off.
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError>;
    /// Set the repeat mode.
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError>;
    /// Nudge volume by `delta` (e.g. +0.05), clamped to `0.0..=1.0`.
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError>;

    /// Playlists exposed via the MPRIS `Playlists` interface (empty when the
    /// backend doesn't support it — gate on [`MediaCaps::playlists`]).
    async fn playlists(&self) -> Result<Vec<Playlist>, MediaError>;
    /// Activate a playlist by its opaque id (an MPRIS object path).
    async fn activate_playlist(&self, id: &str) -> Result<(), MediaError>;

    fn caps(&self) -> MediaCaps;
}

// === backend selection =====================================================

/// The concrete, statically-dispatched media backend chosen for this session.
pub enum MediaClient {
    /// Native MPRIS over D-Bus (preferred).
    Mpris(MprisZbus),
    /// `playerctl` CLI fallback.
    MprisCli(MprisCli),
    /// mpv JSON IPC.
    Mpv(MpvIpc),
}

/// Resolve the media backend from `[media]` config. `None` when the feature is
/// disabled, the backend is `none`, or the chosen transport can't be reached
/// (the caller then shows nothing — the feature is silently inert).
///
/// For `backend = "mpris"` this prefers the native `zbus` path and falls back to
/// `playerctl` when a session bus can't be opened.
pub async fn client_for(cfg: &MediaConfig) -> Option<MediaClient> {
    if !cfg.enabled {
        return None;
    }
    match cfg.backend {
        MediaBackendKind::None => None,
        MediaBackendKind::Mpris => match MprisZbus::connect(cfg.players_priority.clone()).await {
            Ok(m) => Some(MediaClient::Mpris(m)),
            Err(e) => {
                tracing::debug!(target: "szhost::media", error = %e, "MPRIS zbus connect failed; trying playerctl");
                if MprisCli::available() {
                    Some(MediaClient::MprisCli(MprisCli::new(
                        cfg.players_priority.clone(),
                    )))
                } else {
                    tracing::debug!(target: "szhost::media", "playerctl not found; media inert");
                    None
                }
            }
        },
        MediaBackendKind::Mpv => Some(MediaClient::Mpv(MpvIpc::new(cfg.mpv.socket.clone()))),
        MediaBackendKind::Jellyfin => {
            tracing::debug!(target: "szhost::media", "jellyfin backend not implemented yet");
            None
        }
    }
}

/// Delegate every [`MediaBackend`] method to the active variant. Keeping this on
/// the router (not a `dyn`) preserves static dispatch across the async trait.
impl MediaClient {
    pub async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        match self {
            MediaClient::Mpris(b) => b.snapshot().await,
            MediaClient::MprisCli(b) => b.snapshot().await,
            MediaClient::Mpv(b) => b.snapshot().await,
        }
    }
    pub async fn play_pause(&self) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.play_pause().await,
            MediaClient::MprisCli(b) => b.play_pause().await,
            MediaClient::Mpv(b) => b.play_pause().await,
        }
    }
    pub async fn next(&self) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.next().await,
            MediaClient::MprisCli(b) => b.next().await,
            MediaClient::Mpv(b) => b.next().await,
        }
    }
    pub async fn previous(&self) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.previous().await,
            MediaClient::MprisCli(b) => b.previous().await,
            MediaClient::Mpv(b) => b.previous().await,
        }
    }
    pub async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.set_shuffle(on).await,
            MediaClient::MprisCli(b) => b.set_shuffle(on).await,
            MediaClient::Mpv(b) => b.set_shuffle(on).await,
        }
    }
    pub async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.set_loop(mode).await,
            MediaClient::MprisCli(b) => b.set_loop(mode).await,
            MediaClient::Mpv(b) => b.set_loop(mode).await,
        }
    }
    pub async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.volume_step(delta).await,
            MediaClient::MprisCli(b) => b.volume_step(delta).await,
            MediaClient::Mpv(b) => b.volume_step(delta).await,
        }
    }
    pub async fn playlists(&self) -> Result<Vec<Playlist>, MediaError> {
        match self {
            MediaClient::Mpris(b) => b.playlists().await,
            MediaClient::MprisCli(b) => b.playlists().await,
            MediaClient::Mpv(b) => b.playlists().await,
        }
    }
    pub async fn activate_playlist(&self, id: &str) -> Result<(), MediaError> {
        match self {
            MediaClient::Mpris(b) => b.activate_playlist(id).await,
            MediaClient::MprisCli(b) => b.activate_playlist(id).await,
            MediaClient::Mpv(b) => b.activate_playlist(id).await,
        }
    }
    pub fn caps(&self) -> MediaCaps {
        match self {
            MediaClient::Mpris(b) => b.caps(),
            MediaClient::MprisCli(b) => b.caps(),
            MediaClient::Mpv(b) => b.caps(),
        }
    }

    /// A push-signal watcher when the backend supports one (native MPRIS only).
    /// `None` ⇒ the host falls back to the `[media] poll_interval_secs` ticker.
    pub async fn watch(&self) -> Option<MprisWatch> {
        match self {
            MediaClient::Mpris(b) => b.watch().await.ok(),
            _ => None,
        }
    }

    /// List the controllable players (MPRIS bus-name tails) for the picker.
    pub async fn players(&self) -> Vec<String> {
        match self {
            MediaClient::Mpris(b) => b.list_players().await.unwrap_or_default(),
            MediaClient::MprisCli(b) => b.list_players().await.unwrap_or_default(),
            MediaClient::Mpv(_) => vec!["mpv".to_string()],
        }
    }
}
