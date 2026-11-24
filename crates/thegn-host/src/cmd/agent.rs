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
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Pure: the rows `agent list` prints, from config alone.
pub(crate) fn effective_rows(cfg: &Config) -> (Vec<EffectiveRow>, Vec<EffectiveRow>) {
    use thegn_core::agent_task::effective_agent;
    let row = |name: &str, agent: Option<&str>, stage: Option<&str>| {
        let target = agent.unwrap_or(name);
        match effective_agent(cfg, target, stage) {
            Ok(e) => EffectiveRow {
                name: name.to_string(),
                agent: agent.map(str::to_string),
                harness: e.harness,
                model: e.model,
                env_keys: e.env.keys().cloned().collect(),
                permissions: e.permissions.len(),
                error: None,
            },
            Err(why) => EffectiveRow {
                name: name.to_string(),
                agent: agent.map(str::to_string),
                harness: String::new(),
                model: None,
                env_keys: Vec::new(),
                permissions: 0,
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
