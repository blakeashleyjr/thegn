//! UI-side yield to the daemon's headless producers (THE-2208).
//!
//! Each UI producer body (off-loop, on `sched::spawn_bg`) first asks
//! [`daemon_owns`]; while the daemon holds that kind's live lease the UI skips
//! the network fetch and only reloads its panel from the DB cache. Off-loop
//! only: it opens and reads the DB.
//!
//! The UI never takes a lease. Any doubt (the DB will not open, the read
//! fails, no row, an expired row, another holder) answers `false`, so the UI
//! polls as it did before: the safe direction for display, and a duplicate
//! event is deduped by `once_per_key` / `emit_once`.
#![allow(
    dead_code,
    reason = "the UI producer bodies call it from b161 lanes 6-8"
)]

use thegn_core::producer_lease::{DAEMON_HOLDER, LeaseRow, ProducerKind, live};

/// True while the daemon holds `kind`'s live lease. Blocking (opens the DB):
/// call it off the UI loop only.
pub(crate) fn daemon_owns(kind: ProducerKind) -> bool {
    let row = thegn_core::db::Db::open().and_then(|db| db.producer_lease(kind));
    owned_by_daemon(row, now_ms())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The decision, separated from the DB read: only a live row held by the
/// daemon yields; an error, no row, an expired row or another holder do not.
fn owned_by_daemon(row: anyhow::Result<Option<LeaseRow>>, now_ms: i64) -> bool {
    match row {
        Ok(Some(row)) => row.holder == DAEMON_HOLDER && live(&row, now_ms),
        Ok(None) => false,
        Err(error) => {
            tracing::debug!(
                target: "thegn::producers",
                %error,
                "producer lease unreadable; the UI polls itself"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(holder: &str, expires_at_ms: i64) -> LeaseRow {
        LeaseRow {
            kind: ProducerKind::Tracker,
            holder: holder.into(),
            holder_pid: 1,
            acquired_at_ms: 0,
            expires_at_ms,
        }
    }

    #[test]
    fn daemon_owns_is_false_on_error() {
        assert!(!owned_by_daemon(Err(anyhow::anyhow!("db locked")), 0));
    }

    #[test]
    fn only_a_live_daemon_lease_yields() {
        assert!(owned_by_daemon(Ok(Some(row(DAEMON_HOLDER, 100))), 99));
        assert!(!owned_by_daemon(Ok(Some(row(DAEMON_HOLDER, 100))), 100));
        assert!(!owned_by_daemon(Ok(Some(row("ui", 100))), 0));
        assert!(!owned_by_daemon(Ok(None), 0));
    }

    #[test]
    fn daemon_owns_reads_the_lease_from_the_state_db() {
        let _state = crate::testenv::IsolatedState::new();
        assert!(!daemon_owns(ProducerKind::PrPoll), "no row");
        let db = thegn_core::db::Db::open().expect("open isolated db");
        assert!(
            db.try_acquire_producer_lease(ProducerKind::PrPoll, DAEMON_HOLDER, 7, now_ms(), 60_000)
                .unwrap()
        );
        assert!(daemon_owns(ProducerKind::PrPoll));
        assert!(!daemon_owns(ProducerKind::Tracker), "per kind");
        assert!(
            db.release_producer_lease(ProducerKind::PrPoll, DAEMON_HOLDER, 7)
                .unwrap()
        );
        assert!(!daemon_owns(ProducerKind::PrPoll));
    }
}
