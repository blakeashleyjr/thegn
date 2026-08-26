//! Tool-call routing + per-connection JSON-RPC id rewriting.
//!
//! Routing is a *lookup* in the aggregated table
//! ([`super::aggregate`](mod@super::aggregate)), never
//! a re-parse of the `<upstream>__<tool>` string — so a filtered or unknown
//! name is a clean JSON-RPC error and nothing is ever forwarded to an upstream
//! for a name the filter did not admit.
//!
//! When the daemon shares one upstream child across several agents' shims,
//! their JSON-RPC id spaces would collide on that child's stdin. [`IdRewriter`]
//! maps each inbound `(connection, original id)` to a fresh monotone id sent
//! upstream, and maps the upstream's response id back to the originating
//! connection + original id. Pure and unit-testable — the socket pump just
//! calls `rewrite` on the way out and `resolve` on the way back.

use serde_json::Value;
use std::collections::HashMap;

/// Where a namespaced tool call is dispatched: the owning upstream and the
/// original (un-namespaced) tool name to forward to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRoute {
    pub upstream: String,
    pub tool: String,
}

/// The JSON-RPC error the proxy answers for a tool name no exposed upstream
/// advertises (unknown, or filtered out). `-32601` = "method not found", the
/// closest standard code for "no such tool".
pub fn unknown_tool_error(name: &str) -> (i32, String) {
    (
        -32601,
        format!(
            "no such tool `{name}` — not advertised by any exposed upstream (check `thegn mcp list`)"
        ),
    )
}

/// The JSON-RPC error for a call whose upstream breaker is open. `-32000` =
/// generic server error; the message names the upstream so the agent learns
/// which backend is down rather than seeing the tool vanish.
pub fn breaker_open_error(upstream: &str) -> (i32, String) {
    (
        -32000,
        format!(
            "upstream `{upstream}` is temporarily unavailable (circuit breaker open) — retry shortly"
        ),
    )
}

/// A parked outbound call: which connection issued it and its original id, so
/// the response can be delivered back to the right shim under the id it used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingId {
    pub conn: u64,
    pub orig: Value,
}

/// Per-shared-upstream JSON-RPC id remapper.
#[derive(Debug, Default)]
pub struct IdRewriter {
    next: u64,
    pending: HashMap<u64, PendingId>,
}

impl IdRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh upstream-facing id for `(conn, orig)` and remember the
    /// mapping. The returned id is what to put on the request forwarded to the
    /// shared upstream.
    pub fn rewrite(&mut self, conn: u64, orig: Value) -> u64 {
        self.next = self.next.wrapping_add(1);
        let id = self.next;
        self.pending.insert(id, PendingId { conn, orig });
        id
    }

    /// Resolve an upstream response's id back to its originator, consuming the
    /// mapping. `None` if the id is unknown (a duplicate/late response) — the
    /// pump drops it rather than misdelivering.
    pub fn resolve(&mut self, upstream_id: u64) -> Option<PendingId> {
        self.pending.remove(&upstream_id)
    }

    /// Forget every parked call for a connection that has gone away (its shim
    /// disconnected), so their eventual responses are dropped, not misrouted.
    /// Returns how many were dropped.
    pub fn drop_connection(&mut self, conn: u64) -> usize {
        let before = self.pending.len();
        self.pending.retain(|_, p| p.conn != conn);
        before - self.pending.len()
    }

    /// Outstanding parked calls (for status/diagnostics).
    pub fn outstanding(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_tool_is_method_not_found() {
        let (code, msg) = unknown_tool_error("git__nope");
        assert_eq!(code, -32601);
        assert!(msg.contains("git__nope"), "{msg}");
    }

    #[test]
    fn breaker_open_names_upstream() {
        let (code, msg) = breaker_open_error("git");
        assert_eq!(code, -32000);
        assert!(msg.contains("git"), "{msg}");
        assert!(msg.contains("breaker"), "{msg}");
    }

    #[test]
    fn rewrite_allocates_unique_ids_and_resolves_back() {
        let mut r = IdRewriter::new();
        let a = r.rewrite(1, json!(10));
        let b = r.rewrite(2, json!("req-b"));
        assert_ne!(a, b);
        assert_eq!(r.outstanding(), 2);

        let pa = r.resolve(a).unwrap();
        assert_eq!(pa.conn, 1);
        assert_eq!(pa.orig, json!(10));
        // Resolving consumes the mapping.
        assert!(r.resolve(a).is_none());
        assert_eq!(r.outstanding(), 1);

        let pb = r.resolve(b).unwrap();
        assert_eq!(pb.conn, 2);
        assert_eq!(pb.orig, json!("req-b"));
    }

    #[test]
    fn two_connections_reusing_the_same_original_id_do_not_collide() {
        let mut r = IdRewriter::new();
        // Both agents naively use id=1 — the classic multiplex collision.
        let up_a = r.rewrite(1, json!(1));
        let up_b = r.rewrite(2, json!(1));
        assert_ne!(up_a, up_b);
        assert_eq!(r.resolve(up_a).unwrap().conn, 1);
        assert_eq!(r.resolve(up_b).unwrap().conn, 2);
    }

    #[test]
    fn unknown_response_id_resolves_to_none() {
        let mut r = IdRewriter::new();
        assert!(r.resolve(999).is_none());
    }

    #[test]
    fn drop_connection_forgets_only_its_calls() {
        let mut r = IdRewriter::new();
        let a1 = r.rewrite(1, json!(1));
        let _a2 = r.rewrite(1, json!(2));
        let b1 = r.rewrite(2, json!(1));
        assert_eq!(r.drop_connection(1), 2);
        assert_eq!(r.outstanding(), 1);
        assert!(r.resolve(a1).is_none());
        assert_eq!(r.resolve(b1).unwrap().conn, 2);
    }
}
