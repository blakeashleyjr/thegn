//! `thegn mcp <action>` — user-declared MCP servers (`[mcp_servers.<name>]`)
//! plus `serve`, thegn's own read-only docs/help/config MCP endpoint.
//!
//! `list`/`emit`/`install` manage the servers thegn hands to agents: lists
//! declared servers, emits the `mcpServers` settings block the agent consumes,
//! and installs a server's binary via the shared managed-tool resolver —
//! grant-checked: acquisition proceeds only when the server's capability grants
//! cover it. The agent-setup path merges the same block into the managed pi's
//! settings (see [`crate::cmd::agent::inject_mcp_servers`]).
//!
//! `serve` runs thegn *as* an MCP server over stdio (a Context7-style endpoint):
//! it exposes the in-app help corpus, the generated keybindings/config-reference
//! pages, and the user's current secret-redacted config so a coding agent can
//! learn how thegn works. The JSON-RPC handling is the pure
//! [`thegn_core::mcp::docs::DocsRouter`]; this shell only builds its inputs and
//! pumps stdin→router→stdout. Register it with e.g.
//! `claude mcp add thegn -- thegn mcp serve`.

use anyhow::{Result, bail};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use thegn_core::config::Config;
use thegn_core::grants::Grants;
use thegn_core::mcp::config::{launch_argv, settings_block};
use thegn_core::outln;

/// The stable CLI grammar + README, embedded so `serve` can hand them to an
/// agent as `thegn://doc/*` resources without touching the filesystem.
const CLI_DOC: &str = include_str!("../../../../docs/cli.md");
const README_DOC: &str = include_str!("../../../../README.md");

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
    /// Run thegn's read-only docs/help/config MCP server over stdio.
    ///
    /// A Context7-style endpoint for coding agents: search + read the help
    /// pages, the effective keymap, the config reference, and your current
    /// (secret-redacted) config. Register with e.g.
    /// `claude mcp add thegn -- thegn mcp serve`.
    Serve,
}

pub fn run(cfg: &Config, action: Action, config_path: PathBuf) -> Result<()> {
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
        Action::Serve => serve(cfg, config_path),
    }
}

/// Serve the docs/help/config MCP endpoint over stdio (newline-delimited
/// JSON-RPC — the MCP stdio contract). Builds the help registry, the redacted
/// config, and the schema once, then loops reading one request per line.
fn serve(cfg: &Config, config_path: PathBuf) -> Result<()> {
    use thegn_core::mcp::docs::{DocResource, DocsRouter, redact};

    // Authored pages + the generated keybindings & config-reference pages.
    let (reg, errors) = crate::help::pages::build_registry(cfg);
    for e in &errors {
        tracing::warn!(target: "thegn::help", "help page validation: {e}");
    }

    // Current config as JSON with secrets masked, plus its schema.
    let mut config_val = serde_json::to_value(cfg).unwrap_or(serde_json::Value::Null);
    redact(&mut config_val);
    let schema =
        serde_json::to_value(schemars::schema_for!(Config)).unwrap_or(serde_json::Value::Null);

    let docs = vec![
        DocResource {
            id: "cli".to_string(),
            title: "thegn CLI grammar".to_string(),
            body: CLI_DOC.to_string(),
        },
        DocResource {
            id: "readme".to_string(),
            title: "thegn README".to_string(),
            body: README_DOC.to_string(),
        },
    ];

    // `explain_config` resolves layers from the config file — that I/O lives
    // here (host), not in the pure core router.
    let explain = move |key: &str, _repo: Option<&str>| explain_key(key, config_path.clone());

    let router = DocsRouter::new(
        &reg,
        config_val,
        schema,
        docs,
        &crate::fff_backend::fuzzy_rank,
        explain,
    );

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
        // JSON-RPC notifications carry no `id` and get no reply (e.g.
        // `notifications/initialized` sent right after `initialize`).
        if val.get("id").is_none() {
            continue;
        }
        let resp = router.handle(&val);
        writeln!(stdout, "{resp}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// Render a config key's layer-resolution trace (effective value + which layer
/// set it + the value at each layer) as plain text for the `explain_config`
/// tool.
fn explain_key(key: &str, path: PathBuf) -> String {
    use thegn_core::config::ProcessEnv;
    use thegn_core::config_resolve;
    let origin = config_resolve::explain(&ProcessEnv, &[], Some(path), key);
    let mut s = format!(
        "{} = {}\n  set by: {}\n",
        origin.key,
        origin.value,
        origin.origin.as_str()
    );
    for (layer, val) in &origin.trace {
        s.push_str(&format!("    {}: {val}\n", layer.as_str()));
    }
    s
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

fn source_label(srv: &thegn_core::mcp::config::McpServerConfig) -> String {
    use thegn_core::mcp::config::McpSource;
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
