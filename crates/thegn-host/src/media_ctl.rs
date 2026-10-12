//! Media-control glue (optional `[media]` feature): the transport-op enum, the
//! picker result, and the off-thread spawners the event loop calls. Split out of
//! the ratcheted `run.rs` god-file; the loop `use`s these by their bare names.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;

use crate::panel::media::{MediaQueueDelivery, MediaRequest, MediaSourcesDelivery};

const MAX_PENDING_MEDIA_OPS: usize = 8;
const MEDIA_OP_BUDGET: Duration = Duration::from_secs(5);

type MediaOpTail = Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>;

static MEDIA_OP_TAIL: LazyLock<MediaOpTail> = LazyLock::new(|| Arc::new(Mutex::new(None)));
static MEDIA_OP_PENDING: LazyLock<Arc<AtomicUsize>> =
    LazyLock::new(|| Arc::new(AtomicUsize::new(0)));

struct PendingOp(Arc<AtomicUsize>);

impl Drop for PendingOp {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Queue one operation behind the current tail. The reservation bounds both
/// running and queued work, and the tail lock defines the operation order.
fn chain_op<F>(tail: &MediaOpTail, pending: &Arc<AtomicUsize>, cap: usize, future: F) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    if pending
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < cap).then_some(count + 1)
        })
        .is_err()
    {
        return false;
    }

    let mut tail = tail.lock().unwrap_or_else(|error| error.into_inner());
    let previous = tail.take();
    let pending_guard = PendingOp(pending.clone());
    *tail = Some(tokio::spawn(async move {
        let _pending = pending_guard;
        if let Some(previous) = previous {
            match previous.await {
                Ok(()) | Err(_) => {}
            }
        }
        if tokio::time::timeout(MEDIA_OP_BUDGET, future).await.is_err() {
            tracing::warn!(target: "thegn::media", budget_secs = MEDIA_OP_BUDGET.as_secs(), "media operation exceeded its time budget");
        }
    }));
    true
}

