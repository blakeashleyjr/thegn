//! The now-playing watcher task (the optional `[media]` feature's host driver).
//!
//! Extracted from `run.rs` (which is line-ratcheted) so the watcher can grow the
//! resilience it needs without touching the god file. `spawn` owns one tokio task
//! that resolves a [`thegn_media`] backend and streams [`MediaState`] snapshots
//! back to the event loop over an mpsc channel + [`TerminalWaker`] pulse.
//!
//! Resilience contract (media is *strictly additive* — never disrupts the shell,
//! never breaks the ~0%-idle invariant):
//!
//! - **Errors surface, never swallow.** Read failures log on `thegn::media`
//!   (was a blind `unwrap_or(None)`), so `THEGN_LOG=thegn::media=debug`
//!   explains a missing badge instead of leaving it a mystery.
//! - **Self-heal while nothing shows.** On the native MPRIS push path we also run
//!   a slow safety re-snapshot *only while no track is currently displayed*, so a
//!   missed/failed initial read recovers without waiting for a track change. Once
//!   a badge is showing we go signal-only again — no idle polling during playback,
//!   preserving the 0%-idle contract.
//! - **Respawn on stream end.** If the D-Bus signal stream ends (bus restart,
//!   player teardown) the task re-resolves the backend with backoff rather than
//!   dying permanently.

use std::sync::atomic::Ordering;

use tokio::sync::watch as tokio_watch;
use tokio::time::MissedTickBehavior;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::MediaConfig;
use thegn_core::media::{MediaSnapshot, MediaState};

/// Freshness of the most recent watcher delivery shown by the Media panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum MediaHealth {
    /// The last provider read succeeded, including an authoritative empty read.
    #[default]
    Fresh,
    /// A read failed and the shown snapshot is last-known-good within the grace.
    Degraded,
    /// The grace expired and no snapshot is shown.
    Unavailable,
}

/// A now-playing result tagged with the watcher/config/player generation that
/// produced it. The tag is captured before a producer starts async work, so a
/// late result cannot overwrite a newer selection.
#[derive(Debug, Clone)]
pub(crate) struct MediaSnapshotDelivery {
    pub(crate) generation: u64,
    pub(crate) snapshot: Option<MediaSnapshot>,
    pub(crate) health: MediaHealth,
}

pub(crate) type SnapshotSender = tokio_watch::Sender<Option<MediaSnapshotDelivery>>;
pub(crate) type SnapshotReceiver = tokio_watch::Receiver<Option<MediaSnapshotDelivery>>;

/// Store only the newest offered generation in the capacity-one mailbox.
pub(crate) fn offer(tx: &SnapshotSender, delivery: MediaSnapshotDelivery) -> Result<bool, ()> {
    if tx.is_closed() {
        return Err(());
    }
    let mut accepted = false;
    tx.send_if_modified(|stored| {
        if stored
            .as_ref()
            .is_none_or(|current| delivery.generation >= current.generation)
        {
            *stored = Some(delivery);
            accepted = true;
            true
        } else {
            false
        }
    });
    Ok(accepted)
}

/// Spawn the now-playing watcher for `cfg`. Returns the task handle so the caller
/// can abort it on a config/player change; `None` when media is disabled.
pub(crate) fn spawn(
    cfg: MediaConfig,
    generation: u64,
    tx: SnapshotSender,
    waker: TerminalWaker,
    previous: Option<tokio::task::JoinHandle<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        abort_and_join(previous).await;
        run(cfg, generation, tx, waker).await;
    }))
}

/// Abort the prior generation and wait for its task to finish before starting
/// another watcher. This also works when the prior task is waiting silently.
pub(crate) async fn abort_and_join(previous: Option<tokio::task::JoinHandle<()>>) {
    if let Some(previous) = previous {
        previous.abort();
        let joined = join_within(
            async {
                if let Err(error) = previous.await
                    && error.is_panic()
                {
                    tracing::warn!(target: "thegn::media", error = %error, "previous media watcher panicked while stopping");
                }
            },
            crate::media_ctl::MEDIA_OP_BUDGET,
        )
        .await;
        if !joined {
            tracing::warn!(target: "thegn::media", budget_secs = crate::media_ctl::MEDIA_OP_BUDGET.as_secs(), "previous media watcher did not stop within its time budget");
        }
    }
}

async fn join_within<F: std::future::Future>(fut: F, budget: std::time::Duration) -> bool {
    tokio::time::timeout(budget, fut).await.is_ok()
}

