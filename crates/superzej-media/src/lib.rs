//! Cross-platform media-player control — the optional `[media]` feature's engine.
//!
//! Deliberately a **C-dep-free leaf crate** (like `superzej-metrics`): it depends
//! on nothing internal, so `cargo check --target {aarch64-apple-darwin,
//! x86_64-pc-windows-gnu}` typechecks the per-OS backends on a Linux box (see
//! `just check-cross`). The pure model lives in [`model`] and is re-exported by
//! `superzej-core`; config stays in core and is lowered into [`ResolveOpts`] so
//! this crate never needs to see `MediaConfig`.
//!
//! A [`MediaBackend`] normalizes now-playing + transport control onto the
//! [`model`] types, with per-OS impls that degrade gracefully ("a gap is slower
//! or unavailable, never broken"):
//!
//! - [`mpris`] (Linux) — the D-Bus standard (`org.mpris.MediaPlayer2`), native
//!   via `zbus`, with a push **signal watcher** (the ~0%-idle contract).
//! - [`mpris_cli`] (Linux) — the `playerctl` CLI fallback when the session bus
//!   can't be opened.
//! - [`mpv`] (Unix) — a single mpv instance over its JSON IPC socket.
//! - `smtc` (Windows) — the System Media Transport Controls session manager,
//!   with a push event watcher.
//! - `applescript` (macOS) — `osascript` driving Music.app + Spotify (no
//!   entitlement, every macOS version; Apple gates system-wide MediaRemote read
//!   on 15.4+).
//!
//! Like `superzej_core::ci`, the object-unsafe `async fn` [`MediaBackend`] trait
//! is driven through a static-dispatch router ([`MediaClient`]), never a `dyn`.
//! The single-method push watcher ([`MediaWatch`]) *is* a boxed trait object —
//! it's a trivial poll loop, so uniformity across platforms beats avoiding one
//! `Box` alloc per signal.

pub mod model;

pub mod aggregate;
#[cfg(target_os = "macos")]
pub mod applescript;
#[cfg(target_os = "macos")]
pub mod mediaremote;
pub mod mpd;
mod mpd_parse;
#[cfg(target_os = "linux")]
pub mod mpris;
#[cfg(target_os = "linux")]
pub mod mpris_cli;
pub mod mpv;
#[cfg(windows)]
pub mod smtc;
// Pure per-OS decoders, split out so they're unit-tested on Linux without the
// `windows`/osascript deps (the real backend uses them; `test` compiles them
// into the Linux test bin).
#[cfg(any(target_os = "macos", test))]
mod applescript_parse;
#[cfg(any(target_os = "macos", test))]
mod mediaremote_parse;
#[cfg(any(windows, test))]
mod smtc_decode;

use std::future::Future;
use std::pin::Pin;

use std::time::Duration;

use model::{LoopMode, MediaState, Playlist, QueueItem};

#[cfg(target_os = "linux")]
pub use mpris::{MprisWatch, MprisZbus};
#[cfg(target_os = "linux")]
pub use mpris_cli::MprisCli;
pub use mpv::MpvIpc;

