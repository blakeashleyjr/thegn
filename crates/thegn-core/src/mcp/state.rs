//! `StateRouter` — the *live-state* half of `thegn mcp serve`: read-only tools
//! that answer from a running instance (the pane daemon / DB cache), beside
//! the static docs tools in [`super::docs::DocsRouter`].
//!
//! Mirrors the `DocsRouter` pattern: this module is PURE. It owns the tool
//! *descriptions* (name, description, argument schema) and the routing +
//! scope policy; the actual data fetch is injected by the host as a closure
//! (`FetchFn`), so daemon sockets, tokio and the DB stay out of core.
//!
//! Scope model: the router is constructed with the *allowed* capability ids
//! (the host maps `--scopes` → caps via [`crate::control::required_scope`]).
//! `tools/list` advertises only allowed tools; a `tools/call` for a known but
//! not-allowed tool answers a JSON-RPC error naming the missing scope, so an
//! agent learns *why* rather than seeing the tool vanish.
//!
//! Tool names are the catalog ids' MCP-safe projection
//! ([`crate::capability::CapId::tool_name`]): `sessions.list` → `sessions_list`.

use crate::capability::{lookup, scope_of};
use serde_json::{Value, json};

/// One state tool: the catalog capability it projects plus its MCP description.
/// The argument schema is uniform today (no arguments) — every implemented
/// tool is a plain listing; per-tool schemas come with the first parameterised
/// tool.
pub struct StateToolSpec {
    /// Catalog capability id (`sessions.list`); the tool name is its
    /// [`crate::capability::CapId::tool_name`] projection.
    pub cap: &'static str,
    pub description: &'static str,
}

/// The implemented state tools. Must stay in step with [`MCP_STATE_CAPS`]
/// (pinned by `state_tools_match_state_caps`).
pub const STATE_TOOLS: &[StateToolSpec] = &[
    StateToolSpec {
        cap: "sessions.list",
        description: "List the thegn pane daemon's live terminal sessions: id, program, \
                      worktree, geometry, attached clients, lease state. Live state — \
                      requires a running daemon (start thegn or `thegn daemon`).",
    },
    StateToolSpec {
        cap: "worktrees.list",
        description: "List the worktrees registered with thegn: path, branch, repo root, \
                      remote location. Answers from the daemon when one is running, else \
                      from thegn's local DB cache (`source` says which).",
    },
    StateToolSpec {
        cap: "leases.list",
        description: "Relay lease state per session — which detached sessions are being \
                      kept warm, and until when. Requires a running daemon.",
    },
    StateToolSpec {
        cap: "me",
        description: "The caller's identity as thegn's control plane sees it: pairing id, \
                      label and granted scopes. Requires a running daemon.",
    },
];

/// Host capabilities the MCP server exposes as state tools, by catalog id.
/// The docs tools (`search_docs`, `read_doc`, …) are not catalog items — they
/// read the embedded help corpus, not a running instance. Every other
/// `Surface::Mcp` catalog row is still excused in
/// `capability::SURFACE_GAPS`; this table is the thing that grows to retire
/// those excuses (`mcp_tools_cover_catalog` arbitrates both directions).
pub const MCP_STATE_CAPS: &[&str] = &["sessions.list", "worktrees.list", "leases.list", "me"];

/// The injected data fetch: `(capability id, tool arguments) → payload JSON`.
/// `Err(msg)` becomes a JSON-RPC error (e.g. "daemon not reachable — …").
pub type FetchFn<'a> = dyn Fn(&str, &Value) -> Result<Value, String> + 'a;

/// Routes `tools/list` entries and `tools/call`s for the state tools.
/// Composed into [`super::docs::DocsRouter`] via `with_state`, so one MCP
/// endpoint serves docs + state in a single `tools/list` reply.
pub struct StateRouter<'a> {
    /// Allowed capability ids (host-computed from `--scopes`).
    allowed: Vec<&'static str>,
    fetch: Box<FetchFn<'a>>,
}

impl<'a> StateRouter<'a> {
    pub fn new(
        allowed: Vec<&'static str>,
        fetch: impl Fn(&str, &Value) -> Result<Value, String> + 'a,
    ) -> Self {
        Self {
            allowed,
            fetch: Box::new(fetch),
        }
    }

