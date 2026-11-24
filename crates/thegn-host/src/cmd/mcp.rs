//! `thegn mcp <action>` — user-declared MCP servers (`[mcp_servers.<name>]`)
//! plus `serve`, thegn's own read-only docs/help/config MCP endpoint.
//!
//! `list`/`emit`/`install` manage the servers thegn hands to agents: lists
//! declared servers, emits the `mcpServers` settings block the agent consumes,
//! and installs a server's binary via the shared managed-tool resolver —
//! grant-checked: acquisition proceeds only when the server's capability grants
//! cover it. npm/cargo installers run under bubblewrap filesystem containment.
//!
//! `serve` runs thegn *as* an MCP server over stdio (a Context7-style endpoint):
//! it exposes the in-app help corpus, the generated keybindings/config-reference
//! pages, and the user's current secret-redacted config so a coding agent can
//! learn how thegn works — plus, gated by `--scopes`, live *state* tools
//! (`sessions_list`, `worktrees_list`, `worktrees_sandbox_profile`, `git_status`, `merge_list`, `pr_status`, `leases_list`, `me`, `calendar_events`, `calendar_clocks`, `issues_list`, `issues_get`, `dispatches_list`) answered by the
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
use std::fmt::Write as _;
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
        /// Run an npm/cargo installer without bubblewrap containment when bwrap
        /// is unavailable (interactive terminals only; install scripts then run
        /// with your host files).
        #[arg(long)]
        unsafe_host_install: bool,
    },
    /// Run the aggregation hub: one stdio MCP endpoint over every *exposed*
    /// `[mcp_servers.<name>]` upstream (namespaced `<upstream>__<tool>`). An
    /// agent registers this as its single MCP server. Register with e.g.
    /// `thegn mcp wire`, or `claude mcp add thegn -- thegn mcp proxy`.
    Proxy {
        /// Serve the hub for one named `[mcp]` profile (the Lead's scoped
        /// tool set) instead of the default exposure.
        #[arg(long)]
        profile: Option<String>,
    },
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
        /// Wire (or with `--remove`, unwire) the Lead's MCP entry instead of
        /// the agents' proxy entry.
        #[arg(long, conflicts_with_all = ["agent", "all"])]
        lead: bool,
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
        /// read,write,git,exec,admin). When omitted, the ceiling is resolved from
        /// config — the global `[mcp.serve] scopes`, narrowed by the active
        /// profile overlay — defaulting to `read` when nothing is configured;
        /// when given, it intersects that ceiling (clamp-only, never widening).
        /// The default `read` enables only the listing/observing tools
        /// (`sessions_list`, `worktrees_list`, `worktrees_sandbox_profile`, `git_status`,
        /// `merge_list`, `pr_status`, `leases_list`, `me`,
        /// `agent_sessions`, `sessions_wait`, `sessions_snapshot`, `semantic_map`,
        /// `semantic_blast_radius`); the mutating tools (`sessions_open`,
        /// `sessions_input`, `sessions_kill`, `sessions_resize`, `sessions_split`,
        /// `worktrees_open`, `notify_push`, `dispatches_put`,
        /// `dispatches_set_status`, `issues_update`, `issues_comment`) additionally need
        /// `write`, `folders_assign` needs `git`, and `merge_add` and
        /// `merge_drain` need `git`. Pass
        /// `none` (or any empty/unknown set) to serve docs tools only.
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
                    thegn_core::msg::warn(&format!(
                        "`mcp emit` copies env (incl. secrets) into agent settings for: {}.\n\
                         Prefer `thegn mcp wire` / `thegn mcp emit --proxy` — the proxy resolves \
                         secrets only at spawn, so agent files hold no keys.",
                        leaks
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                settings_block(&cfg.mcp_servers)
            };
            outln!("{}", serde_json::to_string_pretty(&block)?);
            Ok(())
        }
        Action::Install {
            name,
            unsafe_host_install,
        } => install(cfg, &name, unsafe_host_install),
        Action::Serve {
            scopes,
            allow_session_input,
        } => serve(cfg, config_path, scopes.as_deref(), allow_session_input),
        Action::Proxy { profile } => crate::lead::mcp::run_proxy(cfg, profile.as_deref()),
        Action::Status { json } => super::mcp_proxy_cmd::status(cfg, json),
        Action::Reload => super::mcp_proxy_cmd::reload(cfg),
        Action::Wire {
            lead: true, remove, ..
        } => crate::lead::mcp::wire_cli(cfg, remove),
        Action::Wire {
            agent, all, remove, ..
        } => super::mcp_proxy_cmd::wire(cfg, agent.as_deref(), all, remove),
        Action::Secret { action } => super::mcp_proxy_cmd::secret(action),
        Action::Preset { action } => super::mcp_proxy_cmd::preset(cfg, config_path, action),
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
    thegn_core::msg::info(&format!(
        "thegn mcp serve: effective scopes = [{}] (clamped by: {})",
        if csv.is_empty() { "none" } else { &csv },
        clamp.as_str()
    ));
    let token_addressed = super::session::env_control_addr(&|k| std::env::var(k).ok()).is_some();
    let allowed = without_local_orchestration(
        allowed_state_caps(scope_set, allow_session_input),
        token_addressed,
    );
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
    .with_state(
        StateRouter::new(allowed, fetch).with_caller(thegn_core::mcp::caller::caller_from_env()),
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
    let mut origin = match config_resolve::explain(&ProcessEnv, &[], Some(path), key) {
        Ok(origin) => origin,
        Err(error) => return format!("{key}: unadmitted configuration: {error}\n"),
    };
    redact_explain_origin(&mut origin);
    let mut s = format!(
        "{} = {}\n  set by: {}\n",
        origin.key,
        origin.value,
        origin.origin.as_str()
    );
    for (layer, val) in &origin.trace {
        writeln!(s, "    {}: {val}", layer.as_str()).expect("writing to String cannot fail");
    }
    s
}

fn redact_explain_origin(origin: &mut thegn_core::config_resolve::KeyOrigin) {
    let key = origin.key.clone();
    origin.value = redact_explain_value(&key, origin.value.take());
    for (_, value) in &mut origin.trace {
        *value = redact_explain_value(&key, value.take());
    }
}

fn redact_explain_value(key: &str, value: serde_json::Value) -> serde_json::Value {
    let segments: Vec<&str> = key.split('.').collect();
    let leaf = segments.last().copied().unwrap_or(key);
    let sensitive_key = segments
        .iter()
        .find(|segment| thegn_core::redact::is_sensitive(segment));
    let wrapper_key = sensitive_key.copied().unwrap_or(leaf);
    let mut wrapped = serde_json::json!({ wrapper_key: value });
    if sensitive_key.is_some() {
        thegn_core::redact::redact_json(&mut wrapped);
    } else {
        thegn_core::redact::redact_json_at(
            &segments[..segments.len().saturating_sub(1)],
            &mut wrapped,
        );
    }
    wrapped
        .get_mut(wrapper_key)
        .map(serde_json::Value::take)
        .unwrap_or(serde_json::Value::Null)
}

