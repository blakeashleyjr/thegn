//! Cross-platform media-player control — the optional `[media]` feature's engine.
//!
//! Deliberately a **C-dep-free leaf crate** (like `thegn-metrics`): it depends
//! on nothing internal, so `cargo check --target {aarch64-apple-darwin,
//! x86_64-pc-windows-gnu}` typechecks the per-OS backends on a Linux box (see
//! `just check-cross`). The pure model lives in [`model`] and is re-exported by
//! `thegn-core`; config stays in core and is lowered into [`ResolveOpts`] so
//! this crate never needs to see `MediaConfig`.
//!
//! A [`MediaBackend`] normalizes now-playing + transport control onto the
//! [`model`] types, with per-OS impls that degrade gracefully ("a gap is slower
//! or unavailable, never broken"):
//!
//! - `platform::linux::mpris` (Linux) — the D-Bus standard (`org.mpris.MediaPlayer2`), native
//!   via `zbus`, with a push **signal watcher** (the ~0%-idle contract).
//! - `platform::linux::mpris_cli` (Linux) — the `playerctl` CLI fallback when the session bus
//!   can't be opened.
//! - [`mpv`] (Unix) — a single mpv instance over its JSON IPC socket.
//! - `smtc` (Windows) — the System Media Transport Controls session manager,
//!   with a push event watcher.
//! - `applescript` (macOS) — `osascript` driving Music.app + Spotify (no
//!   entitlement, every macOS version; Apple gates system-wide MediaRemote read
//!   on 15.4+).
//!
//! [`MediaBackend`] is **object-safe** — its methods return [`BoxFuture`]s
//! (the `ControlApi` house pattern), so the resolved backend is simply a
//! `Box<dyn MediaBackend>` ([`MediaClient`]): no hand-written delegation
//! router. The single-method push watcher ([`MediaWatch`]) is likewise a
//! boxed trait object.

pub mod model;

pub mod aggregate;
#[cfg(target_os = "macos")]
pub mod applescript;
#[cfg(target_os = "macos")]
pub mod mediaremote;
pub mod mpd;
mod mpd_parse;
pub mod mpv;
pub mod platform;
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

use futures::future::BoxFuture;

use model::{LoopMode, MediaState, Playlist, QueueItem};

pub use mpv::MpvIpc;
#[cfg(target_os = "linux")]
pub use platform::linux::mpris::{MprisWatch, MprisZbus};
#[cfg(target_os = "linux")]
pub use platform::linux::mpris_cli::MprisCli;

/// What went wrong talking to a player. Callers treat every variant as "show
/// nothing / no-op" — a missing player or absent tool is never a hard error.
/// Hand-rolled `Display` (no `thiserror` dep), mirroring `thegn_core::ci::CiError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaError {
    /// No player is currently present on the bus / socket.
    NoPlayer,
    /// The backend's transport (D-Bus, the mpv socket, the `playerctl` binary,
    /// the SMTC session manager, `osascript`) could not be reached.
    Unavailable(String),
    /// The selected backend does not expose an optional operation.
    Unsupported(String),
    /// The player rejected the request or returned something unparseable.
    Backend(String),
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::NoPlayer => f.write_str("no media player available"),
            MediaError::Unavailable(m) => write!(f, "media backend unavailable: {m}"),
            MediaError::Unsupported(m) => write!(f, "media operation unsupported: {m}"),
            MediaError::Backend(m) => write!(f, "media backend error: {m}"),
        }
    }
}

impl std::error::Error for MediaError {}

/// Per-backend capabilities — lets the UI hide controls a backend can't do
/// (e.g. `playerctl`/mpv have no MPRIS Playlists; SMTC has no volume). Mirrors
/// `ci::CiCaps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

/// A now-playing snapshot together with the capabilities of the backend that
/// produced it. Keeping these values together prevents a later backend switch
/// (or an aggregate's active-child change) from making the panel infer
/// capabilities from incidental track metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaSnapshot {
    pub state: MediaState,
    pub caps: MediaCaps,
}

