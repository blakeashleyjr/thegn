//! The aggregation hub: owns the live upstream children, builds the merged
//! tool table via the pure core, and serves the JSON-RPC surface an agent talks
//! to (`initialize` / `tools/list` / `tools/call`).
//!
//! This is the in-process aggregator `thegn mcp proxy` runs (and the standalone
//! fallback the spec mandates when no daemon is reachable). It is deliberately
//! synchronous — the agent drives one request at a time over stdio — so the
//! whole thing is `std::process` + threads, no tokio.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
use thegn_core::config::Config;
use thegn_core::mcp::proxy::aggregate::{Aggregate, UpstreamSummary, UpstreamTools, aggregate};
use thegn_core::mcp::proxy::partition::{WorktreeContext, expand_spec};
use thegn_core::mcp::proxy::{filter, initialize_result, route};

use super::upstream::Upstream;

/// One upstream's outcome, for `status` / `mcp list` / doctor. An upstream that
/// is withheld (scope/placeholder) or failed to spawn is reported, not hidden —
/// the whole point of the hub is inspectable policy.
#[derive(Debug, Clone)]
pub struct UpstreamReport {
    pub name: String,
    pub scope: String,
    pub partition_key: Option<String>,
    pub running: bool,
    pub breaker: String,
    pub health_age_ms: Option<i64>,
    pub exposed: Vec<String>,
    pub hidden: Vec<String>,
    /// Withheld because its scope/placeholders could not be satisfied here.
    pub withheld_reason: Option<String>,
    /// Failed to spawn / resolve a secret (running is false; tools absent).
    pub error: Option<String>,
}

/// The live aggregation hub for one connection context.
pub struct Hub {
    upstreams: Vec<Upstream>,
    aggregate: Aggregate,
    reports: Vec<UpstreamReport>,
    request_timeout: Duration,
}

impl Hub {
    /// Build a hub from config for the given worktree context: for every
    /// *exposed* server, derive its instance (partition + placeholder
    /// expansion), resolve its env secret-refs, spawn it, and merge its tools.
    /// A withheld or unspawnable upstream degrades to "its tools absent" — the
    /// endpoint never fails to come up.
    pub fn build(cfg: &Config, ctx: &WorktreeContext) -> Hub {
        let pcfg = &cfg.mcp_proxy;
        let breaker_cfg = pcfg.breaker_config();
        let timeout = Duration::from_secs(pcfg.request_timeout_secs.max(1));

        let mut upstreams: Vec<Upstream> = Vec::new();
        let mut reports: Vec<UpstreamReport> = Vec::new();

        for (name, srv) in &cfg.mcp_servers {
            if !srv.is_proxy_exposed() {
                continue; // default-deny: not opted in.
            }
            let scope = srv.exposure().scope.as_str().to_string();

            let spec = match expand_spec(name, srv, ctx) {
                Ok(s) => s,
                Err(withheld) => {
                    reports.push(UpstreamReport {
                        name: name.clone(),
                        scope,
                        partition_key: None,
                        running: false,
                        breaker: "n/a".into(),
                        health_age_ms: None,
                        exposed: Vec::new(),
                        hidden: Vec::new(),
                        withheld_reason: Some(withheld.reason),
                        error: None,
                    });
                    continue;
                }
            };

            // Resolve env secret-refs at spawn — the agent never sees them.
            let mut env = BTreeMap::new();
            let mut secret_err = None;
            for (k, v) in &spec.env {
                match crate::secret::resolve_mcp_env(v) {
                    Ok(val) => {
                        env.insert(k.clone(), val);
                    }
                    Err(e) => {
                        secret_err = Some(format!("env `{k}`: {e}"));
                        break;
                    }
                }
            }
            if let Some(e) = secret_err {
                reports.push(UpstreamReport {
                    name: name.clone(),
                    scope,
                    partition_key: Some(spec.partition_key.clone()),
                    running: false,
                    breaker: "n/a".into(),
                    health_age_ms: None,
                    exposed: Vec::new(),
                    hidden: Vec::new(),
                    withheld_reason: None,
                    error: Some(e),
                });
                continue;
            }

            match Upstream::spawn(
                name,
                &spec.argv,
                &env,
                spec.exposure.clone(),
                breaker_cfg,
                timeout,
            ) {
                Ok(up) => {
                    reports.push(UpstreamReport {
                        name: name.clone(),
                        scope,
                        partition_key: Some(spec.partition_key.clone()),
                        running: true,
                        breaker: "closed".into(),
                        health_age_ms: None,
                        exposed: Vec::new(), // filled from the aggregate below
                        hidden: Vec::new(),
                        withheld_reason: None,
                        error: None,
                    });
                    upstreams.push(up);
                }
                Err(e) => reports.push(UpstreamReport {
                    name: name.clone(),
                    scope,
                    partition_key: Some(spec.partition_key.clone()),
                    running: false,
                    breaker: "n/a".into(),
                    health_age_ms: None,
                    exposed: Vec::new(),
                    hidden: Vec::new(),
                    withheld_reason: None,
                    error: Some(e),
                }),
            }
        }

        let mut hub = Hub {
            upstreams,
            aggregate: Aggregate::default(),
            reports,
            request_timeout: timeout,
        };
        hub.rebuild_aggregate();
        hub
    }

