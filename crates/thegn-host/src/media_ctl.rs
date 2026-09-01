//! Media-control glue (optional `[media]` feature): the transport-op enum, the
//! picker result, and the off-thread spawners the event loop calls. Split out of
//! the ratcheted `run.rs` god-file; the loop `use`s these by their bare names.

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use crate::panel::media::{MediaQueueDelivery, MediaRequest, MediaSourcesDelivery};

/// Identifies one live media watcher/config/player selection. Snapshot
/// producers capture the value before doing any async work so a result from an
/// operation that began before a restart can never replace the new selection.
#[derive(Clone, Debug)]
pub(crate) struct MediaGeneration(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl MediaGeneration {
    pub(crate) fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)))
    }

    pub(crate) fn current(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    fn advance(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1
    }
}

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
    /// Jump to a queue entry by its opaque id.
    PlayQueueItem(String),
    ChapterNext,
    ChapterPrev,
    FullscreenToggle,
}

/// An async result that opens a secondary media picker palette.
pub(crate) enum MediaPick {
    Playlists(Vec<thegn_core::media::Playlist>),
    Players(Vec<String>),
}

/// The effective media config: the configured `[media]` with the runtime player
/// override (the "Select player" pick) floated to the front of the priority list.
pub(crate) fn media_effective_cfg(
    base: &thegn_core::config::MediaConfig,
    player_override: &Option<String>,
) -> thegn_core::config::MediaConfig {
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
    cfg: thegn_core::config::MediaConfig,
    generation: u64,
    tx: tokio_mpsc::UnboundedSender<crate::media_watch::MediaSnapshotDelivery>,
    waker: TerminalWaker,
) -> Option<tokio::task::JoinHandle<()>> {
    // Body lives in `media_watch`; it resolves the backend, streams snapshots,
    // self-heals, and respawns on stream end.
    crate::media_watch::spawn(cfg, generation, tx, waker)
}

