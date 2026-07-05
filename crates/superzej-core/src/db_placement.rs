//! Persisted placement state (schema **v34**): the `host_capacity` index
//! (declared spec / overcommit / measured sample per host — managed AND
//! independent, so every resource surface renders from one table), the
//! `host_tenancy` reservation ledger (the atomic linearization point for
//! concurrent placements), `placement_health` cooldown markers, and
//! `placement_events` decision traces. Sibling `impl Db` block so pinned
//! `db.rs` carries only the version bump + one migrate call (the
//! `host_db::migrate_v30` pattern). Timestamps are caller-supplied.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::capacity::{HostOwnership, HostSpec, MeasuredLoad, ReservedTotals, ResourceReq};
use crate::db::Db;
use crate::host::HostId;
use crate::store::{
    HealthMarker, HostCapacityRow, PlacementEventRow, PlacementStore, ReserveOutcome, TenancyMode,
    TenancyRow, TenancyState,
};

/// Decision traces kept per DB (newest win; pruned on insert).
const EVENTS_KEEP: i64 = 500;

/// v34: the placement engine. Purely additive (CREATE IF NOT EXISTS).
pub(crate) fn migrate_v34(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        BEGIN;
        CREATE TABLE IF NOT EXISTS host_capacity (
          host_id            TEXT PRIMARY KEY,
          ownership          TEXT NOT NULL DEFAULT 'independent',
          cpu_milli          INTEGER,
          mem_mb             INTEGER,
          overcommit_cpu_pct INTEGER NOT NULL DEFAULT 0,
          overcommit_mem_pct INTEGER NOT NULL DEFAULT 0,
          provider           TEXT NOT NULL DEFAULT '',
          template           TEXT NOT NULL DEFAULT '',
          created_at         INTEGER,
          measured_json      TEXT,
          updated_at         INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS host_tenancy (
          sandbox           TEXT PRIMARY KEY,
          host_id           TEXT NOT NULL,
          worktree          TEXT NOT NULL DEFAULT '',
          zone              TEXT NOT NULL DEFAULT '',
          mode              TEXT NOT NULL DEFAULT 'packed',
          cpu_floor_milli   INTEGER NOT NULL,
          mem_floor_mb      INTEGER NOT NULL,
          cpu_ceiling_milli INTEGER,
          mem_ceiling_mb    INTEGER,
          state             TEXT NOT NULL DEFAULT 'reserved',
          reserved_at       INTEGER NOT NULL,
          activated_at      INTEGER,
          released_at       INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tenancy_host ON host_tenancy(host_id, state);
        CREATE TABLE IF NOT EXISTS placement_health (
          key         TEXT PRIMARY KEY,
          kind        TEXT NOT NULL DEFAULT '',
          reason      TEXT NOT NULL DEFAULT '',
          since_ms    INTEGER NOT NULL DEFAULT 0,
          retry_at_ms INTEGER NOT NULL,
          consecutive INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS placement_events (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          ts         INTEGER NOT NULL,
          worktree   TEXT NOT NULL DEFAULT '',
          decision   TEXT NOT NULL,
          chosen     TEXT NOT NULL DEFAULT '',
          trace_json TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_placement_events_wt
          ON placement_events(worktree, ts);
        COMMIT;
        "#,
    )?;
    Ok(())
}

/// Live-tenant filter reused by every sum/list ("live" = holds capacity).
const LIVE: &str = "state != 'released'";

fn capacity_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, HostCapacityRow)> {
    let id_raw: String = r.get(0)?;
    let ownership: String = r.get(1)?;
    let cpu: Option<i64> = r.get(2)?;
    let mem: Option<i64> = r.get(3)?;
    let measured_json: Option<String> = r.get(9)?;
    let row = HostCapacityRow {
        host: HostId::local(), // caller swaps in the parsed id
        ownership: HostOwnership::parse(&ownership).unwrap_or(HostOwnership::Independent),
        spec: match (cpu, mem) {
            (Some(c), Some(m)) if c > 0 && m > 0 => Some(HostSpec {
                cpu_milli: c as u32,
                mem_mb: m as u64,
            }),
            _ => None,
        },
        overcommit_cpu_pct: r.get::<_, i64>(4)? as u32,
        overcommit_mem_pct: r.get::<_, i64>(5)? as u32,
        provider: r.get(6)?,
        template: r.get(7)?,
        created_at: r.get(8)?,
        measured: measured_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<MeasuredLoad>(j).ok()),
        updated_at: r.get(10)?,
    };
    Ok((id_raw, row))
}

const CAP_COLS: &str = "host_id, ownership, cpu_milli, mem_mb, overcommit_cpu_pct, \
                        overcommit_mem_pct, provider, template, created_at, measured_json, \
                        updated_at";

fn tenancy_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, TenancyRow)> {
    let host_raw: String = r.get(1)?;
    let mode: String = r.get(4)?;
    let state: String = r.get(9)?;
    let row = TenancyRow {
        sandbox: r.get(0)?,
        host: HostId::local(), // caller swaps in the parsed id
        worktree: r.get(2)?,
        zone: r.get(3)?,
        mode: TenancyMode::parse(&mode).unwrap_or(TenancyMode::Packed),
        req: ResourceReq {
            cpu_floor_milli: r.get::<_, i64>(5)? as u32,
            mem_floor_mb: r.get::<_, i64>(6)? as u64,
            cpu_ceiling_milli: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
            mem_ceiling_mb: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        },
        state: TenancyState::parse(&state).unwrap_or(TenancyState::Released),
        reserved_at: r.get(10)?,
        activated_at: r.get(11)?,
        released_at: r.get(12)?,
    };
    Ok((host_raw, row))
}