/// What went wrong talking to a player. Callers treat every variant as "show
/// nothing / no-op" — a missing player or absent tool is never a hard error.
/// Hand-rolled `Display` (no `thiserror` dep), mirroring `superzej_core::ci::CiError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// No player is currently present on the bus / socket.
    NoPlayer,
    /// The backend's transport (D-Bus, the mpv socket, the `playerctl` binary,
    /// the SMTC session manager, `osascript`) could not be reached.
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
/// (e.g. `playerctl`/mpv have no MPRIS Playlists; SMTC has no volume). Mirrors
/// `ci::CiCaps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaCaps {
    pub shuffle: bool,
    pub loop_mode: bool,
    pub volume: bool,
    pub playlists: bool,
    /// Whether the backend offers a push-signal stream (no polling needed).
    pub signals: bool,
    /// Relative/absolute seeking within a track (`seek`/`set_position`).
    pub seek: bool,
    /// Cover art is exposed (`MediaState::art_url` may be populated).
    pub art: bool,
    /// A play queue / up-next list is enumerable (`queue`/`play_queue_item`).
    pub queue: bool,
    /// Absolute volume can be set (`set_volume`), not just stepped.
    pub abs_volume: bool,
    /// Chapter navigation is available (`chapter_next`/`chapter_prev`).
    pub chapters: bool,
    /// A fullscreen toggle is available (`set_fullscreen`).
    pub fullscreen: bool,
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

    /// Seek by `offset` relative to the current position, `forward` or back
    /// (MPRIS `Seek(±µs)`, mpv relative `seek`). Default: unsupported no-op error
    /// — override + set [`MediaCaps::seek`] where the backend can seek.
    async fn seek(&self, offset: Duration, forward: bool) -> Result<(), MediaError> {
        let _ = (offset, forward);
        Err(MediaError::Backend("seek unsupported".into()))
    }
    /// Jump to an absolute `pos` (MPRIS `SetPosition(trackid, µs)`, mpv absolute
    /// `seek`). `track_id` is the current [`MediaState::track_id`] when the
    /// backend needs it. Default: unsupported.
    async fn set_position(&self, pos: Duration, track_id: Option<&str>) -> Result<(), MediaError> {
        let _ = (pos, track_id);
        Err(MediaError::Backend("set_position unsupported".into()))
    }
    /// Set an absolute volume `level` in `0..=100`. Default falls back to a
    /// coarse series of `volume_step`s from an unknown base — override for exact
    /// control and set [`MediaCaps::abs_volume`].
    async fn set_volume(&self, level: u8) -> Result<(), MediaError> {
        let _ = level;
        Err(MediaError::Backend("set_volume unsupported".into()))
    }

    /// The play queue / up-next list, where the backend exposes one (MPRIS
    /// `TrackList`, mpv `playlist`). Empty by default — gate on
    /// [`MediaCaps::queue`].
    async fn queue(&self) -> Result<Vec<QueueItem>, MediaError> {
        Ok(Vec::new())
    }
    /// Jump to a queue entry by its opaque [`QueueItem::id`]. Default: unsupported.
    async fn play_queue_item(&self, id: &str) -> Result<(), MediaError> {
        let _ = id;
        Err(MediaError::Backend("play_queue_item unsupported".into()))
    }

    /// Next chapter (mpv `add chapter 1`; players exposing chapters). Default:
    /// unsupported — gate on [`MediaCaps::chapters`].
    async fn chapter_next(&self) -> Result<(), MediaError> {
        Err(MediaError::Backend("chapters unsupported".into()))
    }
    /// Previous chapter. Default: unsupported.
    async fn chapter_prev(&self) -> Result<(), MediaError> {
        Err(MediaError::Backend("chapters unsupported".into()))
    }

    /// Toggle player fullscreen (mpv `cycle fullscreen`, MPRIS root `Fullscreen`).
    /// Self-contained (reads current state where needed) so the UI holds no
    /// fullscreen state. Default: unsupported — gate on [`MediaCaps::fullscreen`].
    async fn toggle_fullscreen(&self) -> Result<(), MediaError> {
        Err(MediaError::Backend("fullscreen unsupported".into()))
    }

    fn caps(&self) -> MediaCaps;
}

/// A push-change stream for backends that have one (native MPRIS D-Bus signals,
/// the Windows SMTC session events). Unlike [`MediaBackend`] — driven through the
/// static-dispatch [`MediaClient`] because its many `async fn`s aren't
/// object-safe — the watcher is a single-method poll loop, so a boxed trait
/// object is simplest and uniform across platforms (the per-signal `Box` alloc
/// is irrelevant next to a D-Bus/IPC round-trip).
pub trait MediaWatch: Send {
    /// Await the next change. `false` when the underlying stream has ended (the
    /// host then stops watching).
    fn changed(&mut self) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

// === backend selection =====================================================

/// Which control backend to resolve. The leaf-local mirror of core's
/// `MediaBackendKind` (core lowers its config into [`ResolveOpts`] so this crate
/// stays free of any core dependency).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Pick the right backend for the current OS.
    Auto,
    /// Disabled — `client_for` returns `None`.
    None,
    /// Linux MPRIS (native zbus, `playerctl` fallback).
    Mpris,
    /// mpv JSON IPC.
    Mpv,
    /// Native MPD protocol (`localhost:6600`) — covers mpd/mpc/rmpc/ncmpcpp.
    Mpd,
    /// Windows System Media Transport Controls.
    Smtc,
    /// macOS `osascript` (Music.app + Spotify).
    AppleScript,
    /// Reserved.
    Jellyfin,
}

