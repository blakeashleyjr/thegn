//! Media-control glue (optional `[media]` feature): the transport-op enum, the
//! picker result, and the off-thread spawners the event loop calls. Split out of
//! the ratcheted `run.rs` god-file; the loop `use`s these by their bare names.

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

/// A media transport op dispatched from a keybind / palette row / panel control.
#[derive(Debug, Clone)]
pub(crate) enum MediaOp {
    PlayPause,
    Next,
    Previous,
    ShuffleToggle,
    LoopCycle,
    VolumeUp,
    VolumeDown,
    /// Seek within the track; the step is derived from the loaded media kind
    /// (audio vs video) and `[media] seek_step*`.
    SeekForward,
    SeekBack,
    /// Jump to an absolute position (scrubber release).
    SetPosition(std::time::Duration),
    /// Set an absolute volume percent (volume slider).
    SetVolume(u8),
    /// Jump to a queue entry by its opaque id.
    PlayQueueItem(String),
    ChapterNext,
    ChapterPrev,
    FullscreenToggle,
}

impl From<crate::media_overlay::OverlayOp> for MediaOp {
    fn from(op: crate::media_overlay::OverlayOp) -> MediaOp {
        use crate::media_overlay::OverlayOp;
        match op {
            OverlayOp::PlayPause => MediaOp::PlayPause,
            OverlayOp::Next => MediaOp::Next,
            OverlayOp::Previous => MediaOp::Previous,
            OverlayOp::SeekForward => MediaOp::SeekForward,
            OverlayOp::SeekBack => MediaOp::SeekBack,
            OverlayOp::SetPosition(p) => MediaOp::SetPosition(p),
            OverlayOp::Shuffle => MediaOp::ShuffleToggle,
            OverlayOp::Loop => MediaOp::LoopCycle,
            OverlayOp::SetVolume(v) => MediaOp::SetVolume(v),
            OverlayOp::ChapterNext => MediaOp::ChapterNext,
            OverlayOp::ChapterPrev => MediaOp::ChapterPrev,
            OverlayOp::Fullscreen => MediaOp::FullscreenToggle,
            OverlayOp::PlayQueue(id) => MediaOp::PlayQueueItem(id),
        }
    }
}

/// An async result that opens a secondary media picker palette.
pub(crate) enum MediaPick {
    Playlists(Vec<superzej_core::media::Playlist>),
    Players(Vec<String>),
}

/// The effective media config: the configured `[media]` with the runtime player
/// override (the "Select player" pick) floated to the front of the priority list.
pub(crate) fn media_effective_cfg(
    base: &superzej_core::config::MediaConfig,
    player_override: &Option<String>,
) -> superzej_core::config::MediaConfig {
    let mut cfg = base.clone();
    if let Some(p) = player_override {
        cfg.players_priority.retain(|x| x != p);
        cfg.players_priority.insert(0, p.clone());
    }
    cfg
}

/// Spawn the now-playing watcher: a push-signal stream on the native MPRIS path,
/// else a slow poll for backends without signals (mpv / playerctl). Returns the
/// task handle so the caller can abort it on a config/player change; `None` when
/// media is disabled.
fn spawn_media_watch(
    cfg: superzej_core::config::MediaConfig,
    tx: tokio_mpsc::UnboundedSender<Option<superzej_core::media::MediaState>>,
    waker: TerminalWaker,
) -> Option<tokio::task::JoinHandle<()>> {
    // Body lives in `media_watch`; it resolves the backend, streams snapshots,
    // self-heals, and respawns on stream end.
    crate::media_watch::spawn(cfg, tx, waker)
}

/// Abort any running watcher and (re)spawn one for `cfg`. Called at startup, on
/// config reload (handles enable/disable live), and on a player-override change.
pub(crate) fn restart_media_watch(
    handle: &mut Option<tokio::task::JoinHandle<()>>,
    cfg: superzej_core::config::MediaConfig,
    tx: &tokio_mpsc::UnboundedSender<Option<superzej_core::media::MediaState>>,
    waker: &TerminalWaker,
) {
    if let Some(h) = handle.take() {
        h.abort();
    }
    *handle = spawn_media_watch(cfg, tx.clone(), waker.clone());
}

/// Fire a transport op off-thread, then push the resulting snapshot so the badge/
/// panel update immediately (the signal watcher would also catch it).
pub(crate) fn spawn_media_op(
    cfg: superzej_core::config::MediaConfig,
    op: MediaOp,
    tx: tokio_mpsc::UnboundedSender<Option<superzej_core::media::MediaState>>,
    waker: TerminalWaker,
) {
    use superzej_core::media::LoopMode;
    tokio::spawn(async move {
        let Some(client) = superzej_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let cur = client.snapshot().await.unwrap_or(None);
        let _ = match op {
            MediaOp::PlayPause => client.play_pause().await,
            MediaOp::Next => client.next().await,
            MediaOp::Previous => client.previous().await,
            MediaOp::ShuffleToggle => {
                let on = cur.as_ref().and_then(|s| s.shuffle).unwrap_or(false);
                client.set_shuffle(!on).await
            }
            MediaOp::LoopCycle => {
                let next = cur
                    .as_ref()
                    .and_then(|s| s.loop_mode)
                    .unwrap_or(LoopMode::None)
                    .cycle();
                client.set_loop(next).await
            }
            MediaOp::VolumeUp => client.volume_step(cfg.volume_step).await,
            MediaOp::VolumeDown => client.volume_step(-cfg.volume_step).await,
            MediaOp::SeekForward | MediaOp::SeekBack => {
                let kind = cur.as_ref().map(|s| s.kind).unwrap_or_default();
                let step = cfg.seek_step(kind);
                client.seek(step, matches!(op, MediaOp::SeekForward)).await
            }
            MediaOp::SetPosition(pos) => {
                let tid = cur.as_ref().and_then(|s| s.track_id.clone());
                client.set_position(pos, tid.as_deref()).await
            }
            MediaOp::SetVolume(level) => client.set_volume(level).await,
            MediaOp::PlayQueueItem(ref id) => client.play_queue_item(id).await,
            MediaOp::ChapterNext => client.chapter_next().await,
            MediaOp::ChapterPrev => client.chapter_prev().await,
            MediaOp::FullscreenToggle => client.toggle_fullscreen().await,
        };
        let _ = tx.send(client.snapshot().await.unwrap_or(None));
        let _ = waker.wake();
    });
}

/// Fetch the playlist / player list off-thread for the secondary picker.
pub(crate) fn spawn_media_pick(
    cfg: superzej_core::config::MediaConfig,
    players: bool,
    tx: tokio_mpsc::UnboundedSender<MediaPick>,
    waker: TerminalWaker,
) {
    tokio::spawn(async move {
        let Some(client) = superzej_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let pick = if players {
            MediaPick::Players(client.players().await)
        } else {
            MediaPick::Playlists(client.playlists().await.unwrap_or_default())
        };
        let _ = tx.send(pick);
        let _ = waker.wake();
    });
}

/// Fetch the play queue / up-next list off-thread for the Now-Playing overlay.
pub(crate) fn spawn_media_queue(
    cfg: superzej_core::config::MediaConfig,
    tx: tokio_mpsc::UnboundedSender<Vec<superzej_core::media::QueueItem>>,
    waker: TerminalWaker,
) {
    tokio::spawn(async move {
        let Some(client) = superzej_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let q = client.queue().await.unwrap_or_default();
        let _ = tx.send(q);
        let _ = waker.wake();
    });
}
