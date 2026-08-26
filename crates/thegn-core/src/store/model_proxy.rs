//! Store surface for the model proxy's accounting tables (`model_proxy_requests`
//! + `model_proxy_budget_state`).
//!
//! These are the resurrected proxy's *fresh* tables — the orphaned pre-alpha
//! `proxy_*` tables are never reused, migrated, read, or dropped (the
//! multi-branch shared-DB contract). Rows are **metadata only**: no prompt,
//! message, tool-call, or response content is ever stored.

use anyhow::Result;

/// One audited proxy request. Identifiers, counts, and timings only — never
/// message content. `Default` is used by the stats tests and by the failure
/// audit path (which fills only the fields it knows).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct ModelProxyRequestRow {
    /// Wall-clock timestamp (epoch millis).
    pub ts_ms: i64,
    /// Client-facing wire surface: `openai` | `anthropic`.
    pub protocol: String,
    /// Route/tier the request resolved to.
    pub route: String,
    /// Caller scope, most specific available (from the attribution key).
    pub agent: Option<String>,
    pub worktree: Option<String>,
    pub workspace: Option<String>,
    /// The client's requested model id (e.g. `model-proxy/standard`).
    pub client_model: String,
    /// Provider/lane identity that served it (`none` on total failure).
    pub backend: String,
    /// The upstream model id actually dispatched.
    pub backend_model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Tokens served from / written to an upstream prompt cache (0 when absent).
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cost_usd: f64,
    /// Where the cost came from: `subscription` | `header` | `estimate` | `unknown`.
    pub cost_source: String,
    /// Outcome classification: `ok` | `ok_stream` | `all_failed`.
    pub outcome: String,
    /// Optional error/status code on failure.
    pub error_code: Option<String>,
    pub duration_ms: i64,
    /// Time-to-first-byte for streamed responses (None otherwise).
    pub ttfb_ms: Option<i64>,
}

/// Per-scope rolling-window budget accumulator. Limits themselves come from
/// `[model_proxy.budget]` config; this table holds only the moving spend and the
/// window anchor, so state survives restarts and windows roll over cleanly.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct ModelProxyBudgetStateRow {
    /// Budget scope (`global`, `agent:<name>`, `worktree:<path>`,
    /// `workspace:<repo>`, `zone:<name>`).
    pub scope: String,
    /// Start of the current rolling window (epoch millis).
    pub window_start_ms: i64,
    pub spent_tokens: i64,
    pub spent_cost: f64,
    /// Manual kill-switch: refuse the scope regardless of caps.
    pub killed: bool,
}

/// Accounting persistence for the model proxy. Object-safe and synchronous
/// (thegn-core carries no tokio; the DB is hit off-loop by the caller).
pub trait ModelProxyStore {
    /// Appends one audit row.
    fn put_model_proxy_request(&self, row: &ModelProxyRequestRow) -> Result<()>;

    /// Audit rows at or after `since_ms`, newest first, capped at `limit`.
    fn model_proxy_requests_since(
        &self,
        since_ms: i64,
        limit: i64,
    ) -> Result<Vec<ModelProxyRequestRow>>;

    /// The budget accumulator for `scope`, if any.
    fn model_proxy_budget_state(&self, scope: &str) -> Result<Option<ModelProxyBudgetStateRow>>;

    /// Every budget accumulator (for the stats surface).
    fn model_proxy_budget_states(&self) -> Result<Vec<ModelProxyBudgetStateRow>>;

    /// Atomically attribute spend to a scope's rolling window, advancing the
    /// anchor when `window_len_ms > 0` and the current window has lapsed.
    /// Returns the post-update row.
    fn add_model_proxy_spend(
        &self,
        scope: &str,
        tokens: i64,
        cost: f64,
        now_ms: i64,
        window_len_ms: i64,
    ) -> Result<ModelProxyBudgetStateRow>;

    /// Sets (or clears) the manual kill-switch for a scope, creating the row if
    /// it does not exist.
    fn set_model_proxy_kill_switch(&self, scope: &str, killed: bool) -> Result<()>;
}