/// The owned backend-resolution input. Core builds this from `[media]` config;
/// the leaf never sees `MediaConfig`.
#[derive(Debug, Clone)]
pub struct ResolveOpts {
    pub backend: BackendKind,
    /// Preferred players (bus-name tails); first match wins.
    pub players_priority: Vec<String>,
    /// mpv JSON-IPC socket path (only consulted for the mpv backend).
    pub mpv_socket: String,
    /// MPD endpoint: a `host:port` or an absolute unix-socket path (consulted for
    /// the MPD backend and by `auto`). Empty ⇒ MPD source is skipped.
    pub mpd_socket: String,
    /// Optional MPD password.
    pub mpd_password: Option<String>,
}

/// The concrete, statically-dispatched media backend chosen for this session.
pub enum MediaClient {
    #[cfg(target_os = "linux")]
    /// Native MPRIS over D-Bus (preferred on Linux).
    Mpris(MprisZbus),
    #[cfg(target_os = "linux")]
    /// `playerctl` CLI fallback.
    MprisCli(MprisCli),
    /// mpv JSON IPC.
    Mpv(MpvIpc),
    /// Native MPD protocol.
    Mpd(mpd::Mpd),
    #[cfg(windows)]
    /// Windows System Media Transport Controls.
    Smtc(smtc::Smtc),
    #[cfg(target_os = "macos")]
    /// macOS `osascript`.
    AppleScript(applescript::AppleScript),
    #[cfg(target_os = "macos")]
    /// macOS universal now-playing via the MediaRemote adapter.
    MediaRemote(mediaremote::MediaRemote),
    /// Several sources multiplexed into one — `auto` on Linux composes MPRIS,
    /// MPD, and mpv so whatever is actually playing wins.
    Aggregate(aggregate::Aggregate),
}

/// Resolve the media backend from lowered config. `None` when disabled, the
/// backend is `none`/unimplemented, the chosen backend isn't available on this
/// OS, or its transport can't be reached (the caller then shows nothing — the
/// feature is silently inert).
pub async fn client_for(opts: &ResolveOpts) -> Option<MediaClient> {
    match opts.backend {
        BackendKind::None => None,
        BackendKind::Auto => auto_client(opts).await,
        BackendKind::Mpris => mpris_client(opts).await,
        BackendKind::Mpv => mpv_client(opts),
        BackendKind::Mpd => mpd_client(opts).await,
        BackendKind::Smtc => smtc_client(opts).await,
        BackendKind::AppleScript => applescript_client(opts),
        BackendKind::Jellyfin => {
            tracing::debug!(target: "szhost::media", "jellyfin backend not implemented yet");
            None
        }
    }
}

/// Pick the native backend for the current OS. On Linux this composes *every*
/// reachable source (MPRIS + native MPD + a live mpv socket) into an
/// [`aggregate::Aggregate`] so anything actually playing shows up out of the box;
/// with a single source it returns that source directly. Windows/macOS keep one
/// universal backend (SMTC / MediaRemote→AppleScript).
async fn auto_client(opts: &ResolveOpts) -> Option<MediaClient> {
    #[cfg(target_os = "linux")]
    return linux_auto_client(opts).await;
    #[cfg(windows)]
    return smtc_client(opts).await;
    #[cfg(target_os = "macos")]
    return macos_auto_client(opts).await;
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = opts;
        None
    }
}

/// Compose the reachable Linux sources. MPRIS (native or `playerctl`) always
/// leads; MPD joins when the daemon answers; mpv joins only when its IPC socket
/// exists on disk (so we never poll a dead default path). One source ⇒ that
/// source alone; several ⇒ an [`aggregate::Aggregate`].
#[cfg(target_os = "linux")]
async fn linux_auto_client(opts: &ResolveOpts) -> Option<MediaClient> {
    let mut sources: Vec<MediaClient> = Vec::new();
    if let Some(c) = mpris_client(opts).await {
        sources.push(c);
    }
    if let Some(c) = mpd_client(opts).await {
        sources.push(c);
    }
    // Only add mpv when its socket is actually present, else it just fails every
    // poll. mpv-via-`mpv-mpris` still shows up through the MPRIS source above.
    if !opts.mpv_socket.is_empty() && std::path::Path::new(&opts.mpv_socket).exists() {
        sources.push(MediaClient::Mpv(MpvIpc::new(opts.mpv_socket.clone())));
    }
    match sources.len() {
        0 => None,
        1 => sources.pop(),
        _ => Some(MediaClient::Aggregate(aggregate::Aggregate::new(
            sources,
            opts.players_priority.clone(),
        ))),
    }
}

