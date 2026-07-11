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

use tokio::sync::mpsc as tokio_mpsc;

use termwiz::terminal::TerminalWaker;
use thegn_core::config::MediaConfig;
use thegn_core::media::MediaState;

/// Spawn the now-playing watcher for `cfg`. Returns the task handle so the caller
/// can abort it on a config/player change; `None` when media is disabled.
pub(crate) fn spawn(
    cfg: MediaConfig,
    tx: tokio_mpsc::UnboundedSender<Option<MediaState>>,
    waker: TerminalWaker,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.enabled {
        return None;
    }
    Some(tokio::spawn(run(cfg, tx, waker)))
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

/// Read one snapshot, logging read errors instead of swallowing them.
async fn read_snapshot(client: &thegn_media::MediaClient) -> Option<MediaState> {
    match client.snapshot().await {
        Ok(snap) => snap,
        Err(e) => {
            tracing::debug!(target: "thegn::media", error = %e, "media snapshot failed");
            None
        }
    }
}

/// Whether a snapshot would render a badge (i.e. an active player).
fn shows_badge(snap: &Option<MediaState>) -> bool {
    snap.as_ref().and_then(|s| s.badge()).is_some()
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
    tx: &tokio_mpsc::UnboundedSender<Option<MediaState>>,
    waker: &TerminalWaker,
    was_showing: bool,
) -> Option<bool> {
    let mut snap = read_snapshot(client).await;
    if flap_action(was_showing, shows_badge(&snap)) == FlapAction::Confirm {
        tokio::time::sleep(CONFIRM_DELAY).await;
        let confirm = read_snapshot(client).await;
        if shows_badge(&confirm) {
            tracing::debug!(target: "thegn::media", "media badge held (transient empty ignored)");
        } else {
            tracing::debug!(target: "thegn::media", "media badge cleared (empty confirmed)");
        }
        snap = confirm;
    }
    let active = shows_badge(&snap);
    if tx.send(snap).is_err() {
        return None;
    }
    let _ = waker.wake();
    Some(active)
}

/// Log the badge-clearing transition (a track was showing, this push hides it).
/// `Ok(None)` pushes are otherwise silent — e.g. the aggregate when every child
/// failed, or MPD answering "daemon away" — so without this a vanishing badge
/// leaves no trace in `THEGN_LOG=thegn::media=debug` logs.
fn note_cleared(prev_active: bool, now_active: bool) {
    if prev_active && !now_active {
        tracing::debug!(
            target: "thegn::media",
            "now-playing cleared (snapshot returned none/stopped)"
        );
    }
}

async fn run(
    cfg: MediaConfig,
    tx: tokio_mpsc::UnboundedSender<Option<MediaState>>,
    waker: TerminalWaker,
) {
    let period = std::time::Duration::from_secs(cfg.poll_interval_secs.max(1));
    let max_backoff = std::time::Duration::from_secs(30);
    let mut backoff = std::time::Duration::from_secs(1);

    loop {
        let Some(client) = thegn_media::client_for(&cfg.resolve_opts()).await else {
            // No backend yet (bus down, playerctl absent). Retry with backoff so
            // media appears once the transport comes up, rather than never.
            tracing::debug!(target: "thegn::media", "media backend unavailable; retrying");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(max_backoff);
            continue;
        };
        backoff = std::time::Duration::from_secs(1);

        // Initial snapshot. Nothing is shown yet, so no disappear to confirm.
        let mut active = match push_snapshot(&client, &tx, &waker, false).await {
            Some(a) => a,
            None => return, // receiver gone
        };

        if let Some(mut watch) = client.watch().await {
            // Native push path. Re-snapshot on each D-Bus signal; additionally,
            // *while no track is displayed*, run a slow safety poll so a failed
            // initial read self-heals. Once a badge shows, drop back to
            // signal-only (no idle polling during playback).
            let mut safety = tokio::time::interval(period);
            safety.tick().await; // consume the immediate first tick
            let stream_ended = loop {
                let changed = if active {
                    watch.changed().await
                } else {
                    tokio::select! {
                        c = watch.changed() => c,
                        _ = safety.tick() => true,
                    }
                };
                if !changed {
                    break true; // signal stream ended → reconnect
                }
                match push_snapshot(&client, &tx, &waker, active).await {
                    Some(a) => {
                        note_cleared(active, a);
                        active = a;
                    }
                    None => return, // receiver gone
                }
            };
            if stream_ended {
                tracing::debug!(target: "thegn::media", "MPRIS signal stream ended; reconnecting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        } else {
            // Poll path: backends without a signal stream (mpv / playerctl).
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await; // consume the immediate first tick
            loop {
                ticker.tick().await;
                match push_snapshot(&client, &tx, &waker, active).await {
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
/// on screen: while the Media panel section or Now-Playing overlay is open
/// (`coalesce_full` — they show the live position stamp) coalesce full frames
/// to ~1/s, repainting immediately only when the statusbar badge text changed;
/// with both closed, a badge change takes the cheap bars path and a
/// position-only change repaints nothing.
///
/// Returns `(full, bars)` repaint intent for the loop's `dirty` / `bars_dirty`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn drain_snapshots(
    rx: &mut tokio_mpsc::UnboundedReceiver<Option<MediaState>>,
    perf: &mut crate::perf::LoopPerf,
    enabled: bool,
    show_art: bool,
    media: &mut Option<MediaState>,
    overlay: &mut Option<crate::media_overlay::MediaOverlay>,
    art_tx: &tokio_mpsc::UnboundedSender<crate::media_art::ArtMosaic>,
    waker: &TerminalWaker,
    coalesce_full: bool,
    last_full: &mut Option<std::time::Instant>,
) -> (bool, bool) {
    let (mut full, mut bars) = (false, false);
    while let Ok(snap) = rx.try_recv() {
        perf.tick(crate::perf::WakeSource::Refresh);
        let shown = if enabled { snap } else { None };
        if *media == shown {
            continue;
        }
        let badge_changed =
            media.as_ref().and_then(|m| m.badge()) != shown.as_ref().and_then(|m| m.badge());
        *media = shown;
        if let Some(ov) = overlay.as_mut() {
            ov.snapshot = media.clone();
            if let Some(url) = ov.wants_art(show_art) {
                crate::media_art::spawn_fetch(
                    url,
                    crate::media_overlay::ART_COLS,
                    crate::media_overlay::ART_ROWS,
                    art_tx.clone(),
                    waker.clone(),
                );
            }
        }
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
    }
    (full, bars)
}

#[cfg(test)]
mod tests {
    use super::{FlapAction, flap_action};

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
}
