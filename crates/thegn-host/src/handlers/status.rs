//! Daemon/program status: the pure state-mapping for the far-right statusbar
//! chip and the off-loop snapshot builder that fills [`DaemonStatus`] from the
//! control-plane store. The snapshot read happens on the ticker thread (never
//! the event loop); the mapping is a pure function so it can be unit-tested.

use std::collections::HashSet;

use thegn_core::store::ControlStore;

use crate::chrome::{DaemonChipState, DaemonStatus};

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
    } else if status.remote {
        DaemonChipState::Client
    } else if persistent_pane {
        DaemonChipState::Persist
    } else {
        DaemonChipState::NonPersist
    }
}

/// Build a [`DaemonStatus`] for `scope` (the canonical state dir) from the
/// control store. Reads the live daemon row plus its leases to derive the
/// session / attached-client counts. Returns an absent status (`present:
/// false`) when no live daemon is registered. Sync + cheap — ticker-thread only.
pub(crate) fn snapshot(db: &dyn ControlStore, scope: &str, now_ms: i64) -> DaemonStatus {
    use thegn_svc::control::client::DAEMON_HEARTBEAT_TTL_MS;
    let Some(row) = db
        .live_daemons(scope, now_ms, DAEMON_HEARTBEAT_TTL_MS)
        .ok()
        .and_then(|mut rows| rows.drain(..).next())
    else {
        return DaemonStatus::default();
    };
    // Distinct sessions and the attached-client subset, from the lease table.
    let leases = db.leases(&row.daemon_id).unwrap_or_default();
    let sessions: HashSet<&str> = leases.iter().map(|l| l.session_id.as_str()).collect();
    let attached = leases.iter().filter(|l| l.kind == "attached").count();
    DaemonStatus {
        present: true,
        pid: u32::try_from(row.pid).ok(),
        version: row.version,
        hostname: row.hostname,
        endpoint: row.endpoint,
        tcp_addr: row.tcp_addr.unwrap_or_default(),
        started_at_ms: row.started_at,
        sessions: sessions.len(),
        attached,
        // A daemon found under the local scope is reached over the local
        // socket; remote-client detection is set by the caller when this
        // instance attaches to a daemon on another machine.
        remote: false,
    }
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
}
