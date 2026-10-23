//! `thegn forward <action>` — inspect auto port forwards (`[forward]`).
//!
//! Forwarding itself is automatic and lives in the running compositor: a
//! detector watches the active worktree's sandbox and the host brings forwards
//! up/down. This non-interactive surface just reads the persisted records (so
//! scripts/another shell can see what's forwarded) and removes a stale record.

use anyhow::{Context, Result};
use thegn_core::db::Db;
use thegn_core::outln;
use thegn_core::store::WorktreeAuxStore;

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List active port forwards recorded in the DB.
    List {
        /// Emit one JSON array instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Remove a recorded forward for a worktree's container port.
    Stop {
        /// The container (sandbox-internal) port whose forward to drop.
        container_port: u16,
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(json),
        Action::Stop {
            container_port,
            target,
        } => stop(container_port, target.worktree),
    }
}

fn list(json: bool) -> Result<()> {
    let db = Db::open().context("forward: could not open the state DB")?;
    let rows = db
        .list_forwards()
        .context("forward: could not read the recorded forwards")?;
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        outln!("no forwards");
        return Ok(());
    }
    for r in &rows {
        let mapping = if r.host_port == r.container_port {
            format!("{}", r.container_port)
        } else {
            format!("{} → {}", r.container_port, r.host_port)
        };
        outln!("{:<14} {}", mapping, r.url);
    }
    Ok(())
}

fn stop(container_port: u16, worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree).to_string_lossy().into_owned();
    let db = Db::open().context("forward: could not open the state DB")?;
    match stop_in(&db, &wt, container_port)? {
        StopOutcome::Removed => {
            outln!("forward: removed record for container port {container_port}");
            outln!("forward: a running thegn re-detects live ports; this only clears the record");
        }
        StopOutcome::NotRecorded => {
            outln!(
                "forward: no recorded forward for container port {container_port} in {wt}; nothing was removed"
            );
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum StopOutcome {
    Removed,
    NotRecorded,
}

fn stop_in(db: &Db, worktree: &str, container_port: u16) -> Result<StopOutcome> {
    let recorded = db
        .list_forwards()
        .context("forward: could not read the recorded forwards")?
        .iter()
        .any(|row| row.worktree == worktree && row.container_port == container_port);
    if !recorded {
        return Ok(StopOutcome::NotRecorded);
    }
    db.delete_forward(worktree, container_port)?;
    Ok(StopOutcome::Removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reports_a_missing_record_without_claiming_removal() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("forward.db")).unwrap();

        assert_eq!(stop_in(&db, "/wt", 3000).unwrap(), StopOutcome::NotRecorded);
    }

    #[test]
    fn stop_removes_a_recorded_forward() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("forward.db")).unwrap();
        db.upsert_forward("/wt", 3000, 8001, "http://127.0.0.1:8001")
            .unwrap();

        assert_eq!(stop_in(&db, "/wt", 3000).unwrap(), StopOutcome::Removed);
        assert!(db.list_forwards().unwrap().is_empty());
        assert_eq!(stop_in(&db, "/wt", 3000).unwrap(), StopOutcome::NotRecorded);
    }
}