/// A media-control backend for one player protocol. Read (`snapshot`) first;
/// the mutations mirror MPRIS's `Player` interface. Methods take `&self` so a
/// caller can hold one connection.
///
/// Methods return [`BoxFuture`]s (not native `async fn`) so the trait stays
/// **object-safe** and the resolved backend can be driven as a
/// `Box<dyn MediaBackend>` ([`MediaClient`]) — the `ControlApi` house pattern.
/// Impls wrap their bodies in `Box::pin(async move { … })`.
pub trait MediaBackend: Send + Sync {
    /// The current now-playing snapshot, or `None` when nothing is loaded.
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>>;

    /// Read the current snapshot and its provider capabilities as one host
    /// delivery. Backends keep the normalized read operation above; the
    /// default is sufficient because [`MediaBackend::caps`] is synchronous and
    /// the aggregate updates its active child during `snapshot`.
    fn snapshot_with_caps(&self) -> BoxFuture<'_, Result<Option<MediaSnapshot>, MediaError>> {
        Box::pin(async move {
            self.snapshot().await.map(|state| {
                state.map(|state| MediaSnapshot {
                    state,
                    caps: self.caps(),
                })
            })
        })
    }

    /// Toggle play/pause.
    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>>;
    /// Skip to the next track.
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>>;
    /// Return to the previous track.
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>>;
    /// Set shuffle on/off.
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>>;
    /// Set the repeat mode.
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>>;
    /// Nudge volume by `delta` (e.g. +0.05), clamped to `0.0..=1.0`.
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>>;

    /// Playlists exposed via the MPRIS `Playlists` interface (empty when the
    /// backend doesn't support it — gate on [`MediaCaps::playlists`]).
    fn playlists(&self) -> BoxFuture<'_, Result<Vec<Playlist>, MediaError>>;
    /// Activate a playlist by its opaque id (an MPRIS object path).
    fn activate_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), MediaError>>;

    /// Seek by `offset` relative to the current position, `forward` or back
    /// (MPRIS `Seek(±µs)`, mpv relative `seek`). Default: unsupported no-op error
    /// — override + set [`MediaCaps::seek`] where the backend can seek.
    fn seek(&self, _offset: Duration, _forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("seek".into())) })
    }
    /// Jump to an absolute `pos` (MPRIS `SetPosition(trackid, µs)`, mpv absolute
    /// `seek`). `track_id` is the current [`MediaState::track_id`] when the
    /// backend needs it. Default: unsupported.
    fn set_position<'a>(
        &'a self,
        _pos: Duration,
        _track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("set_position".into())) })
    }
    /// Set an absolute volume `level` in `0..=100`. Default: unsupported —
    /// override for exact control and set [`MediaCaps::abs_volume`].
    fn set_volume(&self, _level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("set_volume".into())) })
    }

    /// The play queue / up-next list, where the backend exposes one (MPRIS
    /// `TrackList`, mpv `playlist`). Empty by default — gate on
    /// [`MediaCaps::queue`].
    fn queue(&self) -> BoxFuture<'_, Result<Vec<QueueItem>, MediaError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    /// Jump to a queue entry by its opaque [`QueueItem::id`]. Default: unsupported.
    fn play_queue_item<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("play_queue_item".into())) })
    }

    /// Next chapter (mpv `add chapter 1`; players exposing chapters). Default:
    /// unsupported — gate on [`MediaCaps::chapters`].
    fn chapter_next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("chapter_next".into())) })
    }
    /// Previous chapter. Default: unsupported.
    fn chapter_prev(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("chapter_prev".into())) })
    }

    /// Toggle player fullscreen (mpv `cycle fullscreen`, MPRIS root `Fullscreen`).
    /// Self-contained (reads current state where needed) so the UI holds no
    /// fullscreen state. Default: unsupported — gate on [`MediaCaps::fullscreen`].
    fn toggle_fullscreen(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async { Err(MediaError::Unsupported("toggle_fullscreen".into())) })
    }

    fn caps(&self) -> MediaCaps;

    /// A push-signal watcher when the backend supports one (native MPRIS, MPD
    /// `idle`, SMTC, the MediaRemote adapter). `None` ⇒ the host falls back to
    /// the `[media] poll_interval_secs` ticker (the mpv / playerctl /
    /// AppleScript backends all poll for now).
    fn watch(&self) -> BoxFuture<'_, Option<Box<dyn MediaWatch + Send>>> {
        Box::pin(async { None })
    }

    /// List the controllable players (bus-name tails) for the picker.
    fn players(&self) -> BoxFuture<'_, Vec<String>>;
}

