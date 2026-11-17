//! Vercel cost accrual for `thegn fleet cost`: the current session's active
//! CPU and memory, and snapshot storage, priced at the Pro iad1 rates. An
//! estimate for the current session only.
//!
//! Spine stub (b156 lane 1): signatures are fixed; lane 2 fills the bodies.

/// The current session's usage fields (all optional in the API response).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionUsage {
    pub active_cpu_ms: Option<u64>,
    pub memory_mb: Option<u64>,
    pub vcpus: Option<u32>,
    pub started_at_ms: Option<i64>,
    pub stopped_at_ms: Option<i64>,
}

/// One snapshot of a sandbox.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotUsage {
    pub id: String,
    pub size_bytes: u64,
    pub expires_at_ms: Option<i64>,
    pub status: String,
}

/// Unit prices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub cpu_usd_per_vcpu_hour: f64,
    pub mem_usd_per_gb_hour: f64,
    pub snapshot_usd_per_gb_month: f64,
}

/// Vercel Pro, iad1 (the b154 rates).
pub const PRO_IAD1: Rates = Rates {
    cpu_usd_per_vcpu_hour: 0.128,
    mem_usd_per_gb_hour: 0.0212,
    snapshot_usd_per_gb_month: 0.08,
};

/// Accrued spend for one session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accrual {
    pub cpu_usd: f64,
    pub memory_usd: f64,
}

/// The current session's accrued spend at `now_ms`, or `None` when the
/// response lacks the fields to price it.
pub fn session_accrual(_u: &SessionUsage, _now_ms: i64, _r: &Rates) -> Option<Accrual> {
    None
}

/// Monthly storage cost of the `created` snapshots.
pub fn snapshot_month_usd(_s: &[SnapshotUsage], _r: &Rates) -> f64 {
    0.0
}
