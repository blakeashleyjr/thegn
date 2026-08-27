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
//!
//! `put` appends a row, including the v56 pipeline columns
//! (`stage`/`parent`/`session`/`artifact`). Those are **structure, not
//! judgment**: thegn stores, groups and renders them, and no code path here
//! advances a stage — that is the supervising agent's call.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::issue::{AgentDispatch, AgentDispatchStatus, NewDispatch};
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
    /// Append a row to the roster: this agent is working this issue in this
    /// worktree. The pipeline columns (`--stage`/`--parent`/`--session`/
    /// `--artifact`) are the supervising agent's own bookkeeping — thegn stores
    /// and renders them and never advances a stage itself.
    Put {
        /// Tracker issue id (`"<provider>:<key>"`, e.g. `linear:THE-57`).
        issue_id: String,
        /// The worktree the agent works in (path).
        worktree_path: String,
        /// An `[[agents]]`/`[[tools]]` name (or a provider id).
        agent_name: String,
        /// The `[[pipeline.stages]]` step this row is (e.g. `architect`).
        #[arg(long)]
        stage: Option<String>,
        /// The roster row this one was chunked out of (see `dispatch list`).
        #[arg(long)]
        parent: Option<i64>,
        /// The daemon session running it (see `thegn session open`), so a pane
        /// exit stamps THIS row and not a sibling stage's.
        #[arg(long)]
        session: Option<String>,
        /// Path to the handoff artifact committed in the worktree.
        #[arg(long)]
        artifact: Option<String>,
        /// Emit the created row as JSON.
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
        Action::Put {
            issue_id,
            worktree_path,
            agent_name,
            stage,
            parent,
            session,
            artifact,
            json,
        } => {
            let db = Db::open()?;
            let row = put(
                &db,
                NewDispatch {
                    issue_id: &issue_id,
                    worktree_path: &worktree_path,
                    agent_name: &agent_name,
                    stage: stage.as_deref(),
                    parent_id: parent,
                    session_id: session.as_deref(),
                    artifact_path: artifact.as_deref(),
                },
            )?;
            if json {
                return super::emit_json(&row);
            }
            outln!("dispatch {} → {}", row.id, row.status.as_str());
            Ok(())
        }
        Action::SetStatus { id, status, json } => set_status(id, &status, json),
    }
}

/// Insert one roster row and read it back. Split from the clap arm (and taking
/// an explicit `&Db`) so the insert is testable against an isolated database —
/// `Db::open()` would hit the developer's live state.
fn put(db: &Db, new: NewDispatch<'_>) -> Result<AgentDispatch> {
    // A parent must exist. Nothing enforces it in SQL (the roster is a
    // cache-side ledger, not a foreign-key graph), so a typo would otherwise
    // produce a chunk row silently orphaned from the board.
    if let Some(parent) = new.parent_id
        && db.get_dispatch(parent)?.is_none()
    {
        anyhow::bail!("no dispatch with id {parent} to parent this row on");
    }
    let id = db.put_agent_dispatch(new)?;
    db.get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("dispatch {id} vanished after insert"))
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
        // The pipeline columns print as `-` when absent rather than collapsing
        // the row's shape, so the table stays column-aligned for a roster that
        // mixes pipeline and plain dispatches.
        outln!(
            "{}  {}  {}  {}  {}  {}  {}",
            d.id,
            d.status.as_str(),
            d.stage.as_deref().unwrap_or("-"),
            d.parent_id
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".into()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::NotificationStore;

    fn db(name: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join(format!("{name}.db"))).unwrap();
        (dir, db)
    }

    #[test]
    fn put_records_the_pipeline_columns_and_reads_the_row_back() {
        let (_d, db) = db("put-cols");
        let lead = put(&db, NewDispatch::new("linear:A-1", "/wt/a", "claude")).unwrap();
        assert_eq!(lead.status, AgentDispatchStatus::Queued);
        assert_eq!(lead.stage, None, "a plain dispatch carries no stage");

        let chunk = put(
            &db,
            NewDispatch {
                issue_id: "linear:A-1",
                worktree_path: "/wt/a",
                agent_name: "coder",
                stage: Some("code"),
                parent_id: Some(lead.id),
                session_id: Some("sess-7"),
                artifact_path: Some(".thegn/pipeline/architect/1.md"),
            },
        )
        .unwrap();
        assert_eq!(chunk.stage.as_deref(), Some("code"));
        assert_eq!(chunk.parent_id, Some(lead.id));
        assert_eq!(chunk.session_id.as_deref(), Some("sess-7"));
        assert_eq!(
            chunk.artifact_path.as_deref(),
            Some(".thegn/pipeline/architect/1.md")
        );
        // And the roster read carries them (the columns move together).
        let listed = db.list_dispatches().unwrap();
        assert_eq!(listed[0], chunk);
    }

    #[test]
    fn put_rejects_a_parent_that_does_not_exist() {
        let (_d, db) = db("put-parent");
        let mut new = NewDispatch::new("linear:A-1", "/wt/a", "coder");
        new.parent_id = Some(4242);
        let err = put(&db, new).unwrap_err().to_string();
        assert!(err.contains("4242"), "{err}");
        assert!(
            db.list_dispatches().unwrap().is_empty(),
            "a rejected parent must not leave an orphan row"
        );
    }
}
