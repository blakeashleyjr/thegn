//! Media-control glue (optional `[media]` feature): the transport-op enum, the
//! picker result, and the off-thread spawners the event loop calls. Split out of
//! the ratcheted `run.rs` god-file; the loop `use`s these by their bare names.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use termwiz::terminal::TerminalWaker;

use crate::panel::media::{MediaQueueDelivery, MediaRequest, MediaSourcesDelivery, QueueItemLease};

const MAX_PENDING_MEDIA_OPS: usize = 8;
pub(crate) const MEDIA_OP_BUDGET: Duration = thegn_media::MEDIA_OP_BUDGET;

pub(crate) static MEDIA_DELIVERY_EPOCH: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[derive(Default)]
struct MediaOpChain {
    handles: Vec<tokio::task::JoinHandle<()>>,
    tail: Option<tokio::sync::oneshot::Receiver<()>>,
    drain: Option<tokio::task::JoinHandle<()>>,
}

type MediaOpTail = Arc<Mutex<MediaOpChain>>;

static MEDIA_OP_TAIL: LazyLock<MediaOpTail> =
    LazyLock::new(|| Arc::new(Mutex::new(MediaOpChain::default())));
static MEDIA_OP_PENDING: LazyLock<Arc<AtomicUsize>> =
    LazyLock::new(|| Arc::new(AtomicUsize::new(0)));
static MEDIA_PICK_IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static MEDIA_QUEUE_IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static MEDIA_SOURCES_IN_FLIGHT: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
static MEDIA_OP_OUTCOME: Mutex<Option<MediaOpOutcome>> = Mutex::new(None);

pub(crate) struct Latest<T>(Arc<Mutex<Option<T>>>);

impl<T> Clone for Latest<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Latest<T> {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub(crate) fn put(&self, value: T) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
    }

    pub(crate) fn take(&self) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

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
            Self::Refused("queue") => "Media queue item needs the current player's state",
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

    let mut chain = tail.lock().unwrap_or_else(|error| error.into_inner());
    chain.handles.retain(|handle| !handle.is_finished());
    let previous = chain.tail.take();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let pending_guard = PendingOp(pending.clone());
    let handle = tokio::spawn(async move {
        let _pending = pending_guard;
        if let Some(previous) = previous {
            match previous.await {
                Ok(()) | Err(_) => {}
            }
        }
        let _done = done_tx;
        if tokio::time::timeout(MEDIA_OP_BUDGET, future).await.is_err() {
            tracing::warn!(target: "thegn::media", budget_secs = MEDIA_OP_BUDGET.as_secs(), "media operation exceeded its time budget");
            record_outcome(&MEDIA_OP_OUTCOME, MediaOpOutcome::TimedOut);
            if let Some(waker) = timeout_waker {
                pulse(&waker);
            }
        }
        drop(_pending);
        drop(_done);
    });
    chain.handles.push(handle);
    chain.tail = Some(done_rx);
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
    /// Jump to a queue entry tied to the player that listed it.
    PlayQueueItem(QueueItemLease),
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

fn queue_lease_check(
    lease: &QueueItemLease,
    current: Option<&thegn_core::media::MediaSnapshot>,
) -> Result<(), MediaOpOutcome> {
    let Some(snapshot) = current else {
        return Err(MediaOpOutcome::Refused("queue"));
    };
    if snapshot.state.player != lease.player {
        return Err(MediaOpOutcome::Stale);
    }
    Ok(())
}

/// An async result that opens a secondary media picker palette.
pub(crate) enum MediaPick {
    Playlists(Result<Vec<thegn_core::media::Playlist>, String>),
    Players(Result<Vec<String>, String>),
}

