//! SQLite persistence for producer leases (schema v114, THE-2208). See
//! `producer_lease.rs` for the protocol.

use anyhow::Result;
use rusqlite::{OptionalExtension, named_params, params};

use crate::db::Db;
use crate::producer_lease::{LeaseRow, ProducerKind};

const LEASE_COLS: &str = "producer, holder, holder_pid, acquired_at_ms, expires_at_ms";

fn map_lease(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<LeaseRow>> {
    let producer: String = row.get(0)?;
    let Some(kind) = ProducerKind::parse(&producer) else {
        return Ok(None);
    };
    Ok(Some(LeaseRow {
        kind,
        holder: row.get(1)?,
        holder_pid: row.get(2)?,
        acquired_at_ms: row.get(3)?,
        expires_at_ms: row.get(4)?,
    }))
}

impl Db {
    /// Acquire or renew `kind`'s lease for `(holder, pid)` until
    /// `now_ms + ttl_ms`. Succeeds on an empty slot, an expired lease, or a
    /// lease this same holder already owns (a renewal keeps its original
    /// `acquired_at_ms`). `Ok(false)` while another holder's lease is live.
    pub fn try_acquire_producer_lease(
        &self,
        kind: ProducerKind,
        holder: &str,
        pid: i64,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<bool> {
        let expires = now_ms.saturating_add(ttl_ms.max(0));
        let changed = self.conn().execute(
            "INSERT INTO producer_leases (producer, holder, holder_pid, acquired_at_ms, expires_at_ms)
             VALUES (:producer, :holder, :pid, :now, :expires)
             ON CONFLICT(producer) DO UPDATE SET
               holder = excluded.holder,
               holder_pid = excluded.holder_pid,
               acquired_at_ms = CASE
                 WHEN producer_leases.holder = excluded.holder
                  AND producer_leases.holder_pid = excluded.holder_pid
                 THEN producer_leases.acquired_at_ms
                 ELSE excluded.acquired_at_ms END,
               expires_at_ms = excluded.expires_at_ms
             WHERE producer_leases.expires_at_ms <= :now
                OR (producer_leases.holder = :holder AND producer_leases.holder_pid = :pid)",
            named_params! {
                ":producer": kind.as_str(),
                ":holder": holder,
                ":pid": pid,
                ":now": now_ms,
                ":expires": expires,
            },
        )?;
        Ok(changed == 1)
    }

    /// Drop `kind`'s lease if `(holder, pid)` holds it. `Ok(false)` when
    /// there is no lease or someone else holds it.
    pub fn release_producer_lease(
        &self,
        kind: ProducerKind,
        holder: &str,
        pid: i64,
    ) -> Result<bool> {
        let changed = self.conn().execute(
            "DELETE FROM producer_leases WHERE producer = ?1 AND holder = ?2 AND holder_pid = ?3",
            params![kind.as_str(), holder, pid],
        )?;
        Ok(changed == 1)
    }

    /// `kind`'s lease row, live or expired.
    pub fn producer_lease(&self, kind: ProducerKind) -> Result<Option<LeaseRow>> {
        let row = self
            .conn()
            .query_row(
                &format!("SELECT {LEASE_COLS} FROM producer_leases WHERE producer = ?1"),
                params![kind.as_str()],
                map_lease,
            )
            .optional()?;
        Ok(row.flatten())
    }

    /// Every lease row, ordered by producer name.
    pub fn producer_leases(&self) -> Result<Vec<LeaseRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {LEASE_COLS} FROM producer_leases ORDER BY producer"
        ))?;
        let rows = stmt.query_map([], map_lease)?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(lease) = row? {
                out.push(lease);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::producer_lease::DAEMON_HOLDER;

    const TTL: i64 = 60_000;

    #[test]
    fn acquires_an_empty_slot() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.producer_lease(ProducerKind::Tracker).unwrap(), None);
        assert!(
            db.try_acquire_producer_lease(ProducerKind::Tracker, DAEMON_HOLDER, 7, 1_000, TTL)
                .unwrap()
        );
        let row = db.producer_lease(ProducerKind::Tracker).unwrap().unwrap();
        assert_eq!(
            row,
            LeaseRow {
                kind: ProducerKind::Tracker,
                holder: DAEMON_HOLDER.into(),
                holder_pid: 7,
                acquired_at_ms: 1_000,
                expires_at_ms: 61_000,
            }
        );
    }

    #[test]
    fn a_second_holder_is_refused_while_the_lease_is_live() {
        let db = Db::open_memory().unwrap();
        assert!(
            db.try_acquire_producer_lease(ProducerKind::PrPoll, DAEMON_HOLDER, 7, 1_000, TTL)
                .unwrap()
        );
        assert!(
            !db.try_acquire_producer_lease(ProducerKind::PrPoll, "ui", 9, 2_000, TTL)
                .unwrap()
        );
        assert!(
            !db.try_acquire_producer_lease(ProducerKind::PrPoll, DAEMON_HOLDER, 8, 2_000, TTL)
                .unwrap(),
            "same holder name, other pid, is another holder"
        );
        let row = db.producer_lease(ProducerKind::PrPoll).unwrap().unwrap();
        assert_eq!((row.holder.as_str(), row.holder_pid), (DAEMON_HOLDER, 7));
    }

    #[test]
    fn the_same_holder_renews_and_keeps_acquired_at() {
        let db = Db::open_memory().unwrap();
        assert!(
            db.try_acquire_producer_lease(ProducerKind::Mentions, DAEMON_HOLDER, 7, 1_000, TTL)
                .unwrap()
        );
        assert!(
            db.try_acquire_producer_lease(ProducerKind::Mentions, DAEMON_HOLDER, 7, 30_000, TTL)
                .unwrap()
        );
        let row = db.producer_lease(ProducerKind::Mentions).unwrap().unwrap();
        assert_eq!(row.acquired_at_ms, 1_000);
        assert_eq!(row.expires_at_ms, 90_000);
    }

    #[test]
    fn another_holder_takes_over_after_expiry() {
        let db = Db::open_memory().unwrap();
        assert!(
            db.try_acquire_producer_lease(ProducerKind::PrQueue, DAEMON_HOLDER, 7, 1_000, TTL)
                .unwrap()
        );
        assert!(
            db.try_acquire_producer_lease(ProducerKind::PrQueue, DAEMON_HOLDER, 8, 61_000, TTL)
                .unwrap()
        );
        let row = db.producer_lease(ProducerKind::PrQueue).unwrap().unwrap();
        assert_eq!(row.holder_pid, 8);
        assert_eq!(
            row.acquired_at_ms, 61_000,
            "a takeover restarts acquired_at"
        );
        assert_eq!(row.expires_at_ms, 121_000);
    }

    #[test]
    fn release_by_a_non_holder_is_refused() {
        let db = Db::open_memory().unwrap();
        assert!(
            !db.release_producer_lease(ProducerKind::Tracker, DAEMON_HOLDER, 7)
                .unwrap()
        );
        assert!(
            db.try_acquire_producer_lease(ProducerKind::Tracker, DAEMON_HOLDER, 7, 1_000, TTL)
                .unwrap()
        );
        assert!(
            !db.release_producer_lease(ProducerKind::Tracker, "ui", 7)
                .unwrap()
        );
        assert!(
            !db.release_producer_lease(ProducerKind::Tracker, DAEMON_HOLDER, 9)
                .unwrap()
        );
        assert!(db.producer_lease(ProducerKind::Tracker).unwrap().is_some());
        assert!(
            db.release_producer_lease(ProducerKind::Tracker, DAEMON_HOLDER, 7)
                .unwrap()
        );
        assert_eq!(db.producer_lease(ProducerKind::Tracker).unwrap(), None);
    }

    #[test]
    fn lists_every_lease() {
        let db = Db::open_memory().unwrap();
        for (i, kind) in ProducerKind::ALL.into_iter().enumerate() {
            assert!(
                db.try_acquire_producer_lease(kind, DAEMON_HOLDER, i as i64, 0, TTL)
                    .unwrap()
            );
        }
        let kinds: Vec<_> = db
            .producer_leases()
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                ProducerKind::Mentions,
                ProducerKind::PrPoll,
                ProducerKind::PrQueue,
                ProducerKind::Tracker,
            ]
        );
    }
}