/// The one scope→caps mapping: a state capability is allowed iff the
/// requested scope set satisfies its verb's
/// [`required_scope`](thegn_core::control::required_scope) — the same
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
                && (!matches!(*id, "merge.add" | "merge.drain")
                    || thegn_core::control::route_to_host_token_allows_worktree(scopes, "", ""))
        })
        .collect()
}

/// `agent.dispatch` writes its roster row from this MCP process, so it is
/// neither listed nor runnable when the server reaches the daemon through a
/// scoped control token (`THEGN_CONTROL_ADDR`, the Lead's path). Pure.
fn without_local_orchestration(
    mut caps: Vec<&'static str>,
    token_addressed: bool,
) -> Vec<&'static str> {
    if token_addressed {
        caps.retain(|id| *id != "agent.dispatch");
    }
    caps
}

/// Admission for `agent.dispatch`, before any daemon contact: refuses a
/// token-addressed caller, a non-absolute worktree, a bad identity, an unknown
/// stage or a blank task. Returns `(agent, worktree, issue_id, prompt, stage)`.
/// Pure over `cfg` and `args`.
fn agent_dispatch_admit<'a>(
    cfg: &Config,
    args: &'a serde_json::Value,
    token_addressed: bool,
) -> Result<(&'a str, &'a str, &'a str, &'a str, Option<&'a str>), String> {
    if token_addressed {
        return Err(
            "agent.dispatch is not available over a scoped control token; \
             the Lead opens a configured agent with sessions.open"
                .to_string(),
        );
    }
    let worktree = str_arg(args, "worktree").ok_or("missing `worktree`")?;
    if !std::path::Path::new(worktree).is_absolute() {
        return Err("`worktree` must be an absolute path".to_string());
    }
    let agent = str_arg(args, "agent").ok_or("missing `agent`")?;
    let issue_id = str_arg(args, "issue_id").ok_or("missing `issue_id`")?;
    let prompt = str_arg(args, "prompt").ok_or("missing `prompt`")?;
    let stage = str_arg(args, "stage");
    super::session::open_preflight(
        cfg,
        stage,
        None,
        Some(issue_id),
        Some(worktree),
        prompt,
        true,
    )
    .map_err(|e| e.to_string())?;
    if let Some(stage) = stage {
        super::session::policy_admission(cfg, agent, stage).map_err(|e| e.to_string())?;
    }
    Ok((agent, worktree, issue_id, prompt, stage))
}

/// The control call for an issue write. The route reads `repo` and `account`
/// from the query string, not the body, so they are lifted out of the params
/// before `build_call` turns the rest into the JSON body. Pure.
fn issue_write_call(
    cap: &str,
    args: &serde_json::Value,
) -> Result<(&'static str, String, Option<serde_json::Value>), String> {
    let mut params = routed_params(cap, args)?;
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    for key in ["repo", "account"] {
        if let Some(v) = params.remove(key) {
            let v = v
                .as_str()
                .ok_or_else(|| format!("`{key}` must be a string"))?;
            query.append_pair(key, v);
        }
    }
    let (method, mut path, body) = thegn_svc::control::routes::build_call(cap, params)?;
    let query = query.finish();
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query);
    }
    Ok((method, path, body))
}

/// The state tools' clean no-daemon answer (the JSON-RPC error message).
const NO_DAEMON: &str = "daemon not reachable — start thegn or `thegn daemon`";

/// Control-route params for the routed state tools (`sessions.resize`,
/// `sessions.snapshot`, `sessions.split`, `worktrees.open`) from the
/// already-validated MCP arguments. The session tools rename the MCP `session`
/// argument to the route's `{s}` path parameter. Resize (and split's optional
/// `rows`/`cols`) refuse a dimension over the daemon's shared PTY bound
/// (`MAX_PTY_ROWS` x `MAX_PTY_COLS`, which the daemon enforces too) before any
/// daemon round-trip; split also checks `dir` against `SplitDir` and turns the
/// `env` object into the body's `[name, value]` pairs. `worktrees.open` passes
/// `OpenWorktreeBody`'s `repo`/`branch` through unchanged.
/// `dispatches.set_status` fills the route's `{id}` path parameter from the MCP
/// `id` argument, which must be a positive integer; its `status`/`lead` body
/// passes through (the daemon owns the closed status set), as does
/// `dispatches.put`'s `DispatchPutReq` body. `issues.update` renames the MCP
/// `assignee` (`me`/`none`) to `IssuePatch.assignee_me` and the 0-4 `priority`
/// to its snake_case name (Linear's numbering); `issues.comment` passes
/// `{id, body}` through. Pure.
fn routed_params(
    cap: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    use thegn_svc::control::{MAX_PTY_COLS, MAX_PTY_ROWS};
    let mut params = args.as_object().cloned().unwrap_or_default();
    if matches!(
        cap,
        "sessions.resize" | "sessions.snapshot" | "sessions.split"
    ) {
        let session = params.remove("session").ok_or("missing `session`")?;
        params.insert("s".into(), session);
    }
    if matches!(cap, "sessions.resize" | "sessions.split") {
        for (key, max) in [("rows", MAX_PTY_ROWS), ("cols", MAX_PTY_COLS)] {
            let present = params.get(key);
            // Resize requires both; split's are optional (0 inherits).
            if present.is_none() && cap == "sessions.split" {
                continue;
            }
            let fits = present
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|n| n <= u64::from(max));
            if !fits {
                return Err(format!("`{key}` must be an integer from 0 to {max}"));
            }
        }
    }
    if cap == "issues.update" {
        if let Some(a) = params.remove("assignee") {
            let me = match a.as_str() {
                Some("me") => true,
                Some("none") => false,
                _ => return Err("`assignee` must be `me` or `none`".into()),
            };
            params.insert("assignee_me".into(), serde_json::Value::Bool(me));
        }
        if let Some(p) = params.remove("priority") {
            let name = match p.as_i64() {
                Some(0) => "none",
                Some(1) => "urgent",
                Some(2) => "high",
                Some(3) => "medium",
                Some(4) => "low",
                _ => return Err("`priority` must be an integer from 0 to 4".into()),
            };
            params.insert("priority".into(), serde_json::json!(name));
        }
    }
    if cap == "dispatches.set_status" {
        let id = params.get("id").ok_or("missing `id`")?;
        if !id.as_i64().is_some_and(|n| n > 0) {
            return Err("`id` must be a positive integer".into());
        }
    }
    if cap == "sessions.split" {
        if let Some(dir) = params.get("dir")
            && !matches!(dir.as_str(), Some("right" | "down"))
        {
            return Err("`dir` must be `right` or `down`".into());
        }
        if let Some(env) = params.get("env") {
            let map = env.as_object().ok_or("`env` must be an object")?;
            let mut pairs = Vec::with_capacity(map.len());
            for (k, v) in map {
                let v = v
                    .as_str()
                    .ok_or_else(|| format!("env value for `{k}` must be a string"))?;
                pairs.push(serde_json::json!([k, v]));
            }
            params.insert("env".into(), serde_json::Value::Array(pairs));
        }
    }
    Ok(params)
}

