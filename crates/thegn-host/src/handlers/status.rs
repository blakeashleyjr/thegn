//! Daemon/program status: the pure state-mapping for the far-right statusbar
//! chip, the off-loop snapshot builder that fills [`DaemonStatus`] from the
//! control-plane registry, and the on-demand probe that asks the daemon itself
//! for its live session list.
//!
//! Neither read touches the event loop: the snapshot runs on the ticker thread,
//! and [`probe_sessions`] runs on a tokio worker. The mapping is a pure function
//! so it can be unit-tested.

use thegn_core::store::ControlStore;
use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::{BarBadge, BarItemId, DaemonChipState, DaemonStatus};
use crate::detail::DaemonSessions;
use crate::hydrate::RefreshKind;

/// Resolve the always-on chip's glyph state from the cached daemon status and
/// whether the focused pane is daemon-backed.
///
/// Precedence: a daemon serving remote thin clients (`tcp_addr`) reads as
/// **Server**; an instance attached to a *remote* daemon reads as **Client**;
/// otherwise a daemon-backed focused pane is **Persist** and everything else is
/// **NonPersist** (the plain, always-shown default).
pub(crate) fn daemon_chip_state(status: &DaemonStatus, persistent_pane: bool) -> DaemonChipState {
    if status.present && !status.tcp_addr.is_empty() {
        DaemonChipState::Server
    } else if status.stale {
        // A registered daemon whose heartbeat went stale (crashed or wedged) is
        // an error, not an unremarkable stale value — it takes precedence over
        // the Persist/NonPersist relationship so a crash is visible without
        // `THEGN_LOG`. Additive on the same chip slot as the persistence states.
        DaemonChipState::Error
    } else if status.remote {
        DaemonChipState::Client
    } else if persistent_pane {
        DaemonChipState::Persist
    } else {
        DaemonChipState::NonPersist
    }
}

/// Build a [`DaemonStatus`] for `scope` (the canonical state dir) from the
/// control store — the daemon's **registry row**, nothing more. Returns an
/// absent status (`present: false`) when no live daemon is registered. Sync +
/// cheap — ticker-thread only.
///
/// Session counts are deliberately NOT derived here. They used to be read out
/// of the lease table, but the daemon only writes a lease for a *detached*
/// session (`kind = "relay"`) and deletes it again on attach, so a daemon busy
/// serving panes reported `0`; `kind = "attached"` leases are documented but
/// never written, so the attached count was structurally always `0`. The
/// daemon's in-memory session registry is the source of truth — see
/// [`probe_sessions`].
///
/// `Err` is a control-store read failure — distinct from "no daemon", so the
/// ticker keeps the last known row instead of silently downgrading the chip.
pub(crate) fn snapshot(
    db: &dyn ControlStore,
    scope: &str,
    now_ms: i64,
) -> anyhow::Result<DaemonStatus> {
    use thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS;
    let mut rows = db.live_daemons(scope, now_ms, DAEMON_HEARTBEAT_TTL_MS)?;
    let Some(row) = rows.drain(..).next() else {
        // No LIVE daemon. A crashed/wedged daemon leaves a registry row whose
        // heartbeat is past the TTL — surface that as an error state rather than
        // an unremarkable "no daemon". Newest stale row for this scope wins.
        let stale = db
            .daemons()?
            .into_iter()
            .filter(|r| r.scope == scope)
            .max_by_key(|r| r.heartbeat_at);
        return Ok(match stale {
            Some(row) => DaemonStatus {
                present: false,
                stale: true,
                pid: u32::try_from(row.pid).ok(),
                version: row.version,
                hostname: row.hostname,
                endpoint: row.endpoint,
                tcp_addr: row.tcp_addr.unwrap_or_default(),
                started_at_ms: row.started_at,
                heartbeat_at: row.heartbeat_at,
                daemon_id: row.daemon_id,
                scope: row.scope,
                remote: false,
            },
            None => DaemonStatus::default(),
        });
    };
    Ok(DaemonStatus {
        present: true,
        stale: false,
        pid: u32::try_from(row.pid).ok(),
        version: row.version,
        hostname: row.hostname,
        endpoint: row.endpoint,
        tcp_addr: row.tcp_addr.unwrap_or_default(),
        started_at_ms: row.started_at,
        heartbeat_at: row.heartbeat_at,
        daemon_id: row.daemon_id,
        scope: row.scope,
        // A daemon found under the local scope is reached over the local
        // socket; remote-client detection is set by the caller when this
        // instance attaches to a daemon on another machine.
        remote: false,
    })
}

