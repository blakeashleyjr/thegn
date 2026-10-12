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

use tokio::sync::mpsc as tokio_mpsc;
use tokio::time::MissedTickBehavior;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::MediaConfig;
use thegn_core::media::{MediaSnapshot, MediaState};

/// A now-playing result tagged with the watcher/config/player generation that
/// produced it. The tag is captured before a producer starts async work, so a
/// late result cannot overwrite a newer selection.
#[derive(Debug, Clone)]
pub(crate) struct MediaSnapshotDelivery {
    pub(crate) generation: u64,
    pub(crate) snapshot: Option<MediaSnapshot>,
}

/// Spawn the now-playing watcher for `cfg`. Returns the task handle so the caller
/// can abort it on a config/player change; `None` when media is disabled.
pub(crate) fn spawn(
    cfg: MediaConfig,
    generation: u64,
    tx: tokio_mpsc::UnboundedSender<MediaSnapshotDelivery>,
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
        if let Err(error) = previous.await
            && error.is_panic()
        {
            tracing::warn!(target: "thegn::media", error = %error, "previous media watcher panicked while stopping");
        }
    }
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
    last_epoch: u64,
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
) -> bool {
    last_epoch != current_epoch || last != new
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
    tx: &tokio_mpsc::UnboundedSender<MediaSnapshotDelivery>,
    waker: &TerminalWaker,
    was_showing: bool,
    state: &mut WatchState,
) -> Option<bool> {
    let read = read_snapshot(client).await;
    let read_ok = read.is_ok();
    let mut snap = match read {
        Ok(snap) => {
            state.freshness.on_success();
            snap
        }
        Err(()) => match state
            .freshness
            .on_failure(tokio::time::Instant::now(), state.grace)
        {
            Some(snap) => {
                tracing::debug!(target: "thegn::media", "media provider unavailable; stale badge expired");
                snap
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
            Err(()) => match state
                .freshness
                .on_failure(tokio::time::Instant::now(), state.grace)
            {
                Some(expired) => {
                    tracing::debug!(target: "thegn::media", "media provider unavailable; stale badge expired");
                    snap = expired;
                }
                None => return Some(was_showing),
            },
        }
    }
    let active = shows_badge(&snap);
    let epoch = crate::media_ctl::MEDIA_DELIVERY_EPOCH.load(Ordering::Acquire);
    if !should_deliver(&state.last_sent, state.last_epoch, epoch, &snap) {
        return Some(active);
    }
    if tx
        .send(MediaSnapshotDelivery {
            generation,
            snapshot: snap.clone(),
        })
        .is_err()
    {
        return None;
    }
    if let Err(error) = waker.wake() {
        tracing::debug!(target: "thegn::media", %error, "media watcher wake failed");
    }
    state.last_sent = snap;
    state.last_epoch = epoch;
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

async fn run(
    cfg: MediaConfig,
    generation: u64,
    tx: tokio_mpsc::UnboundedSender<MediaSnapshotDelivery>,
    waker: TerminalWaker,
) {
    let period = std::time::Duration::from_secs(cfg.poll_interval_secs.max(1));
    let max_backoff = std::time::Duration::from_secs(30);
    let mut backoff = std::time::Duration::from_secs(1);
    let grace = stale_grace(period);
    let mut state = WatchState {
        freshness: Freshness::default(),
        grace,
        last_sent: None,
        last_epoch: crate::media_ctl::MEDIA_DELIVERY_EPOCH
            .load(Ordering::Acquire)
            .wrapping_sub(1),
    };
    let mut active = false;

    loop {
        let Some(client) = crate::media_secret::client_for(&cfg).await else {
            // No backend yet (bus down, playerctl absent). Retry with backoff so
            // media appears once the transport comes up, rather than never.
            tracing::debug!(target: "thegn::media", "media backend unavailable; retrying");
            if let Some(snapshot) = state
                .freshness
                .on_failure(tokio::time::Instant::now(), state.grace)
            {
                active = shows_badge(&snapshot);
                let epoch = crate::media_ctl::MEDIA_DELIVERY_EPOCH.load(Ordering::Acquire);
                if should_deliver(&state.last_sent, state.last_epoch, epoch, &snapshot) {
                    if tx
                        .send(MediaSnapshotDelivery {
                            generation,
                            snapshot: snapshot.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    if let Err(error) = waker.wake() {
                        tracing::debug!(target: "thegn::media", %error, "media watcher wake failed");
                    }
                    state.last_sent = snapshot;
                    state.last_epoch = epoch;
                }
            }
            let retry_after = state
                .freshness
                .until_expiry(tokio::time::Instant::now(), state.grace)
                .map_or(backoff, |remaining| backoff.min(remaining));
            tokio::time::sleep(retry_after).await;
            backoff = (backoff * 2).min(max_backoff);
            continue;
        };
        // Refresh after resolution; retain `active` across failed resolutions
        // so a prior generation's badge follows the same stale-grace policy.
        active = match push_snapshot(&client, generation, &tx, &waker, active, &mut state).await {
            Some(a) => a,
            None => return, // receiver gone
        };

        if let Some(mut watch) = client.watch().await {
            let connected_at = tokio::time::Instant::now();
            // Native push path. Re-snapshot on each D-Bus signal; additionally,
            // *while no track is displayed*, run a slow safety poll so a failed
            // initial read self-heals. Once a badge shows, drop back to
            // signal-only (no idle polling during playback).
            let mut safety = interval(period);
            safety.tick().await; // consume the immediate first tick
            let stream_ended = loop {
                let changed = if active {
                    if let Some(delay) = state
                        .freshness
                        .until_expiry(tokio::time::Instant::now(), state.grace)
                    {
                        tokio::select! {
                            changed = watch.changed() => changed,
                            _ = tokio::time::sleep(delay) => true,
                        }
                    } else {
                        watch.changed().await
                    }
                } else {
                    tokio::select! {
                        c = watch.changed() => c,
                        _ = safety.tick() => true,
                    }
                };
                if !changed {
                    break true; // signal stream ended → reconnect
                }
                match push_snapshot(&client, generation, &tx, &waker, active, &mut state).await {
                    Some(a) => {
                        note_cleared(active, a);
                        active = a;
                    }
                    None => return, // receiver gone
                }
            };
            if stream_ended {
                tracing::debug!(target: "thegn::media", "MPRIS signal stream ended; reconnecting");
                backoff = backoff_after_stream(backoff, connected_at.elapsed());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        } else {
            // Poll path: backends without a signal stream (mpv / playerctl).
            let mut ticker = interval(period);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                match push_snapshot(&client, generation, &tx, &waker, active, &mut state).await {
                    Some(a) => {
                        note_cleared(active, a);
                        active = a;
                    }
                    None => return, // receiver gone
                }
            }
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
    rx: &mut tokio_mpsc::UnboundedReceiver<MediaSnapshotDelivery>,
    perf: &mut crate::perf::LoopPerf,
    enabled: bool,
    generation: u64,
    media: &mut Option<MediaState>,
    caps: &mut Option<thegn_core::media::MediaCaps>,
    coalesce_full: bool,
    last_full: &mut Option<std::time::Instant>,
) -> (bool, bool) {
    let mut newest = None;
    while let Ok(delivery) = rx.try_recv() {
        if delivery.generation != generation {
            continue;
        }
        newest = Some(delivery.snapshot);
    }
    let Some(newest) = newest else {
        return (false, false);
    };
    perf.tick(crate::perf::WakeSource::Refresh);
    let shown = if enabled { newest } else { None };
    let shown_state = shown.as_ref().map(|snapshot| snapshot.state.clone());
    let shown_caps = shown.as_ref().map(|snapshot| snapshot.caps);
    if *media == shown_state && *caps == shown_caps {
        return (false, false);
    }
    let badge_changed =
        media.as_ref().and_then(|m| m.badge()) != shown_state.as_ref().and_then(|m| m.badge());
    *media = shown_state;
    *caps = shown_caps;
    let (mut full, mut bars) = (false, false);
    if coalesce_full {
        if badge_changed
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
    use tokio::sync::mpsc;
    use tokio::time::MissedTickBehavior;

    use super::{
        FlapAction, Freshness, MediaSnapshotDelivery, STREAM_STABILITY, abort_and_join,
        backoff_after_stream, drain_snapshots, flap_action, interval, should_confirm_flap,
        should_deliver, stale_grace,
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
        assert!(!should_deliver(&snapshot, 4, 4, &snapshot));
        assert!(should_deliver(&snapshot, 4, 5, &snapshot));
        assert!(should_deliver(&snapshot, 4, 4, &None));
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
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut perf = crate::perf::LoopPerf::new();
        let mut media = None;
        let mut caps = None;
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
        tx.send(MediaSnapshotDelivery {
            generation: 1,
            snapshot: Some(current.clone()),
        })
        .unwrap();
        tx.send(MediaSnapshotDelivery {
            generation: 0,
            snapshot: Some(stale),
        })
        .unwrap();

        drain_snapshots(
            &mut rx,
            &mut perf,
            true,
            1,
            &mut media,
            &mut caps,
            false,
            &mut last_full,
        );

        assert_eq!(media, Some(current.state));
        assert_eq!(caps, Some(current.caps));
        assert!(last_full.is_none());
    }

    #[test]
    fn drain_applies_only_the_newest_current_generation_snapshot() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut perf = crate::perf::LoopPerf::new();
        let mut media = Some(MediaState {
            player: "player".into(),
            title: "first".into(),
            ..Default::default()
        });
        let mut caps = Some(MediaCaps::default());
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
            tx.send(MediaSnapshotDelivery {
                generation: 7,
                snapshot: Some(snapshot),
            })
            .unwrap();
        }
        let repaint = drain_snapshots(
            &mut rx,
            &mut perf,
            true,
            7,
            &mut media,
            &mut caps,
            false,
            &mut last_full,
        );
        assert_eq!(repaint, (false, false));
        assert_eq!(media, Some(first.state));
    }
}