async fn snapshot_or_log(
    client: &thegn_media::MediaClient,
    operation: &'static str,
) -> Option<thegn_core::media::MediaSnapshot> {
    match client.snapshot_with_caps().await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            tracing::debug!(target: "thegn::media", error = %e, operation, "media snapshot failed after operation");
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaPreflightError {
    StateUnknown(&'static str),
}

fn shuffle_target(
    snapshot: Option<&thegn_core::media::MediaSnapshot>,
) -> Result<bool, MediaPreflightError> {
    snapshot
        .and_then(|snapshot| snapshot.state.shuffle)
        .map(|on| !on)
        .ok_or(MediaPreflightError::StateUnknown("shuffle"))
}

fn loop_target(
    snapshot: Option<&thegn_core::media::MediaSnapshot>,
) -> Result<thegn_core::media::LoopMode, MediaPreflightError> {
    snapshot
        .and_then(|snapshot| snapshot.state.loop_mode)
        .map(thegn_core::media::LoopMode::cycle)
        .ok_or(MediaPreflightError::StateUnknown("loop"))
}

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
    let generation = generation.current();
    let accepted = chain_op(
        &MEDIA_OP_TAIL,
        &MEDIA_OP_PENDING,
        MAX_PENDING_MEDIA_OPS,
        async move {
            let Some(client) = crate::media_secret::client_for(&cfg).await else {
                return;
            };
            let cur = snapshot_or_log(&client, "preflight").await;
            let cur_state = cur.as_ref().map(|snapshot| &snapshot.state);
            let res = match op {
                MediaOp::PlayPause => Some(client.play_pause().await),
                MediaOp::Next => Some(client.next().await),
                MediaOp::Previous => Some(client.previous().await),
                MediaOp::ShuffleToggle => match shuffle_target(cur.as_ref()) {
                    Ok(on) => Some(client.set_shuffle(on).await),
                    Err(error) => {
                        tracing::warn!(target: "thegn::media", outcome = ?error, "media shuffle toggle preflight failed");
                        None
                    }
                },
                MediaOp::LoopCycle => match loop_target(cur.as_ref()) {
                    Ok(next) => Some(client.set_loop(next).await),
                    Err(error) => {
                        tracing::warn!(target: "thegn::media", outcome = ?error, "media loop cycle preflight failed");
                        None
                    }
                },
                MediaOp::VolumeUp => Some(client.volume_step(cfg.volume_step).await),
                MediaOp::VolumeDown => Some(client.volume_step(-cfg.volume_step).await),
                MediaOp::SeekForward | MediaOp::SeekBack => {
                    let kind = cur_state.map(|s| s.kind).unwrap_or_default();
                    let step = cfg.seek_step(kind);
                    Some(client.seek(step, matches!(op, MediaOp::SeekForward)).await)
                }
                MediaOp::PlayQueueItem(ref id) => Some(client.play_queue_item(id).await),
                MediaOp::ChapterNext => Some(client.chapter_next().await),
                MediaOp::ChapterPrev => Some(client.chapter_prev().await),
                MediaOp::FullscreenToggle => Some(client.toggle_fullscreen().await),
            };
            if let Some(Err(e)) = res {
                tracing::warn!(target: "thegn::media", error = %e, "media op {op:?} failed");
            }
            let _ = tx.send(crate::media_watch::MediaSnapshotDelivery {
                generation,
                snapshot: snapshot_or_log(&client, "transport").await,
            }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        },
    );
    if !accepted {
        tracing::debug!(target: "thegn::media", "media operation dropped because the pending limit was reached");
    }
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
        if let Some(client) = crate::media_secret::client_for(&cfg).await {
            if let Err(e) = client.activate_playlist(&id).await {
                tracing::warn!(target: "thegn::media", error = %e, "playlist {id} activation failed");
            }
            let _ = tx.send(crate::media_watch::MediaSnapshotDelivery {
                generation,
                snapshot: snapshot_or_log(&client, "playlist").await,
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
        let Some(client) = crate::media_secret::client_for(&cfg).await else {
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
        let queue = match crate::media_secret::client_for(&cfg).await {
            Some(client) => match client.queue().await {
                Ok(queue) => Ok(queue),
                Err(e) => {
                    tracing::debug!(target: "thegn::media", error = %e, "media queue fetch failed");
                    Err(e.to_string())
                }
            },
            None => Err("media provider unavailable".to_string()),
        };
        let _ = tx.send(MediaQueueDelivery { request, queue }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
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
        let Some(client) = crate::media_secret::client_for(&cfg).await else {
            return;
        };
        let sources = client.players().await;
        let _ = tx.send(MediaSourcesDelivery { request, sources });
        let _ = waker.wake();
    });
}

#[cfg(test)]
mod tests {
    use super::{MediaPreflightError, chain_op, loop_target, shuffle_target};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use thegn_core::media::{LoopMode, MediaSnapshot, MediaState};

    #[test]
    fn shuffle_toggle_fails_closed_when_state_is_unknown() {
        assert_eq!(
            shuffle_target(None),
            Err(MediaPreflightError::StateUnknown("shuffle"))
        );
        assert_eq!(
            shuffle_target(Some(&MediaSnapshot {
                state: MediaState::default(),
                caps: Default::default(),
            })),
            Err(MediaPreflightError::StateUnknown("shuffle"))
        );
    }

    #[test]
    fn loop_cycle_fails_closed_when_state_is_unknown() {
        assert_eq!(
            loop_target(None),
            Err(MediaPreflightError::StateUnknown("loop"))
        );
        assert_eq!(
            loop_target(Some(&MediaSnapshot {
                state: MediaState::default(),
                caps: Default::default(),
            })),
            Err(MediaPreflightError::StateUnknown("loop"))
        );
    }

    #[test]
    fn known_toggle_states_produce_their_next_values() {
        let mut state = MediaState {
            shuffle: Some(true),
            loop_mode: Some(LoopMode::Playlist),
            ..MediaState::default()
        };
        let snapshot = MediaSnapshot {
            state: state.clone(),
            caps: Default::default(),
        };
        assert_eq!(shuffle_target(Some(&snapshot)), Ok(false));
        assert_eq!(loop_target(Some(&snapshot)), Ok(LoopMode::Track));

        state.shuffle = Some(false);
        state.loop_mode = Some(LoopMode::None);
        let snapshot = MediaSnapshot {
            state,
            caps: Default::default(),
        };
        assert_eq!(shuffle_target(Some(&snapshot)), Ok(true));
        assert_eq!(loop_target(Some(&snapshot)), Ok(LoopMode::Playlist));
    }

    #[tokio::test]
    async fn transport_operations_run_in_enqueue_order() {
        let tail = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicUsize::new(0));
        let records = Arc::new(Mutex::new(Vec::new()));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let first_records = records.clone();
        assert!(chain_op(&tail, &pending, 8, async move {
            release_rx.await.unwrap();
            first_records.lock().unwrap().push(1);
        }));
        let second_records = records.clone();
        assert!(chain_op(&tail, &pending, 8, async move {
            second_records.lock().unwrap().push(2);
        }));

        release_tx.send(()).unwrap();
        let last = tail.lock().unwrap().take().unwrap();
        last.await.unwrap();
        assert_eq!(*records.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn transport_operation_queue_rejects_work_over_its_cap() {
        let tail = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        assert!(chain_op(&tail, &pending, 1, async move {
            release_rx.await.unwrap();
        }));
        assert!(!chain_op(&tail, &pending, 1, async {}));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 1);

        release_tx.send(()).unwrap();
        let last = tail.lock().unwrap().take().unwrap();
        last.await.unwrap();
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_transport_operation_does_not_block_the_queue() {
        let tail = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicUsize::new(0));
        let (second_tx, second_rx) = tokio::sync::oneshot::channel();

        assert!(chain_op(&tail, &pending, 8, async {
            std::future::pending::<()>().await;
        }));
        assert!(chain_op(&tail, &pending, 8, async move {
            assert!(second_tx.send(()).is_ok());
        }));

        let last = tail.lock().unwrap().take().unwrap();
        last.await.unwrap();
        second_rx.await.unwrap();
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }
}
