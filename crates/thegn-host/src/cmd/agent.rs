//! `thegn agent <action>` — coding-agent introspection over the harness seam.
//!
//! `list` is the compact "what will actually run" view — one line per
//! `[[agents]]`/`[[tools]]` entry and per pipeline stage with its effective
//! harness, model, env keys and permission count (the `agent.list`
//! capability). Written for agents to read: terse by default, `--json` for
//! scripts, no process launched and no secret value printed (env is keys only).
//!
//! `sessions` lists the sessions each configured harness has recorded locally
//! (the `agent.sessions` capability): a bounded, read-on-demand filesystem scan
//! that never launches a harness, spends tokens, or reveals credential material.
//! It answers locally without a running daemon — the same read-on-demand shape
//! the MCP tool and the HTTP route serve.
//!
//! `run` spawns a headless agent into a worktree in one command and, with
//! `--wait`, blocks until it exits and prints its output tail (THE-2005; the
//! logic lives in [`super::agent_spawn`]).
//!
//! `result` is the read-only companion for a run started without `--wait`: it
//! reports the session's status and an output tail without waiting for exit.

use anyhow::Result;
use std::collections::HashSet;
use std::fmt::Write as _;
use thegn_core::config::Config;
use thegn_core::outln;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// Lifecycle-hook entry point written by `thegn mcp wire` into Claude's
    /// and Codex's settings: reports a finished turn (`stop`) or a request
    /// for you (`attention`) to the daemon over the control socket, so the
    /// sidebar dot and attention queue are exact. Always exits 0 and prints
    /// nothing: a hook must never disturb the agent.
    #[command(hide = true)]
    Hook {
        /// The agent kind calling the hook (`claude`, `codex`).
        agent: String,
        /// `stop` (turn finished) or `attention` (needs you); anything else
        /// is ignored.
        event: String,
        /// Trailing payload (Codex appends its event JSON here).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        payload: Vec<String>,
    },
    /// What each agent entry and pipeline stage actually launches as: harness,
    /// model, env keys, permission count (compact; `--json` for scripts).
    List {
        /// Emit a JSON object `{agents: [...], stages: [...]}` instead of lines.
        #[arg(long)]
        json: bool,
    },
    /// List discovered coding-agent sessions (harness, id, worktree, summary).
    Sessions {
        /// Only sessions whose recorded working dir is this worktree.
        #[arg(long)]
        worktree: Option<String>,
        /// Only sessions from this harness id (`claude`, `codex`, …).
        #[arg(long)]
        harness: Option<String>,
        /// Emit a JSON array instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Spawn a headless agent into a worktree in one command: the brief is
    /// resolved, the session opens headless and bound to the worktree (its
    /// env/sandbox; an unbound worktree is first pinned to `pipeline`), and
    /// with `--wait` the command blocks until the agent exits and prints the
    /// ANSI-stripped tail of its output (last 200 non-blank lines). Exit 0
    /// when the agent exits 0, 1 on any other exit code, 2 on `--timeout`
    /// (the session is left running).
    Run {
        /// An `[[agents]]`/`[[tools]]` name, or a provider id (`claude`,
        /// `codex`) when no entry is named that.
        #[arg(long)]
        agent: String,
        /// The worktree to launch into (path). The agent runs here.
        #[arg(long)]
        worktree: String,
        /// Run in this `[env.<name>]` (pins the worktree like `env set`).
        /// Without it, a worktree with no env selection is pinned to
        /// `pipeline`.
        #[arg(long, value_name = "ENV")]
        env: Option<String>,
        /// Read the task from this file (a template when `--var` is given).
        #[arg(long, required_unless_present = "prompt", conflicts_with = "prompt")]
        brief_file: Option<String>,
        /// The task, inline.
        #[arg(long)]
        prompt: Option<String>,
        /// Bind `KEY=VALUE` in `--brief-file` (repeatable).
        // clap skips `requires` when the required arg conflicts with a present
        // one (`--brief-file` conflicts with `--prompt`), so refuse the
        // `--prompt` pairing explicitly too.
        #[arg(
            long,
            value_name = "KEY=VALUE",
            requires = "brief_file",
            conflicts_with = "prompt"
        )]
        var: Vec<String>,
        /// Track the run in the dispatch roster under this issue id
        /// (`linear:THE-2005`). Optional: ad-hoc runs have no issue.
        #[arg(long, value_name = "ISSUE")]
        track: Option<String>,
        /// Block until the agent exits, then print its output tail.
        #[arg(long)]
        wait: bool,
        /// After the agent exits, fetch the commits made in a provider
        /// sandbox back into the worktree branch (like `env pull`); adds
        /// `pull: {ok, ref, error}` to `--json`. Needs `--wait`.
        #[arg(long, requires = "wait")]
        pull: bool,
        /// Fly/machine0: leave the machine running after the agent exits
        /// (by default it is stopped, since it bills while started). Needs
        /// `--wait`.
        #[arg(long, requires = "wait")]
        keep_running: bool,
        /// Seconds `--wait` waits before giving up (exit 2; the session keeps
        /// running).
        #[arg(
            long,
            value_name = "SECS",
            requires = "wait",
            default_value_t = super::agent_spawn::DEFAULT_TIMEOUT_SECS
        )]
        timeout: u64,
        /// Emit JSON: `{session, row, env}`, or with `--wait`
        /// `{session, row, env, status, exit_code, output_tail, duration_ms}`
        /// (`env` is the env pinned by this run, or null).
        #[arg(long)]
        json: bool,
    },
    /// Report on an agent session without waiting: its status (`running`,
    /// `exited`, `unknown`) and the output tail, by dispatch row or session
    /// id. Exit 0 when the agent exited 0, 1 on another exit code, 3 while it
    /// is still running, 2 when the session is unknown or no daemon answers.
    /// `thegn dispatch wait --row` blocks instead.
    #[command(group(
        clap::ArgGroup::new("target").required(true).multiple(false).args(["row", "session"])
    ))]
    Result {
        /// The dispatch row whose session to report on.
        #[arg(long, conflicts_with = "session")]
        row: Option<i64>,
        /// The session id to report on.
        #[arg(long, conflicts_with = "row")]
        session: Option<String>,
        /// Emit JSON `{row, session, status, exit_code, output_tail, tail_note}`.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::Hook {
            agent,
            event,
            payload,
        } => {
            hook(cfg, &agent, &event, &payload);
            Ok(())
        }
        Action::List { json } => list(cfg, json),
        Action::Sessions {
            worktree,
            harness,
            json,
        } => sessions(cfg, worktree.as_deref(), harness.as_deref(), json),
        Action::Run {
            agent,
            worktree,
            env,
            brief_file,
            prompt,
            var,
            track,
            wait,
            pull,
            keep_running,
            timeout,
            json,
        } => super::agent_spawn::run(
            cfg,
            super::agent_spawn::RunArgs {
                agent,
                worktree,
                env,
                brief_file,
                prompt,
                vars: var,
                track,
                wait,
                pull,
                keep_running,
                timeout_secs: timeout,
                json,
            },
        ),
        Action::Result { row, session, json } => {
            super::agent_spawn::result(cfg, super::agent_spawn::ResultArgs { row, session, json })
        }
    }
}

