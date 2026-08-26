//! The mcp-proxy hub host runtime: spawning/owning upstream MCP children,
//! aggregating them behind one stdio endpoint, and the credential custody at
//! spawn. The pure decisions (default-deny filter, namespacing, routing,
//! breaker, reconcile, partition) live in `thegn_core::mcp::proxy`; this module
//! is only the I/O around them.
//!
//! ## Where the shim runs (v1)
//!
//! `thegn mcp proxy` runs the [`hub::Hub`] **in-process**: it spawns each
//! exposed upstream as a child and serves the agent over stdio. This is the
//! spec-sanctioned standalone path (and the fallback whenever the daemon is
//! disabled or unreachable) — it is fully functional and never bricks an agent.
//!
//! The **daemon-owned shared-upstream** path (one child shared by every agent's
//! shim, multiplexed over control IPC with per-connection id rewriting) is the
//! identified follow-up: everything it needs already exists in core
//! (`route::IdRewriter`, `reconcile`, `breaker`) and the daemon already exposes
//! `mcp_proxy.status`/`reload`. Wiring the shim's stdio to the daemon over a
//! bridge is the remaining step; until then each shim owns its own upstreams
//! (degraded sharing, identical behavior — exactly the documented fallback).

pub mod context;
pub mod hub;
pub mod upstream;

use std::io::{self, BufRead, Write};

use anyhow::Result;
use thegn_core::config::Config;

pub use hub::{Hub, UpstreamReport, now_ms};

/// Run `thegn mcp proxy`: the stdio aggregation shim an agent registers as its
/// single MCP server. Newline-delimited JSON-RPC (the MCP stdio contract).
pub fn run_shim(cfg: &Config) -> Result<()> {
    let ctx = context::resolve_from_cwd();
    let mut hub = Hub::build(cfg, &ctx);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": serde_json::Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") } });
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(resp) = hub.handle(&val) {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Build a hub for the current directory's worktree context (for `mcp status`
/// / `mcp list` / doctor — a live probe of the configured upstreams).
pub fn build_hub_for_cwd(cfg: &Config) -> Hub {
    Hub::build(cfg, &context::resolve_from_cwd())
}

// ── daemon control ops (config-reflective) ──────────────────────────────────
//
// The daemon's `mcp_proxy.status`/`reload` report + reconcile the *configured*
// topology. NOTE (v1): the daemon does not yet own long-lived shared upstream
// processes — that (and the shim↔daemon multiplex) is the follow-up; the
// standalone in-process hub above is the working path. So `daemon_owned` is
// reported honestly as false and live breaker/tool-count data comes from the
// CLI `thegn mcp status` live probe, while the daemon answer is the cheap
// config-reflective view + the reconcile plan.

use thegn_svc::control::{
    McpProxyReloadAction, McpProxyReloadReport, McpProxyStatus, McpProxyUpstreamStatus,
};

/// The daemon's `mcp_proxy.status`: the configured exposed upstreams, derived
/// from the daemon's config snapshot without spawning anything.
pub fn daemon_status(cfg: &Config) -> McpProxyStatus {
    use thegn_core::mcp::config::ProxyScope;
    use thegn_core::mcp::proxy::partition::{WorktreeContext, partition_key};

    let ctx = WorktreeContext::default();
    let mut upstreams = Vec::new();
    for (name, srv) in &cfg.mcp_servers {
        if !srv.is_proxy_exposed() {
            continue;
        }
        let exposure = srv.exposure();
        let (partition, withheld) = match partition_key(exposure.scope, &ctx) {
            Ok(k) => (k, None),
            Err(w) => (String::new(), Some(w.reason)),
        };
        upstreams.push(McpProxyUpstreamStatus {
            name: name.clone(),
            partition_key: partition,
            scope: exposure.scope.as_str().to_string(),
            // v1: daemon does not own shared upstream processes yet.
            running: false,
            breaker: "closed".to_string(),
            health_checked_ms_ago: None,
            // Real tool counts need a live handshake — see `thegn mcp status`.
            exposed_tools: 0,
            hidden_tools: 0,
            exposed_names: Vec::new(),
            withheld_reason: if exposure.scope == ProxyScope::Global {
                None
            } else {
                withheld.or(Some(
                    "scope requires a connection context (instantiated per shim)".to_string(),
                ))
            },
        });
    }
    McpProxyStatus {
        enabled: cfg.mcp_proxy.enabled,
        daemon_owned: false,
        upstreams,
    }
}

/// The daemon's `mcp_proxy.reload`: re-read config and diff the global-scope
/// effective set against the daemon's boot snapshot via the pure `reconcile`.
pub fn daemon_reload(baseline: &Config) -> McpProxyReloadReport {
    use thegn_core::config::{Config as CoreConfig, ProcessEnv};
    use thegn_core::mcp::proxy::reconcile::{ReconcileAction, reconcile};

    let disk = CoreConfig::load_layered(&ProcessEnv, &[], None);
    let old = effective_global_instances(baseline);
    let new = effective_global_instances(&disk);
    let actions = reconcile(&old, &new);
    let tools_changed = !actions.is_empty();
    let mapped = actions
        .into_iter()
        .map(|a| {
            let kind = a.kind().to_string();
            let (upstream, partition_key) = match a {
                ReconcileAction::Start(s)
                | ReconcileAction::Restart(s)
                | ReconcileAction::Refilter(s) => (s.upstream, s.partition_key),
                ReconcileAction::Stop {
                    upstream,
                    partition_key,
                } => (upstream, partition_key),
            };
            McpProxyReloadAction {
                kind,
                upstream,
                partition_key,
            }
        })
        .collect();
    McpProxyReloadReport {
        actions: mapped,
        tools_changed,
    }
}

pub use hub::effective_global_instances;
