//! `szhost mcp <action>` — user-declared MCP servers (`[mcp_servers.<name>]`).
//!
//! Lists declared servers, emits the `mcpServers` settings block the agent
//! consumes, and installs a server's binary via the shared managed-tool resolver
//! — grant-checked: acquisition proceeds only when the server's capability
//! grants cover it. The agent-setup path merges the same block into the managed
//! pi's settings (see [`crate::cmd::agent::inject_mcp_servers`]).

use anyhow::{Result, bail};
use superzej_core::config::Config;
use superzej_core::grants::Grants;
use superzej_core::mcp::config::{launch_argv, settings_block};
use superzej_core::outln;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List declared MCP servers with their launch command and grants.
    List,
    /// Print the `mcpServers` settings block (what agent setup injects).
    Emit,
    /// Acquire a declared server's binary via the resolver (grant-checked).
    Install {
        /// The `[mcp_servers.<name>]` to install.
        name: String,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List => list(cfg),
        Action::Emit => {
            outln!(
                "{}",
                serde_json::to_string_pretty(&settings_block(&cfg.mcp_servers))?
            );
            Ok(())
        }
        Action::Install { name } => install(cfg, &name),
    }
}

fn list(cfg: &Config) -> Result<()> {
    if cfg.mcp_servers.is_empty() {
        outln!("no MCP servers declared ([mcp_servers.<name>])");
        return Ok(());
    }
    for (name, srv) in &cfg.mcp_servers {
        outln!("{name}: {}", launch_argv(srv).join(" "));
        if srv.source.is_some() {
            outln!("  source: {}", source_label(srv));
        }
        if srv.grants.is_empty() {
            outln!("  grants: (none — acquisition will be refused)");
        } else {
            for g in &srv.grants {
                outln!("  grant: {} {}", g.kind, g.scope);
            }
        }
    }
    Ok(())
}

fn source_label(srv: &superzej_core::mcp::config::McpServerConfig) -> String {
    use superzej_core::mcp::config::McpSource;
    match &srv.source {
        Some(McpSource::Npm { package, version }) => format!("npm {package}@{version}"),
        Some(McpSource::Cargo {
            crate_name,
            version,
        }) => format!("cargo {crate_name}@{version}"),
        None => "(none)".to_string(),
    }
}

fn install(cfg: &Config, name: &str) -> Result<()> {
    let Some(srv) = cfg.mcp_servers.get(name) else {
        bail!("no such MCP server `{name}` in [mcp_servers]");
    };
    let Some(source) = &srv.source else {
        bail!(
            "MCP server `{name}` has no `source` to install (put its binary on PATH, or add [mcp_servers.{name}.source])"
        );
    };
    // Grant check: the declared grants must cover this acquisition.
    let grants = Grants::new(srv.grants.clone());
    let action = source.install_action();
    if !grants.allows(&action) {
        bail!(
            "refusing to install `{name}`: {}",
            grants.deny_reason(&action)
        );
    }
    let tool = source.to_tool(name, name);
    crate::managed_tool::install(&tool, false)?;
    outln!("installed `{name}` → {}", tool.bin_path().display());
    Ok(())
}