/// A push-change stream for backends that have one (native MPRIS D-Bus signals,
/// MPD `idle`, the Windows SMTC session events). Like [`MediaBackend`], a boxed
/// trait object — a single-method poll loop, uniform across platforms (the
/// per-signal `Box` alloc is irrelevant next to a D-Bus/IPC round-trip).
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
    /// Reserved Spotify Web API/library provider. Desktop control remains
    /// available through the existing platform integrations.
    Spotify,
    /// Reserved Jellyfin provider.
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

/// The resolved media backend for this session: a boxed [`MediaBackend`] trait
/// object (native MPRIS, `playerctl`, mpv, MPD, SMTC, AppleScript, MediaRemote,
/// or the multi-source [`aggregate::Aggregate`]).
pub type MediaClient = Box<dyn MediaBackend>;

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
        BackendKind::Spotify => {
            tracing::debug!(target: "thegn::media", "spotify backend reserved; use MPRIS/SMTC/AppleScript for desktop control");
            None
        }
        BackendKind::Jellyfin => {
            tracing::debug!(target: "thegn::media", "jellyfin backend not implemented yet");
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
        let _ = opts; // best-effort: non-Result discard — opts unused on this platform
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
        sources.push(Box::new(MpvIpc::new(opts.mpv_socket.clone())));
    }
    match sources.len() {
        0 => None,
        1 => sources.pop(),
        _ => Some(Box::new(aggregate::Aggregate::new(
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
        tracing::debug!(target: "thegn::media", "media backend: MediaRemote adapter");
        Box::new(m) as MediaClient
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
                    tracing::debug!(target: "thegn::media", "media backend: native MPRIS (zbus)");
                    Some(Box::new(m))
                }
                probe => {
                    let players = m.list_players().await.unwrap_or_default();
                    if !players.is_empty() && MprisCli::available() {
                        tracing::debug!(
                            target: "thegn::media",
                            ?probe, players = ?players,
                            "native MPRIS read yielded no track despite players present; degrading to playerctl",
                        );
                        Some(Box::new(MprisCli::new(opts.players_priority.clone())))
                    } else {
                        // No player on the bus yet — keep the native push path so
                        // the badge appears the instant one shows up.
                        tracing::debug!(target: "thegn::media", "media backend: native MPRIS (zbus), no player yet");
                        Some(Box::new(m))
                    }
                }
            }
        }
        Err(e) => {
            tracing::debug!(target: "thegn::media", error = %e, "MPRIS zbus connect failed; trying playerctl");
            if MprisCli::available() {
                Some(Box::new(MprisCli::new(opts.players_priority.clone())))
            } else {
                tracing::debug!(target: "thegn::media", "playerctl not found; media inert");
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
    Some(Box::new(MpvIpc::new(opts.mpv_socket.clone())))
}

/// Build the native MPD backend, probing that the daemon actually answers so a
/// dead endpoint doesn't sit in the aggregator. `None` when unreachable.
async fn mpd_client(opts: &ResolveOpts) -> Option<MediaClient> {
    let endpoint = mpd::MpdEndpoint::resolve(&opts.mpd_socket, opts.mpd_password.clone());
    match mpd::Mpd::connect(endpoint).await {
        Ok(m) => {
            tracing::debug!(target: "thegn::media", "media backend: native MPD");
            Some(Box::new(m))
        }
        Err(e) => {
            tracing::debug!(target: "thegn::media", error = %e, "MPD unreachable; skipping");
            None
        }
    }
}

#[cfg(windows)]
async fn smtc_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    smtc::Smtc::connect()
        .await
        .map(|s| Box::new(s) as MediaClient)
}
#[cfg(not(windows))]
async fn smtc_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    None
}

#[cfg(target_os = "macos")]
fn applescript_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    Some(Box::new(applescript::AppleScript::new()))
}
#[cfg(not(target_os = "macos"))]
fn applescript_client(_opts: &ResolveOpts) -> Option<MediaClient> {
    None
}

#[cfg(test)]
mod platform_ratchet_tests;
#[cfg(test)]
mod ratchet;
