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

pub(crate) static MEDIA_DELIVERY_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

type MediaOpTail = Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>;

static MEDIA_OP_TAIL: LazyLock<MediaOpTail> = LazyLock::new(|| Arc::new(Mutex::new(None)));
static MEDIA_OP_PENDING: LazyLock<Arc<AtomicUsize>> =
    LazyLock::new(|| Arc::new(AtomicUsize::new(0)));
static MEDIA_PICK_IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static MEDIA_QUEUE_IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static MEDIA_SOURCES_IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static MEDIA_OP_OUTCOME: Mutex<Option<MediaOpOutcome>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaOpOutcome {
    Stale,
    Refused(&'static str),
    Unsupported,
    Failed,
    TimedOut,
    Busy,
}

impl MediaOpOutcome {
    pub(crate) fn status_text(self) -> &'static str {
        match self {
            Self::Stale => "Media operation skipped because the player changed",
            Self::Refused("shuffle") => "Media operation needs known shuffle state",
            Self::Refused("loop") => "Media operation needs known loop state",
            Self::Refused(_) => "Media operation was refused",
            Self::Unsupported => "This player does not support that control",
            Self::Failed => "Media operation failed",
            Self::TimedOut => "Media operation timed out",
            Self::Busy => "Media controls are busy",
        }
    }
}

fn record_outcome(slot: &Mutex<Option<MediaOpOutcome>>, outcome: MediaOpOutcome) {
    *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(outcome);
}

fn record_refusal(slot: &Mutex<Option<MediaOpOutcome>>, field: &'static str) {
    let outcome = MediaOpOutcome::Refused(field);
    record_outcome(slot, outcome);
}

fn take_outcome(slot: &Mutex<Option<MediaOpOutcome>>) -> Option<MediaOpOutcome> {
    slot.lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}

fn publish_outcome(outcome: MediaOpOutcome, waker: &TerminalWaker) {
    record_outcome(&MEDIA_OP_OUTCOME, outcome);
    if let Err(error) = waker.wake() {
        tracing::debug!(target: "thegn::media", %error, "media operation outcome wake failed");
    }
}

fn pulse(waker: &TerminalWaker) {
    if let Err(error) = waker.wake() {
        tracing::debug!(target: "thegn::media", %error, "media operation outcome wake failed");
    }
}

fn generation_is_current(
    captured: u64,
    generation: &MediaGeneration,
    slot: &Mutex<Option<MediaOpOutcome>>,
) -> bool {
    if generation.current() == captured {
        true
    } else {
        record_outcome(slot, MediaOpOutcome::Stale);
        false
    }
}

pub(crate) fn take_op_outcome() -> Option<MediaOpOutcome> {
    take_outcome(&MEDIA_OP_OUTCOME)
}

fn replace_in_flight(
    slot: &Mutex<Option<tokio::task::JoinHandle<()>>>,
    handle: tokio::task::JoinHandle<()>,
) {
    let previous = slot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .replace(handle);
    if let Some(previous) = previous {
        previous.abort();
    }
}

async fn within_media_budget<F, T>(future: F) -> Option<T>
where
    F: Future<Output = T>,
{
    match tokio::time::timeout(MEDIA_OP_BUDGET, future).await {
        Ok(value) => Some(value),
        Err(_) => {
            tracing::warn!(target: "thegn::media", budget_secs = MEDIA_OP_BUDGET.as_secs(), "media fetch exceeded its time budget");
            None
        }
    }
}

struct PendingOp(Arc<AtomicUsize>);

impl Drop for PendingOp {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Queue one operation behind the current tail. The reservation bounds both
/// running and queued work, and the tail lock defines the operation order.
#[cfg(test)]
fn chain_op<F>(tail: &MediaOpTail, pending: &Arc<AtomicUsize>, cap: usize, future: F) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    chain_op_inner(tail, pending, cap, None, None, future)
}

fn chain_op_with_waker<F>(
    tail: &MediaOpTail,
    pending: &Arc<AtomicUsize>,
    cap: usize,
    waker: TerminalWaker,
    future: F,
) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    chain_op_inner(
        tail,
        pending,
        cap,
        Some(waker),
        Some(&MEDIA_OP_OUTCOME),
        future,
    )
}

#[cfg(test)]
fn chain_op_with_outcome<F>(
    tail: &MediaOpTail,
    pending: &Arc<AtomicUsize>,
    cap: usize,
    outcome: &Mutex<Option<MediaOpOutcome>>,
    future: F,
) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    chain_op_inner(tail, pending, cap, None, Some(outcome), future)
}

