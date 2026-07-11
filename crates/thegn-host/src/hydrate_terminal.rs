//! Small hydration helpers extracted from `hydrate.rs` (pinned at the file-size
//! cap): the sidebar's terminal list and the active worktree's sandbox backend
//! for the tab-bar `(backend)` chip. Both log on a DB read error instead of
//! swallowing it — a silent failure is how the sidebar/chip went blank.

use thegn_core::config::SandboxBackend;
use thegn_core::db::Db;
use thegn_core::models::TerminalRow;
use thegn_core::store::WorkspaceStore;

/// The terminals to show in the sidebar. On a read error, log and return empty
/// (the section then shows its empty-state hint) rather than silently blanking.
pub(crate) fn sidebar_terminals(db: &Db) -> Vec<TerminalRow> {
    match db.terminals() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(target: "thegn::hydrate", error = %e, "terminals() read failed; sidebar Terminals section will be empty");
            Vec::new()
        }
    }
}

/// The sandbox backend for the active worktree's tab-bar `(backend)` chip: the
/// value a launched pane recorded, else — when the DB has nothing yet — the
/// backend the config resolves to (what a launch WOULD record), so the chip
/// shows the intended sandbox before the first sandboxed pane. `auto`/`none`
/// config resolves to empty, matching a plain local worktree. Logs on error.
pub(crate) fn active_backend(db: &Db, path: &str, cfg_backend: SandboxBackend) -> String {
    match db.worktree_sandbox(path) {
        Ok(Some(b)) if !b.trim().is_empty() => b,
        Ok(_) => thegn_core::sandbox::Backend::from_config(cfg_backend)
            .filter(|b| *b != thegn_core::sandbox::Backend::None)
            .map(|b| b.label().to_string())
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!(target: "thegn::hydrate", error = %e, "worktree_sandbox() read failed; location chip may be blank");
            String::new()
        }
    }
}