enum InputPayload {
    Text { text: String, paste: bool },
    Bytes(Vec<u8>),
    Keys(Vec<String>),
}

fn input_payload(args: &serde_json::Value) -> Result<InputPayload, String> {
    let obj = args
        .as_object()
        .ok_or("state tool arguments must be an object")?;
    let has_text = obj.contains_key("text");
    let has_bytes = obj.contains_key("bytes_b64");
    let has_keys = obj.contains_key("keys");
    let sources = [has_text, has_bytes, has_keys]
        .into_iter()
        .filter(|present| *present)
        .count();
    if sources != 1 {
        return Err(
            "exactly one of `text`, `bytes_b64` or `keys` is required (not more than one)".into(),
        );
    }

    let paste = match obj.get("paste") {
        None => false,
        Some(value) => value.as_bool().ok_or("`paste` must be a boolean")?,
    };
    if has_text {
        let text = obj
            .get("text")
            .and_then(serde_json::Value::as_str)
            .ok_or("`text` must be a string")?;
        return Ok(InputPayload::Text {
            text: text.to_string(),
            paste,
        });
    }
    if paste {
        return Err("`paste` may only be used with `text`".into());
    }
    if has_bytes {
        let b64 = obj
            .get("bytes_b64")
            .and_then(serde_json::Value::as_str)
            .ok_or("`bytes_b64` must be a string")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| format!("bad `bytes_b64`: {e}"))?;
        return Ok(InputPayload::Bytes(bytes));
    }

    let values = obj
        .get("keys")
        .and_then(serde_json::Value::as_array)
        .ok_or("`keys` must be an array of strings")?;
    if values.len() > super::session::keys::MAX_KEYS {
        return Err(format!(
            "`keys` must contain at most {} keys",
            super::session::keys::MAX_KEYS
        ));
    }
    let mut keys = Vec::with_capacity(values.len());
    for value in values {
        keys.push(
            value
                .as_str()
                .ok_or("`keys` must be an array of strings")?
                .to_string(),
        );
    }
    Ok(InputPayload::Keys(keys))
}