/// macOS `auto`: the universal MediaRemote adapter when present, else the
/// per-app AppleScript path.
#[cfg(target_os = "macos")]
async fn macos_auto_client(opts: &ResolveOpts) -> Option<MediaClient> {
    if let Some(c) = mediaremote::MediaRemote::connect().await.map(|m| {
        tracing::debug!(target: "szhost::media", "media backend: MediaRemote adapter");
        MediaClient::MediaRemote(m)
    }) {
        return Some(c);
    }
    applescript_client(opts)
}

#[cfg(target_os = "linux")]
async fn mpris_client(opts: &ResolveOpts) -> Option<MediaClient> {
    match MprisZbus::connect(opts.players_priority.clone()).await {
        Ok(m) => {
            // Connecting to the session bus isn't enough: the native path can
            // still fail to *read* a player — a broken proxy squatting on the
            // bus (e.g. a `playerctld` whose object doesn't exist), an
            // unexpected variant shape, a permissions quirk. Probe once; if a
            // player is present on the bus but the native read yields no track,
            // degrade to the `playerctl` CLI, which works wherever the bus does.
            match m.snapshot().await {
                Ok(Some(_)) => {
                    tracing::debug!(target: "szhost::media", "media backend: native MPRIS (zbus)");
                    Some(MediaClient::Mpris(m))
                }
                probe => {
                    let players = m.list_players().await.unwrap_or_default();
                    if !players.is_empty() && MprisCli::available() {
                        tracing::debug!(
                            target: "szhost::media",
                            ?probe, players = ?players,
                            "native MPRIS read yielded no track despite players present; degrading to playerctl",
                        );
                        Some(MediaClient::MprisCli(MprisCli::new(
                            opts.players_priority.clone(),
                        )))
                    } else {
                        // No player on the bus yet — keep the native push path so
                        // the badge appears the instant one shows up.
                        tracing::debug!(target: "szhost::media", "media backend: native MPRIS (zbus), no player yet");
                        Some(MediaClient::Mpris(m))
                    }
                }
            }
        }
        Err(e) => {
            tracing::debug!(target: "szhost::media", error = %e, "MPRIS zbus connect failed; trying playerctl");
            if MprisCli::available() {
                Some(MediaClient::MprisCli(MprisCli::new(
                    opts.players_priority.clone(),
                )))
            } else {
                tracing::debug!(target: "szhost::media", "playerctl not found; media inert");
                None
            }
        }
    }
}
#[cfg(not(target_os = "linux"))]
async fn mpris_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    None
}

fn mpv_client(opts: &ResolveOpts) -> Option<MediaClient> {
    Some(MediaClient::Mpv(MpvIpc::new(opts.mpv_socket.clone())))
}

/// Build the native MPD backend, probing that the daemon actually answers so a
/// dead endpoint doesn't sit in the aggregator. `None` when unreachable.
async fn mpd_client(opts: &ResolveOpts) -> Option<MediaClient> {
    let endpoint = mpd::MpdEndpoint::resolve(&opts.mpd_socket, opts.mpd_password.clone());
    match mpd::Mpd::connect(endpoint).await {
        Ok(m) => {
            tracing::debug!(target: "szhost::media", "media backend: native MPD");
            Some(MediaClient::Mpd(m))
        }
        Err(e) => {
            tracing::debug!(target: "szhost::media", error = %e, "MPD unreachable; skipping");
            None
        }
    }
}

#[cfg(windows)]
async fn smtc_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    smtc::Smtc::connect().await.map(MediaClient::Smtc)
}
#[cfg(not(windows))]
async fn smtc_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    None
}

#[cfg(target_os = "macos")]
fn applescript_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    Some(MediaClient::AppleScript(applescript::AppleScript::new()))
}
#[cfg(not(target_os = "macos"))]
fn applescript_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    None
}

/// Expand a uniform `MediaBackend` call across every compiled-in router variant.
macro_rules! dispatch {
    ($self:expr, $b:ident => $call:expr) => {
        match $self {
            #[cfg(target_os = "linux")]
            MediaClient::Mpris($b) => $call,
            #[cfg(target_os = "linux")]
            MediaClient::MprisCli($b) => $call,
            MediaClient::Mpv($b) => $call,
            MediaClient::Mpd($b) => $call,
            #[cfg(windows)]
            MediaClient::Smtc($b) => $call,
            #[cfg(target_os = "macos")]
            MediaClient::AppleScript($b) => $call,
            #[cfg(target_os = "macos")]
            MediaClient::MediaRemote($b) => $call,
            MediaClient::Aggregate($b) => $call,
        }
    };
}