fn chain_op_inner<F>(
    tail: &MediaOpTail,
    pending: &Arc<AtomicUsize>,
    cap: usize,
    timeout_waker: Option<TerminalWaker>,
    busy_outcome: Option<&Mutex<Option<MediaOpOutcome>>>,
    future: F,
) -> bool
where
    F: Future<Output = ()> + Send + 'static,
{
    if pending
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < cap).then_some(count + 1)
        })
        .is_err()
    {
        if let Some(slot) = busy_outcome {
            record_outcome(slot, MediaOpOutcome::Busy);
        }
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
            record_outcome(&MEDIA_OP_OUTCOME, MediaOpOutcome::TimedOut);
            if let Some(waker) = timeout_waker {
                pulse(&waker);
            }
        }
    }));
    true
}

async fn snapshot_or_log(
    client: &thegn_media::MediaClient,
    operation: &'static str,
) -> Result<Option<thegn_core::media::MediaSnapshot>, ()> {
    match client.snapshot_with_caps().await {
        Ok(snapshot) => Ok(snapshot),
        Err(e) => {
            tracing::debug!(target: "thegn::media", error = %e, operation, "media snapshot failed after operation");
            Err(())
        }
    }
}

fn delivery_from_read(
    generation: u64,
    read: Result<Option<thegn_core::media::MediaSnapshot>, ()>,
) -> Option<crate::media_watch::MediaSnapshotDelivery> {
    read.ok()
        .map(|snapshot| crate::media_watch::MediaSnapshotDelivery {
            generation,
            snapshot,
        })
}

