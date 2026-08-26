//! `thegn agent <action>` — coding-agent introspection over the harness seam.
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
        Action::Sessions {
            worktree,
            harness,
            json,
        } => sessions(cfg, worktree.as_deref(), harness.as_deref(), json),
    }
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
