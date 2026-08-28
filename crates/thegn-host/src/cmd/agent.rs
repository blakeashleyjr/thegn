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

use anyhow::Result;
use std::collections::HashSet;
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
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(cfg, json),
        Action::Sessions {
            worktree,
            harness,
            json,
        } => sessions(cfg, worktree.as_deref(), harness.as_deref(), json),
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
            s.push_str(&format!("  env:{}", r.env_keys.join(",")));
        }
        if r.permissions > 0 {
            s.push_str(&format!("  perms:{}", r.permissions));
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