fn send_delivery(
    tx: &crate::media_watch::SnapshotSender,
    delivery: crate::media_watch::MediaSnapshotDelivery,
) -> bool {
    match crate::media_watch::offer(tx, delivery) {
        Ok(true) => {
            MEDIA_DELIVERY_EPOCH.fetch_add(1, Ordering::AcqRel);
            true
        }
        Ok(false) => false,
        Err(()) => {
            tracing::debug!(target: "thegn::media", "media snapshot receiver closed");
            false
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

fn op_supported(
    op: &MediaOp,
    caps: &thegn_core::media::MediaCaps,
    state: Option<&thegn_core::media::MediaState>,
) -> bool {
    match op {
        MediaOp::ShuffleToggle => caps.shuffle,
        MediaOp::LoopCycle => caps.loop_mode,
        MediaOp::VolumeUp | MediaOp::VolumeDown => caps.volume,
        MediaOp::SeekForward | MediaOp::SeekBack => {
            caps.seek && state.is_none_or(|state| state.can_seek)
        }
        MediaOp::PlayQueueItem(_) => caps.queue,
        MediaOp::ChapterNext | MediaOp::ChapterPrev => caps.chapters,
        MediaOp::FullscreenToggle => caps.fullscreen,
        MediaOp::PlayPause | MediaOp::Next | MediaOp::Previous => true,
    }
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
    tx: crate::media_watch::SnapshotSender,
    waker: TerminalWaker,
    previous: Option<tokio::task::JoinHandle<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    // Body lives in `media_watch`; it resolves the backend, streams snapshots,
    // self-heals, and respawns on stream end.
    crate::media_watch::spawn(cfg, generation, tx, waker, previous)
}

/// Abort any running watcher and (re)spawn one for `cfg`. Called at startup, on
/// config reload (handles enable/disable live), and on a player-override change.
pub(crate) fn restart_media_watch(
    handle: &mut Option<tokio::task::JoinHandle<()>>,
    cfg: thegn_core::config::MediaConfig,
    generation: &MediaGeneration,
    tx: &crate::media_watch::SnapshotSender,
    waker: &TerminalWaker,
) {
    let generation_value = generation.advance();
    let previous = handle.take();
    if !cfg.enabled {
        *handle = previous.map(|previous| {
            tokio::spawn(async move {
                crate::media_watch::abort_and_join(Some(previous)).await;
            })
        });
        // No watcher will run, so nothing would ever push the clearing `None`:
        // push it here, or the last ▶ badge / Media section stick for the rest
        // of the session after `[media] enabled = false` (and activating the
        // badge opened a popup for a stale track).
        if send_delivery(
            tx,
            crate::media_watch::MediaSnapshotDelivery {
                generation: generation_value,
                snapshot: None,
            },
        ) && let Err(error) = waker.wake()
        {
            tracing::debug!(target: "thegn::media", %error, "media clear wake failed");
        }
        return;
    }
    *handle = spawn_media_watch(cfg, generation_value, tx.clone(), waker.clone(), previous);
}

/// Fire a transport op off-thread, then push the resulting snapshot so the badge/
/// panel update immediately (the signal watcher would also catch it).
pub(crate) fn spawn_media_op(
    cfg: thegn_core::config::MediaConfig,
    op: MediaOp,
    generation: &MediaGeneration,
    tx: crate::media_watch::SnapshotSender,
    waker: TerminalWaker,
) {
    let captured_generation = generation.current();
    let generation_guard = generation.clone();
    let outcome_waker = waker.clone();
    let overload_waker = waker.clone();
    let accepted = chain_op_with_waker(
        &MEDIA_OP_TAIL,
        &MEDIA_OP_PENDING,
        MAX_PENDING_MEDIA_OPS,
        waker.clone(),
        async move {
            if !generation_is_current(captured_generation, &generation_guard, &MEDIA_OP_OUTCOME) {
                pulse(&outcome_waker);
                return;
            }
            let Some(client) = crate::media_secret::client_for(&cfg).await else {
                publish_outcome(MediaOpOutcome::Failed, &outcome_waker);
                return;
            };
            let cur = snapshot_or_log(&client, "preflight").await.unwrap_or(None);
            let cur_state = cur.as_ref().map(|snapshot| &snapshot.state);
            if !generation_is_current(captured_generation, &generation_guard, &MEDIA_OP_OUTCOME) {
                pulse(&outcome_waker);
                return;
            }
            let caps = cur
                .as_ref()
                .map(|snapshot| snapshot.caps)
                .unwrap_or_else(|| client.caps());
            if !op_supported(&op, &caps, cur_state) {
                tracing::debug!(target: "thegn::media", op = ?op, "media operation is unsupported by the selected player");
                publish_outcome(MediaOpOutcome::Unsupported, &outcome_waker);
                return;
            }
            let res = match op {
                MediaOp::PlayPause => Some(client.play_pause().await),
                MediaOp::Next => Some(client.next().await),
                MediaOp::Previous => Some(client.previous().await),
                MediaOp::ShuffleToggle => match shuffle_target(cur.as_ref()) {
                    Ok(on) => Some(client.set_shuffle(on).await),
                    Err(error) => {
                        tracing::warn!(target: "thegn::media", outcome = ?error, "media shuffle toggle preflight failed");
                        record_refusal(&MEDIA_OP_OUTCOME, "shuffle");
                        pulse(&outcome_waker);
                        None
                    }
                },
                MediaOp::LoopCycle => match loop_target(cur.as_ref()) {
                    Ok(next) => Some(client.set_loop(next).await),
                    Err(error) => {
                        tracing::warn!(target: "thegn::media", outcome = ?error, "media loop cycle preflight failed");
                        record_refusal(&MEDIA_OP_OUTCOME, "loop");
                        pulse(&outcome_waker);
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
                publish_outcome(MediaOpOutcome::Failed, &outcome_waker);
            }
            if let Some(delivery) = delivery_from_read(
                captured_generation,
                snapshot_or_log(&client, "transport").await,
            ) && send_delivery(&tx, delivery)
                && let Err(error) = waker.wake()
            {
                tracing::debug!(target: "thegn::media", %error, "media transport wake failed");
            }
        },
    );
    if !accepted {
        tracing::debug!(target: "thegn::media", "media operation dropped because the pending limit was reached");
        pulse(&overload_waker);
    }
}

/// Map a panel hit to the same off-loop operation used by keyboard actions.
/// Keeping this conversion here means the panel never learns provider details.
pub(crate) fn spawn_media_action(
    cfg: thegn_core::config::MediaConfig,
    action: crate::panel::MediaAction,
    generation: &MediaGeneration,
    tx: crate::media_watch::SnapshotSender,
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
    tx: crate::media_watch::SnapshotSender,
    waker: TerminalWaker,
) {
    let captured_generation = generation.current();
    let generation_guard = generation.clone();
    let outcome_waker = waker.clone();
    let overload_waker = waker.clone();
    let accepted = chain_op_with_waker(
        &MEDIA_OP_TAIL,
        &MEDIA_OP_PENDING,
        MAX_PENDING_MEDIA_OPS,
        waker.clone(),
        async move {
            if !generation_is_current(captured_generation, &generation_guard, &MEDIA_OP_OUTCOME) {
                pulse(&outcome_waker);
                return;
            }
            if let Some(client) = crate::media_secret::client_for(&cfg).await {
                if !generation_is_current(captured_generation, &generation_guard, &MEDIA_OP_OUTCOME)
                {
                    pulse(&outcome_waker);
                    return;
                }
                if !client.caps().playlists {
                    tracing::debug!(target: "thegn::media", playlist = %id, "playlist activation is unsupported by the selected player");
                    publish_outcome(MediaOpOutcome::Unsupported, &outcome_waker);
                    return;
                }
                if let Err(e) = client.activate_playlist(&id).await {
                    tracing::warn!(target: "thegn::media", error = %e, "playlist {id} activation failed");
                    publish_outcome(MediaOpOutcome::Failed, &outcome_waker);
                }
                if let Some(delivery) = delivery_from_read(
                    captured_generation,
                    snapshot_or_log(&client, "playlist").await,
                ) && send_delivery(&tx, delivery)
                    && let Err(error) = waker.wake()
                {
                    tracing::debug!(target: "thegn::media", %error, "media playlist wake failed");
                }
            } else {
                publish_outcome(MediaOpOutcome::Failed, &outcome_waker);
            }
        },
    );
    if !accepted {
        tracing::debug!(target: "thegn::media", "playlist activation dropped because the pending limit was reached");
        pulse(&overload_waker);
    }
}

/// Fetch the playlist / player list off-thread for the secondary picker.
pub(crate) fn spawn_media_pick(
    cfg: thegn_core::config::MediaConfig,
    players: bool,
    tx: tokio_mpsc::UnboundedSender<MediaPick>,
    waker: TerminalWaker,
) {
    let handle = tokio::spawn(async move {
        let Some(client) = crate::media_secret::client_for(&cfg).await else {
            return;
        };
        let pick = within_media_budget(async {
            if players {
                MediaPick::Players(client.players().await)
            } else {
                MediaPick::Playlists(client.playlists().await.unwrap_or_default())
            }
        })
        .await;
        let Some(pick) = pick else { return };
        let _ = tx.send(pick); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
    replace_in_flight(&MEDIA_PICK_IN_FLIGHT, handle);
}

/// Fetch the play queue / up-next list off-thread for the docked Media panel.
pub(crate) fn spawn_media_queue(
    cfg: thegn_core::config::MediaConfig,
    request: MediaRequest,
    tx: tokio_mpsc::UnboundedSender<MediaQueueDelivery>,
    waker: TerminalWaker,
) {
    let handle = tokio::spawn(async move {
        let queue = within_media_budget(async {
            match crate::media_secret::client_for(&cfg).await {
                Some(client) => match client.queue().await {
                    Ok(queue) => Ok(queue),
                    Err(e) => {
                        tracing::debug!(target: "thegn::media", error = %e, "media queue fetch failed");
                        Err(e.to_string())
                    }
                },
                None => Err("media provider unavailable".to_string()),
            }
        })
        .await
        .unwrap_or_else(|| Err("media queue fetch timed out".to_string()));
        let _ = tx.send(MediaQueueDelivery { request, queue }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
    replace_in_flight(&MEDIA_QUEUE_IN_FLIGHT, handle);
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
    let handle = tokio::spawn(async move {
        let Some(client) = crate::media_secret::client_for(&cfg).await else {
            return;
        };
        let Some(sources) = within_media_budget(client.players()).await else {
            return;
        };
        let _ = tx.send(MediaSourcesDelivery { request, sources });
        let _ = waker.wake();
    });
    replace_in_flight(&MEDIA_SOURCES_IN_FLIGHT, handle);
}

#[cfg(test)]
mod tests {
    use super::{
        MediaGeneration, MediaOp, MediaOpOutcome, MediaPreflightError, chain_op,
        chain_op_with_outcome, delivery_from_read, generation_is_current, loop_target,
        op_supported, record_refusal, replace_in_flight, shuffle_target, take_outcome,
        within_media_budget,
    };
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use thegn_core::media::{LoopMode, MediaCaps, MediaSnapshot, MediaState};

    #[test]
    fn capability_gated_operations_follow_player_caps() {
        let gated: [(MediaOp, fn(&mut MediaCaps)); 10] = [
            (MediaOp::ShuffleToggle, |caps| caps.shuffle = true),
            (MediaOp::LoopCycle, |caps| caps.loop_mode = true),
            (MediaOp::VolumeUp, |caps| caps.volume = true),
            (MediaOp::VolumeDown, |caps| caps.volume = true),
            (MediaOp::SeekForward, |caps| caps.seek = true),
            (MediaOp::SeekBack, |caps| caps.seek = true),
            (MediaOp::PlayQueueItem("queue-id".into()), |caps| {
                caps.queue = true
            }),
            (MediaOp::ChapterNext, |caps| caps.chapters = true),
            (MediaOp::ChapterPrev, |caps| caps.chapters = true),
            (MediaOp::FullscreenToggle, |caps| caps.fullscreen = true),
        ];
        let seekable = MediaState {
            can_seek: true,
            ..MediaState::default()
        };
        for (op, enable) in gated {
            assert!(
                !op_supported(&op, &MediaCaps::default(), Some(&seekable)),
                "{op:?}"
            );
            let mut caps = MediaCaps::default();
            enable(&mut caps);
            assert!(op_supported(&op, &caps, Some(&seekable)), "{op:?}");
        }
        let seek_caps = MediaCaps {
            seek: true,
            ..MediaCaps::default()
        };
        assert!(!op_supported(
            &MediaOp::SeekForward,
            &seek_caps,
            Some(&MediaState::default())
        ));
        for op in [MediaOp::PlayPause, MediaOp::Next, MediaOp::Previous] {
            assert!(op_supported(&op, &MediaCaps::default(), None));
        }
    }

    #[test]
    fn unsupported_outcome_has_distinct_user_text() {
        assert!(!MediaOpOutcome::Unsupported.status_text().is_empty());
        assert_ne!(
            MediaOpOutcome::Unsupported.status_text(),
            MediaOpOutcome::Refused("shuffle").status_text()
        );
    }

    #[test]
    fn failed_post_operation_read_produces_no_delivery() {
        assert!(delivery_from_read(3, Err(())).is_none());
        assert_eq!(
            delivery_from_read(3, Ok(None)).unwrap().snapshot,
            None,
            "a successful empty read remains an authoritative clear"
        );
    }

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

    #[test]
    fn stale_generation_is_reported_before_the_effect_can_run() {
        let generation = MediaGeneration::new();
        let captured = generation.current();
        generation.advance();
        let outcome = Mutex::new(None);
        let mut effect_ran = false;
        if generation_is_current(captured, &generation, &outcome) {
            effect_ran = true;
        }
        assert!(!effect_ran);
        assert_eq!(take_outcome(&outcome), Some(MediaOpOutcome::Stale));
    }

    #[test]
    fn refused_preflight_is_available_as_a_user_outcome() {
        let outcome = Mutex::new(None);
        record_refusal(&outcome, "shuffle");
        assert_eq!(
            take_outcome(&outcome),
            Some(MediaOpOutcome::Refused("shuffle"))
        );
    }

    #[tokio::test]
    async fn full_queue_records_busy_outcome() {
        let tail = Arc::new(Mutex::new(None));
        let pending = Arc::new(AtomicUsize::new(0));
        let outcome = Mutex::new(None);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        assert!(chain_op_with_outcome(
            &tail,
            &pending,
            1,
            &outcome,
            async move {
                release_rx.await.unwrap();
            }
        ));
        assert!(!chain_op_with_outcome(
            &tail,
            &pending,
            1,
            &outcome,
            async {}
        ));
        assert_eq!(take_outcome(&outcome), Some(MediaOpOutcome::Busy));
        release_tx.send(()).unwrap();
        let last = tail.lock().unwrap().take().unwrap();
        last.await.unwrap();
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

    #[tokio::test]
    async fn replacing_a_fetch_slot_aborts_the_previous_task() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let slot = Mutex::new(None);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_dropped = dropped.clone();
        let old = tokio::spawn(async move {
            let _guard = DropFlag(task_dropped);
            std::future::pending::<()>().await;
        });
        replace_in_flight(&slot, old);
        tokio::task::yield_now().await;
        replace_in_flight(&slot, tokio::spawn(async {}));
        let current = slot.lock().unwrap().take().unwrap();
        current.await.unwrap();
        assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test(start_paused = true)]
    async fn media_fetch_budget_resolves_pending_work_as_timeout() {
        let outcome = within_media_budget(std::future::pending::<()>()).await;
        assert!(outcome.is_none(), "pending provider work must time out");
    }
}
