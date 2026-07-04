//! The **host-state** seam: durable host state-machine checkpoints + consent +
//! heartbeat (the `hosts` row), digest-keyed inventory (`host_inventory`), and
//! the forensic event trail (`host_events`) — schema v30.
//!
//! A future server backend that provisions hosts on behalf of many users would
//! implement this against Postgres; the local shell implements it over the
//! embedded SQLite `Db` (`host_db.rs`). Timestamps are caller-supplied unix
//! seconds (deterministic tests; the DB is a cache — hosts are truth).

use anyhow::Result;

use crate::host::{HostCaps, HostId};
use crate::host_config::HostConfig;
use crate::host_db::HostRow;
use crate::host_machine::HostState;
use crate::inventory::{InventoryEntry, InventoryKey};

/// Persisted host state. Object-safe (all `&self` + concrete args), so a
/// `&dyn HostStore` works for backend-agnostic consumers. [`crate::db::Db`] is
/// the embedded-SQLite implementation (`host_db.rs`).
pub trait HostStore {
    /// Fetch a host row by id (`None` when absent).
    fn host_get(&self, id: &HostId) -> Result<Option<HostRow>>;

    /// All host rows, ordered by id.
    fn hosts_all(&self) -> Result<Vec<HostRow>>;

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
    ) -> Result<()>;

    /// Leader liveness: stamp the step being worked plus a heartbeat.
    fn host_heartbeat(&self, id: &HostId, active_step: &str, now: i64) -> Result<()>;

    /// Clear the heartbeat on terminal states so an idle row never looks driven.
    fn host_heartbeat_clear(&self, id: &HostId) -> Result<()>;

    /// Stamp the last successful probe time.
    fn host_touch_probe(&self, id: &HostId, now: i64) -> Result<()>;

    /// Stamp the last used time.
    fn host_touch_used(&self, id: &HostId, now: i64) -> Result<()>;

    /// Persist the per-host install grant (`granted`/`declined`).
    fn host_set_consent(&self, id: &HostId, granted: bool, now: i64) -> Result<()>;

    /// Remove a host row + its inventory + events.
    fn host_delete(&self, id: &HostId) -> Result<()>;

    /// Persist a USER-ADDED host definition (the serialized [`HostConfig`] rides
    /// the host's own row, merged into the config catalog at load).
    fn put_host_def(&self, name: &str, hc: &HostConfig, now: i64) -> Result<()>;

    /// All user-added host definitions (rows carrying a `config_json`).
    fn host_defs(&self) -> Result<Vec<(String, HostConfig)>>;

    /// The inventory entries recorded for a host.
    fn host_inventory(&self, id: &HostId) -> Result<Vec<InventoryEntry>>;

    /// Upsert one inventory entry.
    fn host_inventory_put(&self, e: &InventoryEntry) -> Result<()>;

    /// Stamp a successful on-host verification of one artifact.
    fn host_inventory_verify(&self, key: &InventoryKey, now: i64) -> Result<()>;

    /// Drop one artifact (delivery superseded / digest mismatch cleanup).
    fn host_inventory_remove(&self, key: &InventoryKey) -> Result<()>;

    /// Append to the forensic step trail.
    fn host_event(&self, id: &HostId, step: &str, detail: &str, now: i64) -> Result<()>;

    /// Most-recent-first slice of the event trail: `(at, step, detail)`.
    fn host_events_recent(&self, id: &HostId, limit: usize) -> Result<Vec<(i64, String, String)>>;
}
