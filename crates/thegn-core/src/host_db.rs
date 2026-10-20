//! Persisted host state (schema **v30**): the `hosts` row (durable state-machine
//! checkpoints + consent + cross-process heartbeat), `host_inventory`
//! (digest-keyed images/volume seeds per arch), and the `host_events` forensic
//! trail. Sibling `impl Db` block so pinned `db.rs` only carries the version
//! bump and a `conn()` accessor. All timestamps are caller-supplied unix
//! seconds (deterministic tests; the DB is a cache — git/hosts are truth).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::db::Db;
use crate::host::{Arch, HostCaps, HostFailure, HostId};
use crate::host_machine::HostState;
use crate::image::Digest;
use crate::inventory::{ArtifactKind, InventoryEntry, InventoryKey};
use crate::store::HostStore;

/// v30: hosts as first-class resources. Purely additive (CREATE IF NOT EXISTS),
/// so re-running is a no-op.
pub(crate) fn migrate_v30(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        BEGIN;
        CREATE TABLE IF NOT EXISTS hosts (
          host_id         TEXT PRIMARY KEY,
          name            TEXT NOT NULL DEFAULT '',
          reach_kind      TEXT NOT NULL DEFAULT 'ssh',
          state           TEXT NOT NULL DEFAULT 'unknown',
          state_meta      TEXT,
          caps_json       TEXT,
          arch            TEXT,
          install_consent TEXT,
          consented_at    INTEGER,
          heartbeat       INTEGER,
          active_step     TEXT,
          last_probe      INTEGER,
          last_used       INTEGER,
          updated_at      INTEGER NOT NULL DEFAULT 0,
          config_json     TEXT
        );
        CREATE TABLE IF NOT EXISTS host_inventory (
          host_id     TEXT NOT NULL,
          kind        TEXT NOT NULL,
          digest      TEXT NOT NULL,
          arch        TEXT NOT NULL,
          ref_name    TEXT NOT NULL DEFAULT '',
          present_at  INTEGER NOT NULL DEFAULT 0,
          verified_at INTEGER,
          size_bytes  INTEGER,
          PRIMARY KEY (host_id, kind, digest, arch)
        );
        CREATE TABLE IF NOT EXISTS host_events (
          id      INTEGER PRIMARY KEY AUTOINCREMENT,
          host_id TEXT NOT NULL,
          at      INTEGER NOT NULL,
          step    TEXT NOT NULL,
          detail  TEXT NOT NULL DEFAULT ''
        );
        COMMIT;
        "#,
    )?;
    // Tolerated on dev DBs that ran an earlier v30 without the column.
    let _ = conn.execute("ALTER TABLE hosts ADD COLUMN config_json TEXT", []);
    Ok(())
}

/// v35: the measured resource layer rides the host row (placement engine).
/// Tolerated ALTERs — a no-op when the columns exist.
pub(crate) fn migrate_v35(conn: &Connection) {
    let _ = conn.execute("ALTER TABLE hosts ADD COLUMN headroom_json TEXT", []);
    let _ = conn.execute("ALTER TABLE hosts ADD COLUMN last_headroom INTEGER", []);
}

/// One persisted host. `state` is always a durable checkpoint.
#[derive(Debug, Clone)]
pub struct HostRow {
    pub id: HostId,
    pub name: String,
    pub reach_kind: String,
    pub state: HostState,
    pub caps: Option<HostCaps>,
    pub arch: Option<Arch>,
    /// Per-host install grant: `None` unset, `Some(true)` granted,
    /// `Some(false)` declined (re-askable only via an explicit user action).
    pub install_consent: Option<bool>,
    /// Leader liveness for cross-process arbitration: refreshed each step by
    /// the driving process; a fresh heartbeat means "attach, don't take over".
    pub heartbeat: Option<i64>,
    pub active_step: Option<String>,
    pub last_probe: Option<i64>,
    pub last_used: Option<i64>,
    pub updated_at: i64,
    /// Latest headroom sample (the measured layer) + when it was taken.
    pub headroom: Option<crate::host_probe::Headroom>,
    pub last_headroom: Option<i64>,
}

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, HostRow)> {
    let id_raw: String = r.get(0)?;
    let state_tag: String = r.get(3)?;
    let state_meta: Option<String> = r.get(4)?;
    let failure = state_meta
        .as_deref()
        .and_then(|m| serde_json::from_str::<HostFailure>(m).ok());
    let caps_json: Option<String> = r.get(5)?;
    let arch: Option<String> = r.get(6)?;
    let consent: Option<String> = r.get(7)?;
    let row = HostRow {
        // Placeholder id; the caller swaps in the parsed HostId (kept out of
        // this closure so junk ids surface as an error, not a panic).
        id: HostId::local(),
        name: r.get(1)?,
        reach_kind: r.get(2)?,
        state: HostState::from_durable_tag(&state_tag, failure),
        caps: caps_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<HostCaps>(j).ok()),
        arch: arch.as_deref().and_then(Arch::parse),
        install_consent: consent.as_deref().and_then(|c| match c {
            "granted" => Some(true),
            "declined" => Some(false),
            _ => None,
        }),
        heartbeat: r.get(8)?,
        active_step: r.get(9)?,
        last_probe: r.get(10)?,
        last_used: r.get(11)?,
        updated_at: r.get(12)?,
        headroom: r
            .get::<_, Option<String>>(13)?
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok()),
        last_headroom: r.get(14)?,
    };
    Ok((id_raw, row))
}