/// Abort any running watcher and (re)spawn one for `cfg`. Called at startup, on
/// config reload (handles enable/disable live), and on a player-override change.
pub(crate) fn restart_media_watch(
    handle: &mut Option<tokio::task::JoinHandle<()>>,
    cfg: thegn_core::config::MediaConfig,
    generation: &MediaGeneration,
    tx: &tokio_mpsc::UnboundedSender<crate::media_watch::MediaSnapshotDelivery>,
    waker: &TerminalWaker,
) {
    let generation_value = generation.advance();
    if let Some(h) = handle.take() {
        h.abort();
    }
    if !cfg.enabled {
        // No watcher will run, so nothing would ever push the clearing `None`:
        // push it here, or the last ▶ badge / Media section stick for the rest
        // of the session after `[media] enabled = false` (and activating the
        // badge opened a popup for a stale track).
        if tx
            .send(crate::media_watch::MediaSnapshotDelivery {
                generation: generation_value,
                snapshot: None,
            })
            .is_ok()
        {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
        return;
    }
    *handle = spawn_media_watch(cfg, generation_value, tx.clone(), waker.clone());
}

/// Fire a transport op off-thread, then push the resulting snapshot so the badge/
/// panel update immediately (the signal watcher would also catch it).
pub(crate) fn spawn_media_op(
    cfg: thegn_core::config::MediaConfig,
    op: MediaOp,
    generation: &MediaGeneration,
    tx: tokio_mpsc::UnboundedSender<crate::media_watch::MediaSnapshotDelivery>,
    waker: TerminalWaker,
) {
    use thegn_core::media::LoopMode;
    let generation = generation.current();
    tokio::spawn(async move {
        let Some(client) = thegn_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let cur = client.snapshot_with_caps().await.unwrap_or(None);
        let cur_state = cur.as_ref().map(|snapshot| &snapshot.state);
        let res = match op {
            MediaOp::PlayPause => client.play_pause().await,
            MediaOp::Next => client.next().await,
            MediaOp::Previous => client.previous().await,
            MediaOp::ShuffleToggle => {
                let on = cur_state.and_then(|s| s.shuffle).unwrap_or(false);
                client.set_shuffle(!on).await
            }
            MediaOp::LoopCycle => {
                let next = cur_state
                    .and_then(|s| s.loop_mode)
                    .unwrap_or(LoopMode::None)
                    .cycle();
                client.set_loop(next).await
            }
            MediaOp::VolumeUp => client.volume_step(cfg.volume_step).await,
            MediaOp::VolumeDown => client.volume_step(-cfg.volume_step).await,
            MediaOp::SeekForward | MediaOp::SeekBack => {
                let kind = cur_state.map(|s| s.kind).unwrap_or_default();
                let step = cfg.seek_step(kind);
                client.seek(step, matches!(op, MediaOp::SeekForward)).await
            }
            MediaOp::PlayQueueItem(ref id) => client.play_queue_item(id).await,
            MediaOp::ChapterNext => client.chapter_next().await,
            MediaOp::ChapterPrev => client.chapter_prev().await,
            MediaOp::FullscreenToggle => client.toggle_fullscreen().await,
        };
        if let Err(e) = res {
            tracing::warn!(target: "thegn::media", error = %e, "media op {op:?} failed");
        }
        let _ = tx.send(crate::media_watch::MediaSnapshotDelivery {
            generation,
            snapshot: client.snapshot_with_caps().await.unwrap_or(None),
        }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

/// Map a panel hit to the same off-loop operation used by keyboard actions.
/// Keeping this conversion here means the panel never learns provider details.
pub(crate) fn spawn_media_action(
    cfg: thegn_core::config::MediaConfig,
    action: crate::panel::MediaAction,
    generation: &MediaGeneration,
    tx: tokio_mpsc::UnboundedSender<crate::media_watch::MediaSnapshotDelivery>,
    waker: TerminalWaker,
) {
    let op = match action {
        crate::panel::MediaAction::PlayPause => MediaOp::PlayPause,
        crate::panel::MediaAction::Next => MediaOp::Next,
        crate::panel::MediaAction::Previous => MediaOp::Previous,
        crate::panel::MediaAction::Shuffle => MediaOp::ShuffleToggle,
        crate::panel::MediaAction::Loop => MediaOp::LoopCycle,
        crate::panel::MediaAction::VolumeUp => MediaOp::VolumeUp,
        crate::panel::MediaAction::VolumeDown => MediaOp::VolumeDown,
        crate::panel::MediaAction::SeekForward => MediaOp::SeekForward,
        crate::panel::MediaAction::SeekBack => MediaOp::SeekBack,
        crate::panel::MediaAction::ChapterNext => MediaOp::ChapterNext,
        crate::panel::MediaAction::ChapterPrev => MediaOp::ChapterPrev,
        crate::panel::MediaAction::Fullscreen => MediaOp::FullscreenToggle,
    };
    spawn_media_op(cfg, op, generation, tx, waker);
}

/// Activate a playlist off-loop and publish its result under the generation
/// that was current when the request was made.
pub(crate) fn spawn_media_playlist(
    cfg: thegn_core::config::MediaConfig,
    id: String,
    generation: &MediaGeneration,
    tx: tokio_mpsc::UnboundedSender<crate::media_watch::MediaSnapshotDelivery>,
    waker: TerminalWaker,
) {
    let generation = generation.current();
    tokio::spawn(async move {
        if let Some(client) = thegn_media::client_for(&cfg.resolve_opts()).await {
            if let Err(e) = client.activate_playlist(&id).await {
                tracing::warn!(target: "thegn::media", error = %e, "playlist {id} activation failed");
            }
            let _ = tx.send(crate::media_watch::MediaSnapshotDelivery {
                generation,
                snapshot: client.snapshot_with_caps().await.unwrap_or(None),
            }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// Fetch the playlist / player list off-thread for the secondary picker.
pub(crate) fn spawn_media_pick(
    cfg: thegn_core::config::MediaConfig,
    players: bool,
    tx: tokio_mpsc::UnboundedSender<MediaPick>,
    waker: TerminalWaker,
) {
    tokio::spawn(async move {
        let Some(client) = thegn_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let pick = if players {
            MediaPick::Players(client.players().await)
        } else {
            MediaPick::Playlists(client.playlists().await.unwrap_or_default())
        };
        let _ = tx.send(pick); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

/// Fetch the play queue / up-next list off-thread for the docked Media panel.
pub(crate) fn spawn_media_queue(
    cfg: thegn_core::config::MediaConfig,
    request: MediaRequest,
    tx: tokio_mpsc::UnboundedSender<MediaQueueDelivery>,
    waker: TerminalWaker,
) {
    tokio::spawn(async move {
        let Some(client) = thegn_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let q = client.queue().await.unwrap_or_default();
        let _ = tx.send(MediaQueueDelivery { request, queue: q }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
}

/// Fetch the provider's source/player list off-loop for the docked panel.
/// Request tagging lets the panel reject a response from an obsolete source
/// selection or snapshot generation.
pub(crate) fn spawn_media_sources(
    cfg: thegn_core::config::MediaConfig,
    request: MediaRequest,
    tx: tokio_mpsc::UnboundedSender<MediaSourcesDelivery>,
    waker: TerminalWaker,
) {
    tokio::spawn(async move {
        let Some(client) = thegn_media::client_for(&cfg.resolve_opts()).await else {
            return;
        };
        let sources = client.players().await;
        let _ = tx.send(MediaSourcesDelivery { request, sources });
        let _ = waker.wake();
    });
}
