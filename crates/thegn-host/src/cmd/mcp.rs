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
//! pane daemon over the control client, and daemon-free semantic read tools
//! (`semantic_map`, `semantic_blast_radius`) answered from the state DB + git.
//! The JSON-RPC handling is the pure
//! [`thegn_core::mcp::docs::DocsRouter`] +
//! [`thegn_core::mcp::state::StateRouter`]; this shell only builds their
//! inputs (including the daemon-fetch closure) and pumps
//! stdin→router→stdout. Register it with e.g.
//! `claude mcp add thegn -- thegn mcp serve`.

use anyhow::{Result, bail};
use base64::Engine as _;
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
    /// List declared MCP servers with their launch command, grants, and (per
    /// server) their proxy exposure (exposed-vs-hidden tools).
    List,
    /// Print the `mcpServers` settings block (what agent setup injects).
    /// `--proxy` prints the single secret-free `thegn mcp proxy` entry instead
    /// (what `wire` writes) — no env block, so agent settings hold no secrets.
    Emit {
        /// Emit the aggregated-proxy entry (secret-free) instead of the raw
        /// per-server block.
        #[arg(long)]
        proxy: bool,
    },
    /// Acquire a declared server's binary via the resolver (grant-checked).
    Install {
        /// The `[mcp_servers.<name>]` to install.
        name: String,
    },
    /// Run the aggregation hub: one stdio MCP endpoint over every *exposed*
    /// `[mcp_servers.<name>]` upstream (namespaced `<upstream>__<tool>`). An
    /// agent registers this as its single MCP server. Register with e.g.
    /// `thegn mcp wire`, or `claude mcp add thegn -- thegn mcp proxy`.
    Proxy,
    /// Report the mcp-proxy hub's per-upstream state: exposed/hidden tools,
    /// scope, breaker/handshake. Probes the configured upstreams live.
    Status {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Re-read config and reconcile the daemon's mcp-proxy upstreams
    /// (start/stop/restart/refilter). Requires a running daemon.
    Reload,
    /// Write the single secret-free proxy entry into agent CLIs' MCP settings
    /// (marker-tagged, idempotent, reversible). `[[agents]]` is the source of
    /// truth for which agents by default.
    Wire {
        /// A specific agent kind (claude|codex|cursor|windsurf|vscode|zed|amp|
        /// gemini). Omit to wire every configured `[[agents]]` with a known
        /// adapter (or pass `--all`).
        #[arg(long)]
        agent: Option<String>,
        /// Wire every supported agent adapter, not just configured ones.
        #[arg(long)]
        all: bool,
        /// Remove thegn's proxy entry (restores the pre-wire state).
        #[arg(long)]
        remove: bool,
    },
    /// Manage keyring entries the proxy resolves at spawn (`keyring:<name>`
    /// refs). `list` names entries, never values.
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Curated `[mcp_servers]` presets (memory servers among them): `list`, or
    /// `show <name>` (append with `--write`). References, not dependencies.
    Preset {
        #[command(subcommand)]
        action: PresetAction,
    },
    /// Run thegn's docs/help/config MCP server over stdio, plus scoped
    /// live-state tools that can drive the pane daemon.
    ///
    /// A Context7-style endpoint for coding agents: search + read the help
    /// pages, the effective keymap, the config reference, and your current
    /// (secret-redacted) config — plus live state tools (list sessions/
    /// worktrees/leases, identity, wait) under `--scopes read` (the
    /// default), and mutating tools (open/input/kill a session) under
    /// `--scopes write`. Register with e.g.
    /// `claude mcp add thegn -- thegn mcp serve`.
    Serve {
        /// Scopes granted to the live-state tools (comma-separated:
        /// read,write,git,admin). The default `read` enables only the
        /// listing/observing tools (`sessions_list`, `worktrees_list`,
        /// `leases_list`, `me`, `sessions_wait`, `semantic_map`,
        /// `semantic_blast_radius`); the mutating tools (`sessions_open`,
        /// `sessions_input`, `sessions_kill`) additionally need `write`. Pass
        /// `none` (or any empty/unknown set) to serve docs tools only.
        #[arg(long, value_delimiter = ',', default_value = "read")]
        scopes: Vec<String>,
        /// Also enable `sessions_input` (send raw terminal input/control
        /// characters to a live session) when `write` scope is granted.
        /// Held back by default even under `--scopes write`: unlike opening
        /// or killing a session, sending input types into *whatever is
        /// running* in an arbitrary live session — a materially larger
        /// blast radius for a semi-autonomous MCP caller than the daemon's
        /// other write verbs. This flag is an explicit, per-invocation
        /// opt-in on top of (never instead of) the `write` scope check.
        #[arg(long)]
        allow_session_input: bool,
    },
}