/// Delegate every [`MediaBackend`] method to the active variant. Keeping this on
/// the router (not a `dyn`) preserves static dispatch across the async trait.
impl MediaClient {
    pub async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
        dispatch!(self, b => b.snapshot().await)
    }
    pub async fn play_pause(&self) -> Result<(), MediaError> {
        dispatch!(self, b => b.play_pause().await)
    }
    pub async fn next(&self) -> Result<(), MediaError> {
        dispatch!(self, b => b.next().await)
    }
    pub async fn previous(&self) -> Result<(), MediaError> {
        dispatch!(self, b => b.previous().await)
    }
    pub async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        dispatch!(self, b => b.set_shuffle(on).await)
    }
    pub async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        dispatch!(self, b => b.set_loop(mode).await)
    }
    pub async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        dispatch!(self, b => b.volume_step(delta).await)
    }
    pub async fn playlists(&self) -> Result<Vec<Playlist>, MediaError> {
        dispatch!(self, b => b.playlists().await)
    }
    pub async fn activate_playlist(&self, id: &str) -> Result<(), MediaError> {
        dispatch!(self, b => b.activate_playlist(id).await)
    }
    pub async fn seek(&self, offset: Duration, forward: bool) -> Result<(), MediaError> {
        dispatch!(self, b => b.seek(offset, forward).await)
    }
    pub async fn set_position(
        &self,
        pos: Duration,
        track_id: Option<&str>,
    ) -> Result<(), MediaError> {
        dispatch!(self, b => b.set_position(pos, track_id).await)
    }
    pub async fn set_volume(&self, level: u8) -> Result<(), MediaError> {
        dispatch!(self, b => b.set_volume(level).await)
    }
    pub async fn queue(&self) -> Result<Vec<QueueItem>, MediaError> {
        dispatch!(self, b => b.queue().await)
    }
    pub async fn play_queue_item(&self, id: &str) -> Result<(), MediaError> {
        dispatch!(self, b => b.play_queue_item(id).await)
    }
    pub async fn chapter_next(&self) -> Result<(), MediaError> {
        dispatch!(self, b => b.chapter_next().await)
    }
    pub async fn chapter_prev(&self) -> Result<(), MediaError> {
        dispatch!(self, b => b.chapter_prev().await)
    }
    pub async fn toggle_fullscreen(&self) -> Result<(), MediaError> {
        dispatch!(self, b => b.toggle_fullscreen().await)
    }
    pub fn caps(&self) -> MediaCaps {
        dispatch!(self, b => b.caps())
    }

    /// A push-signal watcher when the backend supports one (native MPRIS today).
    /// `None` ⇒ the host falls back to the `[media] poll_interval_secs` ticker
    /// (SMTC and the mpv/playerctl/AppleScript backends all poll for now).
    pub async fn watch(&self) -> Option<Box<dyn MediaWatch + Send>> {
        match self {
            #[cfg(target_os = "linux")]
            MediaClient::Mpris(b) => b
                .watch()
                .await
                .ok()
                .map(|w| Box::new(w) as Box<dyn MediaWatch + Send>),
            MediaClient::Mpd(b) => b
                .watch()
                .await
                .ok()
                .map(|w| Box::new(w) as Box<dyn MediaWatch + Send>),
            #[cfg(windows)]
            MediaClient::Smtc(b) => b
                .watch()
                .await
                .ok()
                .map(|w| Box::new(w) as Box<dyn MediaWatch + Send>),
            #[cfg(target_os = "macos")]
            MediaClient::MediaRemote(b) => b
                .watch()
                .await
                .ok()
                .map(|w| Box::new(w) as Box<dyn MediaWatch + Send>),
            MediaClient::Aggregate(b) => b.watch().await,
            _ => None,
        }
    }

    /// List the controllable players (bus-name tails) for the picker.
    pub async fn players(&self) -> Vec<String> {
        match self {
            #[cfg(target_os = "linux")]
            MediaClient::Mpris(b) => b.list_players().await.unwrap_or_default(),
            #[cfg(target_os = "linux")]
            MediaClient::MprisCli(b) => b.list_players().await.unwrap_or_default(),
            MediaClient::Mpv(_) => vec!["mpv".to_string()],
            MediaClient::Mpd(_) => vec!["mpd".to_string()],
            #[cfg(windows)]
            MediaClient::Smtc(b) => b.list_players().await,
            #[cfg(target_os = "macos")]
            MediaClient::AppleScript(b) => b.list_players().await,
            #[cfg(target_os = "macos")]
            MediaClient::MediaRemote(b) => b.list_players().await,
            MediaClient::Aggregate(b) => b.players().await,
        }
    }
}