    /// Rebuild the aggregate from the running upstreams' cached tool lists and
    /// fold the exposed/hidden breakdown back onto the reports.
    fn rebuild_aggregate(&mut self) {
        let inputs: Vec<UpstreamTools> = self
            .upstreams
            .iter()
            .map(|u| UpstreamTools {
                name: &u.name,
                tools: u.tools(),
                exposure: &u.exposure,
            })
            .collect();
        let agg = aggregate(&inputs);
        // Fold per-upstream exposed/hidden onto running reports.
        for UpstreamSummary {
            name,
            exposed,
            hidden,
            ..
        } in &agg.summary
        {
            if let Some(r) = self.reports.iter_mut().find(|r| &r.name == name) {
                r.exposed = exposed.clone();
                r.hidden = hidden.clone();
            }
        }
        self.aggregate = agg;
    }

    /// Per-upstream reports (for `status` / `mcp list` / doctor).
    pub fn reports(&self, now_ms: i64) -> Vec<UpstreamReport> {
        let mut reports = self.reports.clone();
        for r in &mut reports {
            if r.running
                && let Some(u) = self.upstreams.iter().find(|u| u.name == r.name)
            {
                r.breaker = u.breaker_state(now_ms).as_str().to_string();
                r.health_age_ms = u.health_age_ms();
            }
        }
        reports
    }

    /// The advertised tool count (post-filter, post-namespace).
    pub fn tool_count(&self) -> usize {
        self.aggregate.tool_count()
    }

    /// Handle one JSON-RPC request, returning the response value — or `None` for
    /// a notification (no `id`), which gets no reply.
    pub fn handle(&mut self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        // Notifications (no id) get no response.
        let id = id?;
        let result = match method {
            "initialize" => Ok(initialize_result()),
            "tools/list" => Ok(json!({ "tools": self.aggregate.tools.clone() })),
            "tools/call" => self.tools_call(req.get("params").unwrap_or(&Value::Null)),
            // The proxy aggregates tools only (v1); answer the other list verbs
            // with empty sets so a strict client does not error.
            "resources/list" => Ok(json!({ "resources": [] })),
            "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            "ping" => Ok(json!({})),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(match result {
            Ok(res) => json!({ "jsonrpc": "2.0", "id": id, "result": res }),
            Err((code, msg)) => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } })
            }
        })
    }

    fn tools_call(&mut self, params: &Value) -> Result<Value, (i32, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        // Route is a table lookup — a filtered/unknown name never reaches an
        // upstream.
        let Some(tr) = self.aggregate.route(name).cloned() else {
            return Err(route::unknown_tool_error(name));
        };
        let now = now_ms();
        let up = self
            .upstreams
            .iter_mut()
            .find(|u| u.name == tr.upstream)
            .ok_or_else(|| route::breaker_open_error(&tr.upstream))?;
        up.call_tool(&tr.tool, &args, now)
    }

    /// Whether the filter admits a given tool for an upstream (for `mcp list`).
    #[expect(
        dead_code,
        reason = "reserved: unwired mcp-proxy capability, wire-or-remove tracked in THE-16 follow-up"
    )]
    pub fn tool_exposed(patterns: &[String], tool: &str) -> bool {
        filter::tool_exposed(patterns, tool)
    }

    /// The per-request timeout (for diagnostics).
    #[expect(
        dead_code,
        reason = "reserved: unwired mcp-proxy capability, wire-or-remove tracked in THE-16 follow-up"
    )]
    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }
}

/// The daemon-derivable effective instance set: the *global-scope* exposed
/// upstreams (workspace/worktree-scoped upstreams are per-connection and cannot
/// be derived without a shim's context). Used by the daemon's `mcp_proxy.status`
/// / `reload` supervisor — the pure `reconcile` diffs two of these.
pub fn effective_global_instances(
    cfg: &Config,
) -> Vec<thegn_core::mcp::proxy::reconcile::InstanceSpec> {
    use thegn_core::mcp::config::ProxyScope;
    let ctx = WorktreeContext::default();
    let mut out = Vec::new();
    for (name, srv) in &cfg.mcp_servers {
        if !srv.is_proxy_exposed() {
            continue;
        }
        if srv.exposure().scope != ProxyScope::Global {
            continue; // scoped instances are created per-connection.
        }
        if let Ok(spec) = expand_spec(name, srv, &ctx) {
            out.push(spec);
        }
    }
    out
}

/// Epoch milliseconds (monotone enough for the breaker's cooldown math).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
