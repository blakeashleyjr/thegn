//! [`PlacementStore`] — the placement engine's persistence seam: the per-host
//! capacity index (declared spec / overcommit / measured load), the tenancy
//! ledger (which sandbox reserves how much on which host — the reservation is
//! the cross-process linearization point), cooldown health markers (autoscale
//! template lanes, later spillover providers), and decision traces for
//! `placement explain`.

use anyhow::Result;

use crate::capacity::{HostOwnership, HostSpec, MeasuredLoad, ReservedTotals, ResourceReq};
use crate::host::HostId;

/// One `host_capacity` row: everything the broker needs to know about a
/// host's resources without re-resolving config. `measured` is the latest
/// observational sample (display + spread hints for every host; a capacity
/// source only where no authoritative spec exists).
#[derive(Debug, Clone, PartialEq)]
pub struct HostCapacityRow {
    pub host: HostId,
    pub ownership: HostOwnership,
    /// Declared machine size; `None` ⇒ unknown ⇒ never pack-eligible.
    pub spec: Option<HostSpec>,
    /// Per-host overcommit override in percent; `0` ⇒ use the resolved config.
    pub overcommit_cpu_pct: u32,
    pub overcommit_mem_pct: u32,
    /// Managed hosts: the creating provider + size template (autoscale lanes).
    pub provider: String,
    pub template: String,
    pub created_at: Option<i64>,
    pub measured: Option<MeasuredLoad>,
    pub updated_at: i64,
}

/// How a tenancy landed on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenancyMode {
    Packed,
    Dedicated,
    /// Explicit `[env.<n>] host = ...` pin — bypassed the broker but is still
    /// accounted so capacity stays truthful.
    Pinned,
}

impl TenancyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TenancyMode::Packed => "packed",
            TenancyMode::Dedicated => "dedicated",
            TenancyMode::Pinned => "pinned",
        }
    }
    pub fn parse(s: &str) -> Option<TenancyMode> {
        match s {
            "packed" => Some(TenancyMode::Packed),
            "dedicated" => Some(TenancyMode::Dedicated),
            "pinned" => Some(TenancyMode::Pinned),
            _ => None,
        }
    }
}

/// Tenancy lifecycle: `Reserved` (slot held, sandbox not yet provisioned) →
/// `Active` (provision marker written) → `Released` (sandbox/worktree gone;
/// the row is kept briefly for forensic joins, excluded from all sums).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenancyState {
    Reserved,
    Active,
    Released,
}

impl TenancyState {
    pub fn as_str(self) -> &'static str {
        match self {
            TenancyState::Reserved => "reserved",
            TenancyState::Active => "active",
            TenancyState::Released => "released",
        }
    }
    pub fn parse(s: &str) -> Option<TenancyState> {
        match s {
            "reserved" => Some(TenancyState::Reserved),
            "active" => Some(TenancyState::Active),
            "released" => Some(TenancyState::Released),
            _ => None,
        }
    }
}

/// One `host_tenancy` row.
#[derive(Debug, Clone, PartialEq)]
pub struct TenancyRow {
    /// Resolved sandbox name — the primary key (one placement per sandbox).
    pub sandbox: String,
    pub host: HostId,
    /// Bound worktree; empty for a pool spare pre-claim.
    pub worktree: String,
    /// Zone co-tenancy key; empty = unzoned (its own co-tenancy class).
    pub zone: String,
    pub mode: TenancyMode,
    pub req: ResourceReq,
    pub state: TenancyState,
    pub reserved_at: i64,
    pub activated_at: Option<i64>,
    pub released_at: Option<i64>,
}

/// Outcome of the guarded atomic reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Reserved,
    /// The sandbox already holds a live tenancy (on the returned host).
    AlreadyPlaced(HostId),
    /// Σ floors + the ask would exceed the passed ceilings.
    NoCapacity,
    /// A live tenant belongs to a different zone.
    ZoneConflict,
    /// The host holds (or the request is) a dedicated tenancy alongside others.
    DedicatedConflict,
}

