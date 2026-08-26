//! `thegn dispatch <action>` — the durable agent-dispatch roster (THE-57).
//!
//! The roster (`agent_dispatches`) is the ledger a supervisor agent resumes
//! from after a crash: which issue is being worked in which worktree by which
//! agent, and each row's status. These verbs read and advance it directly
//! against the local SQLite cache (no daemon needed), the same way `thegn wt
//! list` / `thegn merge` read their tables. The status column is a **closed,
//! parseable set** ([`thegn_core::issue::AgentDispatchStatus`]); `set-status`
//! writes it through the typed value, and `list` coerces any legacy/unknown
//! stored string to `unknown` rather than failing the read.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::AgentDispatchStatus;
use thegn_core::outln;
use thegn_core::store::NotificationStore;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List the dispatch roster (newest first).
    List {
        /// Emit JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Advance one dispatch's status.
    SetStatus {
        /// The dispatch row id (see `dispatch list`).
        id: i64,
        /// A member of the closed set: queued | spawning | running |
        /// waiting_human | pr_open | merged | abandoned | done | failed.
        status: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn run(_cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(json),
        Action::SetStatus { id, status, json } => set_status(id, &status, json),
    }
}

fn list(json: bool) -> Result<()> {
    let db = Db::open()?;
    let rows = db.list_dispatches()?;
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        outln!("no dispatches");
        return Ok(());
    }
    for d in &rows {
        outln!(
            "{}  {}  {}  {}  {}",
            d.id,
            d.status.as_str(),
            d.issue_id,
            d.agent_name,
            d.worktree_path,
        );
    }
    Ok(())
}

fn set_status(id: i64, status: &str, json: bool) -> Result<()> {
    // Reject an unparseable status up front — writing `Unknown` back would
    // corrupt the roster the fix exists to protect. `Unknown` is a read-only
    // coercion, never a target a caller may set.
    let parsed = AgentDispatchStatus::parse(status);
    if parsed == AgentDispatchStatus::Unknown {
        anyhow::bail!(
            "unknown dispatch status {status:?} (expected one of: queued, spawning, running, \
             waiting_human, pr_open, merged, abandoned, done, failed)"
        );
    }
    let db = Db::open()?;
    if db.get_dispatch(id)?.is_none() {
        anyhow::bail!("no dispatch with id {id}");
    }
    db.update_dispatch_status(id, parsed)?;
    if json {
        return super::emit_json(&serde_json::json!({ "id": id, "status": parsed.as_str() }));
    }
    outln!("dispatch {id} → {}", parsed.as_str());
    Ok(())
}