/// Grace applied on the badge active→inactive edge before we believe a player
/// really went away. A single transient empty/`Stopped` read — a D-Bus race, a
/// one-cycle miss from the multi-source aggregate, a brief inter-track `Stopped`
/// blip — would otherwise hide the badge for a whole safety-poll interval
/// (`[media] poll_interval_secs`, ~3s), making it *flash in and out*. On that
/// edge we re-read once after this delay and only clear the badge if it is
/// *still* gone; a recovered read is emitted instead of the transient empty.
/// Kept short so a genuine stop still clears the badge promptly.
const CONFIRM_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// A push connection must remain alive this long before it is considered stable.
/// Briefly flapping streams keep the accumulated exponential reconnect delay;
/// only a stream that survives this window earns a reset.
const STREAM_STABILITY: std::time::Duration = std::time::Duration::from_secs(30);

/// A last-known-good badge may survive a short provider outage, but never
/// longer than three poll periods or ten seconds, whichever is greater.
fn stale_grace(period: std::time::Duration) -> std::time::Duration {
    (period * 3).max(std::time::Duration::from_secs(10))
}

#[derive(Default)]
struct Freshness {
    failure_started: Option<tokio::time::Instant>,
    stale_cleared: bool,
}

impl Freshness {
    /// Errors are distinct from successful empty reads. A failure emits a
    /// single clear only after the bounded last-known-good grace has elapsed.
    fn on_failure(
        &mut self,
        now: tokio::time::Instant,
        grace: std::time::Duration,
    ) -> Option<Option<MediaSnapshot>> {
        let since = *self.failure_started.get_or_insert(now);
        if self.stale_cleared || now.duration_since(since) < grace {
            return None;
        }
        self.stale_cleared = true;
        Some(None)
    }

    fn on_success(&mut self) {
        self.failure_started = None;
        self.stale_cleared = false;
    }

    fn until_expiry(
        &self,
        now: tokio::time::Instant,
        grace: std::time::Duration,
    ) -> Option<std::time::Duration> {
        if self.stale_cleared {
            return None;
        }
        let since = self.failure_started?;
        Some(grace.saturating_sub(now.duration_since(since)))
    }
}

struct WatchState {
    freshness: Freshness,
    grace: std::time::Duration,
    last_sent: Option<MediaSnapshot>,
    last_health: MediaHealth,
    last_epoch: u64,
}

fn failure_delivery(
    state: &mut WatchState,
    now: tokio::time::Instant,
) -> Option<(Option<MediaSnapshot>, MediaHealth)> {
    match state.freshness.on_failure(now, state.grace) {
        Some(expired) => Some((expired, MediaHealth::Unavailable)),
        None if !state.freshness.stale_cleared
            && state.last_sent.is_some()
            && state.last_health != MediaHealth::Degraded =>
        {
            Some((state.last_sent.clone(), MediaHealth::Degraded))
        }
        None => None,
    }
}

fn deliver(
    tx: &SnapshotSender,
    waker: &TerminalWaker,
    generation: u64,
    state: &mut WatchState,
    snapshot: Option<MediaSnapshot>,
    health: MediaHealth,
    epoch: u64,
) -> Result<bool, ()> {
    let accepted = offer(
        tx,
        MediaSnapshotDelivery {
            generation,
            snapshot: snapshot.clone(),
            health,
        },
    )?;
    if accepted {
        if let Err(error) = waker.wake() {
            tracing::debug!(target: "thegn::media", %error, "media watcher wake failed");
        }
        state.last_sent = snapshot;
        state.last_epoch = epoch;
        state.last_health = health;
    }
    Ok(accepted)
}