/// The notification a hook event becomes: `(title, urgency)`; `None` for an
/// event thegn does not report. `attention` is `alert` urgency, which the
/// daemon files as `agent_attention` (the blocked tier); a finished turn is
/// the normal `agent_done` notice.
pub(crate) fn hook_note(
    agent: &str,
    event: &str,
    message: &str,
) -> Option<(String, Option<&'static str>)> {
    match event {
        "stop" => Some((format!("{agent} finished its turn"), None)),
        "attention" => {
            let title = if message.is_empty() {
                format!("{agent} needs you")
            } else {
                format!("{agent}: {message}")
            };
            Some((title, Some("alert")))
        }
        _ => None,
    }
}

/// The source ref a hook push carries: the daemon reads the worktree after
/// the `@` (see `notify_push`), since the wire note has no worktree field.
pub(crate) fn hook_source(agent: &str, worktree: &str) -> String {
    format!("agent-hook:{agent}@{worktree}")
}

/// Pull `cwd` and `message` out of a hook's JSON payload (Claude: stdin).
pub(crate) fn hook_payload_fields(raw: &str) -> (Option<String>, String) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (None, String::new());
    };
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    (text("cwd"), text("message").unwrap_or_default())
}

/// Best-effort: every failure (no daemon, bad payload) is swallowed.
fn hook(cfg: &Config, agent: &str, event: &str, payload: &[String]) {
    use std::io::{IsTerminal, Read};
    let mut raw = payload.last().cloned().unwrap_or_default();
    if raw.is_empty() && agent == "claude" && !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        if let Err(error) = std::io::stdin().take(64 * 1024).read_to_string(&mut buf) {
            // A hook must never fail the agent: an unreadable payload is just empty.
            tracing::debug!(target: "thegn::agent", %error, "agent hook: payload read failed");
        }
        raw = buf;
    }
    let (cwd, message) = hook_payload_fields(&raw);
    let Some((title, urgency)) = hook_note(agent, event, &message) else {
        return;
    };
    let dir = cwd
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    let worktree = dir
        .as_deref()
        .map(|d| {
            thegn_core::repo::worktree_root_for_cwd(d)
                .unwrap_or_else(|| d.to_path_buf())
                .display()
                .to_string()
        })
        .unwrap_or_default();
    let note = thegn_svc::control::PushedNote {
        title,
        body: String::new(),
        urgency: urgency.map(str::to_string),
        source: Some(hook_source(agent, &worktree)),
        automation_origin: None,
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    if let Err(error) = rt.block_on(hook_push(cfg, &note)) {
        // Best-effort: a hook with no reachable daemon must not fail the agent.
        tracing::debug!(target: "thegn::agent", error = %format!("{error:#}"), "agent hook: push failed");
    }
}