/// Fetch (or perform) one state capability's payload against the pane daemon
/// (control client). `args` is the already-schema-validated `tools/call`
/// arguments object (`StateRouter::call` runs `validate_args` before this is
/// ever invoked — see `thegn_core::mcp::state`). `worktrees.list` degrades to
/// the DB cache when no daemon answers; every other tool has no offline
/// truth and errors instead — including the mutating tools, which by
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
    // Admit a dispatch (token path, worktree, identity, stage, task) before
    // connecting: nothing here touches the daemon or the roster.
    let dispatch = if cap == "agent.dispatch" {
        let token_addressed =
            super::session::env_control_addr(&|k| std::env::var(k).ok()).is_some();
        Some(agent_dispatch_admit(cfg, args, token_addressed)?)
    } else {
        None
    };
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
        "worktrees.sandbox_profile" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let worktree = str_arg(args, "worktree").ok_or("missing `worktree`")?;
            let reply = c
                .worktree_sandbox_profile(worktree)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_value(reply).map_err(|e| e.to_string())
        }
        "folders.assign" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let request = thegn_svc::control::FolderAssignReq {
                worktree: str_arg(args, "worktree")
                    .ok_or("missing `worktree`")?
                    .to_string(),
                folder: str_arg(args, "folder").map(str::to_string),
                clear: match args.get("clear") {
                    None | Some(serde_json::Value::Null) => false,
                    Some(serde_json::Value::Bool(b)) => *b,
                    Some(_) => return Err("`clear` must be a boolean".into()),
                },
            };
            c.folder_assign(&request).await.map_err(|e| e.to_string())?;
            Ok(json!({ "ok": true }))
        }
        "sessions.list" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let sessions = c.sessions().await.map_err(|e| e.to_string())?;
            Ok(json!({ "sessions": sessions }))
        }
        "preview.fetch" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let request = thegn_svc::control::PreviewFetchRequest {
                url: str_arg(args, "url").ok_or("missing `url`")?.to_string(),
                worktree: str_arg(args, "worktree").map(str::to_string),
                include_console: args
                    .get("include_console")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            };
            let reply = c.preview_fetch(&request).await.map_err(|e| e.to_string())?;
            serde_json::to_value(reply).map_err(|e| e.to_string())
        }
        "git.status" | "merge.list" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let worktree = str_arg(args, "worktree")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| super::resolve_worktree(None));
            let mut params = serde_json::Map::new();
            params.insert(
                "worktree".into(),
                json!(worktree.to_string_lossy().to_string()),
            );
            let (method, path, body) = thegn_svc::control::routes::build_call(cap, params)?;
            c.call_raw(method, &path, body)
                .await
                .map_err(|e| e.to_string())
        }
        // Additive writes routed generically through the control catalog.
        // `merge.add` resolves an omitted `worktree` like `merge.list`; the
        // `notify.push` arguments are exactly the validated `PushedNote`
        // caller fields (automation ancestry is never MCP-supplied).
        "merge.add" | "notify.push" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let mut params = args.as_object().cloned().unwrap_or_default();
            if cap == "merge.add" && !params.contains_key("worktree") {
                let worktree = super::resolve_worktree(None);
                params.insert(
                    "worktree".into(),
                    json!(worktree.to_string_lossy().to_string()),
                );
            }
            let (method, path, body) = thegn_svc::control::routes::build_call(cap, params)?;
            c.call_raw(method, &path, body)
                .await
                .map_err(|e| e.to_string())
        }
        "pr.status" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let rows = c.pr_status().await.map_err(|e| e.to_string())?;
            Ok(json!({ "prs": rows }))
        }
        "leases.list" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            c.leases().await.map_err(|e| e.to_string())
        }
        "me" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            c.me().await.map_err(|e| e.to_string())
        }
        "ci.runs" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let worktree = str_arg(args, "worktree")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| super::resolve_worktree(None));
            let limit = args
                .get("limit")
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as usize);
            let reply = c
                .ci_runs(&worktree.to_string_lossy(), limit)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_value(reply).map_err(|e| e.to_string())
        }
        "ci.logs" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let worktree = str_arg(args, "worktree")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| super::resolve_worktree(None));
            let run = str_arg(args, "run").ok_or("missing `run`")?;
            let job = str_arg(args, "job").ok_or("missing `job`")?;
            let tail_lines = args
                .get("tail_lines")
                .and_then(serde_json::Value::as_u64)
                .map(|v| v as usize);
            let reply = c
                .ci_logs(&worktree.to_string_lossy(), run, job, tail_lines)
                .await
                .map_err(|e| e.to_string())?;
            serde_json::to_value(reply).map_err(|e| e.to_string())
        }
        "issues.list" | "issues.get" | "dispatches.list" | "calendar.events"
        | "calendar.clocks"
        // Lead tool surface (THE-2172): every route is POST, so the params
        // become the JSON body unchanged.
        | "overview.get"
        // b163: producer and inbox state (no args).
        | "producers.status"
        | "notifications.list"
        | "notifications.ack"
        | "automations.put"
        | "automations.rm"
        | "automations.enable"
        | "automations.runs"
        | "approvals.list"
        | "approvals.request"
        | "merge.drain"
        // THE-2197: the Lead's kill switch (no args; resume is never MCP).
        | "automations.pause" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let params = args
                .as_object()
                .cloned()
                .ok_or("state tool arguments must be an object")?;
            let (method, path, body) = thegn_svc::control::routes::build_call(cap, params)?;
            c.call_raw(method, &path, body)
                .await
                .map_err(|e| e.to_string())
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
            let timeout_ms = args.get("timeout_ms").and_then(serde_json::Value::as_i64);
            if let Some(w) = super::session::keys::parse_screen_wait(condition_str)
                .map_err(|e| e.to_string())?
            {
                return super::session::keys::wait_screen(&c, session, &w, timeout_ms)
                    .await
                    .map_err(|e| e.to_string());
            }
            let condition =
                super::session::parse_wait_condition(condition_str).map_err(|e| e.to_string())?;
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
        "agent.dispatch" => {
            let (agent, worktree, issue_id, prompt, stage) =
                dispatch.ok_or("agent.dispatch was not admitted")?;
            let started = std::time::Instant::now();
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let (info, dispatch_id) = super::session::open_plain(
                cfg,
                &c,
                super::session::PlainOpen {
                    agent: Some(agent.to_string()),
                    stage: stage.map(str::to_string),
                    worktree: Some(worktree.to_string()),
                    prompt: prompt.to_string(),
                    headless: true,
                    // Unlike `agent run --track`: no worktree agent binding, no
                    // headless provisioning, env pin or CLI preflight.
                    bind: false,
                    adopt: false,
                    track: Some(issue_id),
                    permissions: Vec::new(),
                    widen_permissions: false,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            let elapsed_ms = started.elapsed().as_millis();
            Ok(json!({
                "status": "running",
                "session_id": info.id,
                "dispatch_id": dispatch_id,
                "final_message": null,
                "duration_ms": thegn_core::time_policy::saturating_i64(elapsed_ms),
                "exit_status": null,
            }))
        }
        "sessions.fork" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let spec = thegn_svc::control::ForkSpec {
                session: str_arg(args, "session")
                    .ok_or("missing session")?
                    .to_string(),
                harness: str_arg(args, "harness").map(str::to_string),
                agent: str_arg(args, "agent").map(str::to_string),
                cwd: str_arg(args, "cwd").map(str::to_string),
                worktree: str_arg(args, "worktree").map(str::to_string),
                scrollback: args
                    .get("scrollback")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                tab: args
                    .get("tab")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                adopt: args
                    .get("adopt")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            };
            let info = c.fork(&spec).await.map_err(|e| e.to_string())?;
            serde_json::to_value(&info).map_err(|e| e.to_string())
        }
        "editor.open" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let request = thegn_svc::control::EditorOpenRequest {
                worktree: str_arg(args, "worktree")
                    .ok_or("missing `worktree`")?
                    .to_string(),
                path: str_arg(args, "path").map(str::to_string),
                line: optional_usize_arg(args, "line")?,
                col: optional_usize_arg(args, "col")?,
            };
            // Apply semantic target policy at the MCP boundary as well as at
            // the HTTP server: malformed paths never make a daemon round-trip.
            request.target().map_err(|e| e.to_string())?;
            c.open_editor(&request).await.map_err(|e| e.to_string())?;
            Ok(json!({ "queued": true }))
        }
        "sessions.input" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let session = str_arg(args, "session").ok_or("missing `session`")?;
            let enter = args
                .get("enter")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let payload = input_payload(args)?;
            let (bytes, key_count) = match payload {
                InputPayload::Text { text, paste } => {
                    let bytes = if paste {
                        let (_, rows, cols, ansi) = c
                            .snapshot(session)
                            .await
                            .map_err(|e| e.to_string())?;
                        let modes = super::session::keys::screen_modes(rows, cols, &ansi);
                        super::session::keys::paste_bytes(&text, modes.bracketed_paste)
                            .map_err(|e| e.to_string())?
                    } else {
                        text.into_bytes()
                    };
                    (bytes, None)
                }
                InputPayload::Bytes(bytes) => (bytes, None),
                InputPayload::Keys(keys) => {
                    let (_, rows, cols, ansi) = c
                        .snapshot(session)
                        .await
                        .map_err(|e| e.to_string())?;
                    let modes = super::session::keys::screen_modes(rows, cols, &ansi);
                    let parsed = keys
                        .iter()
                        .map(|key| super::session::keys::parse_key(key).map_err(|e| e.to_string()))
                        .collect::<Result<Vec<_>, _>>()?;
                    let n = parsed.len();
                    let bytes = super::session::keys::encode_keys(&parsed, modes.app_cursor)
                        .map_err(|e| e.to_string())?;
                    (bytes, Some(n))
                }
            };
            let n = bytes.len();
            c.send_input(session, &bytes, enter)
                .await
                .map_err(|e| e.to_string())?;
            let mut answer = json!({ "sent_bytes": n, "session": session, "enter": enter });
            if let Some(key_count) = key_count {
                answer["keys"] = json!(key_count);
            }
            Ok(answer)
        }
        "sessions.kill" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let session = str_arg(args, "session").ok_or("missing `session`")?;
            c.kill(session).await.map_err(|e| e.to_string())?;
            Ok(json!({ "killed": session }))
        }
        // Reversible writes routed generically through the control catalog,
        // under the same `write` scope the HTTP and plugin paths require (no
        // extra interlock: neither sends input nor creates anything on disk).
        // The dispatch-roster writes need `write`, as `POST /v1/dispatches`
        // and `POST /v1/dispatches/{id}/status` do; no extra interlock.
        "sessions.resize"
        | "sessions.split"
        | "worktrees.open"
        | "dispatches.put"
        | "dispatches.set_status" => {
            let params = routed_params(cap, args)?;
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let (method, path, body) = thegn_svc::control::routes::build_call(cap, params)?;
            c.call_raw(method, &path, body)
                .await
                .map_err(|e| e.to_string())
        }
        "sessions.snapshot" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let session = str_arg(args, "session").ok_or("missing `session`")?;
            if session.is_empty()
                || session == "."
                || session == ".."
                || session.contains(['/', '?', '#', '%'])
            {
                return Err("invalid `session` id".into());
            }
            let (seq, rows, cols, ansi) = c.snapshot(session).await.map_err(|e| e.to_string())?;
            let modes = super::session::keys::screen_modes(rows, cols, &ansi);
            Ok(json!({
                "session": session,
                "seq": seq,
                "rows": rows,
                "cols": cols,
                "ansi_b64": base64::engine::general_purpose::STANDARD.encode(&ansi),
                "text": super::session::snapshot_text(rows, cols, &ansi),
                "cursor": { "row": modes.cursor_row, "col": modes.cursor_col },
                "alt_screen": modes.alt_screen,
                "app_cursor": modes.app_cursor,
                "bracketed_paste": modes.bracketed_paste,
            }))
        }
        // Tracker writes: `write`, as `POST /v1/issues/{id}` and its comment
        // route require. `account`/`repo` travel in the query string.
        "issues.update" | "issues.comment" => {
            let (method, path, body) = issue_write_call(cap, args)?;
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            c.call_raw(method, &path, body)
                .await
                .map_err(|e| e.to_string())
        }
        "automations.list" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let rules = c.automations_list().await.map_err(|e| e.to_string())?;
            Ok(json!({ "rules": rules }))
        }
        "automations.test" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let request = thegn_svc::control::AutomationTestRequest {
                rule: str_arg(args, "rule").ok_or("missing `rule`")?.to_string(),
                event: args.get("event").cloned().ok_or("missing `event`")?,
                at: args.get("at").and_then(serde_json::Value::as_i64),
            };
            serde_json::to_value(
                c.automations_test(&request)
                    .await
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())
        }
        "tools.run" => {
            let c = client.map_err(|_| NO_DAEMON.to_string())?;
            let request = thegn_svc::control::ToolRunRequest {
                name: str_arg(args, "name").ok_or("missing `name`")?.to_string(),
                worktree: str_arg(args, "worktree").map(str::to_string),
                automation_origin: None,
            };
            serde_json::to_value(c.tools_run(&request).await.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown state capability `{other}`")),
    }
}

