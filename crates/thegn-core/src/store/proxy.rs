//! The **proxy-state** seam: LLM-proxy exhaustion markers, the per-request
//! audit log, virtual keys, and spend budgets.
//!
//! This backs the `thegn-proxy` daemon (a separate crate) — the strongest
//! near-term case for a shared store trait, since the very same audit/budget
//! logic is exactly what a future hosted/multi-user thegn would run against
//! Postgres server-side. The daemon depends only on this trait, so swapping the
//! embedded SQLite `Db` for a server backend is a localized change.
//!
//! Timestamps are caller-supplied epoch-millis (`*_ms`) so the store stays free
//! of wall-clock coupling — the proxy supplies real values from chrono. Audit
//! rows carry only routing/usage/cost metadata, never prompt/completion bodies.

use anyhow::Result;

use crate::db::{ProxyBudgetRow, ProxyHealthRow, ProxyRequestRow};

/// Persisted LLM-proxy state. Object-safe: every method takes `&self` and
/// concrete arguments, so `&dyn ProxyStore` works for backend-agnostic
/// consumers. [`crate::db::Db`] is the embedded-SQLite implementation.
pub trait ProxyStore {
    /// Upsert an exhaustion marker for `(backend, model)`. `kind` is the
    /// [`crate::proxy::ExhaustionKind`] rendered as a short string. Replaces the
    /// Go proxy's `health.json` persistence.
    #[allow(clippy::too_many_arguments)]
    fn put_proxy_health(
        &self,
        backend: &str,
        model: &str,
        kind: &str,
        reason: &str,
        since_ms: i64,
        next_probe_ms: i64,
        is_stale: bool,
        consecutive_failures: i64,
        cred_file: Option<&str>,
        cred_mtime_ms: Option<i64>,
    ) -> Result<()>;

    /// Clear an exhaustion marker (backend recovered).
    fn clear_proxy_health(&self, backend: &str, model: &str) -> Result<()>;

    /// Load all live exhaustion markers (those whose `next_probe_ms` is still in
    /// the future), for hydrating the in-memory health map on startup.
    fn load_proxy_health(&self, now_ms: i64) -> Result<Vec<ProxyHealthRow>>;

    /// Append a request audit row (metadata only). Returns the new row id.
    fn put_proxy_request(&self, r: &ProxyRequestRow) -> Result<i64>;

    /// The most recent `limit` proxy requests for a worktree, newest first.
    fn proxy_requests(&self, worktree: &str, limit: usize) -> Result<Vec<ProxyRequestRow>>;

    /// Total proxy spend (USD) for a worktree since `since_ms` (`0.0` if none).
    fn proxy_spend_since(&self, worktree: &str, since_ms: i64) -> Result<f64>;

    /// Look up a virtual key by id, returning `(scope, upstream)` when the key
    /// exists and is not revoked.
    fn proxy_virtual_key(&self, key_id: &str) -> Result<Option<(String, Option<String>)>>;

    /// Register a virtual key (token already hashed by the caller).
    fn put_proxy_virtual_key(
        &self,
        key_id: &str,
        token_hash: &str,
        label: &str,
        scope: &str,
        upstream: Option<&str>,
        now_ms: i64,
    ) -> Result<()>;

    /// Revoke a virtual key.
    fn revoke_proxy_virtual_key(&self, key_id: &str, now_ms: i64) -> Result<()>;

    /// Add spend (tokens + cost) to a budget scope, creating the row if absent
    /// and rolling the window over first when `reset_ms` has elapsed. Returns
    /// the post-update `(spent_tokens, spent_cost, killed)`.
    fn add_proxy_spend(
        &self,
        scope: &str,
        tokens: i64,
        cost: f64,
        now_ms: i64,
    ) -> Result<(i64, f64, bool)>;

    /// Fetch a budget row for enforcement checks, if one exists.
    fn proxy_budget(&self, scope: &str) -> Result<Option<ProxyBudgetRow>>;

    /// Set the caps + rolling window for a budget scope (creating the row if
    /// absent), without touching accumulated spend. A `None` limit means no cap.
    fn set_proxy_budget_limits(
        &self,
        scope: &str,
        period: &str,
        limit_tokens: Option<i64>,
        limit_cost: Option<f64>,
        reset_ms: i64,
    ) -> Result<()>;

    /// Set or clear the kill-switch on a budget scope.
    fn set_proxy_kill_switch(&self, scope: &str, killed: bool) -> Result<()>;
}