#[derive(clap::Subcommand, Clone)]
pub enum SecretAction {
    /// Store a secret under a keyring account (a `keyring:<name>` ref resolves
    /// to it at upstream spawn). Reads the value from stdin if omitted, so it
    /// never lands in shell history.
    Set {
        /// The keyring account name (the `<name>` in `keyring:<name>`).
        name: String,
        /// The secret value (omit to read one line from stdin).
        value: Option<String>,
    },
    /// Remove a keyring entry.
    Rm {
        /// The keyring account name.
        name: String,
    },
    /// List keyring entry names thegn manages (names only, never values).
    List,
}

#[derive(clap::Subcommand, Clone)]
pub enum PresetAction {
    /// List curated presets (name, category, external requirements).
    List,
    /// Print a preset's `[mcp_servers.<name>]` block; `--write` appends it to
    /// your config after printing.
    Show {
        /// The preset name (see `thegn mcp preset list`).
        name: String,
        /// Append the block to the user config (after printing it).
        #[arg(long)]
        write: bool,
    },
}

pub fn run(cfg: &Config, action: Action, config_path: PathBuf) -> Result<()> {
    match action {
        Action::List => list(cfg),
        Action::Emit { proxy } => {
            let block = if proxy {
                super::mcp_proxy_cmd::proxy_emit_block()
            } else {
                // Direct emit copies each server's `env` — including secret
                // values — into the agent settings verbatim. Warn (to stderr,
                // so the stdout block stays pipeable) and point at the
                // secret-free proxy path.
                let leaks: Vec<&String> = cfg
                    .mcp_servers
                    .iter()
                    .filter(|(_, s)| !s.env.is_empty())
                    .map(|(n, _)| n)
                    .collect();
                if !leaks.is_empty() {
                    eprintln!(
                        "warning: `mcp emit` copies env (incl. secrets) into agent settings for: {}.\n\
                         Prefer `thegn mcp wire` / `thegn mcp emit --proxy` — the proxy resolves \
                         secrets only at spawn, so agent files hold no keys.",
                        leaks
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                settings_block(&cfg.mcp_servers)
            };
            outln!("{}", serde_json::to_string_pretty(&block)?);
            Ok(())
        }
        Action::Install { name } => install(cfg, &name),
        Action::Serve {
            scopes,
            allow_session_input,
        } => serve(cfg, config_path, &scopes, allow_session_input),
        Action::Proxy => crate::mcp_proxy::run_shim(cfg),
        Action::Status { json } => super::mcp_proxy_cmd::status(cfg, json),
        Action::Reload => super::mcp_proxy_cmd::reload(cfg),
        Action::Wire { agent, all, remove } => {
            super::mcp_proxy_cmd::wire(cfg, agent.as_deref(), all, remove)
        }
        Action::Secret { action } => super::mcp_proxy_cmd::secret(action),
        Action::Preset { action } => super::mcp_proxy_cmd::preset(cfg, config_path, action),
    }
}

/// Serve the docs/help/config (+ scoped live-state) MCP endpoint over stdio
/// (newline-delimited JSON-RPC — the MCP stdio contract). Builds the help
/// registry, the redacted config, and the schema once, then loops reading one
/// request per line. State tools fetch from the pane daemon per call
/// (`block_on` on a current-thread runtime — the loop is synchronous stdio),
/// with a DB-cache fallback for `worktrees_list` when no daemon is up.
fn serve(
    cfg: &Config,
    config_path: PathBuf,
    scopes: &[String],
    allow_session_input: bool,
) -> Result<()> {
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
    let allowed = allowed_state_caps(scope_set, allow_session_input);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let fetch = |cap: &str, args: &serde_json::Value| -> Result<serde_json::Value, String> {
        rt.block_on(fetch_state(cfg, cap, args))
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
///
/// `sessions.input` carries one additional, MCP-specific interlock beyond
/// scope: `allow_session_input` must also be true. This is not a second
/// policy table — the scope check still runs unconditionally for every
/// capability including this one — it is a single named exception for the
/// one tool whose blast radius (arbitrary byte injection into a live
/// session) argued for more than `write` scope alone (see
/// `openspec/changes/add-mcp-write-tools/design.md`). `StateRouter` itself
/// never sees this distinction: it only receives the resulting `allowed`
/// list, so the interlock is enforced at exactly the same choke point as
/// every other scope decision (discovery *and* invocation).
fn allowed_state_caps(
    scopes: thegn_core::control::ScopeSet,
    allow_session_input: bool,
) -> Vec<&'static str> {
    use thegn_core::capability::{lookup, scope_of};
    thegn_core::mcp::state::MCP_STATE_CAPS
        .iter()
        .copied()
        .filter(|id| {
            lookup(id).is_some_and(|c| scopes.allows(scope_of(c)))
                && (*id != "sessions.input" || allow_session_input)
        })
        .collect()
}

/// The state tools' clean no-daemon answer (the JSON-RPC error message).
const NO_DAEMON: &str = "daemon not reachable — start thegn or `thegn daemon`";

/// Fetch (or perform) one state capability's payload against the pane daemon
/// (control client). `args` is the already-schema-validated `tools/call`
/// arguments object (`StateRouter::call` runs `validate_args` before this is
/// ever invoked — see `thegn_core::mcp::state`). `worktrees.list` degrades to
/// the DB cache when no daemon answers; every other tool has no offline
/// truth and errors instead — including the four mutating tools, which by
/// construction need a live daemon to act on.
async fn fetch_state(
    cfg: &Config,
    cap: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    // Semantic read tools answer from the state DB + git listing directly — no
    // daemon required (a repo map is a structural summary of source the caller
    // can already open). Handle them before connecting a control client.
    match cap {
        "semantic.map" => return semantic_map(cfg, args),
        "semantic.blast_radius" => return semantic_blast_radius(args),
        _ => {}
    }
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
        "sessions.wait" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let session = str_arg(args, "session").ok_or("missing `session`")?;
            let condition_str = str_arg(args, "condition").ok_or("missing `condition`")?;
            let condition =
                super::session::parse_wait_condition(condition_str).map_err(|e| e.to_string())?;
            let timeout_ms = args.get("timeout_ms").and_then(serde_json::Value::as_i64);
            c.wait(session, condition, timeout_ms)
                .await
                .map_err(|e| e.to_string())
        }
        "sessions.open" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let spec = open_spec_from_args(args)?;
            let info = c.open(&spec).await.map_err(|e| e.to_string())?;
            serde_json::to_value(&info).map_err(|e| e.to_string())
        }
        "sessions.input" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let session = str_arg(args, "session").ok_or("missing `session`")?;
            let enter = args
                .get("enter")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let bytes = match (str_arg(args, "text"), str_arg(args, "bytes_b64")) {
                (Some(_), Some(_)) => {
                    return Err("exactly one of `text`/`bytes_b64` may be given, not both".into());
                }
                (Some(text), None) => text.as_bytes().to_vec(),
                (None, Some(b64)) => base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| format!("bad `bytes_b64`: {e}"))?,
                (None, None) => {
                    return Err("exactly one of `text`/`bytes_b64` is required".into());
                }
            };
            let n = bytes.len();
            c.send_input(session, &bytes, enter)
                .await
                .map_err(|e| e.to_string())?;
            Ok(json!({ "sent_bytes": n, "session": session, "enter": enter }))
        }
        "sessions.kill" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let session = str_arg(args, "session").ok_or("missing `session`")?;
            c.kill(session).await.map_err(|e| e.to_string())?;
            Ok(json!({ "killed": session }))
        }
        other => Err(format!("unknown state capability `{other}`")),
    }
}

