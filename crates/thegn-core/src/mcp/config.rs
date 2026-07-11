//! User-declared MCP servers (`[mcp_servers.<name>]`).
//!
//! Where the core `mcp` router exposes thegn's *own* house tools, this models
//! the MCP servers a **user** declares to extend the agent. Each server has a
//! launch spec (command/args/env), an optional acquisition [`McpSource`] handled
//! by the shared managed-tool resolver, and capability [`Grant`]s that gate that
//! acquisition. The pure [`settings_block`] builder emits the de-facto
//! `mcpServers` JSON the managed agent consumes (merged into its settings during
//! `thegn agent setup`).

use crate::grants::{Action, Grant};
use crate::managed_tool::{ManagedTool, UpdatePolicy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
