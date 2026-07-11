//! [`ComputeLedgerStore`] — the placement engine's spend ledger, deliberately
//! SEPARATE from the LLM proxy's ([`crate::store::ProxyStore`]) but identical
//! in shape (scope → zone → global caps, monthly window, kill-switch) so the
//! mental model transfers. Two cost categories: `fixed` (managed hosts:
//! hourly rate × lifetime, idempotent watermark accrual) and `metered`
//! (spillover: per-create + active-time).

use anyhow::Result;

/// One `compute_budgets` row (mirror of the proxy's budget row minus tokens).
#[derive(Debug, Clone, PartialEq)]
pub struct ComputeBudgetRow {
    /// `global` | `zone:<name>` | `provider:<name>` | `worktree:<path>`.
    pub scope: String,
    pub period: String,
    pub spent_cost: f64,
    pub limit_cost: Option<f64>,
    pub reset_ms: i64,
    pub killed: bool,
}

/// One live billing meter (`compute_meters`): a resource accruing cost by
/// watermark — `rate × (now − last_accrued)` — so ticks are idempotent and
/// catch-up-correct after any gap.
#[derive(Debug, Clone, PartialEq)]
pub struct ComputeMeterRow {
    /// Host name / sandbox id — unique per billed resource.
    pub resource: String,
    pub provider: String,
    /// `fixed` (managed host) | `metered` (spillover active-time).
    pub category: String,
    pub rate_hourly: f64,
    /// Attribution targets (scope + optional zone; global is implicit).
    pub scope: String,
    pub zone: String,
    pub started_at_ms: i64,
    pub last_accrued_ms: i64,
    pub stopped_at_ms: Option<i64>,
}

pub trait ComputeLedgerStore {
    fn compute_budget(&self, scope: &str) -> Result<Option<ComputeBudgetRow>>;
    /// Set caps only — spend is never clobbered (the `sync_budget_caps`
    /// contract).
    fn set_compute_budget_limits(
        &self,
        scope: &str,
        period: &str,
        limit_cost: Option<f64>,
        reset_ms: i64,
    ) -> Result<()>;
    fn set_compute_kill_switch(&self, scope: &str, killed: bool) -> Result<()>;
    /// Add spend to one scope, rolling the window when `reset_ms` elapsed.
    /// Returns `(spent_after, killed)`.
    fn add_compute_spend(&self, scope: &str, cost: f64, now_ms: i64) -> Result<(f64, bool)>;
    fn start_compute_meter(&self, m: &ComputeMeterRow) -> Result<()>;
    /// All meters not yet stopped.
    fn live_compute_meters(&self) -> Result<Vec<ComputeMeterRow>>;
    /// Advance one meter's watermark to `now_ms`, returning the cost accrued
    /// by this call (0 when the watermark is already current).
    fn accrue_compute_meter(&self, resource: &str, now_ms: i64) -> Result<f64>;
    /// Final accrual + stop. Idempotent; returns the final slice's cost.
    fn stop_compute_meter(&self, resource: &str, now_ms: i64) -> Result<f64>;
}
