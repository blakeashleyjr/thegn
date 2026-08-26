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
    /// List declared MCP servers with their launch command and grants.
    List,
    /// Print the `mcpServers` settings block (what agent setup injects).
    Emit,
    /// Acquire a declared server's binary via the resolver (grant-checked).
    Install {
        /// The `[mcp_servers.<name>]` to install.
        name: String,
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
        /// read,write,git,admin). When omitted, the ceiling is resolved from
        /// config — the global `[mcp.serve] scopes`, narrowed by the active
        /// profile overlay — defaulting to `read` when nothing is configured;
        /// when given, it intersects that ceiling (clamp-only, never widening).
        /// The default `read` enables only the listing/observing tools
        /// (`sessions_list`, `worktrees_list`, `leases_list`, `me`,
        /// `agent_sessions`, `sessions_wait`); the mutating tools
        /// (`sessions_open`, `sessions_input`, `sessions_kill`) additionally
        /// need `write`. Pass `none` (or any empty/unknown set) to serve
        /// docs tools only.
        #[arg(long, value_delimiter = ',')]
        scopes: Option<Vec<String>>,
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
        Action::Serve {
            scopes,
            allow_session_input,
        } => serve(cfg, config_path, scopes.as_deref(), allow_session_input),
    }
}

/// Resolve the effective MCP-serve scope set (clamp-only) from config plus the
/// `--scopes` flag: the global `[mcp.serve]` ceiling, narrowed by the active
/// profile overlay, then intersected by the flag. The workspace overlay is not
/// resolved here — the stdio server has no repo context, and repo-local /
/// workspace clamping lands with `add-config-trust-resolution`; the pure
/// resolver already supports that level. Returns the set and the clamping level.
fn resolve_serve_scopes(
    cfg: &Config,
    flag: Option<&[String]>,
) -> (
    thegn_core::control::ScopeSet,
    thegn_core::control::ScopeClamp,
) {
    use thegn_core::control::{Scope, ScopeSet};
    let global = cfg.mcp.serve.scope_set();
    let profile = cfg
        .profiles
        .get(&thegn_core::profile::name())
        .and_then(|p| p.mcp_serve.scope_set());
    let workspace = None;
    let flag = flag.map(|v| ScopeSet::parse(&v.join(",")));
    thegn_core::control::resolve_serve_scopes(
        global,
        profile,
        workspace,
        flag,
        ScopeSet::of(&[Scope::Read]),
    )
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
    scopes: Option<&[String]>,
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

    // State tools: config-resolved (clamp-only) scope ceiling ∩ `--scopes` →
    // allowed capability set; one current-thread runtime reused across calls,
    // control-client fetch per call. The effective set + clamping level go to
    // stderr (stdout is the JSON-RPC channel) so an operator can see the grant.
    let (scope_set, clamp) = resolve_serve_scopes(cfg, scopes);
    let csv = scope_set.to_csv();
    eprintln!(
        "thegn mcp serve: effective scopes = [{}] (clamped by: {})",
        if csv.is_empty() { "none" } else { &csv },
        clamp.as_str()
    );
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
        // A pure filesystem read of the harness session stores — answered
        // locally, no daemon required (like the worktrees.list DB fallback).
        "agent.sessions" => {
            let known: std::collections::HashSet<String> = thegn_core::db::Db::open()
                .ok()
                .and_then(|db| {
                    use thegn_core::store::WorkspaceStore;
                    db.worktrees().ok()
                })
                .map(|rows| rows.into_iter().map(|r| r.worktree).collect())
                .unwrap_or_default();
            let filter = thegn_svc::sessions::SessionFilter {
                worktree: str_arg(args, "worktree"),
                harness: str_arg(args, "harness"),
            };
            let recs = thegn_svc::sessions::discover(cfg, &filter, &known);
            Ok(json!({ "sessions": recs }))
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
        resume: str_arg(args, "resume").map(str::to_string),
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
        "agent.sessions",
        "sessions.wait",
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
