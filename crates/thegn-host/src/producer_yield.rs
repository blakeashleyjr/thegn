//! UI-side yield to the daemon's headless producers (THE-2208).
//!
//! Each UI producer body (off-loop, on `sched::spawn_bg`) first asks
//! [`daemon_owns`]; while the daemon holds that kind's live lease the UI skips
//! the network fetch and only reloads its panel from the DB cache. Off-loop
//! only: it opens and reads the DB.
//!
//! The UI never takes a lease. Any doubt (the DB will not open, the read
//! fails, no row, an expired or implausible row, another holder, a dead
//! holder pid) answers `false`, so the UI
//! polls as it did before: the safe direction for display, and a duplicate
//! event is deduped by `once_per_key` / `emit_once`.
#![allow(
    dead_code,
    reason = "the UI producer bodies call it from b161 lanes 6-8"
)]

use thegn_core::producer_lease::{DAEMON_HOLDER, LeaseRow, ProducerKind, live, plausible};

/// True while the daemon holds `kind`'s live lease. Blocking (opens the DB):
/// call it off the UI loop only.
pub(crate) fn daemon_owns(kind: ProducerKind) -> bool {
    match thegn_core::db::Db::open().and_then(|db| db.producer_lease(kind)) {
        Ok(row) => {
            row.is_some_and(|row| owned_by_daemon(&row, now_ms(), &crate::platform::pid_alive))
        }
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

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The decision, separated from the DB read: only a live, plausible row
/// held by the daemon whose pid is still `alive` yields. Another holder, an
/// expired row, a row whose expiry is further ahead than any lease could be
/// (the clock stepped backwards after a crash) or a dead holder pid do not.
pub(crate) fn owned_by_daemon(row: &LeaseRow, now_ms: i64, alive: &dyn Fn(i64) -> bool) -> bool {
    row.holder == DAEMON_HOLDER
        && live(row, now_ms)
        && plausible(row, now_ms)
        && alive(row.holder_pid)
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

    fn alive(_: i64) -> bool {
        true
    }

    fn dead(_: i64) -> bool {
        false
    }

    #[test]
    fn only_a_live_daemon_lease_yields() {
        assert!(owned_by_daemon(&row(DAEMON_HOLDER, 100), 99, &alive));
        assert!(!owned_by_daemon(&row(DAEMON_HOLDER, 100), 100, &alive));
        assert!(!owned_by_daemon(&row("ui", 100), 0, &alive));
    }

    #[test]
    fn a_dead_pid_or_an_implausible_expiry_does_not_yield() {
        assert!(!owned_by_daemon(&row(DAEMON_HOLDER, 100), 99, &dead));
        let far = thegn_core::producer_lease::MAX_TTL_MS + 1_000;
        assert!(
            !owned_by_daemon(&row(DAEMON_HOLDER, far), 999, &alive),
            "a backward clock step left an expiry beyond any TTL"
        );
        assert!(owned_by_daemon(&row(DAEMON_HOLDER, far), 1_000, &alive));
    }

    #[test]
    fn daemon_owns_reads_the_lease_from_the_state_db() {
        let _state = crate::testenv::IsolatedState::new();
        assert!(!daemon_owns(ProducerKind::PrPoll), "no row");
        let db = thegn_core::db::Db::open().expect("open isolated db");
        // Our own pid: the holder must be alive to yield.
        let pid = i64::from(std::process::id());
        assert!(
            db.try_acquire_producer_lease(
                ProducerKind::PrPoll,
                DAEMON_HOLDER,
                pid,
                now_ms(),
                60_000
            )
            .unwrap()
        );
        assert!(daemon_owns(ProducerKind::PrPoll));
        assert!(!daemon_owns(ProducerKind::Tracker), "per kind");
        assert!(
            db.release_producer_lease(ProducerKind::PrPoll, DAEMON_HOLDER, pid)
                .unwrap()
        );
        assert!(!daemon_owns(ProducerKind::PrPoll));
    }
}
