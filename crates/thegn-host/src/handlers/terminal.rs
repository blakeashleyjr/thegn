//! New-terminal creation glue extracted from `run.rs` (pinned by the file-size
//! ratchet). The wizard UI lives in [`crate::terminal_wizard`]; the loop handles
//! submit inline (it spawns the pane via run-private helpers), delegating only
//! the DB write here.

use thegn_core::config::Config;
use thegn_core::store::WorkspaceStore;

use crate::session::{GroupKind, Session};
use crate::terminal_wizard::{TerminalChoice, TerminalWizard};

/// Open the new-terminal wizard, seeding it with existing terminal names so its
/// random default slug is deduped (back-to-back creates would otherwise collide).
pub(crate) fn open_wizard(cfg: &Config, session: &Session) -> TerminalWizard {
    let taken: Vec<String> = session
        .worktrees
        .iter()
        .filter(|g| g.kind == GroupKind::Terminal)
        .map(|g| g.name.clone())
        .collect();
    TerminalWizard::new(cfg, &taken)
}

/// Persist a terminal from the wizard: upsert the row (keyed by unique name) and
/// record its sandbox backend when local. Best-effort — the DB is a cache; a
/// failed write just means the row isn't remembered across restarts, the live
/// session pane still spawns.
pub(crate) fn persist(choice: &TerminalChoice) {
    let Ok(db) = thegn_core::db::Db::open() else {
        return;
    };
    // best-effort: DB is a cache; git/session is the source of truth for panes.
    let _ = db.put_terminal(&choice.name, &choice.kind, &choice.connection, None);
    if !choice.sandbox.is_empty() && choice.sandbox != "host" && choice.sandbox != "none" {
        let _ = db.set_terminal_sandbox(&choice.name, &choice.sandbox);
    }
}