pub(crate) fn pick_palette(pick: MediaPick) -> Result<Vec<crate::palette::PaletteItem>, String> {
    match pick {
        MediaPick::Playlists(Ok(playlists)) if playlists.is_empty() => {
            Err("Media: nothing to pick".into())
        }
        MediaPick::Players(Ok(players)) if players.is_empty() => {
            Err("Media: nothing to pick".into())
        }
        MediaPick::Playlists(Ok(playlists)) => Ok(playlists
            .into_iter()
            .map(|playlist| {
                crate::palette::PaletteItem::new(
                    format!("media-playlist:{}", playlist.id),
                    format!("\u{1f3b5} {}", playlist.name),
                )
            })
            .collect()),
        MediaPick::Players(Ok(players)) => Ok(players
            .into_iter()
            .map(|player| {
                crate::palette::PaletteItem::new(
                    format!("media-player:{player}"),
                    format!("\u{25b6} {player}"),
                )
            })
            .collect()),
        MediaPick::Playlists(Err(error)) => {
            Err(format!("Media: could not list playlists: {error}"))
        }
        MediaPick::Players(Err(error)) => Err(format!("Media: could not list players: {error}")),
    }
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
    cancel_op_chain(&MEDIA_OP_TAIL);
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

fn cancel_op_chain(tail: &MediaOpTail) {
    let mut chain = tail.lock().unwrap_or_else(|error| error.into_inner());
    if chain.handles.is_empty() {
        return;
    }
    let handles = std::mem::take(&mut chain.handles);
    for handle in &handles {
        handle.abort();
    }
    let previous_drain = chain.drain.take();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let joiner = tokio::spawn(async move {
        if let Some(previous_drain) = previous_drain {
            match previous_drain.await {
                Ok(()) | Err(_) => {}
            }
        }
        for handle in handles {
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => {
                    tracing::warn!(target: "thegn::media", %error, "media operation chain join failed");
                }
            }
        }
        let _done = done_tx;
    });
    chain.tail = Some(done_rx);
    chain.drain = Some(joiner);
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
            let client = match crate::media_secret::client_for(&cfg).await {
                Ok(client) => client,
                Err(error) => {
                    tracing::debug!(target: "thegn::media", %error, "media client unavailable");
                    publish_outcome(MediaOpOutcome::Failed, &outcome_waker);
                    return;
                }
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
                MediaOp::PlayQueueItem(ref lease) => match queue_lease_check(lease, cur.as_ref()) {
                    Ok(()) => Some(client.play_queue_item(&lease.id).await),
                    Err(outcome) => {
                        tracing::debug!(
                            target: "thegn::media",
                            outcome = ?outcome,
                            "media queue item lease refused"
                        );
                        publish_outcome(outcome, &outcome_waker);
                        None
                    }
                },
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
            match crate::media_secret::client_for(&cfg).await {
                Ok(client) => {
                    if !generation_is_current(
                        captured_generation,
                        &generation_guard,
                        &MEDIA_OP_OUTCOME,
                    ) {
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
                }
                Err(error) => {
                    tracing::debug!(target: "thegn::media", %error, "media client unavailable");
                    publish_outcome(MediaOpOutcome::Failed, &outcome_waker);
                }
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
    slot: Latest<MediaPick>,
    waker: TerminalWaker,
) {
    let handle = tokio::spawn(async move {
        let result = within_media_budget(async {
            match crate::media_secret::client_for(&cfg).await {
                Ok(client) if players => MediaPick::Players(
                    client.players().await.map_err(|error| error.to_string()),
                ),
                Ok(client) => match client.playlists().await {
                    Ok(playlists) => MediaPick::Playlists(Ok(playlists)),
                    Err(error) => {
                        tracing::debug!(target: "thegn::media", error = %error, "media playlist fetch failed");
                        MediaPick::Playlists(Err(error.to_string()))
                    }
                },
                Err(error) if players => MediaPick::Players(Err(error.to_string())),
                Err(error) => MediaPick::Playlists(Err(error.to_string())),
            }
        })
        .await;
        let pick = match result {
            Some(pick) => pick,
            None if players => MediaPick::Players(Err("timed out".into())),
            None => MediaPick::Playlists(Err("timed out".into())),
        };
        slot.put(pick);
        let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
    });
    replace_in_flight(&MEDIA_PICK_IN_FLIGHT, handle);
}

/// Fetch the play queue / up-next list off-thread for the docked Media panel.
pub(crate) fn spawn_media_queue(
    cfg: thegn_core::config::MediaConfig,
    request: MediaRequest,
    slot: Latest<MediaQueueDelivery>,
    waker: TerminalWaker,
) {
    let handle = tokio::spawn(async move {
        let queue = within_media_budget(async {
            match crate::media_secret::client_for(&cfg).await {
                Ok(client) => match client.queue_page().await {
                    Ok(queue) => Ok(queue),
                    Err(e) => {
                        tracing::debug!(target: "thegn::media", error = %e, "media queue fetch failed");
                        Err(e.to_string())
                    }
                },
                Err(error) => Err(error.to_string()),
            }
        })
        .await
        .unwrap_or_else(|| Err("media queue fetch timed out".to_string()));
        slot.put(MediaQueueDelivery { request, queue });
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
    slot: Latest<MediaSourcesDelivery>,
    waker: TerminalWaker,
) {
    let handle = tokio::spawn(async move {
        let client = match crate::media_secret::client_for(&cfg).await {
            Ok(client) => client,
            Err(error) => {
                tracing::debug!(target: "thegn::media", %error, "media sources unavailable");
                return;
            }
        };
        let sources = match within_media_budget(client.players()).await {
            Some(Ok(sources)) => sources,
            Some(Err(error)) => {
                tracing::debug!(target: "thegn::media", %error, "media source listing failed");
                return;
            }
            None => return,
        };
        slot.put(MediaSourcesDelivery { request, sources });
        let _ = waker.wake();
    });
    replace_in_flight(&MEDIA_SOURCES_IN_FLIGHT, handle);
}

#[cfg(test)]
mod tests {
    use super::{
        Latest, MediaGeneration, MediaOp, MediaOpChain, MediaOpOutcome, MediaOpTail, MediaPick,
        MediaPreflightError, cancel_op_chain, chain_op, chain_op_with_outcome, delivery_from_read,
        generation_is_current, loop_target, op_supported, pick_palette, queue_lease_check,
        record_refusal, replace_in_flight, shuffle_target, take_outcome, within_media_budget,
    };
    use crate::panel::media::QueueItemLease;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use thegn_core::media::{LoopMode, MediaCaps, MediaSnapshot, MediaState};

    async fn finish_op_chain(tail: &MediaOpTail) {
        let (handles, drain) = {
            let mut chain = tail.lock().unwrap_or_else(|error| error.into_inner());
            (std::mem::take(&mut chain.handles), chain.drain.take())
        };
        for handle in handles {
            match handle.await {
                Ok(()) | Err(_) => {}
            }
        }
        if let Some(drain) = drain {
            match drain.await {
                Ok(()) | Err(_) => {}
            }
        }
    }
    type GatedOperation = (MediaOp, fn(&mut MediaCaps));

    #[test]
    fn pick_palette_distinguishes_error_from_empty() {
        assert!(matches!(
            pick_palette(MediaPick::Playlists(Err("boom".into()))),
            Err(text) if text.contains("could not list playlists") && text.contains("boom")
        ));
        assert!(matches!(
            pick_palette(MediaPick::Playlists(Ok(Vec::new()))),
            Err(text) if text == "Media: nothing to pick"
        ));
        assert!(matches!(
            pick_palette(MediaPick::Players(Err("boom".into()))),
            Err(text) if text.contains("could not list players") && text.contains("boom")
        ));
        assert!(matches!(
            pick_palette(MediaPick::Players(Ok(Vec::new()))),
            Err(text) if text == "Media: nothing to pick"
        ));

        let playlists = pick_palette(MediaPick::Playlists(Ok(vec![
            thegn_core::media::Playlist {
                id: "playlist-id".into(),
                name: "Favorites".into(),
            },
        ])))
        .unwrap();
        assert_eq!(playlists.len(), 1);
        assert_eq!(playlists[0].key, "media-playlist:playlist-id");

        let items = pick_palette(MediaPick::Players(Ok(vec!["mpv".into()]))).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "media-player:mpv");
    }

    #[test]
    fn latest_mailbox_keeps_only_the_newest_value() {
        let mailbox = Latest::new();
        let clone = mailbox.clone();
        mailbox.put(1);
        clone.put(2);

        assert_eq!(mailbox.take(), Some(2));
        assert_eq!(clone.take(), None);
    }

    #[test]
    fn capability_gated_operations_follow_player_caps() {
        let gated: [GatedOperation; 10] = [
            (MediaOp::ShuffleToggle, |caps| caps.shuffle = true),
            (MediaOp::LoopCycle, |caps| caps.loop_mode = true),
            (MediaOp::VolumeUp, |caps| caps.volume = true),
            (MediaOp::VolumeDown, |caps| caps.volume = true),
            (MediaOp::SeekForward, |caps| caps.seek = true),
            (MediaOp::SeekBack, |caps| caps.seek = true),
            (
                MediaOp::PlayQueueItem(QueueItemLease {
                    id: "queue-id".into(),
                    player: "player".into(),
                }),
                |caps| caps.queue = true,
            ),
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
        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
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
        finish_op_chain(&tail).await;
        assert_eq!(*records.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn transport_operation_queue_rejects_work_over_its_cap() {
        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
        let pending = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        assert!(chain_op(&tail, &pending, 1, async move {
            release_rx.await.unwrap();
        }));
        assert!(!chain_op(&tail, &pending, 1, async {}));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 1);

        release_tx.send(()).unwrap();
        finish_op_chain(&tail).await;
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

    #[test]
    fn queue_lease_check_refuses_a_switched_player_as_stale() {
        let lease = QueueItemLease {
            id: "q1".into(),
            player: "spotify".into(),
        };
        let snapshot = MediaSnapshot {
            state: MediaState {
                player: "mpd".into(),
                ..MediaState::default()
            },
            caps: MediaCaps::default(),
        };

        assert_eq!(
            queue_lease_check(&lease, Some(&snapshot)),
            Err(MediaOpOutcome::Stale)
        );
    }

    #[test]
    fn queue_lease_check_refuses_without_a_readable_snapshot() {
        let lease = QueueItemLease {
            id: "q1".into(),
            player: "spotify".into(),
        };

        assert_eq!(
            queue_lease_check(&lease, None),
            Err(MediaOpOutcome::Refused("queue"))
        );
        assert!(
            MediaOpOutcome::Refused("queue")
                .status_text()
                .contains("queue")
        );
    }

    #[test]
    fn queue_lease_check_accepts_the_listing_player() {
        let lease = QueueItemLease {
            id: "q1".into(),
            player: "spotify".into(),
        };
        let snapshot = MediaSnapshot {
            state: MediaState {
                player: "spotify".into(),
                ..MediaState::default()
            },
            caps: MediaCaps::default(),
        };

        assert_eq!(queue_lease_check(&lease, Some(&snapshot)), Ok(()));
    }

    #[tokio::test]
    async fn full_queue_records_busy_outcome() {
        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
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
        finish_op_chain(&tail).await;
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_transport_operation_does_not_block_the_queue() {
        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
        let pending = Arc::new(AtomicUsize::new(0));
        let (second_tx, second_rx) = tokio::sync::oneshot::channel();

        assert!(chain_op(&tail, &pending, 8, async {
            std::future::pending::<()>().await;
        }));
        assert!(chain_op(&tail, &pending, 8, async move {
            assert!(second_tx.send(()).is_ok());
        }));

        finish_op_chain(&tail).await;
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

    #[tokio::test(start_paused = true)]
    async fn op_chain_cancel_stops_a_running_op_before_its_effect() {
        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
        let pending = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let effect_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_effect_ran = effect_ran.clone();

        assert!(chain_op(&tail, &pending, 8, async move {
            started_tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            task_effect_ran.store(true, std::sync::atomic::Ordering::Release);
        }));
        started_rx.await.unwrap();
        cancel_op_chain(&tail);
        tokio::time::advance(std::time::Duration::from_secs(120)).await;
        finish_op_chain(&tail).await;

        assert!(!effect_ran.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn op_chain_cancel_cancels_queued_ops_behind_the_running_one() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
        let pending = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_dropped = dropped.clone();
        let second_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_second_ran = second_ran.clone();

        assert!(chain_op(&tail, &pending, 8, async move {
            let _drop_flag = DropFlag(task_dropped);
            started_tx.send(()).unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }));
        assert!(chain_op(&tail, &pending, 8, async move {
            task_second_ran.store(true, std::sync::atomic::Ordering::Release);
        }));
        started_rx.await.unwrap();
        cancel_op_chain(&tail);
        {
            let chain = tail.lock().unwrap();
            assert!(
                chain.tail.is_some(),
                "cancellation must replace the chain tail"
            );
            assert!(chain.drain.is_some(), "cancellation must install a joiner");
        }
        let saw_drop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_saw_drop = saw_drop.clone();
        let task_dropped = dropped.clone();
        assert!(chain_op(&tail, &pending, 8, async move {
            task_saw_drop.store(
                task_dropped.load(std::sync::atomic::Ordering::Acquire),
                std::sync::atomic::Ordering::Release,
            );
        }));
        tokio::time::advance(std::time::Duration::from_secs(120)).await;
        finish_op_chain(&tail).await;

        assert!(!second_ran.load(std::sync::atomic::Ordering::Acquire));
        assert!(saw_drop.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn op_chain_cancel_next_op_waits_for_the_joined_chain() {
        struct DropFlag {
            started: Option<tokio::sync::oneshot::Sender<()>>,
            release: std::sync::mpsc::Receiver<()>,
            dropped: Arc<std::sync::atomic::AtomicBool>,
        }

        impl Drop for DropFlag {
            fn drop(&mut self) {
                if let Some(started) = self.started.take() {
                    started.send(()).unwrap();
                }
                self.release.recv().unwrap();
                self.dropped
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }

        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
        let pending = Arc::new(AtomicUsize::new(0));
        let (drop_started_tx, drop_started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_dropped = dropped.clone();
        assert!(chain_op(&tail, &pending, 8, async move {
            let _drop_flag = DropFlag {
                started: Some(drop_started_tx),
                release: release_rx,
                dropped: task_dropped,
            };
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        started_rx.await.unwrap();
        assert!(chain_op(&tail, &pending, 8, async {}));

        cancel_op_chain(&tail);
        {
            let chain = tail.lock().unwrap();
            assert!(
                chain.tail.is_some(),
                "cancellation must replace the chain tail"
            );
            assert!(chain.drain.is_some(), "cancellation must install a joiner");
        }
        drop_started_rx.await.unwrap();

        let saw_drop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_saw_drop = saw_drop.clone();
        let task_dropped = dropped.clone();
        let (next_started_tx, mut next_started_rx) = tokio::sync::oneshot::channel();
        assert!(chain_op(&tail, &pending, 8, async move {
            task_saw_drop.store(
                task_dropped.load(std::sync::atomic::Ordering::Acquire),
                std::sync::atomic::Ordering::Release,
            );
            next_started_tx.send(()).unwrap();
        }));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(500), &mut next_started_rx,)
                .await
                .is_err(),
            "the next generation must wait while the old operation is still dropping"
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut next_started_rx)
            .await
            .unwrap()
            .unwrap();
        finish_op_chain(&tail).await;

        assert!(saw_drop.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn op_chain_cancel_on_an_empty_chain_is_a_no_op() {
        let tail = Arc::new(Mutex::new(MediaOpChain::default()));
        let pending = Arc::new(AtomicUsize::new(0));
        cancel_op_chain(&tail);
        {
            let chain = tail.lock().unwrap();
            assert!(chain.handles.is_empty());
            assert!(chain.tail.is_none());
            assert!(chain.drain.is_none());
        }

        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_ran = ran.clone();
        assert!(chain_op(&tail, &pending, 8, async move {
            task_ran.store(true, std::sync::atomic::Ordering::Release);
        }));
        finish_op_chain(&tail).await;

        assert!(ran.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn media_op_budget_is_the_backend_budget() {
        assert_eq!(super::MEDIA_OP_BUDGET, thegn_media::MEDIA_OP_BUDGET);
    }
}
