//! Host-side glue between the core [`thegn_core::connectivity`] holder and the
//! refresh ticker: decides which background refreshes to *skip* while offline,
//! and drives the recovery re-probe. Kept out of the ratcheted `run.rs` so the
//! skip logic is a small, pure, unit-tested function.

use crate::hydrate::RefreshKind;
use std::sync::OnceLock;
use termwiz::terminal::TerminalWaker;
use thegn_core::connectivity::Connectivity;

/// Global waker pulsed on a connectivity transition, so the offline chip
/// repaints immediately instead of waiting for the next model tick.
static WAKER: OnceLock<TerminalWaker> = OnceLock::new();

/// Install the connectivity → UI transition bridge (called once at startup):
/// on every Offline↔Online edge, pulse the loop waker so the statusbar chip
/// updates promptly. The core holder also emits a `tracing` line per edge.
pub(crate) fn install_transition_waker(waker: TerminalWaker) {
    let _ = WAKER.set(waker);
    thegn_core::connectivity::set_on_transition(on_transition);
}

fn on_transition(_new: Connectivity) {
    if let Some(w) = WAKER.get() {
        // best-effort: a missed pulse just defers the chip to the next tick.
        let _ = w.wake();
    }
}

/// Spawn the offline recovery probe: one bounded PR-cache refresh against the
/// active worktree whose `panel.state` feeds the holder (via [`report_pr_panel`]),
/// flipping it back online on success. Reuses the existing PR path — no new
/// network op, no "which host to ping".
pub(crate) fn spawn_recovery_probe(
    cwd: std::path::PathBuf,
    cfg: &thegn_core::config::Config,
    waker: Option<TerminalWaker>,
) {
    crate::hydrate::spawn_pr_cache_refresh(cwd, cfg.issues.clone(), cfg.disk.clone(), waker);
}

/// Feed the connectivity holder from a PR-panel fetch result. The CLI PR path
/// (`github::pr_status_full`, run every 20s and as the offline recovery probe)
/// is the reliable internet-reachability signal: a definitive answer means
/// GitHub was reached (online); an `Offline` note is a dropped link.
pub(crate) fn report_pr_panel(state: &thegn_core::github::PanelState) {
    use thegn_core::github::PanelState;
    match state {
        PanelState::Pr(_) | PanelState::NoPr | PanelState::RateLimited => {
            thegn_core::connectivity::report_success()
        }
        PanelState::Offline => thegn_core::connectivity::report_failure(),
        _ => {}
    }
}

/// Whether a ticker-driven refresh should be skipped for the given connectivity
/// state. Only network-backed refreshes are gated, and only while offline:
///
/// - `Pr` / `PrQueue` / `Issues` / `Ci { force: false }` / `AutoFetch` — remote fetches;
///   skipped offline (the local sidebar hydration still runs; caches serve
///   stale, and the `↓behind` markers keep their last-known counts).
/// - `Ci { force: true }` — a user-initiated refresh (the `g` key); **never**
///   skipped, and doubles as a legible manual recovery probe.
/// - Everything else (`Model`, `Disk`, `HostHeal`, `MainRefMoved`, detail
///   payloads, `ConnRecover`, …) is local or already offline-aware — never gated.
///
/// Pure — no globals, no I/O — so `run.rs` reads `connectivity::current()` once
/// and passes it in, and the truth table is exhaustively testable.
pub(crate) fn should_skip_refresh(kind: &RefreshKind, state: Connectivity) -> bool {
    if state != Connectivity::Offline {
        return false;
    }
    matches!(
        kind,
        RefreshKind::Pr
            | RefreshKind::PrQueue
            | RefreshKind::Issues
            | RefreshKind::Ci { force: false }
            | RefreshKind::AutoFetch { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed_ci_detail() -> RefreshKind {
        // A non-network variant that isn't Copy — proves we never gate it.
        RefreshKind::MainRefMoved
    }

    #[test]
    fn online_and_unknown_skip_nothing() {
        for state in [Connectivity::Online, Connectivity::Unknown] {
            for kind in [
                RefreshKind::Pr,
                RefreshKind::PrQueue,
                RefreshKind::Issues,
                RefreshKind::Ci { force: false },
                RefreshKind::Ci { force: true },
                RefreshKind::AutoFetch { sweep: true },
                RefreshKind::Model,
                RefreshKind::Disk,
                RefreshKind::HostHeal,
                RefreshKind::ConnRecover,
            ] {
                assert!(
                    !should_skip_refresh(&kind, state),
                    "{kind:?} @ {state:?} must not skip"
                );
            }
        }
    }

    #[test]
    fn offline_skips_only_network_backstops() {
        let s = Connectivity::Offline;
        assert!(should_skip_refresh(&RefreshKind::Pr, s));
        // A PR-queue pass is entirely forge round trips; running it offline
        // would just record "unreachable" on every row and burn the backoff.
        assert!(should_skip_refresh(&RefreshKind::PrQueue, s));
        assert!(should_skip_refresh(&RefreshKind::Issues, s));
        assert!(should_skip_refresh(&RefreshKind::Ci { force: false }, s));
        // The remote poll is a network round trip — pointless while offline
        // (and its failure backoff would decay for no reason).
        assert!(should_skip_refresh(
            &RefreshKind::AutoFetch { sweep: true },
            s
        ));
        assert!(should_skip_refresh(
            &RefreshKind::AutoFetch { sweep: false },
            s
        ));
    }

    #[test]
    fn offline_never_skips_forced_ci_or_local() {
        let s = Connectivity::Offline;
        // A user-forced CI refresh always attempts (manual recovery probe).
        assert!(!should_skip_refresh(&RefreshKind::Ci { force: true }, s));
        // Local / already-offline-aware kinds always run.
        for kind in [
            RefreshKind::Model,
            RefreshKind::Disk,
            RefreshKind::HostHeal,
            RefreshKind::ConnRecover,
            boxed_ci_detail(),
        ] {
            assert!(!should_skip_refresh(&kind, s), "{kind:?} must not skip");
        }
    }
}
