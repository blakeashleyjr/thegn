//! User-declared MCP servers (`[mcp_servers.<name>]`).
//!
//! Where the core `mcp` router exposes thegn's *own* house tools, this models
//! the MCP servers a **user** declares to extend the agent. Each server has a
//! launch spec (command/args/env), an optional acquisition [`McpSource`] handled
//! by the shared managed-tool resolver, and capability [`Grant`]s that gate that
//! acquisition. The pure [`settings_block`] builder emits the de-facto
//! `mcpServers` JSON the managed agent consumes (merged into its settings during
//! `thegn agent setup`).

use crate::config::{config_enum, config_warn};
use crate::grants::{Action, Grant};
use crate::managed_tool::{ManagedTool, UpdatePolicy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

config_enum! {
    /// `[mcp_servers.<name>.proxy] scope` — the partition granularity the
    /// mcp-proxy hub runs an upstream at. `global` shares one instance across
    /// every connection; `workspace`/`worktree` give one instance per scope
    /// key, with `{workspace}`/`{worktree}`/`{repo_root}`/`{branch}` templated
    /// into that instance's env/args (per-project memory namespaces — THE-49).
    pub enum ProxyScope: "mcp proxy scope" {
        Global = "global", Workspace = "workspace", Worktree = "worktree",
    } default = Global;
}

/// `[mcp_servers.<name>.proxy]` — how the mcp-proxy hub exposes this upstream.
///
/// **Default-deny**: a server contributes NOTHING to the proxy until `tools`
/// is a non-empty glob list. `["*"]` is the explicit everything opt-in. This
/// is the tool-poisoning blast-radius control — upstream tool names and
/// descriptions are untrusted input, and aggregation multiplies one malicious
/// upstream's reach across every wired agent, so exposure must be typed
/// deliberately per server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct McpProxyExposure {
    /// Glob patterns (grants-style: `*` within a segment, `**` across) naming
    /// the tools this upstream may contribute. Empty ⇒ the server is not
    /// exposed through the proxy at all (still usable via direct `mcp emit`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Partition granularity (default `global`).
    pub scope: ProxyScope,
}

impl McpProxyExposure {
    /// Whether this exposure opts the server into the proxy at all
    /// (default-deny: an empty `tools` list is no exposure).
    pub fn is_exposed(&self) -> bool {
        !self.tools.is_empty()
    }
}

/// `[mcp_proxy]` — the aggregation hub itself (health/breaker tuning).
/// Additive: the proxy does nothing until at least one `[mcp_servers.<name>]`
/// opts in via its `proxy.tools` list, so this table is inert by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct McpProxyConfig {
    /// Master switch for the hub. On by default, but default-deny filtering
    /// means it still exposes nothing until a server declares `proxy.tools`.
    pub enabled: bool,
    /// Seconds between upstream health checks (the daemon's reconcile/health
    /// tick cadence). `0` disables active health-checking (breakers still trip
    /// on real call failures).
    pub health_interval_secs: u64,
    /// Consecutive failures/timeouts before an upstream's breaker opens.
    pub failure_threshold: u32,
    /// Seconds an open breaker waits before a half-open probe.
    pub cooldown_secs: u64,
    /// Per-request timeout (seconds) applied to an upstream call before it
    /// counts as a failure against the breaker.
    pub request_timeout_secs: u64,
}

impl Default for McpProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            health_interval_secs: 30,
            failure_threshold: 3,
            cooldown_secs: 30,
            request_timeout_secs: 30,
        }
    }
}

impl McpProxyConfig {
    /// The pure breaker tuning derived from this config.
    pub fn breaker_config(&self) -> crate::mcp::proxy::breaker::BreakerConfig {
        crate::mcp::proxy::breaker::BreakerConfig {
            failure_threshold: self.failure_threshold.max(1),
            cooldown_ms: (self.cooldown_secs as i64).saturating_mul(1000),
        }
    }
}

/// How to acquire a declared server's binary. Only the single-artifact cases
/// (npm / cargo) are declarable; a server already on PATH needs no source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpSource {
    Npm { package: String, version: String },
    Cargo { crate_name: String, version: String },
}

impl McpSource {
    /// The managed-tool spec for this source (binary `bin`, resolved under the
    /// namespaced tools dir with the given PATH fallback).
    pub fn to_tool(&self, name: &str, bin: &str) -> ManagedTool {
        match self {
            McpSource::Npm { package, version } => {
                ManagedTool::npm(name, package, bin, version).with_policy(UpdatePolicy::Once)
            }
            McpSource::Cargo {
                crate_name,
                version,
            } => ManagedTool::cargo(name, crate_name, bin, version).with_policy(UpdatePolicy::Once),
        }
    }

    /// The grant [`Action`] this acquisition performs (for the grant check).
    pub fn install_action(&self) -> Action<'_> {
        match self {
            McpSource::Npm { package, .. } => Action::Npm(package),
            McpSource::Cargo { crate_name, .. } => Action::Cargo(crate_name),
        }
    }
}

/// One declared MCP server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct McpServerConfig {
    /// Launch argv (e.g. `["npx", "-y", "@modelcontextprotocol/server-git"]`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Extra args appended to `command`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Environment for the server process.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Optional acquisition of the server binary via the managed-tool resolver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<McpSource>,
    /// Capability grants gating this server's acquisition/launch.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
    /// `[mcp_servers.<name>.proxy]` — how the mcp-proxy hub exposes this
    /// server (default-deny; absent ⇒ never proxied). See [`McpProxyExposure`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<McpProxyExposure>,
}