async fn hook_push(cfg: &Config, note: &thegn_svc::control::PushedNote) -> Result<()> {
    let client = super::session::connect(cfg).await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), client.notify_push(note))
        .await
        .map_err(|_| anyhow::anyhow!("daemon did not answer"))??;
    Ok(())
}

/// One resolved row of `agent list`: an entry (`stage = None`) or a stage.
#[derive(serde::Serialize)]
pub(crate) struct EffectiveRow {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    harness: String,
    model: Option<String>,
    env_keys: Vec<String>,
    permissions: usize,
    backends: Vec<BackendRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(serde::Serialize)]
struct BackendRow {
    name: String,
    harness: String,
    model: Option<String>,
    env_keys: Vec<String>,
}

/// Pure: the rows `agent list` prints, from config alone.
pub(crate) fn effective_rows(cfg: &Config) -> (Vec<EffectiveRow>, Vec<EffectiveRow>) {
    use thegn_core::agent_task::effective_agent;
    let row = |name: &str, agent: Option<&str>, stage: Option<&str>| {
        let target = agent.unwrap_or(name);
        let backends = cfg
            .agents
            .iter()
            .chain(cfg.tools.iter())
            .find(|entry| entry.name == target)
            .map(|entry| {
                entry
                    .backends
                    .iter()
                    .map(|backend| BackendRow {
                        name: backend.name.clone(),
                        harness: backend.harness.clone(),
                        model: backend.model.clone(),
                        env_keys: backend.env.keys().cloned().collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        match effective_agent(cfg, target, stage) {
            Ok(e) => EffectiveRow {
                name: name.to_string(),
                agent: agent.map(str::to_string),
                harness: e.harness,
                model: e.model,
                env_keys: e.env.keys().cloned().collect(),
                permissions: e.permissions.len(),
                backends,
                error: None,
            },
            Err(why) => EffectiveRow {
                name: name.to_string(),
                agent: agent.map(str::to_string),
                harness: String::new(),
                model: None,
                env_keys: Vec::new(),
                permissions: 0,
                backends,
                error: Some(why),
            },
        }
    };
    let agents = cfg
        .agents
        .iter()
        .chain(cfg.tools.iter())
        // The plain shell is not a harness launch; listing it is noise.
        .filter(|a| a.command != "__shell__")
        .map(|a| row(&a.name, None, None))
        .collect();
    let stages = cfg
        .pipeline
        .stages
        .iter()
        .filter_map(|s| {
            s.stage_name()
                .map(|n| row(n, Some(s.agent.trim()), Some(n)))
        })
        .collect();
    (agents, stages)
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    let (agents, stages) = effective_rows(cfg);
    if json {
        return super::emit_json(&serde_json::json!({ "agents": agents, "stages": stages }));
    }
    let line = |r: &EffectiveRow, via: &str| {
        if let Some(e) = &r.error {
            return format!("{}{via}  INVALID: {e}", r.name);
        }
        let mut s = format!(
            "{}{via}  {}  {}",
            r.name,
            r.harness,
            r.model.as_deref().unwrap_or("-")
        );
        if !r.env_keys.is_empty() {
            write!(s, "  env:{}", r.env_keys.join(",")).expect("writing to String cannot fail");
        }
        if r.permissions > 0 {
            write!(s, "  perms:{}", r.permissions).expect("writing to String cannot fail");
        }
        if !r.backends.is_empty() {
            let names = r
                .backends
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>();
            write!(s, "  backends:{}", names.join(",")).expect("writing to String cannot fail");
        }
        s
    };
    if agents.is_empty() {
        outln!("no [[agents]] configured");
    }
    for r in &agents {
        outln!("{}", line(r, ""));
    }
    if !stages.is_empty() {
        outln!("stages:");
        for r in &stages {
            let via = r
                .agent
                .as_deref()
                .map(|a| format!(" ({a})"))
                .unwrap_or_default();
            outln!("  {}", line(r, &via));
        }
    }
    Ok(())
}

fn sessions(cfg: &Config, worktree: Option<&str>, harness: Option<&str>, json: bool) -> Result<()> {
    // The tracked-worktree set is only for the `unlinked` flag; a DB miss
    // degrades to "all unlinked" rather than failing the listing.
    let known: HashSet<String> = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| {
            use thegn_core::store::WorkspaceStore;
            db.worktrees().ok()
        })
        .map(|rows| rows.into_iter().map(|r| r.worktree).collect())
        .unwrap_or_default();

    let filter = thegn_svc::sessions::SessionFilter { worktree, harness };
    let recs = thegn_svc::sessions::discover(cfg, &filter, &known);

    if json {
        // One emitter: a single JSON array of the discovered records.
        super::emit_json(&recs)?;
    } else if recs.is_empty() {
        outln!("no agent sessions discovered");
    } else {
        for r in &recs {
            let wt = r.worktree.as_deref().unwrap_or("-");
            let flag = if r.unlinked { " (unlinked)" } else { "" };
            outln!("{:<10}  {:<30}  {wt}{flag}  {}", r.harness, r.id, r.summary);
        }
    }
    Ok(())
}

#[cfg(test)]
mod hook_tests {
    use super::*;

    #[test]
    fn stop_is_a_done_notice_and_attention_is_an_alert() {
        assert_eq!(
            hook_note("claude", "stop", "ignored"),
            Some(("claude finished its turn".to_string(), None))
        );
        assert_eq!(
            hook_note("claude", "attention", "Allow Bash?"),
            Some(("claude: Allow Bash?".to_string(), Some("alert")))
        );
        assert_eq!(
            hook_note("codex", "attention", ""),
            Some(("codex needs you".to_string(), Some("alert")))
        );
        assert_eq!(hook_note("claude", "working", ""), None);
    }

    #[test]
    fn payload_fields_read_cwd_and_message_and_tolerate_garbage() {
        let (cwd, msg) = hook_payload_fields(r#"{"cwd":"/w/a","message":"hi","x":1}"#);
        assert_eq!(cwd.as_deref(), Some("/w/a"));
        assert_eq!(msg, "hi");
        assert_eq!(hook_payload_fields("not json"), (None, String::new()));
        assert_eq!(hook_payload_fields(""), (None, String::new()));
    }

    #[test]
    fn source_carries_the_worktree_after_the_at() {
        assert_eq!(hook_source("claude", "/w/a"), "agent-hook:claude@/w/a");
    }
}