/// Ask the daemon for its live session list and deliver it into the open status
/// modal via `RefreshKind::DaemonSessions`. No-op for any other bar item.
///
/// **Fired only from the chip-activation path** (a click, or `↵` on the focused
/// chip) — never on a timer. That is what keeps the 0%-idle invariant intact:
/// the control socket is touched exactly when a human is looking at the modal,
/// and the modal has already painted from the cached [`DaemonStatus`] by the
/// time this lands.
///
/// Uses `connect_daemon` (discovery + health probe), never `ensure_daemon`:
/// opening a status modal must not spawn a daemon as a side effect.
pub(crate) fn probe_sessions(
    id: &BarItemId,
    dcfg: &thegn_core::config::DaemonConfig,
    slot: &mut DaemonSessions,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
) {
    if !matches!(id, BarItemId::Badge(BarBadge::Persist)) {
        return;
    }
    // Flip to Probing *before* the modal is built so its first paint says so —
    // unless a live list is already held: a re-probe (the modal re-runs this
    // while open) keeps the table on screen, with its "as of" age, rather than
    // blanking it every few seconds.
    if !matches!(slot, DaemonSessions::Live(_)) {
        *slot = DaemonSessions::Probing;
    }
    let (tx, waker, dcfg) = (refresh_tx.clone(), waker.clone(), dcfg.clone());
    tokio::spawn(async move {
        let payload = match crate::daemon::client::connect_daemon(&dcfg).await {
            // A daemon that answers `/health` but fails `/v1/sessions` is a
            // real anomaly, not "no sessions" — fall back to Unknown so the
            // modal says so instead of claiming an empty daemon.
            Some(c) => c
                .sessions()
                .await
                .map_or(DaemonSessions::Unknown, DaemonSessions::Live),
            None => DaemonSessions::NoDaemon,
        };
        if tx
            .send(RefreshKind::DaemonSessions(Box::new(payload)))
            .is_ok()
        {
            let _ = waker.wake();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_state_precedence() {
        let mut s = DaemonStatus::default();
        // No daemon, inline pane → the plain default.
        assert_eq!(daemon_chip_state(&s, false), DaemonChipState::NonPersist);
        // Daemon-backed focused pane → Persist.
        assert_eq!(daemon_chip_state(&s, true), DaemonChipState::Persist);
        // Serving remote clients wins over pane persistence → Server.
        s.present = true;
        s.tcp_addr = "0.0.0.0:8484".into();
        assert_eq!(daemon_chip_state(&s, true), DaemonChipState::Server);
        // Remote attachment (no serve) → Client.
        s.tcp_addr.clear();
        s.remote = true;
        assert_eq!(daemon_chip_state(&s, false), DaemonChipState::Client);
    }

    #[test]
    fn stale_daemon_renders_error_over_persistence() {
        // A stale registry row (crashed/wedged daemon) is an error state, and it
        // wins over the Persist/NonPersist pane relationship so a crash is
        // visible without THEGN_LOG.
        let mut s = DaemonStatus {
            stale: true,
            ..Default::default()
        };
        assert_eq!(daemon_chip_state(&s, false), DaemonChipState::Error);
        assert_eq!(daemon_chip_state(&s, true), DaemonChipState::Error);
        // A live server still wins over stale (present + tcp_addr).
        s.present = true;
        s.tcp_addr = "0.0.0.0:8484".into();
        assert_eq!(daemon_chip_state(&s, false), DaemonChipState::Server);
    }
}
