//! `superzej forward <action>` — inspect auto port forwards (`[forward]`).
//!
//! Forwarding itself is automatic and lives in the running compositor: a
//! detector watches the active worktree's sandbox and the host brings forwards
//! up/down. This non-interactive surface just reads the persisted records (so
//! scripts/another shell can see what's forwarded) and removes a stale record.

use anyhow::Result;
use superzej_core::db::Db;
use superzej_core::outln;
use superzej_core::store::WorktreeAuxStore;

use crate::cmd::resolve_worktree;

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List active port forwards recorded in the DB.
    List,
    /// Remove a recorded forward for a worktree's container port.
    Stop {
        /// The container (sandbox-internal) port whose forward to drop.
        container_port: u16,
        #[arg(long)]
        worktree: Option<String>,
    },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::List => list(),
        Action::Stop {
            container_port,
            worktree,
        } => stop(container_port, worktree),
    }
}

fn list() -> Result<()> {
    let Ok(db) = Db::open() else {
        outln!("forward: could not open the state DB");
        return Ok(());
    };
    let rows = db.list_forwards().unwrap_or_default();
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
    let Ok(db) = Db::open() else {
        outln!("forward: could not open the state DB");
        return Ok(());
    };
    db.delete_forward(&wt, container_port)?;
    outln!("forward: removed record for container port {container_port}");
    outln!("forward: a running superzej re-detects live ports; this only clears the record");
    Ok(())
}
