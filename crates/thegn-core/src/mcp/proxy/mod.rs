//! The mcp-proxy hub — pure aggregation, filtering, routing, breaker, reconcile
//! and partition logic for `thegn mcp proxy`.
//!
//! `thegn mcp proxy` is one aggregated MCP stdio endpoint an agent registers as
//! its single MCP server. It fans every *exposed* `[mcp_servers.<name>]`
//! upstream out behind namespaced tool names (`<upstream>__<tool>`), routes
//! each `tools/call` to the owning upstream, and merges `tools/list` — so an
//! agent registers one thegn entry instead of one per upstream.
//!
//! Everything here is **pure** (`serde_json::Value` in, decisions out — no child
//! processes, no sockets, no tokio, no clock except injected `now_ms`). The
//! host (`thegn-host`) owns the I/O: spawning upstream children, pumping their
//! stdio, running the health/reconcile ticks, and resolving secret refs at
//! spawn. This split is what lets the security-critical decisions — default-deny
//! tool exposure, the circuit breaker, the reconcile diff, partition-key
//! derivation — be exhaustively unit-tested against the 95% core gate.
//!
//! Submodules:
//! - [`filter`] — default-deny `proxy.tools` glob evaluation.
//! - [`aggregate`](mod@aggregate) — merge + namespace + filter upstream tool
//!   lists.
//! - [`route`] — namespaced call → (upstream, tool) + per-connection id rewrite.
//! - [`breaker`] — the Closed/Open/HalfOpen circuit state machine.
//! - [`reconcile`](mod@reconcile) — old × new effective config →
//!   start/stop/restart/refilter.
//! - [`partition`] — scope-key derivation + placeholder expansion (THE-49).

pub mod aggregate;
pub mod breaker;
pub mod filter;
pub mod partition;
pub mod reconcile;
pub mod route;

pub use aggregate::{Aggregate, UpstreamSummary, UpstreamTools, aggregate, initialize_result};
pub use breaker::{Breaker, BreakerConfig, BreakerState};
pub use filter::{partition_tools, tool_exposed};
pub use partition::{Withheld, WorktreeContext, expand_spec, partition_key};
pub use reconcile::{InstanceSpec, ReconcileAction, reconcile};
pub use route::{IdRewriter, PendingId, ToolRoute, unknown_tool_error};

/// The proxy's stable MCP server name (the `serverInfo.name` an agent sees).
pub const PROXY_SERVER_NAME: &str = "thegn-mcp-proxy";

/// Build the `<upstream>__<tool>` namespaced tool name. Both segments already
/// match the MCP name charset (`[A-Za-z0-9_-]`), so the join needs no escaping;
/// routing is a table lookup (never a re-parse of this string), so the doubled
/// `_` separator being non-unique across pathological names is harmless — the
/// aggregator dedupes on the joined string itself.
pub fn namespaced(upstream: &str, tool: &str) -> String {
    format!("{upstream}__{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_joins_with_double_underscore() {
        assert_eq!(namespaced("git", "search"), "git__search");
        assert_eq!(namespaced("a", "b"), "a__b");
    }
}
