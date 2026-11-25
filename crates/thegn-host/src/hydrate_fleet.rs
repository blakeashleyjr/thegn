//! The masthead `fleet` badge's read, off the event loop.
//!
//! Threading contract: one `tokio::task::spawn_blocking` pass runs
//! [`crate::fleet_read::read`] (a provider API round trip per env, 20 s budget
//! each, plus a DB read for the worktree join), sends one
//! [`RefreshKind::Fleet`] through the schedule fence and pulses the waker.
//! No new thread and no new wake source: the poll is asked for by the shared
//! ticker's `FleetPoll` slot, which exists only while `fleet` is placed in a
//! bar and a provider env is configured.
//!
//! Rules:
//! 1. **Single flight.** A process-wide flag drops a poll while one is still
//!    reading; a slow provider can never stack reads behind itself.
//! 2. **Thinned while idle.** [`should_poll`] lets the five-minute slot
//!    through only while something runs or the popup is open; a fleet of
//!    stopped (or no) VMs is re-read every 30 minutes.
//! 3. **Errors are data.** A failed read becomes a snapshot whose
//!    `view.partial` carries the reason, shown in the popup. Never a panic,
//!    never a toast.

use std::sync::atomic::{AtomicBool, Ordering};

use termwiz::terminal::TerminalWaker;
use thegn_core::config::Config;
use thegn_core::placement::fleet_cost::{self, FleetView, PartialEnv};
use tokio::sync::mpsc as tokio_mpsc;

use crate::fleet_read::FleetSnapshot;
use crate::hydrate::RefreshKind;

/// Re-read interval while nothing runs and the popup is closed.
pub(crate) const IDLE_REPOLL_MS: i64 = 30 * 60 * 1000;

/// Set while a fleet read is in flight (rule 1).
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// What the last delivered read looked like, for [`should_poll`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LastRead {
    /// Epoch ms the read was generated.
    pub(crate) at_ms: i64,
    /// Rows not in a stopped state.
    pub(crate) running: usize,
}

impl LastRead {
    pub(crate) fn of(snap: &FleetSnapshot) -> Self {
        Self {
            at_ms: snap.generated_ms,
            // Metered AI burning in the trailing hour keeps the cadence live.
            running: running_rows(&snap.view) + usize::from(snap.ai.burn_hourly > 0.0),
        }
    }
}

/// Rows a provider lists in a state that still burns compute.
pub(crate) fn running_rows(view: &FleetView) -> usize {
    view.rows
        .iter()
        .filter(|row| !fleet_cost::is_stopped_state(row.state.as_deref()))
        .count()
}

/// Everything one poll needs, gathered on the loop (cheap clones only).
pub(crate) struct PollParams {
    pub(crate) cfg: Config,
    pub(crate) tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    pub(crate) waker: TerminalWaker,
    pub(crate) generation: Option<crate::hydrate_schedule::ScheduleFence>,
    pub(crate) last: Option<LastRead>,
    pub(crate) popup_open: bool,
    pub(crate) now_ms: i64,
}

/// Whether a `FleetPoll` slot should actually read: always on the first read;
/// at the slot's own cadence while the last read listed a running row or the
/// popup is open; otherwise only once [`IDLE_REPOLL_MS`] has passed.
pub(crate) fn should_poll(last: Option<LastRead>, now_ms: i64, popup_open: bool) -> bool {
    let Some(last) = last else {
        return true;
    };
    if last.running > 0 || popup_open {
        return true;
    }
    now_ms.saturating_sub(last.at_ms) >= IDLE_REPOLL_MS
}

/// Consider a fleet read: start one unless [`should_poll`] declines or a
/// read is already in flight.
pub(crate) fn spawn_poll(params: PollParams) {
    if !should_poll(params.last, params.now_ms, params.popup_open) {
        return;
    }
    if IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let PollParams {
        cfg,
        tx,
        waker,
        generation,
        ..
    } = params;
    tokio::task::spawn_blocking(move || {
        let snap = {
            // Cleared on unwind too, so a panicking provider cannot wedge the
            // badge for the rest of the session.
            let _flight = FlightGuard;
            read_or_partial(&cfg)
        };
        deliver(&tx, &waker, snap, generation.as_ref());
    });
}

