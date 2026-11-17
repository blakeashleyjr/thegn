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

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use thegn_core::config::Config;

pub use hub::{EnvSource, Hub, now_ms};

/// Run `thegn mcp proxy`: the stdio aggregation shim an agent registers as its
/// single MCP server. Newline-delimited JSON-RPC (the MCP stdio contract).
///
/// `profile` selects one `[mcp_proxy.profiles]` entry (`--profile`): only that
/// profile's servers. `None` is the default hub, which excludes every profiled
/// server so a profile (e.g. the Lead's) never reaches workers. An unknown
/// profile errors before anything is spawned.
///
/// A `--profile` hub never resolves its servers' secrets itself (THE-2222):
/// it is refused inside a worker ([`profile_shim_credentials`]), and the env
/// comes from the daemon's `mcp_proxy.profile_env`, which answers only the
/// Lead's own token (for `[lead] mcp_profile`) or an admin. A failed fetch is
/// an error; there is no local fallback.
pub fn run_shim(cfg: &Config, profile: Option<&str>) -> Result<()> {
    let selection = hub::ServerSelection::resolve(&cfg.mcp_proxy.profiles, profile)?;
    let env_source = match profile {
        None => EnvSource::Local,
        Some(name) => {
            let (addr, token) = profile_shim_credentials(&|k| std::env::var(k).ok())?;
            EnvSource::Daemon(fetch_profile_env(addr, token, name)?)
        }
    };
    let ctx = context::resolve_from_cwd();
    let mut hub = Hub::build_selected(cfg, &ctx, &selection, &env_source);

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

/// The `--profile` shim's preflight, as a pure fn over the environment:
/// refused inside a worker (a stage launch, or an `agent:` caller), and
/// otherwise needs the Lead's `THEGN_CONTROL_ADDR` + `THEGN_CONTROL_TOKEN`
/// (both non-empty). Returns `(addr, token)`. Error text never carries the
/// token.
pub(crate) fn profile_shim_credentials(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<(String, String)> {
    if get("THEGN_STAGE_LAUNCH").is_some() {
        anyhow::bail!(
            "`thegn mcp proxy --profile` is refused inside a pipeline stage (THEGN_STAGE_LAUNCH is set): profile hubs belong to the Lead"
        );
    }
    if get("THEGN_CALLER").is_some_and(|c| c.trim_start().starts_with("agent:")) {
        anyhow::bail!(
            "`thegn mcp proxy --profile` is refused for an agent caller: profile hubs belong to the Lead"
        );
    }
    let nonempty = |k: &str| get(k).filter(|v| !v.trim().is_empty());
    let Some(addr) = nonempty("THEGN_CONTROL_ADDR") else {
        anyhow::bail!(
            "`thegn mcp proxy --profile` needs THEGN_CONTROL_ADDR and THEGN_CONTROL_TOKEN (the Lead's control credential): the profile's secrets come from the daemon"
        );
    };
    let Some(token) = nonempty("THEGN_CONTROL_TOKEN") else {
        anyhow::bail!(
            "`thegn mcp proxy --profile` needs THEGN_CONTROL_TOKEN (the Lead's control token): the profile's secrets come from the daemon"
        );
    };
    Ok((addr, token))
}

/// Fetch `profile`'s server env from the daemon (`mcp_proxy.profile_env`)
/// with the bearer `token`. Blocking: a private current-thread runtime.
fn fetch_profile_env(addr: String, token: String, profile: &str) -> Result<ProfileEnvReply> {
    use thegn_svc::control::client::{ControlAddr, ControlClient};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start the control client runtime")?;
    let client = ControlClient::new(ControlAddr::Tcp { addr, token });
    let body = serde_json::json!({ "profile": profile });
    let reply = rt
        .block_on(client.call_raw("POST", "/v1/mcp_proxy/profile_env", Some(body)))
        .map_err(|e| {
            anyhow::anyhow!(
                "the daemon refused MCP proxy profile {profile:?} env ({e}); no local fallback"
            )
        })?;
    serde_json::from_value(reply)
        .with_context(|| format!("malformed mcp_proxy.profile_env reply for {profile:?}"))
}

/// One server's slot in a `mcp_proxy.profile_env` reply: its resolved env, or
/// why it could not be resolved (never a value).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerEnv {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The `mcp_proxy.profile_env` reply: `{servers: {name: {env}}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEnvReply {
    #[serde(default)]
    pub servers: BTreeMap<String, ServerEnv>,
}

impl ProfileEnvReply {
    /// Server `name`'s env, or the reason it has none. A server the daemon
    /// did not answer for is an error (never resolved locally).
    pub fn env_for(&self, name: &str) -> Result<BTreeMap<String, String>, String> {
        match self.servers.get(name) {
            Some(ServerEnv {
                error: Some(error), ..
            }) => Err(error.clone()),
            Some(ServerEnv { env, error: None }) => Ok(env.clone()),
            None => Err("the daemon returned no env for this server".to_string()),
        }
    }
}

/// Resolve one server's (placeholder-expanded) env secret refs to concrete
/// values. Shared by the default hub (at spawn) and the daemon's
/// `mcp_proxy.profile_env`. The error names the key and the ref, never a value.
pub fn resolve_server_env(
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (k, v) in env {
        let consumer = format!("mcp:{name}:{k}");
        let value =
            crate::secret::resolve_mcp_env(v, &consumer).map_err(|e| format!("env `{k}`: {e}"))?;
        out.insert(k.clone(), value);
    }
    Ok(out)
}

/// Resolve every exposed server of `[mcp_proxy.profiles] <profile>` (the
/// daemon side of `mcp_proxy.profile_env`). Placeholders expand against an
/// empty worktree context (the Lead has no worktree), so a worktree-scoped
/// server is answered with its withheld reason. Errors on an unknown profile.
pub fn resolve_profile_env(cfg: &Config, profile: &str) -> Result<ProfileEnvReply> {
    use thegn_core::mcp::proxy::partition::{WorktreeContext, expand_spec};
    let selection = hub::ServerSelection::resolve(&cfg.mcp_proxy.profiles, Some(profile))?;
    let ctx = WorktreeContext::default();
    let mut servers = BTreeMap::new();
    for (name, srv) in &cfg.mcp_servers {
        if !selection.includes(name) || !srv.is_proxy_exposed() {
            continue;
        }
        let slot = match expand_spec(name, srv, &ctx) {
            Err(withheld) => ServerEnv {
                env: BTreeMap::new(),
                error: Some(withheld.reason),
            },
            Ok(spec) => match resolve_server_env(name, &spec.env) {
                Ok(env) => ServerEnv { env, error: None },
                Err(error) => ServerEnv {
                    env: BTreeMap::new(),
                    error: Some(error),
                },
            },
        };
        servers.insert(name.clone(), slot);
    }
    Ok(ProfileEnvReply { servers })
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

    // Reconcile against a freshly admitted generation. A failed reload keeps
    // the prior generation, so nothing is reconciled (no partial/default
    // set is ever applied); a superseded reload uses what is current.
    use crate::config_startup::ReloadOutcome;
    let disk = match crate::config_startup::reload() {
        ReloadOutcome::Published(admitted) => admitted.config().clone(),
        ReloadOutcome::Superseded => crate::config_startup::display()
            .map(|admitted| admitted.config().clone())
            .unwrap_or_else(|| baseline.clone()),
        ReloadOutcome::Failed(_) | ReloadOutcome::FailedCoalesced => baseline.clone(),
        ReloadOutcome::Unavailable => CoreConfig::load_layered(&ProcessEnv, &[], None),
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    const LEAD: &[(&str, &str)] = &[
        ("THEGN_CONTROL_ADDR", "127.0.0.1:4242"),
        ("THEGN_CONTROL_TOKEN", "tgc1_secret"),
        ("THEGN_CALLER", "lead:lead/0"),
    ];

    #[test]
    fn profile_shim_takes_the_lead_credential() {
        let (addr, token) = profile_shim_credentials(&env_of(LEAD)).unwrap();
        assert_eq!(addr, "127.0.0.1:4242");
        assert_eq!(token, "tgc1_secret");
    }

    #[test]
    fn profile_shim_refuses_workers_and_missing_credentials() {
        let refusal = |pairs: &'static [(&'static str, &'static str)]| {
            profile_shim_credentials(&env_of(pairs))
                .unwrap_err()
                .to_string()
        };
        // A stage launch is refused even holding a token (any value, even empty).
        let stage = refusal(&[
            ("THEGN_STAGE_LAUNCH", ""),
            ("THEGN_CONTROL_ADDR", "127.0.0.1:1"),
            ("THEGN_CONTROL_TOKEN", "tgc1_secret"),
        ]);
        assert!(stage.contains("pipeline stage"), "{stage}");
        assert!(!stage.contains("tgc1_secret"));
        // An agent caller is refused.
        let agent = refusal(&[
            ("THEGN_CALLER", "agent:claude"),
            ("THEGN_CONTROL_ADDR", "127.0.0.1:1"),
            ("THEGN_CONTROL_TOKEN", "tgc1_secret"),
        ]);
        assert!(agent.contains("agent caller"), "{agent}");
        // Missing or empty credentials.
        assert!(refusal(&[]).contains("THEGN_CONTROL_ADDR"));
        assert!(refusal(&[("THEGN_CONTROL_ADDR", "127.0.0.1:1")]).contains("THEGN_CONTROL_TOKEN"));
        assert!(
            refusal(&[
                ("THEGN_CONTROL_ADDR", "127.0.0.1:1"),
                ("THEGN_CONTROL_TOKEN", "  ")
            ])
            .contains("THEGN_CONTROL_TOKEN")
        );
    }

    #[test]
    fn profile_env_reply_is_strict_and_unanswered_servers_error() {
        let reply: ProfileEnvReply = serde_json::from_value(serde_json::json!({
            "servers": {
                "todoist": { "env": { "TOKEN": "v" } },
                "mail": { "error": "keyring entry `mail` is not set" },
            }
        }))
        .unwrap();
        assert_eq!(reply.env_for("todoist").unwrap()["TOKEN"], "v");
        assert!(reply.env_for("mail").unwrap_err().contains("keyring"));
        assert!(reply.env_for("absent").is_err());
        assert!(
            serde_json::from_value::<ProfileEnvReply>(serde_json::json!({"servers": {}, "x": 1}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ServerEnv>(serde_json::json!({"env": {}, "value": "s"}))
                .is_err()
        );
    }

    #[test]
    fn resolve_profile_env_answers_only_the_profiles_exposed_servers() {
        let server =
            |tools: &[&str], env: serde_json::Value| -> thegn_core::mcp::config::McpServerConfig {
                serde_json::from_value(serde_json::json!({
                    "command": ["true"],
                    "env": env,
                    "proxy": { "tools": tools },
                }))
                .unwrap()
            };
        let mut cfg = Config::default();
        // Bare strings are literal values: no keyring is touched.
        cfg.mcp_servers.insert(
            "todoist".into(),
            server(&["*"], serde_json::json!({ "K": "lit" })),
        );
        cfg.mcp_servers.insert(
            "hidden".into(),
            server(&[], serde_json::json!({ "K": "x" })),
        );
        cfg.mcp_servers.insert(
            "github".into(),
            server(&["*"], serde_json::json!({ "K": "y" })),
        );
        cfg.mcp_proxy.profiles = BTreeMap::from([(
            "lead".to_string(),
            vec!["todoist".to_string(), "hidden".to_string()],
        )]);
        let reply = resolve_profile_env(&cfg, "lead").unwrap();
        let names: Vec<&str> = reply.servers.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["todoist"],
            "default-deny and other servers excluded"
        );
        assert_eq!(reply.env_for("todoist").unwrap()["K"], "lit");
        assert!(resolve_profile_env(&cfg, "nope").is_err());
    }
}