    /// The `tools/list` entries for the *allowed* tools.
    pub fn tool_entries(&self) -> Vec<Value> {
        STATE_TOOLS
            .iter()
            .filter(|t| self.allowed.contains(&t.cap))
            .map(|t| {
                json!({
                    "name": tool_name(t.cap),
                    "description": t.description,
                    "inputSchema": { "type": "object", "properties": {} },
                })
            })
            .collect()
    }

    /// Handle a `tools/call` if `name` is a state tool (allowed or not);
    /// `None` means "not mine" so the caller can fall through to its own
    /// unknown-tool error.
    pub fn call(&self, name: &str, args: &Value) -> Option<Result<Value, (i32, String)>> {
        let spec = STATE_TOOLS.iter().find(|t| tool_name(t.cap) == name)?;
        if !self.allowed.contains(&spec.cap) {
            let scope = lookup(spec.cap)
                .map(|c| scope_of(c).as_str())
                .unwrap_or("read");
            return Some(Err((
                -32001,
                format!(
                    "tool `{name}` requires scope `{scope}` — grant it with \
                     `thegn mcp serve --scopes {scope}`"
                ),
            )));
        }
        Some(match (self.fetch)(spec.cap, args) {
            Ok(v) => Ok(super::docs::text_result(super::docs::pretty(&v))),
            Err(msg) => Err((-32000, msg)),
        })
    }
}

/// The catalog id's MCP tool-name projection (`sessions.list` → `sessions_list`).
fn tool_name(cap: &str) -> String {
    cap.replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Surface, coverage_problems};

    fn all_caps() -> Vec<&'static str> {
        MCP_STATE_CAPS.to_vec()
    }

    #[test]
    fn mcp_tools_cover_catalog() {
        let problems = coverage_problems(Surface::Mcp, MCP_STATE_CAPS);
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn state_tools_match_state_caps() {
        let spec_caps: Vec<&str> = STATE_TOOLS.iter().map(|t| t.cap).collect();
        assert_eq!(spec_caps, MCP_STATE_CAPS);
    }

    #[test]
    fn tool_entries_advertise_only_allowed_tools() {
        let r = StateRouter::new(vec!["me", "worktrees.list"], |_, _| Ok(json!(null)));
        let names: Vec<String> = r
            .tool_entries()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["worktrees_list", "me"]);
        // Every entry carries a description and an object arg schema.
        for t in r.tool_entries() {
            assert!(!t["description"].as_str().unwrap().is_empty());
            assert_eq!(t["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn call_routes_cap_id_to_fetch() {
        let r = StateRouter::new(all_caps(), |cap, _args| Ok(json!({ "cap": cap })));
        let res = r.call("sessions_list", &json!({})).unwrap().unwrap();
        let text = res["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("sessions.list"), "{text}");
    }

    #[test]
    fn call_surfaces_fetch_error() {
        let r = StateRouter::new(all_caps(), |_, _| {
            Err("daemon not reachable — start thegn or `thegn daemon`".to_string())
        });
        let (code, msg) = r.call("me", &json!({})).unwrap().unwrap_err();
        assert_eq!(code, -32000);
        assert!(msg.contains("daemon not reachable"), "{msg}");
    }

    #[test]
    fn call_denied_names_the_missing_scope() {
        let r = StateRouter::new(vec![], |_, _| Ok(json!(null)));
        let (code, msg) = r.call("worktrees_list", &json!({})).unwrap().unwrap_err();
        assert_eq!(code, -32001);
        assert!(msg.contains("scope `read`"), "{msg}");
        assert!(msg.contains("--scopes read"), "{msg}");
    }

    #[test]
    fn call_unknown_tool_is_not_mine() {
        let r = StateRouter::new(all_caps(), |_, _| Ok(json!(null)));
        assert!(r.call("read_doc", &json!({})).is_none());
        assert!(r.call("nope", &json!({})).is_none());
    }

    #[test]
    fn every_state_cap_is_read_scope_today() {
        // The default `--scopes read` must cover the whole implemented set;
        // this pins that a future write-side tool forces a deliberate
        // scope-model decision rather than silently widening `read`.
        for cap in MCP_STATE_CAPS {
            let c = lookup(cap).expect("state cap in catalog");
            assert_eq!(scope_of(c), crate::control::Scope::Read, "{cap}");
        }
    }
}
