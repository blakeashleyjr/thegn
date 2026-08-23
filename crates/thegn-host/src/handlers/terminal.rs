//! New-terminal creation glue extracted from `run.rs` (pinned by the file-size
//! ratchet). The wizard UI lives in [`crate::terminal_wizard`]; on submit the
//! loop persists the row (tiny best-effort upsert) and stages a placeholder
//! leaf via [`push_terminal_group`] — the actual spawn resolution (which can
//! open SQLite, shell out to git, and probe sandbox backends) rides the
//! off-thread lazy-materialize path (`handlers::materialize`), never the loop.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use thegn_core::config::Config;
use thegn_core::store::WorkspaceStore;

use crate::panes::Panes;
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
/// record its sandbox backend when local. Best-effort — the DB is a cache and
/// the live spawn reads the in-process choice registry ([`live_choice`]) rather
/// than this row, so a failed write only means the terminal isn't remembered
/// across restarts. Returns `false` (open or upsert failed) so the caller can
/// surface that on the status line instead of swallowing it.
pub(crate) fn persist(choice: &TerminalChoice) -> bool {
    let Ok(db) = thegn_core::db::Db::open() else {
        return false;
    };
    let ok = db
        .put_terminal(&choice.name, &choice.kind, &choice.connection, None)
        .is_ok();
    if !choice.sandbox.is_empty() && choice.sandbox != "host" && choice.sandbox != "none" {
        // best-effort: the sandbox column only matters across restarts; the
        // live session spawns from the registry below.
        let _ = db.set_terminal_sandbox(&choice.name, &choice.sandbox);
    }
    ok
}

/// Record the containment a terminal's launch ACTUALLY entered (argv-derived,
/// from `sandbox_truth`). Written to a column separate from the wizard's pick:
/// the pick must survive so a later launch can honour it once the runtime is
/// running, while every surface displays THIS value. Best-effort — the DB is a
/// cache, and a failed write only costs a chip until the next launch.
pub(crate) fn record_observed(name: &str, backend: &str) {
    if let Ok(db) = thegn_core::db::Db::open() {
        let _ = db.set_terminal_observed(name, backend);
    }
}

/// This session's wizard-submitted `(connection, sandbox)` choices, keyed by
/// the unique terminal name. The off-thread materialize/prewarm workers consult
/// this BEFORE the DB row ([`crate::run::terminal_launch_for`]) so a failed
/// best-effort [`persist`] can never silently downgrade what actually spawns in
/// the live session (e.g. a remote-ssh or sandboxed terminal to a plain host
/// shell). Process-wide for the same reason as `agent::request_force_host`: the
/// consumer runs on `spawn_blocking`, loop turns after the submit arm returned.
fn live_choices() -> &'static Mutex<HashMap<String, (String, String)>> {
    static LIVE: OnceLock<Mutex<HashMap<String, (String, String)>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record `name`'s live `(connection, sandbox)` choice (see [`live_choices`]).
fn remember_choice(name: &str, connection: &str, sandbox: &str) {
    if let Ok(mut m) = live_choices().lock() {
        m.insert(
            name.to_string(),
            (connection.to_string(), sandbox.to_string()),
        );
    }
}

/// The live `(connection, sandbox)` for terminal `name` when this session's
/// wizard created it; `None` for terminals only known from the DB (sidebar
/// activation of a persisted terminal falls back to the DB row).
pub(crate) fn live_choice(name: &str) -> Option<(String, String)> {
    live_choices().lock().ok()?.get(name).cloned()
}

/// Push a fresh Terminal group for a wizard-submitted choice and stage its tab
/// for the lazy materialize path (mirrors the sidebar-activation arm in
/// `handlers::sidebar_activate`): the tab's center becomes a freshly reserved
/// placeholder leaf, which `maybe_materialize` picks up as a missing leaf on
/// the next loop turn and resolves off-thread. The choice is recorded in the
/// live registry first so the worker spawns exactly what the user picked.
///
/// The placeholder comes from `reserve_ids` rather than keeping `Tab::new`'s
/// default `Leaf(0)` — pane id 0 is never allocated (ids start at 1), so
/// `Leaf(0)` would also register as missing and materialize, but the fresh id
/// honors the `reserve_ids` disjoint-range contract and keeps per-tab id maps
/// unambiguous across multiple dormant tabs. Returns the placeholder id.
pub(crate) fn push_terminal_group(
    session: &mut Session,
    panes: &mut Panes,
    choice: &TerminalChoice,
) -> u32 {
    remember_choice(&choice.name, &choice.connection, &choice.sandbox);
    let placeholder = panes.reserve_ids(1);
    let mut group = crate::session::WorktreeGroup::terminal(&choice.name);
    let tab = &mut group.tabs[0];
    tab.center = crate::center::CenterTree::Leaf(placeholder);
    tab.focused_pane = placeholder;
    session.worktrees.push(group);
    session.active = session.worktrees.len() - 1;
    placeholder
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_panes() -> Panes {
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        Panes::new(tx)
    }

    fn choice(name: &str, connection: &str, sandbox: &str) -> TerminalChoice {
        TerminalChoice {
            name: name.to_string(),
            kind: if connection.is_empty() {
                "local".to_string()
            } else {
                "remote".to_string()
            },
            connection: connection.to_string(),
            sandbox: sandbox.to_string(),
        }
    }

    fn test_session() -> Session {
        Session {
            id: "test".to_string(),
            worktrees: vec![crate::session::WorktreeGroup::new(
                "home",
                GroupKind::Home,
                "/tmp/home",
            )],
            active: 0,
        }
    }

    #[test]
    fn push_terminal_group_stages_placeholder_leaf_for_materialize() {
        let mut session = test_session();
        let mut panes = test_panes();
        // Occupy some ids so the placeholder is demonstrably freshly reserved.
        let live = panes.reserve_ids(3);
        panes.insert_test_pane(live);
        let ph = push_terminal_group(
            &mut session,
            &mut panes,
            &choice("push-tg-test", "", "bwrap"),
        );
        assert!(ph > live, "placeholder must be a freshly reserved id");
        assert_eq!(session.active, session.worktrees.len() - 1);
        let g = &session.worktrees[session.active];
        assert_eq!(g.kind, GroupKind::Terminal);
        assert_eq!(g.name, "push-tg-test");
        assert!(g.path.is_empty());
        let tab = &g.tabs[0];
        assert_eq!(tab.center, crate::center::CenterTree::Leaf(ph));
        assert_eq!(tab.focused_pane, ph);
        // The staged leaf is missing ⇒ `maybe_materialize` will pick it up.
        assert_eq!(panes.missing_leaves(tab), vec![ph]);
    }

    #[test]
    fn push_terminal_group_records_live_choice_for_the_worker() {
        let mut session = test_session();
        let mut panes = test_panes();
        push_terminal_group(
            &mut session,
            &mut panes,
            &choice("live-choice-test", "ssh user@host", ""),
        );
        assert_eq!(
            live_choice("live-choice-test"),
            Some(("ssh user@host".to_string(), String::new()))
        );
        assert_eq!(live_choice("live-choice-never-created"), None);
    }

    #[test]
    fn placeholder_ids_stay_disjoint_across_back_to_back_creates() {
        let mut session = test_session();
        let mut panes = test_panes();
        let a = push_terminal_group(&mut session, &mut panes, &choice("disjoint-a", "", ""));
        let b = push_terminal_group(&mut session, &mut panes, &choice("disjoint-b", "", ""));
        assert_ne!(a, b);
        assert_eq!(session.worktrees.len(), 3);
    }
}
