//! `thegn mcp <action>` — user-declared MCP servers (`[mcp_servers.<name>]`)
//! plus `serve`, thegn's own read-only docs/help/config MCP endpoint.
//!
//! `list`/`emit`/`install` manage the servers thegn hands to agents: lists
//! declared servers, emits the `mcpServers` settings block the agent consumes,
//! and installs a server's binary via the shared managed-tool resolver —
//! grant-checked: acquisition proceeds only when the server's capability grants
//! cover it.
//!
//! `serve` runs thegn *as* an MCP server over stdio (a Context7-style endpoint):
//! it exposes the in-app help corpus, the generated keybindings/config-reference
//! pages, and the user's current secret-redacted config so a coding agent can
//! learn how thegn works — plus, gated by `--scopes`, live *state* tools
//! (`sessions_list`, `worktrees_list`, `leases_list`, `me`) answered by the
//! pane daemon over the control client. The JSON-RPC handling is the pure
//! [`thegn_core::mcp::docs::DocsRouter`] +
//! [`thegn_core::mcp::state::StateRouter`]; this shell only builds their
//! inputs (including the daemon-fetch closure) and pumps
//! stdin→router→stdout. Register it with e.g.
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
    /// (secret-redacted) config — plus live state tools (sessions,
    /// worktrees, leases, identity) under `--scopes`. Register with e.g.
    /// `claude mcp add thegn -- thegn mcp serve`.
    Serve {
        /// Scopes granted to the live-state tools (comma-separated:
        /// read,write,git,admin). Every state tool today is read-only, so
        /// the default `read` enables them all; the flag exists so a future
        /// write-side tool set is opt-in rather than implied. Pass `none`
        /// (or any empty/unknown set) to serve docs tools only.
        #[arg(long, value_delimiter = ',', default_value = "read")]
        scopes: Vec<String>,
    },
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
        Action::Serve { scopes } => serve(cfg, config_path, &scopes),
    }
}

/// Serve the docs/help/config (+ scoped live-state) MCP endpoint over stdio
/// (newline-delimited JSON-RPC — the MCP stdio contract). Builds the help
/// registry, the redacted config, and the schema once, then loops reading one
/// request per line. State tools fetch from the pane daemon per call
/// (`block_on` on a current-thread runtime — the loop is synchronous stdio),
/// with a DB-cache fallback for `worktrees_list` when no daemon is up.
fn serve(cfg: &Config, config_path: PathBuf, scopes: &[String]) -> Result<()> {
    use thegn_core::mcp::docs::{DocResource, DocsRouter, redact};
    use thegn_core::mcp::state::StateRouter;

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

    // State tools: `--scopes` → allowed capability set, one current-thread
    // runtime reused across calls, control-client fetch per call.
    let scope_set = thegn_core::control::ScopeSet::parse(&scopes.join(","));
    let allowed = allowed_state_caps(scope_set);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let fetch = |cap: &str, _args: &serde_json::Value| -> Result<serde_json::Value, String> {
        rt.block_on(fetch_state(cfg, cap))
    };

    let router = DocsRouter::new(
        &reg,
        config_val,
        schema,
        docs,
        &crate::fff_backend::fuzzy_rank,
        explain,
    )
    .with_state(StateRouter::new(allowed, fetch));

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

/// The one scope→caps mapping: a state capability is allowed iff the
/// requested scope set satisfies its verb's [`required_scope`] — the same
/// policy table control tokens answer to. Pure, so the mapping is unit-tested
/// without a daemon.
fn allowed_state_caps(scopes: thegn_core::control::ScopeSet) -> Vec<&'static str> {
    use thegn_core::capability::{lookup, scope_of};
    thegn_core::mcp::state::MCP_STATE_CAPS
        .iter()
        .copied()
        .filter(|id| lookup(id).is_some_and(|c| scopes.allows(scope_of(c))))
        .collect()
}

/// The state tools' clean no-daemon answer (the JSON-RPC error message).
const NO_DAEMON: &str = "daemon not reachable — start thegn or `thegn daemon`";

/// Fetch one state capability's payload from the pane daemon (control
/// client). `worktrees.list` degrades to the DB cache when no daemon answers;
/// the session/lease/identity tools have no offline truth and error instead.
async fn fetch_state(cfg: &Config, cap: &str) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let client = super::session::connect(cfg).await;
    match cap {
        "worktrees.list" => match client {
            Ok(c) => {
                let wts = c.worktrees().await.map_err(|e| e.to_string())?;
                Ok(json!({ "source": "daemon", "worktrees": wts }))
            }
            // No daemon: the DB is the registration cache the sidebar
            // resurrects from — good enough for a listing, and labeled so.
            Err(_) => db_worktrees()
                .map(|wts| json!({ "source": "db-cache", "worktrees": wts }))
                .map_err(|db_err| format!("{NO_DAEMON}; DB cache also failed: {db_err}")),
        },
        "sessions.list" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let sessions = c.sessions().await.map_err(|e| e.to_string())?;
            Ok(json!({ "sessions": sessions }))
        }
        "leases.list" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            c.leases().await.map_err(|e| e.to_string())
        }
        "me" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            c.me().await.map_err(|e| e.to_string())
        }
        other => Err(format!("unknown state capability `{other}`")),
    }
}

/// The DB-cache worktree listing (same rows the daemon serves), shaped like
/// the control API's `WorktreeInfo`.
fn db_worktrees() -> Result<Vec<serde_json::Value>, String> {
    use thegn_core::store::WorkspaceStore;
    let db = thegn_core::db::Db::open().map_err(|e| e.to_string())?;
    let rows = db.worktrees().map_err(|e| e.to_string())?;
    Ok(rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "path": r.worktree,
                "branch": r.branch,
                "repo_root": r.repo_root,
                "location": r.location,
                "created_at": r.created_at,
            })
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::allowed_state_caps;
    use thegn_core::control::ScopeSet;

    #[test]
    fn mcp_scope_mapping_read_covers_every_state_cap() {
        // The default `--scopes read` (and anything implying read) enables
        // the whole implemented set — every state tool is read-only today.
        for csv in ["read", "write", "read,git", "admin"] {
            assert_eq!(
                allowed_state_caps(ScopeSet::parse(csv)),
                thegn_core::mcp::state::MCP_STATE_CAPS,
                "--scopes {csv}"
            );
        }
    }

    #[test]
    fn mcp_scope_mapping_none_disables_state_tools() {
        // `none` (or any unknown name) parses to the empty set → docs only.
        assert!(allowed_state_caps(ScopeSet::parse("none")).is_empty());
        assert!(allowed_state_caps(ScopeSet::empty()).is_empty());
    }
}