const HOST_COLS: &str = "host_id, name, reach_kind, state, state_meta, caps_json, arch, \
                         install_consent, heartbeat, active_step, last_probe, last_used, \
                         updated_at, headroom_json, last_headroom";

fn host_authority_summary(config: &crate::host_config::HostConfig) -> String {
    use crate::host_config::HostReach;
    match config.reach {
        HostReach::Local => "local".to_string(),
        HostReach::Ssh => format!("ssh {}:{}", config.ssh.host, config.ssh.port),
        HostReach::Iroh => "iroh authority".to_string(),
        HostReach::Cloud => format!("cloud provider {}", config.cloud.provider),
    }
}

fn check_stored_authority(
    name: &str,
    existing_json: &str,
    hc: &crate::host_config::HostConfig,
) -> Result<()> {
    let existing_config = serde_json::from_str::<crate::host_config::HostConfig>(existing_json)
        .map_err(|_| {
            anyhow::anyhow!(
                "host alias {name:?} has an unreadable stored authority; remove it with `thegn host rm {name}` or choose `--name <other>`"
            )
        })?;
    if !crate::host_config::same_host_authority(&existing_config, hc) {
        anyhow::bail!(
            "host alias {name:?} is already bound to {} and cannot be rebound to {}; remove it with `thegn host rm {name}` or choose `--name <other>`",
            host_authority_summary(&existing_config),
            host_authority_summary(hc),
        );
    }
    Ok(())
}

impl HostStore for Db {
    fn host_get(&self, id: &HostId) -> Result<Option<HostRow>> {
        let got = self
            .conn()
            .query_row(
                &format!("SELECT {HOST_COLS} FROM hosts WHERE host_id=?1"),
                params![id.as_str()],
                row_from,
            )
            .optional()?;
        Ok(got.map(|(_, mut row)| {
            row.id = id.clone();
            row
        }))
    }

    fn hosts_all(&self) -> Result<Vec<HostRow>> {
        let mut stmt = self
            .conn()
            .prepare(&format!("SELECT {HOST_COLS} FROM hosts ORDER BY host_id"))?;
        let rows = stmt.query_map([], row_from)?;
        let mut out = Vec::new();
        for r in rows {
            let (raw, mut row) = r?;
            let Some(id) = HostId::parse(&raw) else {
                continue; // junk row (hand-edited DB): skip, never panic
            };
            row.id = id;
            out.push(row);
        }
        Ok(out)
    }

    /// Upsert a durable checkpoint. `caps`/`arch` refresh when provided and are
    /// preserved otherwise; a non-`failed` state clears `state_meta`.
    fn host_checkpoint(
        &self,
        id: &HostId,
        name: &str,
        reach_kind: &str,
        state: &HostState,
        caps: Option<&HostCaps>,
        now: i64,
    ) -> Result<()> {
        let Some(tag) = state.durable_tag() else {
            anyhow::bail!("host_checkpoint: {state:?} is not a durable state");
        };
        let meta = match state {
            HostState::Failed(f) => Some(serde_json::to_string(f)?),
            _ => None,
        };
        let caps_json = caps.map(serde_json::to_string).transpose()?;
        let arch = caps.map(|c| c.arch.oci_name());
        self.conn().execute(
            "INSERT INTO hosts (host_id, name, reach_kind, state, state_meta, caps_json, arch, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(host_id) DO UPDATE SET
               name = excluded.name,
               reach_kind = excluded.reach_kind,
               state = excluded.state,
               state_meta = excluded.state_meta,
               caps_json = COALESCE(excluded.caps_json, hosts.caps_json),
               arch = COALESCE(excluded.arch, hosts.arch),
               updated_at = excluded.updated_at",
            params![id.as_str(), name, reach_kind, tag, meta, caps_json, arch, now],
        )?;
        Ok(())
    }