/// A tool call's string argument by name (the JSON-RPC `arguments` object,
/// already validated against the tool's schema — this just extracts).
fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

fn optional_usize_arg(args: &serde_json::Value, key: &str) -> Result<Option<usize>, String> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let n = value
        .as_u64()
        .ok_or_else(|| format!("`{key}` must be a non-negative integer"))?;
    usize::try_from(n)
        .map(Some)
        .map_err(|_| format!("`{key}` is too large"))
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
        resume: str_arg(args, "resume").map(str::to_string),
        // The transport-retry observer is the only continue_last caller (THE-86);
        // the MCP tool surface stays resume/cold.
        continue_last: false,
        stage: str_arg(args, "stage").map(str::to_string),
        permissions: Vec::new(),
        widen_permissions: false,
        fork: false,
        native_session_id: None,
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
        automation_origin: None,
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

fn host_install_choice(
    unsafe_flag: bool,
    interactive: bool,
) -> Result<crate::managed_tool::HostInstall> {
    if unsafe_flag && !interactive {
        bail!(
            "--unsafe-host-install requires an interactive terminal; connect a terminal and rerun the install"
        );
    }
    Ok(if unsafe_flag {
        crate::managed_tool::HostInstall::AllowUnsafe
    } else {
        crate::managed_tool::HostInstall::Refuse
    })
}