/// A tool call's string argument by name (the JSON-RPC `arguments` object,
/// already validated against the tool's schema — this just extracts).
fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

/// The worktree a semantic tool targets: the `worktree` argument, else the
/// server's own worktree resolution (env / cwd git toplevel).
fn semantic_root(args: &serde_json::Value) -> std::path::PathBuf {
    match str_arg(args, "worktree") {
        Some(w) => std::path::PathBuf::from(w),
        None => super::resolve_worktree(None),
    }
}

/// `semantic.map`: the ranked, budgeted repo map for a worktree, read from the
/// entity index (built inline and capped on first use). Never fabricates — a
/// worktree with no indexable files says so via `has_indexable_files: false`.
fn semantic_map(cfg: &Config, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let root = semantic_root(args);
    let budget = args
        .get("budget")
        .and_then(serde_json::Value::as_u64)
        .map(|b| (b as usize).max(1))
        .unwrap_or_else(|| cfg.semantic.budget());
    let cap = cfg.semantic.file_cap();
    let db = thegn_core::db::Db::open().map_err(|e| e.to_string())?;
    let load = crate::repo_index::load_repo_map(&root, cap, &db, str_arg(args, "file"));
    let rows = load.map.rows(budget);
    Ok(json!({
        "worktree": root.to_string_lossy(),
        "has_indexable_files": load.has_ts_files,
        "partial": load.map.partial(),
        "total": load.map.total(),
        "shown": rows.len(),
        "rows": rows,
    }))
}

