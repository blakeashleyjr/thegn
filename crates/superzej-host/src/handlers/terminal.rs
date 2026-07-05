//! New-terminal creation glue extracted from `run.rs` (pinned by the file-size
//! ratchet). The wizard UI lives in [`crate::terminal_wizard`]; the loop handles
//! submit inline (it spawns the pane via run-private helpers), delegating only
//! the DB write here.

use superzej_core::store::WorkspaceStore;

use crate::terminal_wizard::TerminalChoice;

/// Persist a terminal from the wizard: upsert the row (keyed by unique name) and
/// record its sandbox backend when local. Best-effort — the DB is a cache; a
/// failed write just means the row isn't remembered across restarts, the live
/// session pane still spawns.
pub(crate) fn persist(choice: &TerminalChoice) {
    let Ok(db) = superzej_core::db::Db::open() else {
        return;
    };
    // best-effort: DB is a cache; git/session is the source of truth for panes.
    let _ = db.put_terminal(&choice.name, &choice.kind, &choice.connection, None);
    if !choice.sandbox.is_empty() && choice.sandbox != "host" && choice.sandbox != "none" {
        let _ = db.set_terminal_sandbox(&choice.name, &choice.sandbox);
    }
}