    /// Leader liveness: stamp the step being worked plus a heartbeat. Another
    /// process seeing `heartbeat` fresher than its takeover threshold attaches
    /// (renders `active_step`) instead of double-driving.
    fn host_heartbeat(&self, id: &HostId, active_step: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE hosts SET heartbeat=?2, active_step=?3, updated_at=?2 WHERE host_id=?1",
            params![id.as_str(), now, active_step],
        )?;
        Ok(())
    }

    /// Clear the heartbeat on terminal states so an idle row never looks driven.
    fn host_heartbeat_clear(&self, id: &HostId) -> Result<()> {
        self.conn().execute(
            "UPDATE hosts SET heartbeat=NULL, active_step=NULL WHERE host_id=?1",
            params![id.as_str()],
        )?;
        Ok(())
    }

    fn host_set_headroom(
        &self,
        id: &HostId,
        h: &crate::host_probe::Headroom,
        now: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE hosts SET headroom_json=?2, last_headroom=?3 WHERE host_id=?1",
            params![id.as_str(), serde_json::to_string(h)?, now],
        )?;
        Ok(())
    }

    fn host_touch_probe(&self, id: &HostId, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE hosts SET last_probe=?2, updated_at=?2 WHERE host_id=?1",
            params![id.as_str(), now],
        )?;
        Ok(())
    }

    fn host_touch_used(&self, id: &HostId, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE hosts SET last_used=?2 WHERE host_id=?1",
            params![id.as_str(), now],
        )?;
        Ok(())
    }

    /// Persist the per-host install grant (`granted`/`declined`).
    fn host_set_consent(&self, id: &HostId, granted: bool, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE hosts SET install_consent=?2, consented_at=?3, updated_at=?3 WHERE host_id=?1",
            params![
                id.as_str(),
                if granted { "granted" } else { "declined" },
                now
            ],
        )?;
        Ok(())
    }

    /// Remove a host row + its inventory + events (the `host rm-cache` /
    /// remove action; on-host artifacts are the caller's job).
    fn host_delete(&self, id: &HostId) -> Result<()> {
        self.conn().execute(
            "DELETE FROM host_inventory WHERE host_id=?1",
            params![id.as_str()],
        )?;
        self.conn().execute(
            "DELETE FROM host_events WHERE host_id=?1",
            params![id.as_str()],
        )?;
        self.conn()
            .execute("DELETE FROM hosts WHERE host_id=?1", params![id.as_str()])?;
        Ok(())
    }

    /// Persist a USER-ADDED host definition (the in-TUI / CLI "add host"
    /// flow): the serialized [`HostConfig`](crate::host_config::HostConfig)
    /// rides the host's own row and is
    /// merged into the config catalog at load —
    /// [`crate::host_config::merge_db_hosts`]. Declarative `[host.<name>]`
    /// config SHADOWS a DB def of the same name.
    fn put_host_def(
        &self,
        name: &str,
        hc: &crate::host_config::HostConfig,
        now: i64,
    ) -> Result<()> {
        let id = HostId::named(name);
        let json = serde_json::to_string(hc)?;
        let tx = self.conn().unchecked_transaction()?;
        let existing: Option<Option<String>> = tx
            .query_row(
                "SELECT config_json FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(Some(existing_json)) = &existing {
            check_stored_authority(name, existing_json, hc)?;
        } else if existing == Some(None) {
            let occupied: bool = tx.query_row(
                "SELECT state != 'unknown'
                        OR heartbeat IS NOT NULL
                        OR install_consent IS NOT NULL
                        OR caps_json IS NOT NULL
                        OR last_probe IS NOT NULL
                        OR last_used IS NOT NULL
                        OR headroom_json IS NOT NULL
                        OR EXISTS (SELECT 1 FROM host_inventory WHERE host_id=?1)
                        OR EXISTS (SELECT 1 FROM host_events WHERE host_id=?1)
                   FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| row.get(0),
            )?;
            if occupied {
                anyhow::bail!(
                    "host alias {name:?} holds recorded state from a host that is no longer defined (for example, one removed from config.toml); take it over with `thegn host add <target> --name {name} --replace` or choose `--name <other>`"
                );
            }
        }
        tx.execute(
            "INSERT INTO hosts (host_id, name, reach_kind, state, config_json, updated_at)
             VALUES (?1, ?2, ?3, 'unknown', ?4, ?5)
             ON CONFLICT(host_id) DO UPDATE SET
               name = excluded.name,
               reach_kind = excluded.reach_kind,
               config_json = excluded.config_json,
               updated_at = excluded.updated_at",
            params![id.as_str(), name, hc.reach.as_str(), json, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn put_host_def_replacing(
        &self,
        name: &str,
        hc: &crate::host_config::HostConfig,
        now: i64,
    ) -> Result<bool> {
        let id = HostId::named(name);
        let json = serde_json::to_string(hc)?;
        let tx = self.conn().unchecked_transaction()?;
        let existing: Option<Option<String>> = tx
            .query_row(
                "SELECT config_json FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let mut cleared_state = false;
        if let Some(Some(existing_json)) = &existing {
            check_stored_authority(name, existing_json, hc)?;
        } else if existing.is_some() {
            cleared_state = tx.query_row(
                "SELECT state != 'unknown'
                        OR state_meta IS NOT NULL
                        OR caps_json IS NOT NULL
                        OR arch IS NOT NULL
                        OR install_consent IS NOT NULL
                        OR consented_at IS NOT NULL
                        OR heartbeat IS NOT NULL
                        OR active_step IS NOT NULL
                        OR last_probe IS NOT NULL
                        OR last_used IS NOT NULL
                        OR headroom_json IS NOT NULL
                        OR last_headroom IS NOT NULL
                        OR EXISTS (SELECT 1 FROM host_inventory WHERE host_id=?1)
                        OR EXISTS (SELECT 1 FROM host_events WHERE host_id=?1)
                   FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| row.get(0),
            )?;
            tx.execute(
                "DELETE FROM host_inventory WHERE host_id=?1",
                params![id.as_str()],
            )?;
            tx.execute(
                "DELETE FROM host_events WHERE host_id=?1",
                params![id.as_str()],
            )?;
            tx.execute("DELETE FROM hosts WHERE host_id=?1", params![id.as_str()])?;
        }
        tx.execute(
            "INSERT INTO hosts (host_id, name, reach_kind, state, config_json, updated_at)
             VALUES (?1, ?2, ?3, 'unknown', ?4, ?5)
             ON CONFLICT(host_id) DO UPDATE SET
               name = excluded.name,
               reach_kind = excluded.reach_kind,
               config_json = excluded.config_json,
               updated_at = excluded.updated_at",
            params![id.as_str(), name, hc.reach.as_str(), json, now],
        )?;
        tx.commit()?;
        Ok(cleared_state)
    }

    /// All user-added host definitions (rows carrying a config_json).
    fn host_defs(&self) -> Result<Vec<(String, crate::host_config::HostConfig)>> {
        let mut stmt = self.conn().prepare(
            "SELECT name, config_json FROM hosts
              WHERE config_json IS NOT NULL AND name != '' ORDER BY name",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (name, json) = r?;
            let Ok(hc) = serde_json::from_str::<crate::host_config::HostConfig>(&json) else {
                continue; // junk def (hand-edited): skip, never panic
            };
            out.push((name, hc));
        }
        Ok(out)
    }

    fn host_defs_checked(
        &self,
    ) -> std::result::Result<
        crate::host_definition_snapshot::HostDefinitionsSnapshot,
        crate::host_definition_snapshot::HostDefinitionReadError,
    > {
        // A newer-schema handle exists only under the explicit
        // THEGN_ALLOW_SCHEMA_DOWNGRADE override (otherwise `Db::open`
        // refuses it); that override must be able to read the hosts table.
        if self
            .schema_mismatch
            .is_some_and(|observed| observed > crate::db::SCHEMA_VERSION)
        {
            crate::host_db_snapshot::read_allowing_newer(self.conn())
        } else {
            crate::host_db_snapshot::read(self.conn())
        }
    }

    fn host_inventory(&self, id: &HostId) -> Result<Vec<InventoryEntry>> {
        let mut stmt = self.conn().prepare(
            "SELECT kind, digest, arch, ref_name, present_at, verified_at, size_bytes
               FROM host_inventory WHERE host_id=?1 ORDER BY present_at",
        )?;
        let rows = stmt.query_map(params![id.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (kind, digest, arch, ref_name, present_at, verified_at, size) = r?;
            let (Some(kind), Ok(digest), Some(arch)) = (
                ArtifactKind::parse(&kind),
                Digest::parse(&digest),
                Arch::parse(&arch),
            ) else {
                continue; // junk row: skip, never panic
            };
            out.push(InventoryEntry {
                key: InventoryKey {
                    host: id.clone(),
                    kind,
                    digest,
                    arch,
                },
                ref_name,
                present_at,
                verified_at,
                size_bytes: size.map(|s| s as u64),
            });
        }
        Ok(out)
    }

    fn host_inventory_put(&self, e: &InventoryEntry) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO host_inventory
               (host_id, kind, digest, arch, ref_name, present_at, verified_at, size_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                e.key.host.as_str(),
                e.key.kind.as_str(),
                e.key.digest.as_str(),
                e.key.arch.oci_name(),
                e.ref_name,
                e.present_at,
                e.verified_at,
                e.size_bytes.map(|s| s as i64),
            ],
        )?;
        Ok(())
    }

    /// Stamp a successful on-host verification of one artifact.
    fn host_inventory_verify(&self, key: &InventoryKey, now: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE host_inventory SET verified_at=?5
              WHERE host_id=?1 AND kind=?2 AND digest=?3 AND arch=?4",
            params![
                key.host.as_str(),
                key.kind.as_str(),
                key.digest.as_str(),
                key.arch.oci_name(),
                now
            ],
        )?;
        Ok(())
    }

    /// Drop one artifact (delivery superseded / digest mismatch cleanup).
    fn host_inventory_remove(&self, key: &InventoryKey) -> Result<()> {
        self.conn().execute(
            "DELETE FROM host_inventory
              WHERE host_id=?1 AND kind=?2 AND digest=?3 AND arch=?4",
            params![
                key.host.as_str(),
                key.kind.as_str(),
                key.digest.as_str(),
                key.arch.oci_name(),
            ],
        )?;
        Ok(())
    }

    /// Append to the forensic step trail.
    fn host_event(&self, id: &HostId, step: &str, detail: &str, now: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO host_events (host_id, at, step, detail) VALUES (?1, ?2, ?3, ?4)",
            params![id.as_str(), now, step, detail],
        )?;
        Ok(())
    }

    /// Most-recent-first slice of the event trail: `(at, step, detail)`.
    fn host_events_recent(&self, id: &HostId, limit: usize) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn().prepare(
            "SELECT at, step, detail FROM host_events
              WHERE host_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![id.as_str(), limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostStep;
    use crate::store::HostStore;

    const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    fn db() -> Db {
        Db::open_memory().unwrap()
    }

    fn caps() -> HostCaps {
        HostCaps::parse_probe("ARCH=x86_64\nOS=linux\nPODMAN=5.0\nRSYNC=1\n").unwrap()
    }

    fn ssh_host(target: &str, port: u16) -> crate::host_config::HostConfig {
        crate::host_config::HostConfig {
            reach: crate::host_config::HostReach::Ssh,
            ssh: crate::config::EnvSshConfig {
                host: target.into(),
                port,
                ..crate::config::EnvSshConfig::default()
            },
            ..crate::host_config::HostConfig::default()
        }
    }

    #[test]
    fn host_authority_summary_covers_each_reach_without_ticket_material() {
        assert_eq!(
            host_authority_summary(&crate::host_config::HostConfig {
                reach: crate::host_config::HostReach::Local,
                ..crate::host_config::HostConfig::default()
            }),
            "local"
        );
        assert_eq!(
            host_authority_summary(&ssh_host("me@box", 2222)),
            "ssh me@box:2222"
        );
        let iroh = crate::host_config::HostConfig {
            reach: crate::host_config::HostReach::Iroh,
            iroh: crate::host_config::HostIrohConfig {
                ticket: "secret-ticket".into(),
                ..crate::host_config::HostIrohConfig::default()
            },
            ..crate::host_config::HostConfig::default()
        };
        let summary = host_authority_summary(&iroh);
        assert_eq!(summary, "iroh authority");
        assert!(!summary.contains("secret-ticket"));
        assert_eq!(
            host_authority_summary(&crate::host_config::HostConfig {
                reach: crate::host_config::HostReach::Cloud,
                cloud: crate::host_config::HostCloudConfig {
                    provider: "sprites".into(),
                    ..crate::host_config::HostCloudConfig::default()
                },
                ..crate::host_config::HostConfig::default()
            }),
            "cloud provider sprites"
        );
    }

    #[test]
    fn put_host_def_rejects_authority_collision_without_mutating_row() {
        let db = db();
        let original = ssh_host("me@a.b", 22);
        db.put_host_def("a-b", &original, 10).unwrap();
        let id = HostId::named("a-b");
        db.host_checkpoint(&id, "a-b", "ssh", &HostState::Ready, None, 11)
            .unwrap();
        db.host_event(&id, "provision", "kept", 12).unwrap();
        let before: (String, String, String, String, i64) = db
            .conn()
            .query_row(
                "SELECT host_id, name, reach_kind, config_json, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();

        let collision = ssh_host("me@a-b", 22);
        let error = db.put_host_def("a-b", &collision, 99).unwrap_err();
        assert!(error.to_string().contains("a-b"));
        let after: (String, String, String, String, i64) = db
            .conn()
            .query_row(
                "SELECT host_id, name, reach_kind, config_json, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            after, before,
            "collision must leave the stored row byte-for-byte unchanged"
        );
        assert_eq!(db.host_get(&id).unwrap().unwrap().state, HostState::Ready);
        assert_eq!(db.host_events_recent(&id, 10).unwrap()[0].2, "kept");
    }

    #[test]
    fn put_host_def_is_idempotent_for_same_authority_and_rejects_changed_port() {
        let db = db();
        let original = ssh_host("me@box", 22);
        db.put_host_def("box", &original, 10).unwrap();
        let mut updated = original.clone();
        updated.image = "new:image".into();
        updated.install_runtime = crate::host_config::InstallConsent::Auto;
        db.put_host_def("box", &updated, 20).unwrap();
        assert_eq!(db.host_defs().unwrap()[0].1.image, "new:image");

        let changed_port = ssh_host("me@box", 2222);
        assert!(db.put_host_def("box", &changed_port, 30).is_err());
        let changed_user = ssh_host("you@box", 22);
        assert!(db.put_host_def("box", &changed_user, 40).is_err());
        assert_eq!(db.host_defs().unwrap()[0].1.ssh.port, 22);
        assert_eq!(db.host_defs().unwrap()[0].1.ssh.host, "me@box");
    }

    #[test]
    fn put_host_def_fails_closed_on_unreadable_existing_config() {
        let db = db();
        let id = HostId::named("broken");
        db.host_checkpoint(&id, "broken", "ssh", &HostState::Ready, None, 1)
            .unwrap();
        db.conn()
            .execute(
                "UPDATE hosts SET config_json='not-json' WHERE host_id=?1",
                params![id.as_str()],
            )
            .unwrap();
        let before: (String, String, i64) = db
            .conn()
            .query_row(
                "SELECT config_json, state, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert!(
            db.put_host_def("broken", &ssh_host("me@box", 22), 9)
                .is_err()
        );
        let after: (String, String, i64) = db
            .conn()
            .query_row(
                "SELECT config_json, state, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn put_host_def_rejects_iroh_ticket_and_reach_collisions() {
        let db = db();
        let iroh = crate::host_config::HostConfig {
            reach: crate::host_config::HostReach::Iroh,
            iroh: crate::host_config::HostIrohConfig {
                ticket: "ticket-one".into(),
                user: "me".into(),
                ..crate::host_config::HostIrohConfig::default()
            },
            ..crate::host_config::HostConfig::default()
        };
        db.put_host_def("shared", &iroh, 1).unwrap();
        let changed_ticket = crate::host_config::HostConfig {
            iroh: crate::host_config::HostIrohConfig {
                ticket: "ticket-two".into(),
                ..iroh.iroh.clone()
            },
            ..iroh.clone()
        };
        assert!(db.put_host_def("shared", &changed_ticket, 2).is_err());
        assert!(
            db.put_host_def("shared", &ssh_host("me@box", 22), 3)
                .is_err()
        );
        assert_eq!(db.host_defs().unwrap()[0].1.iroh.ticket, "ticket-one");
    }

    #[test]
    fn host_row_round_trips_states_and_caps() {
        let db = db();
        let id = HostId::named("box");
        assert!(db.host_get(&id).unwrap().is_none());

        db.host_checkpoint(
            &id,
            "box",
            "ssh",
            &HostState::RuntimeReady,
            Some(&caps()),
            100,
        )
        .unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.state, HostState::RuntimeReady);
        assert_eq!(row.reach_kind, "ssh");
        assert_eq!(row.arch, Some(Arch::Amd64));
        assert_eq!(row.caps.as_ref().unwrap(), &caps());
        assert_eq!(row.updated_at, 100);

        // Later checkpoint without caps preserves the probed caps.
        db.host_checkpoint(&id, "box", "ssh", &HostState::Ready, None, 200)
            .unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.state, HostState::Ready);
        assert!(row.caps.is_some(), "caps preserved via COALESCE");
    }

    #[test]
    fn failed_state_carries_meta() {
        let db = db();
        let id = HostId::named("box");
        let failure = HostFailure {
            step: HostStep::Deliver,
            error: "stalled".into(),
            retryable: true,
        };
        db.host_checkpoint(
            &id,
            "box",
            "ssh",
            &HostState::Failed(failure.clone()),
            None,
            1,
        )
        .unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.state, HostState::Failed(failure));

        // A transient state can never be persisted.
        assert!(
            db.host_checkpoint(&id, "box", "ssh", &HostState::Probing, None, 2)
                .is_err()
        );
    }

    #[test]
    fn heartbeat_probe_used_consent_lifecycle() {
        let db = db();
        let id = HostId::anon_ssh("blake@box", 22);
        db.host_checkpoint(&id, "", "ssh", &HostState::Unknown, None, 1)
            .unwrap();

        db.host_heartbeat(&id, "deliver", 50).unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.heartbeat, Some(50));
        assert_eq!(row.active_step.as_deref(), Some("deliver"));

        db.host_heartbeat_clear(&id).unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.heartbeat, None);
        assert_eq!(row.active_step, None);

        db.host_touch_probe(&id, 60).unwrap();
        db.host_touch_used(&id, 70).unwrap();
        db.host_set_consent(&id, true, 80).unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.last_probe, Some(60));
        assert_eq!(row.last_used, Some(70));
        assert_eq!(row.install_consent, Some(true));

        db.host_set_consent(&id, false, 90).unwrap();
        assert_eq!(
            db.host_get(&id).unwrap().unwrap().install_consent,
            Some(false)
        );
    }

    #[test]
    fn inventory_round_trip_verify_remove() {
        let db = db();
        let id = HostId::named("box");
        let key = InventoryKey {
            host: id.clone(),
            kind: ArtifactKind::Image,
            digest: Digest::parse(D1).unwrap(),
            arch: Arch::Amd64,
        };
        let entry = InventoryEntry {
            key: key.clone(),
            ref_name: "ghcr.io/x/base:v1".into(),
            present_at: 100,
            verified_at: None,
            size_bytes: Some(2_000_000),
        };
        db.host_inventory_put(&entry).unwrap();
        let got = db.host_inventory(&id).unwrap();
        assert_eq!(got, vec![entry.clone()]);

        db.host_inventory_verify(&key, 200).unwrap();
        assert_eq!(db.host_inventory(&id).unwrap()[0].verified_at, Some(200));

        // Upsert replaces (same PK).
        db.host_inventory_put(&entry).unwrap();
        assert_eq!(db.host_inventory(&id).unwrap().len(), 1);

        db.host_inventory_remove(&key).unwrap();
        assert!(db.host_inventory(&id).unwrap().is_empty());
    }

    #[test]
    fn events_are_most_recent_first_and_bounded() {
        let db = db();
        let id = HostId::named("box");
        for i in 0..5 {
            db.host_event(&id, "deliver", &format!("chunk {i}"), i)
                .unwrap();
        }
        let got = db.host_events_recent(&id, 3).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].2, "chunk 4");
        assert_eq!(got[2].2, "chunk 2");
    }

    #[test]
    fn hosts_all_lists_and_delete_cleans() {
        let db = db();
        let a = HostId::named("a");
        let b = HostId::cloud("sprites", "base");
        db.host_checkpoint(&a, "a", "ssh", &HostState::Ready, None, 1)
            .unwrap();
        db.host_checkpoint(&b, "", "cloud", &HostState::Unknown, None, 2)
            .unwrap();
        let all = db.hosts_all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|r| r.id == b));

        db.host_event(&a, "connect", "x", 1).unwrap();
        db.host_delete(&a).unwrap();
        assert!(db.host_get(&a).unwrap().is_none());
        assert!(db.host_events_recent(&a, 10).unwrap().is_empty());
        assert_eq!(db.hosts_all().unwrap().len(), 1);
    }

    #[test]
    fn headroom_persists_and_reads_back() {
        let db = db();
        let id = HostId::named("box");
        db.host_checkpoint(&id, "box", "ssh", &HostState::Ready, None, 1)
            .unwrap();
        assert!(db.host_get(&id).unwrap().unwrap().headroom.is_none());
        let h = crate::host_probe::parse_headroom(
            "NPROC=8\nMEM_TOTAL_KB=32768000\nMEM_AVAIL_KB=16384000\n",
        )
        .unwrap();
        db.host_set_headroom(&id, &h, 42).unwrap();
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.headroom, Some(h));
        assert_eq!(row.last_headroom, Some(42));
        migrate_v35(db.conn()); // idempotent on existing columns
        assert_eq!(db.host_get(&id).unwrap().unwrap().last_headroom, Some(42));
    }

    #[test]
    fn migration_is_idempotent() {
        let db = db();
        // open_memory already ran init (v30); run again directly.
        migrate_v30(db.conn()).unwrap();
        migrate_v30(db.conn()).unwrap();
        assert!(db.hosts_all().unwrap().is_empty());
    }

    #[test]
    fn put_host_def_refuses_an_occupied_null_config_row() {
        let db = db();
        let id = HostId::named("box");
        db.host_checkpoint(&id, "box", "ssh", &HostState::Ready, None, 1)
            .unwrap();
        db.host_event(&id, "provision", "recorded", 2).unwrap();
        let before: (String, String, String, bool, i64) = db
            .conn()
            .query_row(
                "SELECT host_id, name, state, config_json IS NULL, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        let events_before = db.host_events_recent(&id, 10).unwrap();

        let error = db
            .put_host_def("box", &ssh_host("me@new", 22), 9)
            .unwrap_err();
        assert!(error.to_string().contains("--replace"));
        let after: (String, String, String, bool, i64) = db
            .conn()
            .query_row(
                "SELECT host_id, name, state, config_json IS NULL, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(db.host_events_recent(&id, 10).unwrap(), events_before);
    }

    #[test]
    fn occupied_by_inventory_or_heartbeat_alone() {
        let db = db();
        let inventory_id = HostId::named("inventory-only");
        db.host_checkpoint(
            &inventory_id,
            "inventory-only",
            "ssh",
            &HostState::Unknown,
            None,
            1,
        )
        .unwrap();
        db.host_inventory_put(&InventoryEntry {
            key: InventoryKey {
                host: inventory_id,
                kind: ArtifactKind::Image,
                digest: Digest::parse(D1).unwrap(),
                arch: Arch::Amd64,
            },
            ref_name: "base".into(),
            present_at: 1,
            verified_at: None,
            size_bytes: None,
        })
        .unwrap();
        assert!(
            db.put_host_def("inventory-only", &ssh_host("me@new", 22), 2)
                .is_err()
        );

        let heartbeat_id = HostId::named("heartbeat-only");
        db.host_checkpoint(
            &heartbeat_id,
            "heartbeat-only",
            "ssh",
            &HostState::Unknown,
            None,
            1,
        )
        .unwrap();
        db.host_heartbeat(&heartbeat_id, "provision", 2).unwrap();
        assert!(
            db.put_host_def("heartbeat-only", &ssh_host("me@new", 22), 3)
                .is_err()
        );
    }

    #[test]
    fn bare_placeholder_row_is_still_claimable() {
        let db = db();
        let id = HostId::named("placeholder");
        db.conn()
            .execute(
                "INSERT INTO hosts (host_id, name) VALUES (?1, ?2)",
                params![id.as_str(), "placeholder"],
            )
            .unwrap();

        db.put_host_def("placeholder", &ssh_host("me@new", 22), 7)
            .unwrap();
        assert_eq!(db.host_defs().unwrap()[0].0, "placeholder");
    }

    #[test]
    fn put_host_def_refuses_unknown_row_with_persisted_install_grant() {
        let db = db();
        let id = HostId::named("box");
        db.host_checkpoint(&id, "box", "ssh", &HostState::Unknown, None, 1)
            .unwrap();
        db.host_set_consent(&id, true, 2).unwrap();
        let before: (String, String, String, bool, Option<String>, Option<i64>, i64) = db
            .conn()
            .query_row(
                "SELECT host_id, name, state, config_json IS NULL, install_consent, consented_at, updated_at
                   FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();

        let error = db
            .put_host_def("box", &ssh_host("me@new", 22), 9)
            .unwrap_err();
        assert!(error.to_string().contains("--replace"));
        let after: (String, String, String, bool, Option<String>, Option<i64>, i64) = db
            .conn()
            .query_row(
                "SELECT host_id, name, state, config_json IS NULL, install_consent, consented_at, updated_at
                   FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn replace_clears_state_inventory_and_events_atomically() {
        let db = db();
        let id = HostId::named("box");
        db.host_checkpoint(&id, "box", "ssh", &HostState::Ready, Some(&caps()), 1)
            .unwrap();
        db.host_heartbeat(&id, "provision", 2).unwrap();
        db.host_event(&id, "provision", "old machine", 3).unwrap();
        db.host_inventory_put(&InventoryEntry {
            key: InventoryKey {
                host: id.clone(),
                kind: ArtifactKind::Image,
                digest: Digest::parse(D1).unwrap(),
                arch: Arch::Amd64,
            },
            ref_name: "base".into(),
            present_at: 3,
            verified_at: Some(3),
            size_bytes: Some(10),
        })
        .unwrap();

        let replacement = ssh_host("me@new", 22);
        assert!(db.put_host_def_replacing("box", &replacement, 9).unwrap());
        let row = db.host_get(&id).unwrap().unwrap();
        assert_eq!(row.state, HostState::Unknown);
        assert_eq!(row.heartbeat, None);
        assert_eq!(row.caps, None);
        let reset_columns: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM hosts WHERE host_id=?1 AND state='unknown'
                   AND state_meta IS NULL AND caps_json IS NULL AND arch IS NULL
                   AND install_consent IS NULL AND consented_at IS NULL
                   AND heartbeat IS NULL AND active_step IS NULL AND last_probe IS NULL
                   AND last_used IS NULL AND headroom_json IS NULL AND last_headroom IS NULL",
                params![id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reset_columns, 1);
        assert_eq!(db.host_inventory(&id).unwrap().len(), 0);
        assert!(db.host_events_recent(&id, 10).unwrap().is_empty());
        assert_eq!(db.host_defs().unwrap()[0].1.ssh.host, "me@new");

        assert!(
            !db.put_host_def_replacing("fresh", &ssh_host("me@fresh", 22), 10)
                .unwrap()
        );
        let placeholder_id = HostId::named("placeholder");
        db.conn()
            .execute(
                "INSERT INTO hosts (host_id, name) VALUES (?1, ?2)",
                params![placeholder_id.as_str(), "placeholder"],
            )
            .unwrap();
        assert!(
            !db.put_host_def_replacing("placeholder", &ssh_host("me@placeholder", 22), 11)
                .unwrap()
        );
        assert_eq!(db.host_defs().unwrap().len(), 3);
    }

    #[test]
    fn replace_refuses_a_different_db_authority_and_preserves_same_authority() {
        let db = db();
        let original = ssh_host("me@a", 22);
        db.put_host_def("box", &original, 1).unwrap();
        let id = HostId::named("box");
        db.host_checkpoint(&id, "box", "ssh", &HostState::Ready, None, 2)
            .unwrap();
        let before: (String, String, i64) = db
            .conn()
            .query_row(
                "SELECT config_json, state, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert!(
            db.put_host_def_replacing("box", &ssh_host("me@b", 22), 3)
                .is_err()
        );
        let after: (String, String, i64) = db
            .conn()
            .query_row(
                "SELECT config_json, state, updated_at FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after, before);

        let mut same = original;
        same.image = "new:image".into();
        db.put_host_def_replacing("box", &same, 4).unwrap();
        assert_eq!(db.host_get(&id).unwrap().unwrap().state, HostState::Ready);
        assert_eq!(db.host_defs().unwrap()[0].1.image, "new:image");
    }

    #[test]
    fn replace_fails_closed_on_unreadable_db_authority() {
        let db = db();
        let id = HostId::named("broken");
        db.put_host_def("broken", &ssh_host("me@a", 22), 1).unwrap();
        db.conn()
            .execute(
                "UPDATE hosts SET config_json='not-json' WHERE host_id=?1",
                params![id.as_str()],
            )
            .unwrap();
        assert!(
            db.put_host_def_replacing("broken", &ssh_host("me@a", 22), 2)
                .is_err()
        );
        let stored: String = db
            .conn()
            .query_row(
                "SELECT config_json FROM hosts WHERE host_id=?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "not-json");
    }
}