fn interval(period: std::time::Duration) -> tokio::time::Interval {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

fn should_deliver(
    last: &Option<MediaSnapshot>,
    last_epoch: u64,
    current_epoch: u64,
    new: &Option<MediaSnapshot>,
    last_health: MediaHealth,
    health: MediaHealth,
) -> bool {
    last_epoch != current_epoch || last != new || last_health != health
}

fn backoff_after_stream(
    backoff: std::time::Duration,
    lifetime: std::time::Duration,
) -> std::time::Duration {
    if lifetime >= STREAM_STABILITY {
        std::time::Duration::from_secs(1)
    } else {
        backoff
    }
}

fn jittered(backoff: std::time::Duration, sample: u32) -> std::time::Duration {
    let lower = backoff.as_nanos() / 2;
    let span = backoff.as_nanos() - lower;
    let nanos = lower + span * u128::from(sample) / u128::from(u32::MAX);
    std::time::Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

fn jitter_sample() -> u32 {
    let mut bytes = [0_u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return u32::MAX;
    }
    u32::from_le_bytes(bytes)
}

async fn next_push_wake(
    watch: &mut (dyn thegn_media::MediaWatch + Send),
    active: bool,
    expiry: Option<std::time::Duration>,
    safety: &mut tokio::time::Interval,
    probe: &mut tokio::time::Interval,
) -> bool {
    if active && let Some(delay) = expiry {
        tokio::select! {
            changed = watch.changed() => changed,
            _ = tokio::time::sleep(delay) => true,
            _ = probe.tick() => true,
        }
    } else if active {
        tokio::select! {
            changed = watch.changed() => changed,
            _ = probe.tick() => true,
        }
    } else {
        tokio::select! {
            changed = watch.changed() => changed,
            _ = safety.tick() => true,
        }
    }
}

fn reset_probe_on_activation(was_active: bool, active: bool, probe: &mut tokio::time::Interval) {
    if !was_active && active {
        probe.reset();
    }
}

/// What to do with a freshly read snapshot given whether a badge was already up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlapAction {
    /// Emit the snapshot as-is.
    Emit,
    /// The badge was showing and this read is empty — confirm before believing
    /// the disappearance (anti-flap hysteresis).
    Confirm,
}

/// Decide whether a newly read snapshot can be emitted directly or whether a
/// *disappearance* needs confirming first. Pure so the flap-suppression contract
/// (confirm **only** on the showing→empty edge, never otherwise) is unit-tested.
fn flap_action(was_showing: bool, new_active: bool) -> FlapAction {
    if was_showing && !new_active {
        FlapAction::Confirm
    } else {
        FlapAction::Emit
    }
}

fn should_confirm_flap(read_ok: bool, was_showing: bool, new_active: bool) -> bool {
    read_ok && flap_action(was_showing, new_active) == FlapAction::Confirm
}

/// Read one snapshot, logging read errors instead of swallowing them.
async fn read_snapshot(client: &thegn_media::MediaClient) -> Result<Option<MediaSnapshot>, ()> {
    match client.snapshot_with_caps().await {
        Ok(snap) => Ok(snap),
        Err(e) => {
            tracing::debug!(target: "thegn::media", error = %e, "media snapshot failed");
            Err(())
        }
    }
}

/// Whether a snapshot would render a badge (i.e. an active player).
fn shows_badge(snap: &Option<MediaSnapshot>) -> bool {
    snap.as_ref().and_then(|s| s.state.badge()).is_some()
}

/// Read a snapshot and push it to the loop, suppressing a *transient*
/// disappearance. `was_showing` is whether a badge is currently displayed: when
/// it is and the fresh read has no badge, we re-read once after [`CONFIRM_DELAY`]
/// and only propagate the empty state if it persists — so a one-off empty read no
/// longer flashes the badge off until the next safety poll. Returns `Some(active)`
/// (whether a track is now displayed) on success, or `None` when the receiver is
/// gone and the task should stop.
async fn push_snapshot(
    client: &thegn_media::MediaClient,
    generation: u64,
    tx: &SnapshotSender,
    waker: &TerminalWaker,
    was_showing: bool,
    state: &mut WatchState,
) -> Option<bool> {
    let read = read_snapshot(client).await;
    let read_ok = read.is_ok();
    let (mut snap, mut health) = match read {
        Ok(snap) => {
            state.freshness.on_success();
            (snap, MediaHealth::Fresh)
        }
        Err(()) => match failure_delivery(state, tokio::time::Instant::now()) {
            Some((snapshot, health)) => {
                if health == MediaHealth::Unavailable {
                    tracing::debug!(target: "thegn::media", "media provider unavailable; stale badge expired");
                } else {
                    tracing::debug!(target: "thegn::media", "media read failed; retaining last-known-good snapshot");
                }
                (snapshot, health)
            }
            None => return Some(was_showing),
        },
    };
    if should_confirm_flap(read_ok, was_showing, shows_badge(&snap)) {
        tokio::time::sleep(CONFIRM_DELAY).await;
        match read_snapshot(client).await {
            Ok(confirm) => {
                state.freshness.on_success();
                if shows_badge(&confirm) {
                    tracing::debug!(target: "thegn::media", "media badge held (transient empty ignored)");
                } else {
                    tracing::debug!(target: "thegn::media", "media badge cleared (empty confirmed)");
                }
                snap = confirm;
            }
            Err(()) => match failure_delivery(state, tokio::time::Instant::now()) {
                Some((snapshot, failure_health)) => {
                    if failure_health == MediaHealth::Unavailable {
                        tracing::debug!(target: "thegn::media", "media provider unavailable; stale badge expired");
                    } else {
                        tracing::debug!(target: "thegn::media", "media confirm read failed; retaining last-known-good snapshot");
                    }
                    snap = snapshot;
                    health = failure_health;
                }
                None => return Some(was_showing),
            },
        }
    }
    let active = shows_badge(&snap);
    let epoch = crate::media_ctl::MEDIA_DELIVERY_EPOCH.load(Ordering::Acquire);
    if !should_deliver(
        &state.last_sent,
        state.last_epoch,
        epoch,
        &snap,
        state.last_health,
        health,
    ) {
        return Some(active);
    }
    if let Err(()) = deliver(tx, waker, generation, state, snap, health, epoch) {
        return None;
    }
    Some(active)
}

/// Log a badge-clearing transition without assuming the provider reported an
/// authoritative empty snapshot; the same edge can follow the stale grace.
fn note_cleared(prev_active: bool, now_active: bool) {
    if prev_active && !now_active {
        tracing::debug!(
            target: "thegn::media",
            "now-playing badge cleared"
        );
    }
}

async fn run(cfg: MediaConfig, generation: u64, tx: SnapshotSender, waker: TerminalWaker) {
    let period = std::time::Duration::from_secs(cfg.poll_interval_secs.max(1));
    let max_backoff = std::time::Duration::from_secs(30);
    let mut backoff = std::time::Duration::from_secs(1);
    let grace = stale_grace(period);
    let mut state = WatchState {
        freshness: Freshness::default(),
        grace,
        last_sent: None,
        last_health: MediaHealth::Fresh,
        last_epoch: crate::media_ctl::MEDIA_DELIVERY_EPOCH
            .load(Ordering::Acquire)
            .wrapping_sub(1),
    };
    let mut active = false;

    loop {
        let client = match crate::media_secret::client_for(&cfg).await {
            Ok(client) => client,
            Err(error) => {
                // No backend yet (bus down, playerctl absent). Retry with backoff so
                // media appears once the transport comes up, rather than never.
                tracing::debug!(target: "thegn::media", %error, "media backend unavailable; retrying");
                if let Some((snapshot, health)) =
                    failure_delivery(&mut state, tokio::time::Instant::now())
                {
                    active = shows_badge(&snapshot);
                    let epoch = crate::media_ctl::MEDIA_DELIVERY_EPOCH.load(Ordering::Acquire);
                    if should_deliver(
                        &state.last_sent,
                        state.last_epoch,
                        epoch,
                        &snapshot,
                        state.last_health,
                        health,
                    ) && let Err(()) =
                        deliver(&tx, &waker, generation, &mut state, snapshot, health, epoch)
                    {
                        return;
                    }
                }
                let retry_after = jittered(backoff, jitter_sample());
                let retry_after = state
                    .freshness
                    .until_expiry(tokio::time::Instant::now(), state.grace)
                    .map_or(retry_after, |remaining| retry_after.min(remaining));
                tokio::time::sleep(retry_after).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };
        // Refresh after resolution; retain `active` across failed resolutions
        // so a prior generation's badge follows the same stale-grace policy.
        active = match push_snapshot(&client, generation, &tx, &waker, active, &mut state).await {
            Some(a) => a,
            None => return, // receiver gone
        };

        match client.watch().await {
            Ok(Some(mut watch)) => {
                let connected_at = tokio::time::Instant::now();
                // Native push path. Re-snapshot on each D-Bus signal; run a slow
                // safety poll while hidden and a slower liveness probe while a
                // badge shows, so a silent open stream cannot leave it stale.
                let mut safety = interval(period);
                safety.tick().await; // consume the immediate first tick
                let mut probe = interval(STREAM_STABILITY);
                probe.tick().await; // consume the immediate first tick
                let stream_ended = loop {
                    let expiry = state
                        .freshness
                        .until_expiry(tokio::time::Instant::now(), state.grace);
                    let changed =
                        next_push_wake(&mut *watch, active, expiry, &mut safety, &mut probe).await;
                    if !changed {
                        break true; // signal stream ended → reconnect
                    }
                    match push_snapshot(&client, generation, &tx, &waker, active, &mut state).await
                    {
                        Some(a) => {
                            reset_probe_on_activation(active, a, &mut probe);
                            note_cleared(active, a);
                            active = a;
                        }
                        None => return, // receiver gone
                    }
                };
                if stream_ended {
                    tracing::debug!(target: "thegn::media", "MPRIS signal stream ended; reconnecting");
                    backoff = backoff_after_stream(backoff, connected_at.elapsed());
                    tokio::time::sleep(jittered(backoff, jitter_sample())).await;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }
            Ok(None) => {
                poll_media(
                    &client,
                    generation,
                    &tx,
                    &waker,
                    &mut active,
                    &mut state,
                    period,
                )
                .await;
                return;
            }
            Err(error) => {
                tracing::warn!(target: "thegn::media", %error, "media push watcher failed to start; polling");
                poll_media(
                    &client,
                    generation,
                    &tx,
                    &waker,
                    &mut active,
                    &mut state,
                    period,
                )
                .await;
                return;
            }
        }
    }
}

async fn poll_media(
    client: &thegn_media::MediaClient,
    generation: u64,
    tx: &SnapshotSender,
    waker: &TerminalWaker,
    active: &mut bool,
    state: &mut WatchState,
    period: std::time::Duration,
) {
    let mut ticker = interval(period);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match push_snapshot(client, generation, tx, waker, *active, state).await {
            Some(next_active) => {
                note_cleared(*active, next_active);
                *active = next_active;
            }
            None => return,
        }
    }
}

/// Drain pending now-playing snapshots into the model — the event loop's
/// `media_rx` handler, extracted from the ratchet-pinned `run.rs`.
///
/// Repaint discipline: a `Playing` snapshot ticks its position several times a
/// second, and a full chrome recompose per tick is a self-sustaining flicker
/// storm (and an idle-CPU violation). So repaint only what the snapshot moves
/// on screen: while the Media panel section is open (`coalesce_full` — it shows
/// the live position stamp) coalesce full frames
/// to ~1/s, repainting immediately only when the statusbar badge text changed;
/// with both closed, a badge change takes the cheap bars path and a
/// position-only change repaints nothing.
///
/// Returns `(full, bars)` repaint intent for the loop's `dirty` / `bars_dirty`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drain_snapshots(
    rx: &mut SnapshotReceiver,
    perf: &mut crate::perf::LoopPerf,
    enabled: bool,
    generation: u64,
    media: &mut Option<MediaState>,
    caps: &mut Option<thegn_core::media::MediaCaps>,
    health: &mut MediaHealth,
    coalesce_full: bool,
    last_full: &mut Option<std::time::Instant>,
) -> (bool, bool) {
    let Ok(true) = rx.has_changed() else {
        return (false, false);
    };
    let Some(delivery) = rx.borrow_and_update().clone() else {
        return (false, false);
    };
    if delivery.generation != generation {
        return (false, false);
    }
    let newest = delivery.snapshot;
    perf.tick(crate::perf::WakeSource::Refresh);
    let shown = if enabled { newest } else { None };
    let shown_health = if enabled {
        delivery.health
    } else {
        MediaHealth::Fresh
    };
    let shown_state = shown.as_ref().map(|snapshot| snapshot.state.clone());
    let shown_caps = shown.as_ref().map(|snapshot| snapshot.caps);
    if *media == shown_state && *caps == shown_caps && *health == shown_health {
        return (false, false);
    }
    let badge_changed =
        media.as_ref().and_then(|m| m.badge()) != shown_state.as_ref().and_then(|m| m.badge());
    *media = shown_state;
    *caps = shown_caps;
    let health_changed = *health != shown_health;
    *health = shown_health;
    let (mut full, mut bars) = (false, false);
    if coalesce_full {
        if badge_changed
            || health_changed
            || last_full
                .map(|t| t.elapsed() >= std::time::Duration::from_millis(900))
                .unwrap_or(true)
        {
            full = true;
            *last_full = Some(std::time::Instant::now());
        }
    } else if badge_changed {
        bars = true;
    }
    (full, bars)
}

#[cfg(test)]
mod tests {
    use tokio::sync::watch;
    use tokio::time::MissedTickBehavior;

    use super::{
        FlapAction, Freshness, MediaHealth, MediaSnapshotDelivery, STREAM_STABILITY, WatchState,
        abort_and_join, backoff_after_stream, drain_snapshots, failure_delivery, flap_action,
        interval, jittered, join_within, next_push_wake, offer, reset_probe_on_activation,
        should_confirm_flap, should_deliver, stale_grace,
    };
    use thegn_core::media::{MediaCaps, MediaSnapshot, MediaState};

    #[test]
    fn confirms_only_on_the_disappear_edge() {
        // Badge is up and the read went empty → confirm before hiding it.
        assert_eq!(flap_action(true, false), FlapAction::Confirm);
        // Still playing → emit straight through (no cost while active).
        assert_eq!(flap_action(true, true), FlapAction::Emit);
        // A player appearing → emit; nothing to confirm.
        assert_eq!(flap_action(false, true), FlapAction::Emit);
        // Already hidden and still empty → emit (no repeated re-confirm loop).
        assert_eq!(flap_action(false, false), FlapAction::Emit);
    }

    #[test]
    fn successful_stopped_read_is_confirmed_but_failed_read_is_not() {
        // A successful Some(snapshot) whose state is Stopped has no badge,
        // just like Ok(None), and must pass through the anti-flap delay.
        assert!(should_confirm_flap(true, true, false));
        assert!(!should_confirm_flap(false, true, false));
        assert!(!should_confirm_flap(true, false, false));
    }

    #[test]
    fn reconnect_backoff_resets_only_after_a_stable_stream() {
        let accumulated = std::time::Duration::from_secs(8);
        assert_eq!(
            backoff_after_stream(
                accumulated,
                STREAM_STABILITY - std::time::Duration::from_secs(1)
            ),
            accumulated,
            "an immediately ending stream must keep its exponential delay"
        );
        assert_eq!(
            backoff_after_stream(accumulated, STREAM_STABILITY),
            std::time::Duration::from_secs(1),
            "only a stream that meets the stability window resets backoff"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn freshness_keeps_last_good_during_grace_then_clears_once() {
        let now = tokio::time::Instant::now();
        let grace = std::time::Duration::from_secs(10);
        let mut freshness = Freshness::default();
        freshness.on_success();
        let failure_now = now + std::time::Duration::from_secs(3600);
        assert!(freshness.on_failure(failure_now, grace).is_none());
        assert_eq!(
            freshness.until_expiry(failure_now, grace),
            Some(grace),
            "the grace starts with the first failure, even after an old success"
        );
        assert!(
            freshness
                .on_failure(
                    failure_now + grace - std::time::Duration::from_millis(1),
                    grace
                )
                .is_none()
        );
        assert_eq!(freshness.on_failure(failure_now + grace, grace), Some(None));
        assert!(
            freshness
                .on_failure(failure_now + grace + grace, grace)
                .is_none()
        );
        assert_eq!(
            stale_grace(std::time::Duration::from_secs(4)),
            std::time::Duration::from_secs(12)
        );
        assert_eq!(
            stale_grace(std::time::Duration::from_secs(5)),
            std::time::Duration::from_secs(15)
        );
    }

    #[tokio::test]
    async fn media_intervals_skip_missed_ticks() {
        assert_eq!(
            interval(std::time::Duration::from_secs(2)).missed_tick_behavior(),
            MissedTickBehavior::Skip
        );
    }

    #[test]
    fn unchanged_snapshots_are_suppressed_unless_media_ctl_sent() {
        let snapshot = Some(MediaSnapshot {
            state: MediaState::default(),
            caps: MediaCaps::default(),
        });
        assert!(!should_deliver(
            &snapshot,
            4,
            4,
            &snapshot,
            MediaHealth::Fresh,
            MediaHealth::Fresh
        ));
        assert!(should_deliver(
            &snapshot,
            4,
            5,
            &snapshot,
            MediaHealth::Fresh,
            MediaHealth::Fresh
        ));
        assert!(should_deliver(
            &snapshot,
            4,
            4,
            &None,
            MediaHealth::Fresh,
            MediaHealth::Fresh
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn failure_delivery_is_degraded_once_then_unavailable_after_grace() {
        let now = tokio::time::Instant::now();
        let grace = std::time::Duration::from_secs(10);
        let snapshot = MediaSnapshot {
            state: MediaState {
                title: "track".into(),
                ..Default::default()
            },
            caps: MediaCaps::default(),
        };
        let mut state = WatchState {
            freshness: Freshness::default(),
            grace,
            last_sent: Some(snapshot.clone()),
            last_health: MediaHealth::Fresh,
            last_epoch: 0,
        };
        assert_eq!(
            failure_delivery(&mut state, now),
            Some((Some(snapshot.clone()), MediaHealth::Degraded))
        );
        state.last_health = MediaHealth::Degraded;
        assert_eq!(failure_delivery(&mut state, now + grace / 2), None);
        assert_eq!(
            failure_delivery(&mut state, now + grace),
            Some((None, MediaHealth::Unavailable))
        );
        assert_eq!(failure_delivery(&mut state, now + grace * 2), None);
    }

    #[tokio::test(start_paused = true)]
    async fn failure_delivery_without_a_shown_snapshot_stays_silent_until_expiry() {
        let now = tokio::time::Instant::now();
        let grace = std::time::Duration::from_secs(10);
        let mut state = WatchState {
            freshness: Freshness::default(),
            grace,
            last_sent: None,
            last_health: MediaHealth::Fresh,
            last_epoch: 0,
        };
        assert_eq!(failure_delivery(&mut state, now), None);
        assert_eq!(
            failure_delivery(&mut state, now + grace),
            Some((None, MediaHealth::Unavailable))
        );
    }

    #[test]
    fn health_only_change_is_delivered() {
        let snapshot = Some(MediaSnapshot {
            state: MediaState::default(),
            caps: MediaCaps::default(),
        });
        assert!(should_deliver(
            &snapshot,
            4,
            4,
            &snapshot,
            MediaHealth::Degraded,
            MediaHealth::Fresh
        ));
        assert!(!should_deliver(
            &snapshot,
            4,
            4,
            &snapshot,
            MediaHealth::Fresh,
            MediaHealth::Fresh
        ));
    }

    #[tokio::test]
    async fn watcher_replacement_joins_the_aborted_task_before_continuing() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = dropped.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let previous = tokio::spawn(async move {
            let _guard = DropFlag(task_dropped);
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        abort_and_join(Some(previous)).await;

        // This is the first operation the replacement watcher performs after
        // the join helper returns.
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn stale_snapshot_after_restart_cannot_replace_current_generation() {
        let (tx, mut rx) = watch::channel(None);
        let mut perf = crate::perf::LoopPerf::new();
        let mut media = None;
        let mut caps = None;
        let mut health = MediaHealth::Fresh;
        let mut last_full = None;
        let current = MediaSnapshot {
            state: MediaState {
                player: "new-player".into(),
                title: "new-track".into(),
                ..Default::default()
            },
            caps: MediaCaps::default(),
        };
        let stale = MediaSnapshot {
            state: MediaState {
                player: "old-player".into(),
                title: "old-track".into(),
                ..Default::default()
            },
            caps: MediaCaps::default(),
        };
        offer(
            &tx,
            MediaSnapshotDelivery {
                generation: 1,
                snapshot: Some(current.clone()),
                health: MediaHealth::Fresh,
            },
        )
        .unwrap();
        offer(
            &tx,
            MediaSnapshotDelivery {
                generation: 0,
                snapshot: Some(stale),
                health: MediaHealth::Fresh,
            },
        )
        .unwrap();

        drain_snapshots(
            &mut rx,
            &mut perf,
            true,
            1,
            &mut media,
            &mut caps,
            &mut health,
            false,
            &mut last_full,
        );

        assert_eq!(media, Some(current.state));
        assert_eq!(caps, Some(current.caps));
        assert!(last_full.is_none());
    }

    #[test]
    fn drain_applies_only_the_newest_current_generation_snapshot() {
        let (tx, mut rx) = watch::channel(None);
        let mut perf = crate::perf::LoopPerf::new();
        let mut media = Some(MediaState {
            player: "player".into(),
            title: "first".into(),
            ..Default::default()
        });
        let mut caps = Some(MediaCaps::default());
        let mut health = MediaHealth::Fresh;
        let mut last_full = None;
        let first = MediaSnapshot {
            state: media.clone().unwrap(),
            caps: MediaCaps::default(),
        };
        let middle = MediaSnapshot {
            state: MediaState {
                title: "middle".into(),
                ..first.state.clone()
            },
            caps: MediaCaps::default(),
        };
        for snapshot in [first.clone(), middle, first.clone()] {
            offer(
                &tx,
                MediaSnapshotDelivery {
                    generation: 7,
                    snapshot: Some(snapshot),
                    health: MediaHealth::Fresh,
                },
            )
            .unwrap();
        }
        let repaint = drain_snapshots(
            &mut rx,
            &mut perf,
            true,
            7,
            &mut media,
            &mut caps,
            &mut health,
            false,
            &mut last_full,
        );
        assert_eq!(repaint, (false, false));
        assert_eq!(media, Some(first.state));
    }

    #[test]
    fn drain_applies_health_and_repaints_open_panel_on_health_only_change() {
        let snapshot = MediaSnapshot {
            state: MediaState {
                player: "player".into(),
                title: "track".into(),
                ..Default::default()
            },
            caps: MediaCaps::default(),
        };
        let (tx, mut rx) = watch::channel(None);
        offer(
            &tx,
            MediaSnapshotDelivery {
                generation: 1,
                snapshot: Some(snapshot.clone()),
                health: MediaHealth::Degraded,
            },
        )
        .unwrap();
        let mut perf = crate::perf::LoopPerf::new();
        let mut media = Some(snapshot.state.clone());
        let mut caps = Some(snapshot.caps);
        let mut health = MediaHealth::Fresh;
        let mut last_full = None;
        let repaint = drain_snapshots(
            &mut rx,
            &mut perf,
            true,
            1,
            &mut media,
            &mut caps,
            &mut health,
            true,
            &mut last_full,
        );
        assert_eq!(health, MediaHealth::Degraded);
        assert_eq!(repaint, (true, false));

        let (tx, mut rx) = watch::channel(None);
        offer(
            &tx,
            MediaSnapshotDelivery {
                generation: 1,
                snapshot: Some(snapshot.clone()),
                health: MediaHealth::Degraded,
            },
        )
        .unwrap();
        let mut health = MediaHealth::Fresh;
        let mut last_full = None;
        let repaint = drain_snapshots(
            &mut rx,
            &mut perf,
            true,
            1,
            &mut media,
            &mut caps,
            &mut health,
            false,
            &mut last_full,
        );
        assert_eq!(health, MediaHealth::Degraded);
        assert_eq!(repaint, (false, false));
    }

    #[test]
    fn offer_keeps_newest_generation() {
        let (tx, rx) = watch::channel(None);
        offer(
            &tx,
            MediaSnapshotDelivery {
                generation: 1,
                snapshot: None,
                health: MediaHealth::Fresh,
            },
        )
        .unwrap();
        assert!(
            !offer(
                &tx,
                MediaSnapshotDelivery {
                    generation: 0,
                    snapshot: None,
                    health: MediaHealth::Fresh,
                }
            )
            .unwrap()
        );
        assert_eq!(
            rx.borrow().as_ref().map(|delivery| delivery.generation),
            Some(1)
        );
    }

    #[test]
    fn offer_reports_closed_receiver() {
        let (tx, rx) = watch::channel(None);
        drop(rx);
        assert!(
            offer(
                &tx,
                MediaSnapshotDelivery {
                    generation: 1,
                    snapshot: None,
                    health: MediaHealth::Fresh,
                }
            )
            .is_err()
        );
    }

    struct SilentWatch;

    impl thegn_media::MediaWatch for SilentWatch {
        fn changed(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(std::future::pending())
        }
    }

    struct EndedWatch;

    impl thegn_media::MediaWatch for EndedWatch {
        fn changed(
            &mut self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            Box::pin(async { false })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_silent_stream_wakes_on_the_probe_while_active() {
        let mut watch = SilentWatch;
        let mut safety = interval(std::time::Duration::from_secs(3));
        safety.tick().await;
        let mut probe = interval(STREAM_STABILITY);
        probe.tick().await;
        let started = tokio::time::Instant::now();
        let wake = next_push_wake(&mut watch, true, None, &mut safety, &mut probe);
        tokio::pin!(wake);
        tokio::select! {
            result = &mut wake => panic!("silent stream woke before the probe: {result}"),
            _ = tokio::time::sleep(STREAM_STABILITY - std::time::Duration::from_millis(1)) => {},
        }
        let result = wake.await;
        assert!(result);
        assert_eq!(tokio::time::Instant::now() - started, STREAM_STABILITY);
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_activation_resets_an_overdue_probe() {
        let mut watch = SilentWatch;
        let mut safety = interval(std::time::Duration::from_secs(3));
        safety.tick().await;
        let mut probe = interval(STREAM_STABILITY);
        probe.tick().await;
        tokio::time::sleep(STREAM_STABILITY * 2).await;

        reset_probe_on_activation(false, true, &mut probe);
        let started = tokio::time::Instant::now();
        assert!(next_push_wake(&mut watch, true, None, &mut safety, &mut probe).await);
        assert_eq!(tokio::time::Instant::now() - started, STREAM_STABILITY);
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_inactive_wait_keeps_the_safety_period() {
        let period = std::time::Duration::from_secs(3);
        let mut watch = SilentWatch;
        let mut safety = interval(period);
        safety.tick().await;
        let mut probe = interval(STREAM_STABILITY);
        probe.tick().await;
        let started = tokio::time::Instant::now();
        assert!(next_push_wake(&mut watch, false, None, &mut safety, &mut probe).await);
        assert_eq!(tokio::time::Instant::now() - started, period);
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_ended_stream_returns_false() {
        let mut watch = EndedWatch;
        let mut safety = interval(std::time::Duration::from_secs(3));
        safety.tick().await;
        let mut probe = interval(STREAM_STABILITY);
        probe.tick().await;
        assert!(!next_push_wake(&mut watch, true, None, &mut safety, &mut probe).await);
    }

    #[test]
    fn liveness_jitter_stays_within_half_to_full_backoff() {
        for backoff in [
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(8),
            std::time::Duration::from_secs(30),
        ] {
            for sample in [0, 1, u32::MAX / 2, u32::MAX] {
                let value = jittered(backoff, sample);
                assert!(
                    value >= backoff / 2 && value <= backoff,
                    "{value:?} for {backoff:?}"
                );
            }
            assert_eq!(jittered(backoff, 0), backoff / 2);
            assert_eq!(jittered(backoff, u32::MAX), backoff);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn liveness_join_within_times_out_on_a_never_finishing_future() {
        assert!(
            !join_within(
                std::future::pending::<()>(),
                crate::media_ctl::MEDIA_OP_BUDGET
            )
            .await
        );
        assert!(join_within(std::future::ready(()), crate::media_ctl::MEDIA_OP_BUDGET).await);
    }
}