/// Clears [`IN_FLIGHT`] when the read ends, however it ends.
struct FlightGuard;

impl Drop for FlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// [`crate::fleet_read::read`], with a whole-read failure folded into the
/// snapshot (rule 3).
fn read_or_partial(cfg: &Config) -> FleetSnapshot {
    match crate::fleet_read::read(cfg) {
        Ok(snap) => snap,
        Err(error) => {
            tracing::warn!(target: "thegn::fleet", error = %format!("{error:#}"), "fleet read failed");
            failed_snapshot(&format!("{error:#}"), thegn_core::util::now_ms())
        }
    }
}

/// A snapshot that carries only the failure reason.
fn failed_snapshot(reason: &str, now_ms: i64) -> FleetSnapshot {
    let view = FleetView {
        partial: vec![PartialEnv {
            env: "*".into(),
            reason: format!("unavailable: {reason}"),
        }],
        ..FleetView::default()
    };
    FleetSnapshot {
        generated_ms: now_ms,
        cost: fleet_cost::fleet_cost(
            &view,
            fleet_cost::BUNDLED_PRICES,
            &Default::default(),
            now_ms,
        ),
        view,
        ai: Default::default(),
    }
}

fn deliver(
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    snap: FleetSnapshot,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
) {
    let Some(kind) =
        crate::hydrate_schedule::scheduled_delivery(generation, RefreshKind::Fleet(Box::new(snap)))
    else {
        return;
    };
    if tx.send(kind).is_err() {
        // The loop is gone: nothing left to wake.
        return;
    }
    if let Err(error) = waker.wake() {
        // best-effort: a missed pulse costs at most a deferred paint.
        tracing::debug!(target: "thegn::fleet", error = %error, "waker pulse failed");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FleetSnapshot, IDLE_REPOLL_MS, LastRead, failed_snapshot, running_rows, should_poll,
    };
    use thegn_core::placement::fleet_cost::{FleetRow, FleetView};

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn should_poll_table() {
        let idle = |at_ms| Some(LastRead { at_ms, running: 0 });
        let busy = Some(LastRead {
            at_ms: NOW - 1,
            running: 2,
        });
        // (last, now, popup open) -> poll
        let cases = [
            (None, NOW, false, true),
            (None, NOW, true, true),
            (busy, NOW, false, true),
            (idle(NOW - 60_000), NOW, false, false),
            (idle(NOW - 60_000), NOW, true, true),
            (idle(NOW - IDLE_REPOLL_MS + 1), NOW, false, false),
            (idle(NOW - IDLE_REPOLL_MS), NOW, false, true),
            (idle(NOW - 2 * IDLE_REPOLL_MS), NOW, false, true),
            // A clock that went backwards is never "due" by age alone.
            (idle(NOW + 60_000), NOW, false, false),
        ];
        for (i, (last, now, popup, want)) in cases.into_iter().enumerate() {
            assert_eq!(should_poll(last, now, popup), want, "case {i}");
        }
    }

    #[test]
    fn last_read_counts_only_running_rows() {
        let row = |state: Option<&str>| FleetRow {
            provider: "fly".into(),
            env: "e".into(),
            id: format!("{state:?}"),
            state: state.map(String::from),
            ..FleetRow::default()
        };
        let view = FleetView {
            rows: vec![row(Some("started")), row(Some("stopped")), row(None)],
            ..FleetView::default()
        };
        assert_eq!(running_rows(&view), 2);
        let snap = FleetSnapshot {
            generated_ms: NOW,
            view,
            ..FleetSnapshot::default()
        };
        assert_eq!(
            LastRead::of(&snap),
            LastRead {
                at_ms: NOW,
                running: 2
            }
        );
    }

    #[test]
    fn a_failed_read_is_a_partial_snapshot_not_an_error() {
        let snap = failed_snapshot("no runtime", NOW);
        assert_eq!(snap.generated_ms, NOW);
        assert!(snap.view.rows.is_empty());
        assert_eq!(snap.view.partial.len(), 1);
        assert!(snap.view.partial[0].reason.contains("no runtime"));
        assert!(snap.cost.total_hourly.abs() < 1e-9);
        assert_eq!(LastRead::of(&snap).running, 0);
    }
}
