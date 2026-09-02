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

/// Every terminal name already spoken for: the live session's terminal groups
/// UNIONED with the global `terminals` registry (`model.sidebar_db_terminals`,
/// already resident — no DB read on the loop).
///
/// The registry half is not optional. `db.put_terminal` upserts on a globally
/// unique name while a session only ever holds the terminals it happens to have
/// opened, so deduping against the session alone let a "local" created in one
/// project overwrite another project's row — leaving two live groups sharing one
/// name, which makes the by-name lookups that reunite a terminal with its shell
/// (`workspace_pool::take_terminal_group`, `db.terminal_group_tabs`) ambiguous.
pub(crate) fn taken_names(
    session: &Session,
    db_terminals: &[thegn_core::models::TerminalRow],
) -> Vec<String> {
    let mut taken: Vec<String> = db_terminals.iter().map(|t| t.name.clone()).collect();
    for g in &session.worktrees {
        if g.kind == GroupKind::Terminal && !taken.contains(&g.name) {
            taken.push(g.name.clone());
        }
    }
    taken
}

/// Open the new-terminal wizard, seeding it with existing terminal names so its
/// random default slug is deduped (back-to-back creates would otherwise collide).
pub(crate) fn open_wizard(
    cfg: &Config,
    session: &Session,
    db_terminals: &[thegn_core::models::TerminalRow],
) -> TerminalWizard {
    TerminalWizard::new(cfg, &taken_names(session, db_terminals))
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
        let _ = db.set_terminal_sandbox(&choice.name, &choice.sandbox); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
        let _ = db.set_terminal_observed(name, backend); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
    let group = fresh_group(&choice.name, panes);
    let placeholder = group.tabs[0].focused_pane;
    session.worktrees.push(group);
    session.active = session.worktrees.len() - 1;
    placeholder
}

/// A never-opened terminal's group: one `main` tab holding a freshly reserved
/// placeholder leaf for the materialize path to spawn over. See
/// [`push_terminal_group`] for why the placeholder is reserved rather than left
/// at `Tab::new`'s default `Leaf(0)`.
pub(crate) fn fresh_group(name: &str, panes: &mut Panes) -> crate::session::WorktreeGroup {
    let placeholder = panes.reserve_ids(1);
    let mut group = crate::session::WorktreeGroup::terminal(name);
    let tab = &mut group.tabs[0];
    tab.center = crate::center::CenterTree::Leaf(placeholder);
    tab.focused_pane = placeholder;
    group
}

/// Rebuild terminal `name`'s group from the layout it was last persisted under,
/// wherever that was, returning it beside the donor session key.
///
/// This is the COLD half of keeping one terminal to one shell: with no live
/// group anywhere (a fresh launch, or a workspace evicted past
/// `[session] resident_pool_limit`), the persisted tab is the only thing that
/// still knows the terminal's daemon session id. Restoring it means
/// `materialize_with_specs` takes its warm-reattach branch and reconnects the
/// running shell; if that session is gone the relay's `SessionFallback` at least
/// repaints the persisted scrollback tail. Building an empty group instead —
/// what every re-open used to do — throws both away.
///
/// Ids are remapped onto a fresh disjoint range (with all four id-keyed side
/// maps, via [`crate::workspace_pool::remap_group_ids`]) so a persisted id can't
/// alias a live pane of some other resident workspace.
///
/// One indexed SELECT on the loop, on a user-initiated activation only — the
/// same trade `switch_workspace`'s registry re-read already makes.
pub(crate) fn restore_group(
    name: &str,
    panes: &mut Panes,
) -> Option<(String, crate::session::WorktreeGroup)> {
    let db = thegn_core::db::Db::open().ok()?;
    let (donor, grow, rows) = db.terminal_group_tabs(name).ok().flatten()?;
    if rows.is_empty() {
        return None;
    }
    let tabs: Vec<crate::session::Tab> = rows.iter().map(crate::session::Tab::from_row).collect();
    let active_tab = (grow.active_tab.max(0) as usize).min(tabs.len() - 1);
    let mut group = crate::session::WorktreeGroup {
        name: name.to_string(),
        kind: GroupKind::Terminal,
        path: String::new(),
        tabs,
        active_tab,
    };
    crate::workspace_pool::remap_group_ids(&mut group, panes);
    Some((donor, group))
}

/// Drop terminal `name`'s layout rows from the session that used to hold it,
/// after its group has moved into the active one.
///
/// Two sessions both claiming one terminal is how a cold resurrect of the donor
/// would fork a phantom duplicate — a second group with the same name and a
/// stale pane tree, racing the real one for the sidebar row. `write_layout` only
/// clears the session it is writing, so the donor's rows have to be deleted
/// explicitly. Best-effort and off-loop: the DB is a cache, and the worst a lost
/// delete costs is that phantom, which the next persist of the donor clears
/// anyway.
pub(crate) fn forget_layout(donor: String, name: String) {
    crate::db_task::persist(move |db| {
        let _ = db.delete_tab_group(&donor, &name); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    });
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

    fn db_terminal(name: &str) -> thegn_core::models::TerminalRow {
        thegn_core::models::TerminalRow {
            id: 0,
            name: name.to_string(),
            kind: "local".into(),
            connection_string: String::new(),
            folder_id: None,
            created_at: 0,
            last_active: 0,
            position: 0,
            sandbox_backend: String::new(),
            env_name: String::new(),
            observed_backend: String::new(),
        }
    }

    /// The wizard must dedupe against the GLOBAL registry, not just this
    /// session: `put_terminal` upserts on a unique name, so a name only free in
    /// the current project would overwrite another project's row and leave two
    /// live groups sharing it — which the by-name reunion can't disambiguate.
    #[test]
    fn taken_names_unions_the_global_registry_with_the_live_session() {
        let mut session = test_session();
        session
            .worktrees
            .push(crate::session::WorktreeGroup::terminal("live-only"));
        let taken = taken_names(&session, &[db_terminal("db-only"), db_terminal("both")]);

        assert!(
            taken.contains(&"db-only".to_string()),
            "registry name taken"
        );
        assert!(
            taken.contains(&"live-only".to_string()),
            "session name taken"
        );
        assert_eq!(
            taken.iter().filter(|n| *n == "db-only").count(),
            1,
            "no duplicates for `dedupe` to trip over"
        );
        assert!(
            !taken.contains(&"home".to_string()),
            "a worktree group is not a terminal name"
        );
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