/// `semantic.blast_radius`: the changed entities + callers + untested set + risk
/// band for a worktree's pending changes, from the persisted graph. Returns a
/// clear "graph unavailable" result (never an error or fabricated emptiness)
/// when no graph contributes.
fn semantic_blast_radius(args: &serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let root = semantic_root(args);
    let db = thegn_core::db::Db::open().map_err(|e| e.to_string())?;
    match crate::blast_radius::blast_report_for_worktree(&root, &db) {
        Some(report) => {
            let mut v = serde_json::to_value(&report).map_err(|e| e.to_string())?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("worktree".into(), json!(root.to_string_lossy()));
                obj.insert("available".into(), json!(true));
            }
            Ok(v)
        }
        None => Ok(json!({
            "worktree": root.to_string_lossy(),
            "available": false,
            "message": "graph unavailable — no changes with resolvable callers \
                        (LSP off, graph not yet built, or the change has no dependents)",
        })),
    }
}

/// Parse `sessions_open`'s tool arguments into the control API's `OpenSpec`.
/// `adopt`/`already_capped` are deliberately not exposed as tool arguments —
/// see design.md §2: `adopt` asks a *running compositor* to graft the
/// session into a real pane (a local-UI concern no MCP caller should
/// request), and `already_capped` exists only for the compositor's own
/// already-sandboxed spawn path, so an MCP-originated open must always get
/// the resource cap applied for it.
fn open_spec_from_args(args: &serde_json::Value) -> Result<thegn_svc::control::OpenSpec, String> {
    use thegn_svc::control::{AgentLaunch, OpenSpec};

    let argv: Vec<String> = args
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let env = match args.get("env") {
        Some(serde_json::Value::Object(map)) => {
            let mut out = Vec::with_capacity(map.len());
            for (k, v) in map {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("env value for `{k}` must be a string"))?;
                out.push((k.clone(), s.to_string()));
            }
            out
        }
        _ => Vec::new(),
    };

    let rows = args
        .get("rows")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(24) as u16;
    let cols = args
        .get("cols")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(80) as u16;

    let agent = str_arg(args, "agent").map(|agent| AgentLaunch {
        agent: agent.to_string(),
        prompt: str_arg(args, "prompt").unwrap_or_default().to_string(),
        headless: args.get("headless").and_then(serde_json::Value::as_bool),
        bind_worktree: args
            .get("bind_worktree")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    });

    Ok(OpenSpec {
        argv,
        cwd: str_arg(args, "cwd").map(str::to_string),
        env,
        rows,
        cols,
        worktree: str_arg(args, "worktree").map(str::to_string),
        agent,
        adopt: false,
        already_capped: false,
    })
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
        // Proxy exposure — default-deny: shown so the effective policy is visible.
        match &srv.proxy {
            Some(p) if p.is_exposed() => {
                outln!(
                    "  proxy: exposed (scope={}, tools=[{}])",
                    p.scope,
                    p.tools.join(", ")
                );
            }
            _ => {
                outln!("  proxy: not exposed (default-deny — add [mcp_servers.{name}.proxy] tools)")
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

    const READ_CAPS: &[&str] = &[
        "sessions.list",
        "worktrees.list",
        "leases.list",
        "me",
        "sessions.wait",
        "semantic.map",
        "semantic.blast_radius",
    ];

    fn sorted(mut v: Vec<&'static str>) -> Vec<&'static str> {
        v.sort_unstable();
        v
    }

    #[test]
    fn mcp_scope_mapping_read_covers_only_the_read_scope_tools() {
        // The default `--scopes read` enables the listing/observing tools —
        // NOT the mutating ones. This is the deliberate split this change
        // introduces (see `every_state_cap_maps_to_the_scope_it_documents`
        // in thegn-core for the scope-table half of this pin).
        for csv in ["read", "read,git"] {
            assert_eq!(
                sorted(allowed_state_caps(ScopeSet::parse(csv), false)),
                sorted(READ_CAPS.to_vec()),
                "--scopes {csv}"
            );
        }
    }

    #[test]
    fn mcp_scope_mapping_write_adds_open_and_kill_but_not_input() {
        let allowed = allowed_state_caps(ScopeSet::parse("write"), false);
        assert!(allowed.contains(&"sessions.open"), "{allowed:?}");
        assert!(allowed.contains(&"sessions.kill"), "{allowed:?}");
        assert!(!allowed.contains(&"sessions.input"), "{allowed:?}");
        // read-scope tools are still covered (write implies read).
        for cap in READ_CAPS {
            assert!(allowed.contains(cap), "{allowed:?} missing {cap}");
        }
    }

    #[test]
    fn mcp_scope_mapping_session_input_needs_write_and_the_flag() {
        // Neither alone is enough…
        assert!(!allowed_state_caps(ScopeSet::parse("write"), false).contains(&"sessions.input"));
        assert!(!allowed_state_caps(ScopeSet::parse("read"), true).contains(&"sessions.input"));
        // …admin scope does not bypass the interlock either — it is not a
        // scope decision, it is an explicit, separate per-invocation opt-in.
        assert!(!allowed_state_caps(ScopeSet::parse("admin"), false).contains(&"sessions.input"));
        // …both together enable it.
        assert!(allowed_state_caps(ScopeSet::parse("write"), true).contains(&"sessions.input"));
        assert!(allowed_state_caps(ScopeSet::parse("admin"), true).contains(&"sessions.input"));
    }

    #[test]
    fn mcp_scope_mapping_none_disables_state_tools() {
        // `none` (or any unknown name) parses to the empty set → docs only.
        assert!(allowed_state_caps(ScopeSet::parse("none"), true).is_empty());
        assert!(allowed_state_caps(ScopeSet::empty(), true).is_empty());
    }

    #[test]
    fn mcp_scope_mapping_write_and_flag_covers_every_implemented_cap() {
        assert_eq!(
            sorted(allowed_state_caps(ScopeSet::parse("write"), true)),
            sorted(thegn_core::mcp::state::MCP_STATE_CAPS.to_vec()),
        );
    }
}