/// A cooldown marker (`placement_health`): the compute analog of the proxy's
/// exhaustion tracking. `key` namespaces the subject — `tpl:<provider>/<size>`
/// for autoscale lanes, `provider:<name>` for spillover, `host:<id>` reserved
/// for per-host cooldowns. Fail-back is implicit: eligible again once
/// `now ≥ retry_at_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthMarker {
    pub key: String,
    pub kind: String,
    pub reason: String,
    pub since_ms: i64,
    pub retry_at_ms: i64,
    pub consecutive: u32,
}

/// One persisted placement decision (for `placement explain` / `events`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementEventRow {
    pub ts: i64,
    pub worktree: String,
    /// `dedicated` | `packed` | `pinned` | `provision` | `spillover` |
    /// `queued` | `rejected` | `error`.
    pub decision: String,
    /// Chosen target (`host:<id>` / `tpl:...` / `provider:...`), empty when none.
    pub chosen: String,
    /// Serialized `PlacementTrace` (per-candidate outcomes).
    pub trace_json: String,
}

pub trait PlacementStore {
    // ── capacity index ──────────────────────────────────────────────────
    fn capacity_get(&self, host: &HostId) -> Result<Option<HostCapacityRow>>;
    fn capacity_all(&self) -> Result<Vec<HostCapacityRow>>;
    fn capacity_put(&self, row: &HostCapacityRow) -> Result<()>;
    fn capacity_delete(&self, host: &HostId) -> Result<()>;
    /// Refresh only the observational sample (every reachable host gets one —
    /// managed and independent alike — for display + ranking hints).
    fn capacity_set_measured(&self, host: &HostId, m: &MeasuredLoad, now: i64) -> Result<()>;

    // ── tenancy ledger ──────────────────────────────────────────────────
    /// The atomic check-and-reserve: a single guarded INSERT enforcing the
    /// capacity ceilings, zone co-tenancy, and dedicated exclusivity
    /// (single-writer SQLite ⇒ cross-process linearization point).
    /// `ceilings` = the host's `(cpu_milli, mem_mb)` packing ceilings under
    /// the resolved overcommit.
    fn tenancy_reserve(
        &self,
        t: &TenancyRow,
        ceilings: (u64, u64),
        now: i64,
    ) -> Result<ReserveOutcome>;
    /// Unconditional insert/replace — pins and dedicated placements bypass the
    /// fits math but are still accounted.
    fn tenancy_force(&self, t: &TenancyRow, now: i64) -> Result<()>;
    fn tenancy_activate(&self, sandbox: &str, now: i64) -> Result<()>;
    fn tenancy_release(&self, sandbox: &str, now: i64) -> Result<()>;
    /// Pool-spare claim: rebind the reservation to its worktree — same
    /// sandbox, same host, amounts unchanged (never a release+re-reserve).
    fn tenancy_rebind(&self, sandbox: &str, worktree: &str) -> Result<()>;
    fn tenancy_for(&self, sandbox: &str) -> Result<Option<TenancyRow>>;
    /// Live (reserved|active) tenants of a host.
    fn tenants_of(&self, host: &HostId) -> Result<Vec<TenancyRow>>;
    /// Σ floors of a host's live tenants.
    fn reserved_totals(&self, host: &HostId) -> Result<ReservedTotals>;
    /// Release `reserved` rows older than `before` (crashed drivers).
    /// Returns how many were swept.
    fn tenancy_sweep_stale(&self, before: i64) -> Result<usize>;

    // ── cooldown health ─────────────────────────────────────────────────
    fn health_mark(&self, m: &HealthMarker) -> Result<()>;
    fn health_clear(&self, key: &str) -> Result<()>;
    fn health_get(&self, key: &str) -> Result<Option<HealthMarker>>;
    /// All markers still cooling at `now_ms` (expired ones are pruned).
    fn health_cooling(&self, now_ms: i64) -> Result<Vec<HealthMarker>>;

    // ── decision traces ─────────────────────────────────────────────────
    /// Insert a decision trace (pruned to the newest ~500).
    fn placement_event_put(&self, e: &PlacementEventRow) -> Result<()>;
    /// Newest-first traces, optionally filtered to one worktree.
    fn placement_events(
        &self,
        worktree: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlacementEventRow>>;
}