fn install(cfg: &Config, name: &str, unsafe_host_install: bool) -> Result<()> {
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
    let choice = host_install_choice(
        unsafe_host_install,
        std::io::IsTerminal::is_terminal(&std::io::stdin()),
    )?;
    crate::managed_tool::install_with(&tool, false, choice)?;
    outln!("installed `{name}` → {}", tool.bin_path().display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::allowed_state_caps;
    use super::explain_key;
    use super::host_install_choice;
    use super::input_payload;
    use super::issue_write_call;
    use super::routed_params;
    use thegn_core::control::ScopeSet;

    const READ_CAPS: &[&str] = &[
        "sessions.list",
        "worktrees.list",
        "worktrees.sandbox_profile",
        "preview.fetch",
        "git.status",
        "merge.list",
        "leases.list",
        "me",
        "pr.status",
        "ci.runs",
        "ci.logs",
        "calendar.events",
        "calendar.clocks",
        "issues.list",
        "issues.get",
        "dispatches.list",
        "agent.sessions",
        "sessions.wait",
        "sessions.snapshot",
        "semantic.map",
        "semantic.blast_radius",
        "automations.list",
        "automations.test",
        // Lead tool surface (THE-2172).
        "overview.get",
        "notifications.list",
        "automations.runs",
        "approvals.list",
        // b163.
        "producers.status",
    ];

    /// The Lead's write-scoped MCP tools (THE-2172).
    const LEAD_WRITE_CAPS: &[&str] = &[
        "notifications.ack",
        "automations.put",
        "automations.rm",
        "automations.enable",
        "approvals.request",
        "automations.pause",
    ];

    #[test]
    fn unsafe_host_install_requires_a_terminal() {
        assert!(matches!(
            host_install_choice(true, false),
            Err(error) if error.to_string().contains("--unsafe-host-install")
        ));
        assert_eq!(
            host_install_choice(true, true).unwrap(),
            crate::managed_tool::HostInstall::AllowUnsafe
        );
        assert_eq!(
            host_install_choice(false, false).unwrap(),
            crate::managed_tool::HostInstall::Refuse
        );
        assert_eq!(
            host_install_choice(false, true).unwrap(),
            crate::managed_tool::HostInstall::Refuse
        );
    }

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
        assert_eq!(
            sorted(allowed_state_caps(ScopeSet::parse("read"), false)),
            sorted(READ_CAPS.to_vec()),
            "--scopes read"
        );
        assert_eq!(READ_CAPS.len(), 28);
    }

    #[test]
    fn mcp_scope_mapping_write_adds_open_and_kill_but_not_input() {
        let allowed = allowed_state_caps(ScopeSet::parse("write"), false);
        assert!(allowed.contains(&"sessions.open"), "{allowed:?}");
        assert!(allowed.contains(&"sessions.kill"), "{allowed:?}");
        assert!(allowed.contains(&"editor.open"), "{allowed:?}");
        assert!(!allowed.contains(&"sessions.input"), "{allowed:?}");
        assert!(!allowed.contains(&"tools.run"), "{allowed:?}");
        // read-scope tools are still covered (write implies read).
        for cap in READ_CAPS {
            assert!(allowed.contains(cap), "{allowed:?} missing {cap}");
        }
    }

    #[test]
    fn read_plus_git_adds_exactly_folder_assignment_and_merge_add() {
        // `git` grants merge enqueue alongside folder assignment.
        let mut expected = READ_CAPS.to_vec();
        expected.push("folders.assign");
        expected.push("merge.add");
        expected.push("merge.drain");
        assert_eq!(
            sorted(allowed_state_caps(ScopeSet::parse("read,git"), false)),
            sorted(expected),
            "--scopes read,git"
        );
    }

    #[test]
    fn lead_read_scope_exposes_only_the_lead_reads() {
        let read = allowed_state_caps(ScopeSet::parse("read"), true);
        for cap in [
            "overview.get",
            "notifications.list",
            "automations.runs",
            "approvals.list",
        ] {
            assert!(read.contains(&cap), "{read:?} missing {cap}");
        }
        for cap in LEAD_WRITE_CAPS
            .iter()
            .chain(&["merge.drain", "approvals.resolve"])
        {
            assert!(!read.contains(cap), "--scopes read must not grant {cap}");
        }
    }

    #[test]
    fn lead_write_scope_adds_ack_put_rm_enable_and_request() {
        let write = allowed_state_caps(ScopeSet::parse("write"), false);
        for cap in LEAD_WRITE_CAPS {
            assert!(write.contains(cap), "{write:?} missing {cap}");
            assert!(!READ_CAPS.contains(cap), "{cap} must not be read-scoped");
        }
        assert!(!write.contains(&"merge.drain"), "{write:?}");
    }

    #[test]
    fn merge_drain_is_gated_exactly_like_merge_add() {
        for scopes in ["read", "write", "merge_add", "exec", "read,write,exec"] {
            let allowed = allowed_state_caps(ScopeSet::parse(scopes), true);
            assert!(!allowed.contains(&"merge.drain"), "--scopes {scopes}");
        }
        // A naked merge_add grant is a worktree-bound route-to-host scope;
        // unbound MCP callers need git, as for merge.add.
        assert!(allowed_state_caps(ScopeSet::parse("merge_add"), false).is_empty());
        for scopes in ["git", "admin"] {
            let allowed = allowed_state_caps(ScopeSet::parse(scopes), false);
            assert!(allowed.contains(&"merge.drain"), "--scopes {scopes}");
            assert!(allowed.contains(&"merge.add"), "--scopes {scopes}");
        }
    }

    #[test]
    fn automations_resume_is_never_listed_on_any_scope() {
        // THE-2197: resume is Admin and http+cli only; pause needs write.
        for scopes in ["none", "read", "write", "git", "merge_add", "exec", "admin"] {
            let allowed = allowed_state_caps(ScopeSet::parse(scopes), true);
            assert!(
                !allowed.contains(&"automations.resume"),
                "--scopes {scopes}"
            );
        }
        assert!(!allowed_state_caps(ScopeSet::parse("read"), true).contains(&"automations.pause"));
        assert!(allowed_state_caps(ScopeSet::parse("write"), false).contains(&"automations.pause"));
    }

    #[test]
    fn approvals_resolve_is_never_listed_on_any_scope() {
        for scopes in ["none", "read", "write", "git", "merge_add", "exec", "admin"] {
            let allowed = allowed_state_caps(ScopeSet::parse(scopes), true);
            assert!(!allowed.contains(&"approvals.resolve"), "--scopes {scopes}");
        }
    }

    #[test]
    fn merge_add_and_notify_push_are_write_tools_never_read() {
        for cap in ["merge.add", "notify.push"] {
            assert!(!READ_CAPS.contains(&cap), "{cap} must not be read-scoped");
            assert!(
                !allowed_state_caps(ScopeSet::parse("read"), true).contains(&cap),
                "--scopes read must not grant {cap}"
            );
        }
        let write = allowed_state_caps(ScopeSet::parse("write"), false);
        assert!(write.contains(&"notify.push"), "{write:?}");
        assert!(!write.contains(&"merge.add"), "{write:?}");
        // A naked merge_add grant is a worktree-bound route-to-host scope; unbound MCP callers need git (parity with HTTP/plugin).
        assert!(allowed_state_caps(ScopeSet::parse("merge_add"), false).is_empty());
        let git = allowed_state_caps(ScopeSet::parse("git"), false);
        assert!(git.contains(&"merge.add"), "{git:?}");
        assert!(!git.contains(&"notify.push"), "{git:?}");
        let both = allowed_state_caps(ScopeSet::parse("write,git"), false);
        assert!(both.contains(&"merge.add") && both.contains(&"notify.push"));
    }

    #[test]
    fn sessions_resize_and_worktrees_open_need_write_like_http_and_plugins() {
        for cap in ["sessions.resize", "worktrees.open"] {
            assert!(!READ_CAPS.contains(&cap), "{cap} must not be read-scoped");
            for scopes in ["read", "git", "merge_add", "read,git", "exec"] {
                assert!(
                    !allowed_state_caps(ScopeSet::parse(scopes), true).contains(&cap),
                    "--scopes {scopes} must not grant {cap}"
                );
            }
            // Write alone grants both; unlike sessions.input there is no
            // MCP-only interlock (HTTP and plugin host.call require write only).
            assert!(
                allowed_state_caps(ScopeSet::parse("write"), false).contains(&cap),
                "--scopes write must grant {cap}"
            );
            assert!(
                allowed_state_caps(ScopeSet::parse("read,write"), true).contains(&cap),
                "{cap}"
            );
        }
    }

    #[test]
    fn sessions_snapshot_is_read_and_split_needs_write() {
        assert!(READ_CAPS.contains(&"sessions.snapshot"));
        assert!(!READ_CAPS.contains(&"sessions.split"));
        // Snapshot: read alone grants it, with or without the input interlock.
        for flag in [false, true] {
            assert!(
                allowed_state_caps(ScopeSet::parse("read"), flag).contains(&"sessions.snapshot")
            );
        }
        assert!(!allowed_state_caps(ScopeSet::parse("none"), false).contains(&"sessions.snapshot"));
        // Split: write only, no MCP-only interlock (it sends no input).
        for scopes in ["read", "git", "merge_add", "read,git", "exec"] {
            assert!(
                !allowed_state_caps(ScopeSet::parse(scopes), true).contains(&"sessions.split"),
                "--scopes {scopes} must not grant sessions.split"
            );
        }
        for (scopes, flag) in [("write", false), ("read,write", true)] {
            assert!(allowed_state_caps(ScopeSet::parse(scopes), flag).contains(&"sessions.split"));
        }
    }

    #[test]
    fn snapshot_and_split_params_fill_the_route_path_and_validate_the_body() {
        let params = routed_params(
            "sessions.snapshot",
            &serde_json::json!({ "session": "a/b" }),
        )
        .unwrap();
        let (method, path, body) =
            thegn_svc::control::routes::build_call("sessions.snapshot", params).unwrap();
        assert_eq!(method, "GET");
        assert_eq!(path, "/v1/sessions/a%2Fb/snapshot");
        assert!(body.is_none());
        assert!(routed_params("sessions.snapshot", &serde_json::json!({})).is_err());

        let args = serde_json::json!({
            "session": "s1", "dir": "down", "argv": ["sh"], "cwd": "/w",
            "env": { "A": "1" }, "rows": 30, "cols": 100
        });
        let params = routed_params("sessions.split", &args).unwrap();
        let (method, path, body) =
            thegn_svc::control::routes::build_call("sessions.split", params).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/sessions/s1/split");
        assert_eq!(
            body.unwrap(),
            serde_json::json!({
                "dir": "down", "argv": ["sh"], "cwd": "/w",
                "env": [["A", "1"]], "rows": 30, "cols": 100
            })
        );
        // Only `session` is required.
        assert!(routed_params("sessions.split", &serde_json::json!({ "session": "s" })).is_ok());
        for bad in [
            serde_json::json!({ "session": "s", "dir": "left" }),
            serde_json::json!({ "session": "s", "rows": 1001 }),
            serde_json::json!({ "session": "s", "cols": -1 }),
            serde_json::json!({ "session": "s", "env": { "A": 1 } }),
            serde_json::json!({ "dir": "down" }),
        ] {
            assert!(routed_params("sessions.split", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn input_payload_enforces_exactly_one_source_and_key_limit() {
        assert!(input_payload(&serde_json::json!({})).is_err());
        assert!(
            input_payload(&serde_json::json!({
                "text": "hello",
                "bytes_b64": "aGVsbG8="
            }))
            .is_err()
        );
        assert!(
            input_payload(&serde_json::json!({
                "text": "hello",
                "keys": ["enter"]
            }))
            .is_err()
        );

        let text = input_payload(&serde_json::json!({
            "text": "hello",
            "paste": true
        }))
        .unwrap();
        assert!(matches!(
            text,
            super::InputPayload::Text { text, paste: true } if text == "hello"
        ));
        assert!(
            input_payload(&serde_json::json!({
                "bytes_b64": "aGVsbG8=",
                "paste": true
            }))
            .is_err()
        );
        assert!(
            input_payload(&serde_json::json!({
                "keys": ["enter"],
                "paste": true
            }))
            .is_err()
        );

        let too_many = serde_json::Value::Array(
            (0..=super::super::session::keys::MAX_KEYS)
                .map(|_| serde_json::json!("a"))
                .collect(),
        );
        assert!(input_payload(&serde_json::json!({ "keys": too_many })).is_err());
    }

    #[test]
    fn dispatches_put_and_set_status_need_write_like_http() {
        for cap in ["dispatches.put", "dispatches.set_status"] {
            assert!(!READ_CAPS.contains(&cap), "{cap} must not be read-scoped");
            for scopes in ["none", "read", "git", "merge_add", "read,git", "exec"] {
                assert!(
                    !allowed_state_caps(ScopeSet::parse(scopes), true).contains(&cap),
                    "--scopes {scopes} must not grant {cap}"
                );
            }
            // Write alone grants both; no MCP-only interlock.
            for (scopes, flag) in [("write", false), ("read,write", true)] {
                assert!(
                    allowed_state_caps(ScopeSet::parse(scopes), flag).contains(&cap),
                    "--scopes {scopes} must grant {cap}"
                );
            }
        }
    }

    #[test]
    fn dispatch_params_fill_the_route_path_and_pass_the_body_through() {
        let args = serde_json::json!({
            "issue_id": "THE-203", "worktree_path": "/w", "agent_name": "coder",
            "stage": "code", "parent_id": 7
        });
        let params = routed_params("dispatches.put", &args).unwrap();
        let (method, path, body) =
            thegn_svc::control::routes::build_call("dispatches.put", params).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/dispatches");
        assert_eq!(body.unwrap(), args);

        let lead = serde_json::json!({ "lease": "pipeline", "owner": "lead-1", "lease_gen": 3 });
        let args = serde_json::json!({ "id": 42, "status": "done", "lead": lead });
        let params = routed_params("dispatches.set_status", &args).unwrap();
        let (method, path, body) =
            thegn_svc::control::routes::build_call("dispatches.set_status", params).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/dispatches/42/status");
        assert_eq!(
            body.unwrap(),
            serde_json::json!({ "status": "done", "lead": lead })
        );
        // `lead` is optional; an unknown status passes through to the daemon.
        let params = routed_params(
            "dispatches.set_status",
            &serde_json::json!({ "id": 42, "status": "no_such_status" }),
        )
        .unwrap();
        let (_, path, body) =
            thegn_svc::control::routes::build_call("dispatches.set_status", params).unwrap();
        assert_eq!(path, "/v1/dispatches/42/status");
        assert_eq!(
            body.unwrap(),
            serde_json::json!({ "status": "no_such_status" })
        );

        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "status": "done" }),
            serde_json::json!({ "id": 0, "status": "done" }),
            serde_json::json!({ "id": -1, "status": "done" }),
            serde_json::json!({ "id": "x", "status": "done" }),
        ] {
            assert!(
                routed_params("dispatches.set_status", &bad).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            routed_params(
                "dispatches.set_status",
                &serde_json::json!({ "id": 0, "status": "done" })
            )
            .unwrap_err(),
            "`id` must be a positive integer"
        );
    }

    #[test]
    fn issue_writes_need_write_like_http() {
        for cap in ["issues.update", "issues.comment"] {
            assert!(!READ_CAPS.contains(&cap), "{cap} must not be read-scoped");
            for scopes in ["none", "read", "git", "merge_add", "read,git", "exec"] {
                assert!(
                    !allowed_state_caps(ScopeSet::parse(scopes), true).contains(&cap),
                    "--scopes {scopes} must not grant {cap}"
                );
            }
            for (scopes, flag) in [("write", false), ("read,write", true)] {
                assert!(
                    allowed_state_caps(ScopeSet::parse(scopes), flag).contains(&cap),
                    "--scopes {scopes} must grant {cap}"
                );
            }
        }
    }

    #[test]
    fn issue_update_maps_to_the_patch_body_and_lifts_account_into_the_query() {
        let args = serde_json::json!({
            "id": "linear:T-1", "status": "in_progress", "assignee": "me",
            "priority": 1, "title": "New", "account": "work acct", "repo": "/r"
        });
        let (method, path, body) = issue_write_call("issues.update", &args).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/issues/linear%3AT-1?repo=%2Fr&account=work+acct");
        assert_eq!(
            body.unwrap(),
            serde_json::json!({
                "status": "in_progress", "assignee_me": true,
                "priority": "urgent", "title": "New"
            })
        );
        // The body deserializes as the daemon's IssuePatch.
        let none = serde_json::json!({ "id": "x:1", "assignee": "none", "priority": 0 });
        let (_, _, body) = issue_write_call("issues.update", &none).unwrap();
        let patch: thegn_core::issue::IssuePatch = serde_json::from_value(body.unwrap()).unwrap();
        assert_eq!(patch.assignee_me, Some(false));
        assert_eq!(patch.priority, Some(thegn_core::issue::IssuePriority::None));
        for (n, name) in [(2, "high"), (3, "medium"), (4, "low")] {
            let args = serde_json::json!({ "id": "x:1", "priority": n });
            let (_, _, body) = issue_write_call("issues.update", &args).unwrap();
            assert_eq!(body.unwrap()["priority"], name);
        }
        let bad_priority = serde_json::json!({ "id": "x:1", "priority": 9 });
        assert!(issue_write_call("issues.update", &bad_priority).is_err());
        let bad_assignee = serde_json::json!({ "id": "x:1", "assignee": "bob" });
        assert!(issue_write_call("issues.update", &bad_assignee).is_err());
    }

    #[test]
    fn issue_comment_maps_to_the_comment_route() {
        let args = serde_json::json!({ "id": "linear:T-1", "body": "hello" });
        let (method, path, body) = issue_write_call("issues.comment", &args).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/issues/linear%3AT-1/comment");
        assert_eq!(body.unwrap(), serde_json::json!({ "body": "hello" }));
        let args = serde_json::json!({ "id": "linear:T-1", "body": "b", "account": "w" });
        let (_, path, body) = issue_write_call("issues.comment", &args).unwrap();
        assert_eq!(path, "/v1/issues/linear%3AT-1/comment?account=w");
        assert_eq!(body.unwrap(), serde_json::json!({ "body": "b" }));
    }

    #[test]
    fn resize_params_fill_the_route_path_and_open_params_mirror_the_body() {
        let args = serde_json::json!({ "session": "a/b", "rows": 40, "cols": 120 });
        let params = routed_params("sessions.resize", &args).unwrap();
        let (method, path, body) =
            thegn_svc::control::routes::build_call("sessions.resize", params).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/sessions/a%2Fb/resize");
        assert_eq!(
            body.unwrap(),
            serde_json::json!({ "rows": 40, "cols": 120 })
        );

        for bad in [
            serde_json::json!({ "session": "s", "rows": 65_536, "cols": 80 }),
            serde_json::json!({ "session": "s", "rows": 24, "cols": -1 }),
            serde_json::json!({ "session": "s", "rows": 24, "cols": 1001 }),
            serde_json::json!({ "session": "s", "rows": 1001, "cols": 80 }),
            serde_json::json!({ "rows": 24, "cols": 80 }),
        ] {
            assert!(routed_params("sessions.resize", &bad).is_err(), "{bad}");
        }

        let at_bound = serde_json::json!({ "session": "s", "rows": 1000, "cols": 1000 });
        assert!(routed_params("sessions.resize", &at_bound).is_ok());
        let err = routed_params(
            "sessions.resize",
            &serde_json::json!({ "session": "s", "rows": 24, "cols": 1001 }),
        )
        .unwrap_err();
        assert_eq!(err, "`cols` must be an integer from 0 to 1000");

        let args = serde_json::json!({ "repo": "/repo", "branch": "main" });
        let params = routed_params("worktrees.open", &args).unwrap();
        let (method, path, body) =
            thegn_svc::control::routes::build_call("worktrees.open", params).unwrap();
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/worktrees/open");
        assert_eq!(body.unwrap(), args);
    }

    #[test]
    fn sandbox_profile_set_is_never_an_mcp_tool() {
        for flag in [false, true] {
            let allowed = allowed_state_caps(ScopeSet::parse("read,write,git,exec,admin"), flag);
            assert!(
                allowed.contains(&"worktrees.sandbox_profile"),
                "{allowed:?}"
            );
            assert!(
                !allowed.contains(&"worktrees.sandbox_profile_set"),
                "{allowed:?}"
            );
        }
    }

    #[test]
    fn folder_assignment_is_git_scoped_on_mcp() {
        for csv in ["read", "write"] {
            for flag in [false, true] {
                assert!(
                    !allowed_state_caps(ScopeSet::parse(csv), flag).contains(&"folders.assign"),
                    "--scopes {csv} (input flag {flag}) must not grant folders.assign"
                );
            }
        }
        assert!(allowed_state_caps(ScopeSet::parse("git"), false).contains(&"folders.assign"));
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
            sorted(allowed_state_caps(ScopeSet::parse("write,git,exec"), true)),
            sorted(thegn_core::mcp::state::MCP_STATE_CAPS.to_vec()),
        );
    }

    #[test]
    fn mcp_explain_config_redacts_effective_values_and_every_trace_layer() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
[[calendar.accounts]]
name = "work"
provider = "ics_url"
url = "https://cal.example.test/feed.ics?sig=mcp-url-sentinel-1a2"
username = "mcp-user-sentinel-3b4"
command = ["khal-thegn", "--auth", "mcp-argv-sentinel-5c6"]
env = { KHAL_AUTH = "mcp-env-sentinel-7d8" }

[issues.linear]
api_key = "mcp-token-sentinel-9e0"
"#,
        )
        .expect("write config");

        let calendar = explain_key("calendar.accounts.0", path.clone());
        let token = explain_key("issues.linear.api_key", path);
        let output = format!("{calendar}{token}");
        for sentinel in [
            "mcp-url-sentinel-1a2",
            "mcp-user-sentinel-3b4",
            "mcp-argv-sentinel-5c6",
            "mcp-env-sentinel-7d8",
            "mcp-token-sentinel-9e0",
        ] {
            assert!(!output.contains(sentinel), "leaked {sentinel}: {output}");
        }
        assert!(calendar.contains("work"), "{calendar}");
        assert!(calendar.contains("khal-thegn"), "{calendar}");
        assert!(calendar.contains("KHAL_AUTH"), "{calendar}");
        assert!(token.contains(thegn_core::redact::PLACEHOLDER), "{token}");
    }

    #[test]
    fn agent_dispatch_admission_refuses_before_the_daemon() {
        let cfg = thegn_core::config::Config::default();
        let ok = serde_json::json!({
            "agent": "worker",
            "worktree": "/tmp/not-a-worktree",
            "issue_id": "linear:THE 2586",
            "prompt": "private task text"
        });
        let err = super::agent_dispatch_admit(&cfg, &ok, false).expect_err("bad identity");
        assert!(err.contains("invalid pipeline issue id"), "got {err}");
        assert!(!err.contains("private task text"), "got {err}");

        let relative = serde_json::json!({
            "agent": "worker",
            "worktree": "relative/wt",
            "issue_id": "linear:THE-2586",
            "prompt": "task"
        });
        let err = super::agent_dispatch_admit(&cfg, &relative, false).expect_err("relative");
        assert!(err.contains("absolute"), "got {err}");

        // A scoped-token caller (the Lead) is refused before anything else.
        let err = super::agent_dispatch_admit(&cfg, &ok, true).expect_err("token");
        assert!(err.contains("scoped control token"), "got {err}");
    }

    #[test]
    fn agent_dispatch_is_not_listed_over_a_scoped_token() {
        let write = allowed_state_caps(ScopeSet::parse("write"), false);
        assert!(write.contains(&"agent.dispatch"));
        let local = super::without_local_orchestration(write.clone(), false);
        assert!(local.contains(&"agent.dispatch"));
        let token = super::without_local_orchestration(write, true);
        assert!(!token.contains(&"agent.dispatch"));
        assert!(token.contains(&"sessions.open"));
    }
}
