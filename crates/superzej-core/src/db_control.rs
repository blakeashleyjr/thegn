//! v40: the control-plane registry — `daemons` (running pane daemons),
//! `session_leases` (grace periods keeping detached PTYs warm), and `pairings`
//! (hashed scoped tokens for thin clients). SQLite impl of
//! [`crate::store::ControlStore`].
//!
//! Secrets never land here in plaintext: the caller (host/svc, which has the
//! CSPRNG and the hasher) stores only the sha-256 hex of a token's secret half;
//! this layer stores + matches opaque strings so it stays pure and testable.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::Db;
use crate::store::{ControlStore, DaemonRow, LeaseRow, PairingRow};

/// Create the control-plane tables. Idempotent (`IF NOT EXISTS`), so re-running
/// an already-migrated DB is a no-op.
pub(crate) fn migrate_v40(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS daemons (
            daemon_id    TEXT PRIMARY KEY,
            pid          INTEGER NOT NULL,
            scope        TEXT NOT NULL,
            endpoint     TEXT NOT NULL,
            tcp_addr     TEXT,
            hostname     TEXT NOT NULL,
            version      TEXT NOT NULL DEFAULT '',
            started_at   INTEGER NOT NULL,
            heartbeat_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_daemons_scope ON daemons(scope);
        CREATE TABLE IF NOT EXISTS session_leases (
            lease_id   INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            daemon_id  TEXT NOT NULL,
            client_id  TEXT,
            kind       TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            expires_at INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_leases_daemon  ON session_leases(daemon_id, expires_at);
        CREATE INDEX IF NOT EXISTS idx_leases_session ON session_leases(session_id);
        CREATE TABLE IF NOT EXISTS pairings (
            pairing_id  TEXT PRIMARY KEY,
            kind        TEXT NOT NULL,
            token_hash  TEXT NOT NULL,
            scope       TEXT NOT NULL,
            label       TEXT NOT NULL DEFAULT '',
            parent_id   TEXT,
            created_at  INTEGER NOT NULL,
            expires_at  INTEGER,
            redeemed_at INTEGER,
            revoked_at  INTEGER
        );
        "#,
    )?;
    Ok(())
}

fn daemon_row(r: &Row<'_>) -> rusqlite::Result<DaemonRow> {
    Ok(DaemonRow {
        daemon_id: r.get(0)?,
        pid: r.get(1)?,
        scope: r.get(2)?,
        endpoint: r.get(3)?,
        tcp_addr: r.get(4)?,
        hostname: r.get(5)?,
        version: r.get(6)?,
        started_at: r.get(7)?,
        heartbeat_at: r.get(8)?,
    })
}

const DAEMON_COLS: &str =
    "daemon_id, pid, scope, endpoint, tcp_addr, hostname, version, started_at, heartbeat_at";

fn lease_row(r: &Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok(LeaseRow {
        lease_id: r.get(0)?,
        session_id: r.get(1)?,
        daemon_id: r.get(2)?,
        client_id: r.get(3)?,
        kind: r.get(4)?,
        created_at: r.get(5)?,
        expires_at: r.get(6)?,
    })
}

const LEASE_COLS: &str = "lease_id, session_id, daemon_id, client_id, kind, created_at, expires_at";

fn pairing_row(r: &Row<'_>) -> rusqlite::Result<PairingRow> {
    Ok(PairingRow {
        pairing_id: r.get(0)?,
        kind: r.get(1)?,
        token_hash: r.get(2)?,
        scope: r.get(3)?,
        label: r.get(4)?,
        parent_id: r.get(5)?,
        created_at: r.get(6)?,
        expires_at: r.get(7)?,
        redeemed_at: r.get(8)?,
        revoked_at: r.get(9)?,
    })
}

const PAIRING_COLS: &str = "pairing_id, kind, token_hash, scope, label, parent_id, \
                            created_at, expires_at, redeemed_at, revoked_at";

impl ControlStore for Db {
    fn put_daemon(&self, row: &DaemonRow) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO daemons \
             (daemon_id, pid, scope, endpoint, tcp_addr, hostname, version, started_at, heartbeat_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                row.daemon_id,
                row.pid,
                row.scope,
                row.endpoint,
                row.tcp_addr,
                row.hostname,
                row.version,
                row.started_at,
                row.heartbeat_at
            ],
        )?;
        Ok(())
    }

    fn daemons(&self) -> Result<Vec<DaemonRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {DAEMON_COLS} FROM daemons ORDER BY started_at"
        ))?;
        let rows = stmt.query_map([], daemon_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn del_daemon(&self, daemon_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM daemons WHERE daemon_id = ?1",
            params![daemon_id],
        )?;
        Ok(())
    }

    fn touch_daemon_heartbeat(&self, daemon_id: &str, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE daemons SET heartbeat_at = ?2 WHERE daemon_id = ?1",
            params![daemon_id, now_ms],
        )?;
        Ok(())
    }

    fn live_daemons(&self, scope: &str, now_ms: i64, ttl_ms: i64) -> Result<Vec<DaemonRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {DAEMON_COLS} FROM daemons \
             WHERE scope = ?1 AND heartbeat_at >= ?2 ORDER BY started_at"
        ))?;
        let rows = stmt.query_map(params![scope, now_ms - ttl_ms], daemon_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn put_lease(
        &self,
        session_id: &str,
        daemon_id: &str,
        client_id: Option<&str>,
        kind: &str,
        expires_at: Option<i64>,
        now_ms: i64,
    ) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO session_leases \
             (session_id, daemon_id, client_id, kind, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, daemon_id, client_id, kind, now_ms, expires_at],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn leases(&self, daemon_id: &str) -> Result<Vec<LeaseRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {LEASE_COLS} FROM session_leases WHERE daemon_id = ?1 ORDER BY lease_id"
        ))?;
        let rows = stmt.query_map(params![daemon_id], lease_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn refresh_lease(&self, lease_id: i64, expires_at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE session_leases SET expires_at = ?2 WHERE lease_id = ?1",
            params![lease_id, expires_at],
        )?;
        Ok(())
    }

    fn release_lease(&self, lease_id: i64) -> Result<()> {
        self.conn().execute(
            "DELETE FROM session_leases WHERE lease_id = ?1",
            params![lease_id],
        )?;
        Ok(())
    }

    fn release_session_leases(&self, session_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM session_leases WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    fn reap_expired_leases(&self, daemon_id: &str, now_ms: i64) -> Result<Vec<LeaseRow>> {
        // DELETE ... RETURNING makes the reap atomic: no window where a racing
        // refresh resurrects a lease this call already decided to reap.
        let mut stmt = self.conn().prepare(&format!(
            "DELETE FROM session_leases \
             WHERE daemon_id = ?1 AND kind = 'relay' \
               AND expires_at IS NOT NULL AND expires_at <= ?2 \
             RETURNING {LEASE_COLS}"
        ))?;
        let rows = stmt.query_map(params![daemon_id, now_ms], lease_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn clear_daemon_leases(&self, daemon_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM session_leases WHERE daemon_id = ?1",
            params![daemon_id],
        )?;
        Ok(())
    }

    fn put_pairing(&self, row: &PairingRow) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO pairings \
             (pairing_id, kind, token_hash, scope, label, parent_id, \
              created_at, expires_at, redeemed_at, revoked_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                row.pairing_id,
                row.kind,
                row.token_hash,
                row.scope,
                row.label,
                row.parent_id,
                row.created_at,
                row.expires_at,
                row.redeemed_at,
                row.revoked_at
            ],
        )?;
        Ok(())
    }

    fn pairings(&self) -> Result<Vec<PairingRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {PAIRING_COLS} FROM pairings ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], pairing_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn pairing_for_auth(&self, pairing_id: &str, now_ms: i64) -> Result<Option<PairingRow>> {
        let row = self
            .conn()
            .query_row(
                &format!(
                    "SELECT {PAIRING_COLS} FROM pairings \
                     WHERE pairing_id = ?1 AND kind = 'token' AND revoked_at IS NULL \
                       AND (expires_at IS NULL OR expires_at > ?2)"
                ),
                params![pairing_id, now_ms],
                pairing_row,
            )
            .optional()?;
        Ok(row)
    }

    fn redeem_pairing_code(&self, pairing_id: &str, now_ms: i64) -> Result<Option<PairingRow>> {
        // Single UPDATE ... RETURNING: the redeemed_at guard in the WHERE makes
        // the consume atomic — a racing second redeem matches zero rows.
        let row = self
            .conn()
            .query_row(
                &format!(
                    "UPDATE pairings SET redeemed_at = ?2 \
                     WHERE pairing_id = ?1 AND kind = 'code' \
                       AND redeemed_at IS NULL AND revoked_at IS NULL \
                       AND (expires_at IS NULL OR expires_at > ?2) \
                     RETURNING {PAIRING_COLS}"
                ),
                params![pairing_id, now_ms],
                pairing_row,
            )
            .optional()?;
        Ok(row)
    }

    fn revoke_pairing(&self, pairing_id: &str, now_ms: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE pairings SET revoked_at = ?2 WHERE pairing_id = ?1 AND revoked_at IS NULL",
            params![pairing_id, now_ms],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon(id: &str, scope: &str, hb: i64) -> DaemonRow {
        DaemonRow {
            daemon_id: id.into(),
            pid: 4242,
            scope: scope.into(),
            endpoint: format!("/run/{id}.sock"),
            tcp_addr: None,
            hostname: "testhost".into(),
            version: "0.0-test".into(),
            started_at: 1_000,
            heartbeat_at: hb,
        }
    }

    fn pairing(id: &str, kind: &str, hash: &str, expires_at: Option<i64>) -> PairingRow {
        PairingRow {
            pairing_id: id.into(),
            kind: kind.into(),
            token_hash: hash.into(),
            scope: "read,git".into(),
            label: "phone".into(),
            parent_id: None,
            created_at: 1_000,
            expires_at,
            redeemed_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn daemon_registry_round_trip_and_heartbeat() {
        let db = Db::open_memory().unwrap();
        db.put_daemon(&daemon("d1", "/state/a", 5_000)).unwrap();
        db.put_daemon(&daemon("d2", "/state/b", 5_000)).unwrap();
        assert_eq!(db.daemons().unwrap().len(), 2);

        // Discovery is scope-keyed and heartbeat-gated.
        let live = db.live_daemons("/state/a", 10_000, 6_000).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].daemon_id, "d1");
        // A heartbeat older than the TTL cutoff is not live.
        assert!(
            db.live_daemons("/state/a", 20_000, 6_000)
                .unwrap()
                .is_empty()
        );
        db.touch_daemon_heartbeat("d1", 19_000).unwrap();
        assert_eq!(db.live_daemons("/state/a", 20_000, 6_000).unwrap().len(), 1);

        // put_daemon replaces by id (a restarted daemon re-registers).
        db.put_daemon(&daemon("d1", "/state/a", 30_000)).unwrap();
        assert_eq!(db.daemons().unwrap().len(), 2);

        db.del_daemon("d1").unwrap();
        assert!(
            db.live_daemons("/state/a", 20_000, 60_000)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn lease_round_trip_reap_and_refresh() {
        let db = Db::open_memory().unwrap();
        let attached = db
            .put_lease("s1", "d1", Some("client-a"), "attached", None, 1_000)
            .unwrap();
        let relay = db
            .put_lease("s2", "d1", None, "relay", Some(5_000), 1_000)
            .unwrap();
        let _other_daemon = db
            .put_lease("s3", "d2", None, "relay", Some(2_000), 1_000)
            .unwrap();
        assert_eq!(db.leases("d1").unwrap().len(), 2);

        // Nothing expires before its time; the attached lease (NULL expiry)
        // never reaps; other daemons' leases are untouched.
        assert!(db.reap_expired_leases("d1", 4_999).unwrap().is_empty());
        let reaped = db.reap_expired_leases("d1", 5_000).unwrap();
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].session_id, "s2");
        assert_eq!(reaped[0].lease_id, relay);
        assert_eq!(db.leases("d1").unwrap().len(), 1);
        assert_eq!(db.leases("d2").unwrap().len(), 1);

        // A refreshed lease survives the reap that would have taken it.
        let relay2 = db
            .put_lease("s4", "d1", None, "relay", Some(6_000), 5_500)
            .unwrap();
        db.refresh_lease(relay2, 9_000).unwrap();
        assert!(db.reap_expired_leases("d1", 6_500).unwrap().is_empty());

        // Attach cancels a session's relay lease by session id.
        db.release_session_leases("s4").unwrap();
        db.release_lease(attached).unwrap();
        assert!(db.leases("d1").unwrap().is_empty());
    }

    #[test]
    fn clear_daemon_leases_boot_sweep() {
        let db = Db::open_memory().unwrap();
        db.put_lease("s1", "d1", None, "relay", Some(5_000), 1_000)
            .unwrap();
        db.put_lease("s2", "d1", Some("c"), "attached", None, 1_000)
            .unwrap();
        db.put_lease("s3", "d2", None, "relay", Some(5_000), 1_000)
            .unwrap();
        db.clear_daemon_leases("d1").unwrap();
        assert!(db.leases("d1").unwrap().is_empty());
        assert_eq!(db.leases("d2").unwrap().len(), 1);
    }

    #[test]
    fn pairing_auth_lookup_gates_kind_expiry_and_revocation() {
        let db = Db::open_memory().unwrap();
        db.put_pairing(&pairing("tok1", "token", "hash-a", None))
            .unwrap();
        db.put_pairing(&pairing("tok2", "token", "hash-b", Some(5_000)))
            .unwrap();
        db.put_pairing(&pairing("code1", "code", "hash-c", None))
            .unwrap();

        // The stored value is the caller-supplied hash — never a plaintext secret.
        let hit = db.pairing_for_auth("tok1", 1_000).unwrap().unwrap();
        assert_eq!(hit.token_hash, "hash-a");
        assert_eq!(hit.scope, "read,git");

        // Expiry boundary: valid strictly before expires_at.
        assert!(db.pairing_for_auth("tok2", 4_999).unwrap().is_some());
        assert!(db.pairing_for_auth("tok2", 5_000).unwrap().is_none());

        // A code row is never a bearer credential.
        assert!(db.pairing_for_auth("code1", 1_000).unwrap().is_none());

        // Revocation is immediate and idempotent.
        db.revoke_pairing("tok1", 2_000).unwrap();
        db.revoke_pairing("tok1", 3_000).unwrap();
        assert!(db.pairing_for_auth("tok1", 2_500).unwrap().is_none());
        let revoked = db
            .pairings()
            .unwrap()
            .into_iter()
            .find(|p| p.pairing_id == "tok1")
            .unwrap();
        assert_eq!(revoked.revoked_at, Some(2_000));
    }

    #[test]
    fn redeem_pairing_code_is_single_use_and_atomic() {
        let db = Db::open_memory().unwrap();
        db.put_pairing(&pairing("code1", "code", "hash-c", Some(10_000)))
            .unwrap();

        let first = db.redeem_pairing_code("code1", 2_000).unwrap().unwrap();
        assert_eq!(first.token_hash, "hash-c");
        assert_eq!(first.redeemed_at, Some(2_000));
        // Second redeem of the same code fails (single-use).
        assert!(db.redeem_pairing_code("code1", 2_001).unwrap().is_none());

        // Expired and revoked codes refuse redemption.
        db.put_pairing(&pairing("code2", "code", "h", Some(1_000)))
            .unwrap();
        assert!(db.redeem_pairing_code("code2", 1_000).unwrap().is_none());
        db.put_pairing(&pairing("code3", "code", "h", None))
            .unwrap();
        db.revoke_pairing("code3", 500).unwrap();
        assert!(db.redeem_pairing_code("code3", 1_000).unwrap().is_none());

        // A token row is not redeemable (wrong kind).
        db.put_pairing(&pairing("tok1", "token", "h", None))
            .unwrap();
        assert!(db.redeem_pairing_code("tok1", 1_000).unwrap().is_none());
    }

    #[test]
    fn migration_is_idempotent() {
        let db = Db::open_memory().unwrap();
        super::migrate_v40(db.conn()).expect("re-migrate");
        db.put_daemon(&daemon("d", "/s", 1)).unwrap();
        assert_eq!(db.daemons().unwrap().len(), 1);
    }
}