const TEN_COLS: &str = "sandbox, host_id, worktree, zone, mode, cpu_floor_milli, mem_floor_mb, \
                        cpu_ceiling_milli, mem_ceiling_mb, state, reserved_at, activated_at, \
                        released_at";

impl PlacementStore for Db {
    fn capacity_get(&self, host: &HostId) -> Result<Option<HostCapacityRow>> {
        let got = self
            .conn()
            .query_row(
                &format!("SELECT {CAP_COLS} FROM host_capacity WHERE host_id=?1"),
                params![host.as_str()],
                capacity_from,
            )
            .optional()?;
        Ok(got.map(|(_, mut row)| {
            row.host = host.clone();
            row
        }))
    }

    fn capacity_all(&self) -> Result<Vec<HostCapacityRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {CAP_COLS} FROM host_capacity ORDER BY host_id"
        ))?;
        let rows = stmt.query_map([], capacity_from)?;
        let mut out = Vec::new();
        for r in rows {
            let (raw, mut row) = r?;
            let Some(id) = HostId::parse(&raw) else {
                continue; // junk row: skip, never panic
            };
            row.host = id;
            out.push(row);
        }
        Ok(out)
    }

    fn capacity_put(&self, row: &HostCapacityRow) -> Result<()> {
        let measured_json = row
            .measured
            .map(|m| serde_json::to_string(&m))
            .transpose()?;
        self.conn().execute(
            "INSERT INTO host_capacity (host_id, ownership, cpu_milli, mem_mb,
               overcommit_cpu_pct, overcommit_mem_pct, provider, template, created_at,
               measured_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(host_id) DO UPDATE SET
               ownership = excluded.ownership,
               cpu_milli = excluded.cpu_milli,
               mem_mb = excluded.mem_mb,
               overcommit_cpu_pct = excluded.overcommit_cpu_pct,
               overcommit_mem_pct = excluded.overcommit_mem_pct,
               provider = excluded.provider,
               template = excluded.template,
               created_at = COALESCE(excluded.created_at, host_capacity.created_at),
               measured_json = COALESCE(excluded.measured_json, host_capacity.measured_json),
               updated_at = excluded.updated_at",
            params![
                row.host.as_str(),
                row.ownership.as_str(),
                row.spec.map(|s| s.cpu_milli as i64),
                row.spec.map(|s| s.mem_mb as i64),
                row.overcommit_cpu_pct as i64,
                row.overcommit_mem_pct as i64,
                row.provider,
                row.template,
                row.created_at,
                measured_json,
                row.updated_at,
            ],
        )?;
        Ok(())
    }

    fn capacity_delete(&self, host: &HostId) -> Result<()> {
        self.conn().execute(
            "DELETE FROM host_capacity WHERE host_id=?1",
            params![host.as_str()],
        )?;
        Ok(())
    }

    fn capacity_set_measured(&self, host: &HostId, m: &MeasuredLoad, now: i64) -> Result<()> {
        let json = serde_json::to_string(m)?;
        // Upsert: a measured sample may arrive before capacity_put (e.g. an
        // anonymous host probed for display before any placement).
        self.conn().execute(
            "INSERT INTO host_capacity (host_id, measured_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(host_id) DO UPDATE SET
               measured_json = excluded.measured_json,
               updated_at = excluded.updated_at",
            params![host.as_str(), json, now],
        )?;
        Ok(())
    }

    fn tenancy_reserve(
        &self,
        t: &TenancyRow,
        ceilings: (u64, u64),
        now: i64,
    ) -> Result<ReserveOutcome> {
        // A released husk under the same sandbox name never blocks a re-reserve.
        self.conn().execute(
            "DELETE FROM host_tenancy WHERE sandbox=?1 AND state='released'",
            params![t.sandbox],
        )?;
        // The guarded atomic insert: capacity, zone co-tenancy, and dedicated
        // exclusivity all checked inside ONE statement (single-writer SQLite ⇒
        // the cross-process linearization point). `ON CONFLICT DO NOTHING`
        // keeps a live duplicate from erroring; the diagnostic below classifies.
        let inserted = self.conn().execute(
            &format!(
                "INSERT INTO host_tenancy
                   (sandbox, host_id, worktree, zone, mode, cpu_floor_milli, mem_floor_mb,
                    cpu_ceiling_milli, mem_ceiling_mb, state, reserved_at)
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'reserved', ?10
                 WHERE NOT EXISTS (
                         SELECT 1 FROM host_tenancy
                          WHERE host_id=?2 AND {LIVE}
                            AND (zone != ?4 OR mode='dedicated' OR ?5='dedicated'))
                   AND COALESCE((SELECT SUM(cpu_floor_milli) FROM host_tenancy
                                  WHERE host_id=?2 AND {LIVE}), 0) + ?6 <= ?11
                   AND COALESCE((SELECT SUM(mem_floor_mb) FROM host_tenancy
                                  WHERE host_id=?2 AND {LIVE}), 0) + ?7 <= ?12
                 ON CONFLICT(sandbox) DO NOTHING"
            ),
            params![
                t.sandbox,
                t.host.as_str(),
                t.worktree,
                t.zone,
                t.mode.as_str(),
                t.req.cpu_floor_milli as i64,
                t.req.mem_floor_mb as i64,
                t.req.cpu_ceiling_milli.map(|v| v as i64),
                t.req.mem_ceiling_mb.map(|v| v as i64),
                now,
                ceilings.0 as i64,
                ceilings.1 as i64,
            ],
        )?;
        if inserted == 1 {
            return Ok(ReserveOutcome::Reserved);
        }
        // Refused: classify why (diagnostic reads may race a concurrent writer,
        // but the *refusal* above was authoritative — the reason is best-effort).
        if let Some(existing) = self.tenancy_for(&t.sandbox)?
            && existing.state != TenancyState::Released
        {
            return Ok(ReserveOutcome::AlreadyPlaced(existing.host));
        }
        let tenants = self.tenants_of(&t.host)?;
        if tenants
            .iter()
            .any(|x| x.mode == TenancyMode::Dedicated || t.mode == TenancyMode::Dedicated)
        {
            return Ok(ReserveOutcome::DedicatedConflict);
        }
        if tenants.iter().any(|x| x.zone != t.zone) {
            return Ok(ReserveOutcome::ZoneConflict);
        }
        Ok(ReserveOutcome::NoCapacity)
    }

    fn tenancy_force(&self, t: &TenancyRow, now: i64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO host_tenancy
               (sandbox, host_id, worktree, zone, mode, cpu_floor_milli, mem_floor_mb,
                cpu_ceiling_milli, mem_ceiling_mb, state, reserved_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'reserved', ?10)",
            params![
                t.sandbox,
                t.host.as_str(),
                t.worktree,
                t.zone,
                t.mode.as_str(),
                t.req.cpu_floor_milli as i64,
                t.req.mem_floor_mb as i64,
                t.req.cpu_ceiling_milli.map(|v| v as i64),
                t.req.mem_ceiling_mb.map(|v| v as i64),
                now,
            ],
        )?;
        Ok(())
    }

    fn tenancy_activate(&self, sandbox: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE host_tenancy SET state='active', activated_at=?2
              WHERE sandbox=?1 AND state='reserved'",
            params![sandbox, now],
        )?;
        Ok(())
    }

    fn tenancy_release(&self, sandbox: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE host_tenancy SET state='released', released_at=?2
              WHERE sandbox=?1 AND state != 'released'",
            params![sandbox, now],
        )?;
        Ok(())
    }

    fn tenancy_rebind(&self, sandbox: &str, worktree: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE host_tenancy SET worktree=?2 WHERE sandbox=?1 AND state != 'released'",
            params![sandbox, worktree],
        )?;
        Ok(())
    }

    fn tenancy_for(&self, sandbox: &str) -> Result<Option<TenancyRow>> {
        let got = self
            .conn()
            .query_row(
                &format!("SELECT {TEN_COLS} FROM host_tenancy WHERE sandbox=?1"),
                params![sandbox],
                tenancy_from,
            )
            .optional()?;
        Ok(got.and_then(|(raw, mut row)| {
            row.host = HostId::parse(&raw)?;
            Some(row)
        }))
    }

    fn tenants_of(&self, host: &HostId) -> Result<Vec<TenancyRow>> {
        let mut stmt = self.conn().prepare(&format!(
            "SELECT {TEN_COLS} FROM host_tenancy
              WHERE host_id=?1 AND {LIVE} ORDER BY reserved_at"
        ))?;
        let rows = stmt.query_map(params![host.as_str()], tenancy_from)?;
        let mut out = Vec::new();
        for r in rows {
            let (raw, mut row) = r?;
            let Some(id) = HostId::parse(&raw) else {
                continue;
            };
            row.host = id;
            out.push(row);
        }
        Ok(out)
    }

    fn reserved_totals(&self, host: &HostId) -> Result<ReservedTotals> {
        let (cpu, mem, n): (Option<i64>, Option<i64>, i64) = self.conn().query_row(
            &format!(
                "SELECT SUM(cpu_floor_milli), SUM(mem_floor_mb), COUNT(*)
                   FROM host_tenancy WHERE host_id=?1 AND {LIVE}"
            ),
            params![host.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(ReservedTotals {
            cpu_milli: cpu.unwrap_or(0).max(0) as u64,
            mem_mb: mem.unwrap_or(0).max(0) as u64,
            tenants: n.max(0) as u32,
        })
    }

    fn tenancy_sweep_stale(&self, before: i64) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE host_tenancy SET state='released', released_at=reserved_at
              WHERE state='reserved' AND reserved_at < ?1",
            params![before],
        )?;
        Ok(n)
    }

    fn health_mark(&self, m: &HealthMarker) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO placement_health
               (key, kind, reason, since_ms, retry_at_ms, consecutive)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                m.key,
                m.kind,
                m.reason,
                m.since_ms,
                m.retry_at_ms,
                m.consecutive as i64
            ],
        )?;
        Ok(())
    }

    fn health_clear(&self, key: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM placement_health WHERE key=?1", params![key])?;
        Ok(())
    }

    fn health_get(&self, key: &str) -> Result<Option<HealthMarker>> {
        let got = self
            .conn()
            .query_row(
                "SELECT key, kind, reason, since_ms, retry_at_ms, consecutive
                   FROM placement_health WHERE key=?1",
                params![key],
                |r| {
                    Ok(HealthMarker {
                        key: r.get(0)?,
                        kind: r.get(1)?,
                        reason: r.get(2)?,
                        since_ms: r.get(3)?,
                        retry_at_ms: r.get(4)?,
                        consecutive: r.get::<_, i64>(5)? as u32,
                    })
                },
            )
            .optional()?;
        Ok(got)
    }

    fn health_cooling(&self, now_ms: i64) -> Result<Vec<HealthMarker>> {
        // Expired markers are pruned here (fail-back is implicit).
        self.conn().execute(
            "DELETE FROM placement_health WHERE retry_at_ms <= ?1",
            params![now_ms],
        )?;
        let mut stmt = self.conn().prepare(
            "SELECT key, kind, reason, since_ms, retry_at_ms, consecutive
               FROM placement_health ORDER BY key",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(HealthMarker {
                key: r.get(0)?,
                kind: r.get(1)?,
                reason: r.get(2)?,
                since_ms: r.get(3)?,
                retry_at_ms: r.get(4)?,
                consecutive: r.get::<_, i64>(5)? as u32,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    fn placement_event_put(&self, e: &PlacementEventRow) -> Result<()> {
        self.conn().execute(
            "INSERT INTO placement_events (ts, worktree, decision, chosen, trace_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![e.ts, e.worktree, e.decision, e.chosen, e.trace_json],
        )?;
        self.conn().execute(
            "DELETE FROM placement_events WHERE id <=
               (SELECT MAX(id) FROM placement_events) - ?1",
            params![EVENTS_KEEP],
        )?;
        Ok(())
    }

    fn placement_events(
        &self,
        worktree: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlacementEventRow>> {
        let mut out = Vec::new();
        let mut push = |r: &rusqlite::Row<'_>| -> rusqlite::Result<()> {
            out.push(PlacementEventRow {
                ts: r.get(0)?,
                worktree: r.get(1)?,
                decision: r.get(2)?,
                chosen: r.get(3)?,
                trace_json: r.get(4)?,
            });
            Ok(())
        };
        match worktree {
            Some(wt) => {
                let mut stmt = self.conn().prepare(
                    "SELECT ts, worktree, decision, chosen, trace_json FROM placement_events
                      WHERE worktree=?1 ORDER BY id DESC LIMIT ?2",
                )?;
                let mut rows = stmt.query(params![wt, limit as i64])?;
                while let Some(r) = rows.next()? {
                    push(r)?;
                }
            }
            None => {
                let mut stmt = self.conn().prepare(
                    "SELECT ts, worktree, decision, chosen, trace_json FROM placement_events
                      ORDER BY id DESC LIMIT ?1",
                )?;
                let mut rows = stmt.query(params![limit as i64])?;
                while let Some(r) = rows.next()? {
                    push(r)?;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    fn cap_row(host: &HostId, cpu: u32, mem: u64) -> HostCapacityRow {
        HostCapacityRow {
            host: host.clone(),
            ownership: HostOwnership::Managed,
            spec: Some(HostSpec {
                cpu_milli: cpu,
                mem_mb: mem,
            }),
            overcommit_cpu_pct: 0,
            overcommit_mem_pct: 0,
            provider: "hetzner".into(),
            template: "cx32".into(),
            created_at: Some(100),
            measured: None,
            updated_at: 100,
        }
    }

    fn ten(
        sandbox: &str,
        host: &HostId,
        zone: &str,
        mode: TenancyMode,
        cpu: u32,
        mem: u64,
    ) -> TenancyRow {
        TenancyRow {
            sandbox: sandbox.into(),
            host: host.clone(),
            worktree: String::new(),
            zone: zone.into(),
            mode,
            req: ResourceReq {
                cpu_floor_milli: cpu,
                mem_floor_mb: mem,
                cpu_ceiling_milli: None,
                mem_ceiling_mb: None,
            },
            state: TenancyState::Reserved,
            reserved_at: 0,
            activated_at: None,
            released_at: None,
        }
    }

    #[test]
    fn capacity_round_trip_and_measured_upsert() {
        let db = db();
        let id = HostId::named("box");
        assert!(db.capacity_get(&id).unwrap().is_none());
        db.capacity_put(&cap_row(&id, 4000, 8192)).unwrap();
        let got = db.capacity_get(&id).unwrap().unwrap();
        assert_eq!(got.spec.unwrap().cpu_milli, 4000);
        assert_eq!(got.ownership, HostOwnership::Managed);
        assert_eq!(got.provider, "hetzner");

        // Measured sample refreshes without clobbering the spec.
        let m = MeasuredLoad {
            cpu_milli: 1200,
            mem_mb: 3000,
            at: 200,
        };
        db.capacity_set_measured(&id, &m, 200).unwrap();
        let got = db.capacity_get(&id).unwrap().unwrap();
        assert_eq!(got.measured, Some(m));
        assert_eq!(got.spec.unwrap().mem_mb, 8192, "spec preserved");

        // Re-put without measured keeps the sample (COALESCE).
        db.capacity_put(&cap_row(&id, 4000, 8192)).unwrap();
        assert!(db.capacity_get(&id).unwrap().unwrap().measured.is_some());

        // A sample may land before any capacity row exists (display-only host).
        let anon = HostId::anon_ssh("blake@box", 22);
        db.capacity_set_measured(&anon, &m, 201).unwrap();
        let got = db.capacity_get(&anon).unwrap().unwrap();
        assert_eq!(got.spec, None);
        assert_eq!(got.ownership, HostOwnership::Independent, "column default");

        assert_eq!(db.capacity_all().unwrap().len(), 2);
        db.capacity_delete(&anon).unwrap();
        assert_eq!(db.capacity_all().unwrap().len(), 1);
    }

    #[test]
    fn reserve_respects_exact_ceiling() {
        let db = db();
        let host = HostId::named("box");
        // Ceiling 4000/8192.
        assert_eq!(
            db.tenancy_reserve(
                &ten("a", &host, "", TenancyMode::Packed, 3000, 6144),
                (4000, 8192),
                1
            )
            .unwrap(),
            ReserveOutcome::Reserved
        );
        // Exactly the remainder fits.
        assert_eq!(
            db.tenancy_reserve(
                &ten("b", &host, "", TenancyMode::Packed, 1000, 2048),
                (4000, 8192),
                2
            )
            .unwrap(),
            ReserveOutcome::Reserved
        );
        // One more milli-core over ⇒ NoCapacity.
        assert_eq!(
            db.tenancy_reserve(
                &ten("c", &host, "", TenancyMode::Packed, 1, 1),
                (4000, 8192),
                3
            )
            .unwrap(),
            ReserveOutcome::NoCapacity
        );
        let totals = db.reserved_totals(&host).unwrap();
        assert_eq!(
            (totals.cpu_milli, totals.mem_mb, totals.tenants),
            (4000, 8192, 2)
        );
    }

    #[test]
    fn reserve_zone_and_dedicated_conflicts() {
        let db = db();
        let host = HostId::named("box");
        db.tenancy_reserve(
            &ten("a", &host, "clientA", TenancyMode::Packed, 100, 100),
            (10_000, 10_000),
            1,
        )
        .unwrap();
        // Different zone refused despite room.
        assert_eq!(
            db.tenancy_reserve(
                &ten("b", &host, "clientB", TenancyMode::Packed, 100, 100),
                (10_000, 10_000),
                2
            )
            .unwrap(),
            ReserveOutcome::ZoneConflict
        );
        // Unzoned is its own co-tenancy class too.
        assert_eq!(
            db.tenancy_reserve(
                &ten("c", &host, "", TenancyMode::Packed, 100, 100),
                (10_000, 10_000),
                3
            )
            .unwrap(),
            ReserveOutcome::ZoneConflict
        );
        // A dedicated request refuses an occupied host…
        assert_eq!(
            db.tenancy_reserve(
                &ten("d", &host, "clientA", TenancyMode::Dedicated, 100, 100),
                (10_000, 10_000),
                4
            )
            .unwrap(),
            ReserveOutcome::DedicatedConflict
        );
        // …and an empty host with a dedicated tenant refuses everyone else.
        let solo = HostId::named("solo");
        assert_eq!(
            db.tenancy_reserve(
                &ten("e", &solo, "clientA", TenancyMode::Dedicated, 100, 100),
                (10_000, 10_000),
                5
            )
            .unwrap(),
            ReserveOutcome::Reserved
        );
        assert_eq!(
            db.tenancy_reserve(
                &ten("f", &solo, "clientA", TenancyMode::Packed, 100, 100),
                (10_000, 10_000),
                6
            )
            .unwrap(),
            ReserveOutcome::DedicatedConflict
        );
    }

    #[test]
    fn reserve_duplicate_sandbox_reports_placed_host() {
        let db = db();
        let a = HostId::named("a");
        let b = HostId::named("b");
        db.tenancy_reserve(
            &ten("sb", &a, "", TenancyMode::Packed, 100, 100),
            (1000, 1000),
            1,
        )
        .unwrap();
        let out = db
            .tenancy_reserve(
                &ten("sb", &b, "", TenancyMode::Packed, 100, 100),
                (1000, 1000),
                2,
            )
            .unwrap();
        assert_eq!(out, ReserveOutcome::AlreadyPlaced(a.clone()));
        // Released husk does not block a re-reserve elsewhere.
        db.tenancy_release("sb", 3).unwrap();
        assert_eq!(
            db.tenancy_reserve(
                &ten("sb", &b, "", TenancyMode::Packed, 100, 100),
                (1000, 1000),
                4
            )
            .unwrap(),
            ReserveOutcome::Reserved
        );
        assert_eq!(db.tenancy_for("sb").unwrap().unwrap().host, b);
    }

    #[test]
    fn lifecycle_activate_rebind_release() {
        let db = db();
        let host = HostId::named("box");
        db.tenancy_reserve(
            &ten("spare-1", &host, "z", TenancyMode::Packed, 500, 512),
            (4000, 4096),
            10,
        )
        .unwrap();
        db.tenancy_activate("spare-1", 20).unwrap();
        let row = db.tenancy_for("spare-1").unwrap().unwrap();
        assert_eq!(row.state, TenancyState::Active);
        assert_eq!(row.activated_at, Some(20));

        // Pool claim: rebind keeps host + amounts.
        db.tenancy_rebind("spare-1", "/wt/feat").unwrap();
        let row = db.tenancy_for("spare-1").unwrap().unwrap();
        assert_eq!(row.worktree, "/wt/feat");
        let before = db.reserved_totals(&host).unwrap();
        assert_eq!(before.tenants, 1, "rebind adds nothing");

        db.tenancy_release("spare-1", 30).unwrap();
        assert_eq!(db.reserved_totals(&host).unwrap().tenants, 0);
        let row = db.tenancy_for("spare-1").unwrap().unwrap();
        assert_eq!(row.state, TenancyState::Released);
        assert_eq!(row.released_at, Some(30));
        // Releasing again is a no-op (released_at unchanged).
        db.tenancy_release("spare-1", 40).unwrap();
        assert_eq!(
            db.tenancy_for("spare-1").unwrap().unwrap().released_at,
            Some(30)
        );
    }

    #[test]
    fn force_places_unconditionally() {
        let db = db();
        let host = HostId::named("box");
        db.tenancy_reserve(
            &ten("a", &host, "zoneX", TenancyMode::Packed, 100, 100),
            (200, 200),
            1,
        )
        .unwrap();
        // A pin lands despite zone mismatch and zero remaining capacity.
        db.tenancy_force(
            &ten("pinned", &host, "zoneY", TenancyMode::Pinned, 9000, 9000),
            2,
        )
        .unwrap();
        let totals = db.reserved_totals(&host).unwrap();
        assert_eq!(totals.tenants, 2);
        assert_eq!(
            db.tenancy_for("pinned").unwrap().unwrap().mode,
            TenancyMode::Pinned
        );
    }

    #[test]
    fn sweep_releases_only_stale_reserved() {
        let db = db();
        let host = HostId::named("box");
        db.tenancy_reserve(
            &ten("old", &host, "", TenancyMode::Packed, 1, 1),
            (100, 100),
            10,
        )
        .unwrap();
        db.tenancy_reserve(
            &ten("fresh", &host, "", TenancyMode::Packed, 1, 1),
            (100, 100),
            90,
        )
        .unwrap();
        db.tenancy_reserve(
            &ten("act", &host, "", TenancyMode::Packed, 1, 1),
            (100, 100),
            10,
        )
        .unwrap();
        db.tenancy_activate("act", 11).unwrap();
        let swept = db.tenancy_sweep_stale(50).unwrap();
        assert_eq!(swept, 1, "only the stale reserved row");
        assert_eq!(
            db.tenancy_for("old").unwrap().unwrap().state,
            TenancyState::Released
        );
        assert_eq!(
            db.tenancy_for("fresh").unwrap().unwrap().state,
            TenancyState::Reserved
        );
        assert_eq!(
            db.tenancy_for("act").unwrap().unwrap().state,
            TenancyState::Active
        );
    }

    #[test]
    fn health_markers_cool_and_prune() {
        let db = db();
        let m = HealthMarker {
            key: "tpl:hetzner/cx32".into(),
            kind: "create_failure".into(),
            reason: "api 500".into(),
            since_ms: 1000,
            retry_at_ms: 5000,
            consecutive: 2,
        };
        db.health_mark(&m).unwrap();
        assert_eq!(db.health_get("tpl:hetzner/cx32").unwrap(), Some(m.clone()));
        // Still cooling before retry_at.
        let cooling = db.health_cooling(4000).unwrap();
        assert_eq!(cooling.len(), 1);
        // At/after retry_at the marker is pruned (implicit fail-back).
        assert!(db.health_cooling(5000).unwrap().is_empty());
        assert_eq!(db.health_get("tpl:hetzner/cx32").unwrap(), None);

        db.health_mark(&m).unwrap();
        db.health_clear(&m.key).unwrap();
        assert!(db.health_cooling(0).unwrap().is_empty());
    }

    #[test]
    fn events_newest_first_filtered_and_pruned() {
        let db = db();
        for i in 0..(EVENTS_KEEP + 20) {
            db.placement_event_put(&PlacementEventRow {
                ts: i,
                worktree: if i % 2 == 0 {
                    "/wt/a".into()
                } else {
                    "/wt/b".into()
                },
                decision: "packed".into(),
                chosen: format!("host:{i}"),
                trace_json: String::new(),
            })
            .unwrap();
        }
        let all = db.placement_events(None, 10).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].ts, EVENTS_KEEP + 19, "newest first");
        let a = db.placement_events(Some("/wt/a"), 5).unwrap();
        assert!(a.iter().all(|e| e.worktree == "/wt/a"));
        // Pruned to the keep window.
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM placement_events", [], |r| r.get(0))
            .unwrap();
        assert!(count <= EVENTS_KEEP, "pruned ({count})");
    }

    #[test]
    fn migration_is_idempotent() {
        let db = db();
        migrate_v34(db.conn()).unwrap();
        migrate_v34(db.conn()).unwrap();
        assert!(db.capacity_all().unwrap().is_empty());
    }
}