impl McpServerConfig {
    /// This server's proxy exposure, defaulted to "not exposed" when the
    /// `[proxy]` subtable is absent — so a caller never has to distinguish
    /// "no subtable" from "empty tools" (both are default-deny).
    pub fn exposure(&self) -> McpProxyExposure {
        self.proxy.clone().unwrap_or_default()
    }

    /// Whether this server is exposed through the proxy at all (default-deny).
    pub fn is_proxy_exposed(&self) -> bool {
        self.proxy
            .as_ref()
            .is_some_and(McpProxyExposure::is_exposed)
    }
}

/// Whether launching this server requires network access to *acquire* its
/// binary — either a declared [`McpSource`] (npm/cargo fetch) or a fetch-on-run
/// launcher (`npx`/`uvx`/`bunx`/…, the common `npx -y @scope/server` form).
/// Servers already on PATH (a plain command, no source) launch offline fine and
/// return `false`. Pure — used to skip network MCPs while the app is offline.
pub fn needs_network_acquire(cfg: &McpServerConfig) -> bool {
    if cfg.source.is_some() {
        return true;
    }
    let bin = cfg
        .command
        .first()
        .map(|s| s.rsplit(['/', '\\']).next().unwrap_or(s));
    matches!(bin, Some("npx" | "uvx" | "bunx" | "pnpx" | "dlx"))
}

/// The full launch argv (`command` then `args`).
pub fn launch_argv(cfg: &McpServerConfig) -> Vec<String> {
    let mut argv = cfg.command.clone();
    argv.extend(cfg.args.iter().cloned());
    argv
}

/// Build the standard `mcpServers` settings block:
/// `{ "<name>": { "command": <argv0>, "args": [<argv1..>], "env": {..} }, .. }`.
/// Servers with no launch command are skipped. Deterministic (BTreeMap order).
pub fn settings_block(servers: &BTreeMap<String, McpServerConfig>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, cfg) in servers {
        let argv = launch_argv(cfg);
        let Some((command, rest)) = argv.split_first() else {
            continue; // no command → nothing the agent can launch
        };
        map.insert(
            name.clone(),
            serde_json::json!({
                "command": command,
                "args": rest,
                "env": cfg.env,
            }),
        );
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(cmd: &[&str]) -> McpServerConfig {
        McpServerConfig {
            command: cmd.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn settings_block_maps_command_args_env() {
        let mut servers = BTreeMap::new();
        let mut git = server(&["npx", "-y", "@modelcontextprotocol/server-git"]);
        git.args = vec!["--repo".into(), ".".into()];
        git.env.insert("TOKEN".into(), "x".into());
        servers.insert("git".to_string(), git);
        // A command-less server is skipped.
        servers.insert("broken".to_string(), McpServerConfig::default());

        let block = settings_block(&servers);
        let obj = block.as_object().unwrap();
        assert!(!obj.contains_key("broken"));
        assert_eq!(obj["git"]["command"], "npx");
        assert_eq!(
            obj["git"]["args"],
            serde_json::json!(["-y", "@modelcontextprotocol/server-git", "--repo", "."])
        );
        assert_eq!(obj["git"]["env"]["TOKEN"], "x");
    }

    #[test]
    fn empty_servers_empty_block() {
        let block = settings_block(&BTreeMap::new());
        assert_eq!(block, serde_json::json!({}));
    }

    #[test]
    fn needs_network_acquire_classifies() {
        // A declared npm/cargo source needs the network to acquire.
        let mut with_source = server(&["server-git"]);
        with_source.source = Some(McpSource::Npm {
            package: "@scope/srv".into(),
            version: "1.0.0".into(),
        });
        assert!(needs_network_acquire(&with_source));

        // Fetch-on-run launchers (with or without a path prefix).
        assert!(needs_network_acquire(&server(&["npx", "-y", "@scope/srv"])));
        assert!(needs_network_acquire(&server(&["/usr/bin/uvx", "srv"])));
        assert!(needs_network_acquire(&server(&["bunx", "srv"])));

        // A plain on-PATH command launches offline fine.
        assert!(!needs_network_acquire(&server(&[
            "mcp-server-git",
            "--repo",
            "."
        ])));
        assert!(!needs_network_acquire(&server(&["/opt/tools/my-mcp"])));
        // No command at all → nothing to launch, nothing to acquire.
        assert!(!needs_network_acquire(&McpServerConfig::default()));
    }

    #[test]
    fn source_maps_to_tool_and_action() {
        let npm = McpSource::Npm {
            package: "@scope/srv".into(),
            version: "1.2.3".into(),
        };
        let tool = npm.to_tool("srv", "srv-bin");
        assert_eq!(tool.name, "srv");
        assert_eq!(tool.version, "1.2.3");
        assert!(matches!(npm.install_action(), Action::Npm("@scope/srv")));

        let cargo = McpSource::Cargo {
            crate_name: "mcp-thing".into(),
            version: "0.1.0".into(),
        };
        assert!(cargo.to_tool("t", "t").bin_path().ends_with("bin/t"));
        assert!(matches!(cargo.install_action(), Action::Cargo("mcp-thing")));
    }

    #[test]
    fn config_round_trips_from_toml() {
        let cfg: McpServerConfig = toml::from_str(
            r#"
command = ["npx", "-y", "@modelcontextprotocol/server-git"]
grants = [{ kind = "npm:install", scope = "@modelcontextprotocol/*" }]

[source]
type = "npm"
package = "@modelcontextprotocol/server-git"
version = "0.5.0"
"#,
        )
        .unwrap();
        assert_eq!(cfg.command[0], "npx");
        assert_eq!(cfg.grants[0].kind, "npm:install");
        assert!(matches!(cfg.source, Some(McpSource::Npm { .. })));
    }
}
