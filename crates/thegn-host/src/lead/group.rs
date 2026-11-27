//! The Lead's reserved terminal group (THE-2171). There is no `GroupKind`
//! variant: the Lead is a `terminals` registry row with `kind = "lead"`.
//!
//! Filled by b152 lane 7: row lookup, the jump plan, and the jump glue.

use crate::sidebar::RowTarget;
use crate::terminal_wizard::TerminalChoice;
use thegn_core::models::TerminalRow;

/// The reserved terminal name of the Lead group.
pub(crate) const LEAD_GROUP_NAME: &str = "lead";

/// The `terminals.kind` value that marks the Lead group.
pub(crate) const LEAD_TERMINAL_KIND: &str = "lead";

/// Whether a terminal group name is the reserved Lead group.
pub(crate) fn is_lead_group(name: &str) -> bool {
    name == LEAD_GROUP_NAME
}

/// Whether a sidebar activation targets the Lead, either as a resident tab or
/// through the global-terminal sentinel. Mouse activation uses this to share
/// the jump key's setup, creation, and focus path.
pub(crate) fn is_lead_target(target: &RowTarget, session: &crate::session::Session) -> bool {
    match target {
        RowTarget::Tab(gi, _) => session.worktrees.get(*gi).is_some_and(|group| {
            group.kind == crate::session::GroupKind::Terminal && is_lead_group(&group.name)
        }),
        RowTarget::Workspace { repo_path, group } => {
            repo_path == "terminal" && group.as_deref().is_some_and(is_lead_group)
        }
    }
}

/// The Lead's registry row, if one has been persisted.
pub(crate) fn lead_row(db_terminals: &[TerminalRow]) -> Option<&TerminalRow> {
    db_terminals
        .iter()
        .find(|t| t.kind == LEAD_TERMINAL_KIND && t.name == LEAD_GROUP_NAME)
}

/// The choice that registers the Lead terminal.
pub(crate) fn lead_choice() -> TerminalChoice {
    TerminalChoice {
        name: LEAD_GROUP_NAME.into(),
        kind: LEAD_TERMINAL_KIND.into(),
        connection: String::new(),
        sandbox: LEAD_GROUP_NAME.into(),
    }
}

/// What the jump key should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JumpPlan {
    /// Lead is unconfigured, unavailable, or not ready: open in-app setup.
    Disabled,
    /// The Lead exists (resident or registered): activate this target.
    Activate(RowTarget),
    /// No Lead yet: persist the registry row and push a fresh group.
    Create,
}

/// Pure decision for the jump key.
pub(crate) fn plan(
    cfg: &thegn_core::config::Config,
    session: &crate::session::Session,
    db_terminals: &[TerminalRow],
) -> JumpPlan {
    if !super::enabled(cfg) {
        return JumpPlan::Disabled;
    }
    let host = super::setup::RealHost { cfg };
    plan_with_facts(
        cfg,
        session,
        db_terminals,
        &super::setup::Host::facts(&host),
    )
}

fn plan_with_facts(
    cfg: &thegn_core::config::Config,
    session: &crate::session::Session,
    db_terminals: &[TerminalRow],
    facts: &super::setup::Facts,
) -> JumpPlan {
    if !super::enabled(cfg)
        || (!facts.dev_channel && !facts.file_preview)
        || !facts.bwrap
        || !facts.pasta
    {
        return JumpPlan::Disabled;
    }
    // A registered or already-open Lead can outlive the daemon listener it
    // needs (for example after `[lead]` was added to an already-running
    // daemon). Route the jump through setup before activating a tab that can
    // only fail at its MCP sandbox step.
    if super::setup::incomplete(facts) {
        return JumpPlan::Disabled;
    }
    if let Some(i) = session
        .worktrees
        .iter()
        .position(|w| w.name == LEAD_GROUP_NAME && w.kind == crate::session::GroupKind::Terminal)
    {
        return JumpPlan::Activate(RowTarget::Tab(i, session.worktrees[i].active_tab));
    }
    if lead_row(db_terminals).is_some() {
        return JumpPlan::Activate(RowTarget::Workspace {
            repo_path: "terminal".into(),
            group: Some(LEAD_GROUP_NAME.into()),
        });
    }
    JumpPlan::Create
}

/// The run.rs arm's one-liner. Returns a target for the caller to activate
/// through its normal row door, or `None` when it handled the jump itself
/// (disabled/unready: the caller opens the setup; created: group pushed and registry row queued, the
/// caller still refreshes/persists its layout via `created`).
pub(crate) fn jump(
    cfg: &thegn_core::config::Config,
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    db_terminals: &[TerminalRow],
    tx: &tokio::sync::mpsc::UnboundedSender<crate::run::run_pending_db::PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) -> JumpOutcome {
    if !super::enabled(cfg) {
        return JumpOutcome::Setup;
    }
    // Readiness applies to resident and registered groups too. Otherwise a
    // click on an existing Lead row can bypass setup and land on a failed tab.
    let host = super::setup::RealHost { cfg };
    if super::setup::incomplete(&super::setup::Host::facts(&host)) {
        return JumpOutcome::Setup;
    }
    match plan(cfg, session, db_terminals) {
        JumpPlan::Disabled => JumpOutcome::Setup,
        JumpPlan::Activate(t) => JumpOutcome::Activate(t),
        JumpPlan::Create => {
            let choice = lead_choice();
            crate::handlers::terminal::persist(&choice, tx, waker);
            crate::handlers::terminal::push_terminal_group(session, panes, &choice);
            JumpOutcome::Created
        }
    }
}

/// Retry after the setup flow has waited for `thegn daemon stop` to finish.
/// The normal jump checks the old listener to avoid a doomed launch; this
/// path intentionally skips that stale-listener check because creating or
/// restoring the terminal is what starts the daemon on its next attach.
pub(crate) fn jump_after_restart(
    cfg: &thegn_core::config::Config,
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    tx: &tokio::sync::mpsc::UnboundedSender<crate::run::run_pending_db::PendingDbResult>,
    waker: &termwiz::terminal::TerminalWaker,
) -> JumpOutcome {
    if !super::enabled(cfg) || super::disabled_reason(cfg).is_some() {
        return JumpOutcome::Setup;
    }
    let choice = lead_choice();
    crate::handlers::terminal::persist(&choice, tx, waker);
    reset_lead_group(session, panes, &choice);
    JumpOutcome::Created
}

/// Replace an existing Lead group after its daemon sessions have been stopped,
/// or create one if it was only present in the registry.
fn reset_lead_group(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    choice: &TerminalChoice,
) {
    // `daemon stop` ends the sessions hosted by the old daemon. Reusing the
    // old Lead tabs would only focus dead PTYs, so install a fresh placeholder
    // that starts a new Lead on attach. Keep the group's sidebar position.
    let existing = session
        .worktrees
        .iter()
        .position(|w| w.name == LEAD_GROUP_NAME && w.kind == crate::session::GroupKind::Terminal);
    if let Some(i) = existing {
        session.worktrees.remove(i);
        crate::handlers::terminal::push_terminal_group(session, panes, choice);
        let fresh = session
            .worktrees
            .pop()
            .expect("the Lead group was just pushed");
        session.worktrees.insert(i, fresh);
        session.active = i;
    } else {
        crate::handlers::terminal::push_terminal_group(session, panes, choice);
    }
}

/// Whether activating `target` is the sidebar's "Lead — set up" row: the Lead
/// group's target while no `[lead]` is configured.
pub(crate) fn is_setup_target(target: &RowTarget, cfg: &thegn_core::config::Config) -> bool {
    (!super::enabled(cfg) || super::disabled_reason(cfg).is_some())
        && matches!(
            target,
            RowTarget::Workspace { repo_path, group: Some(g) }
                if repo_path == "terminal" && is_lead_group(g)
        )
}

/// Result of [`jump`] for the caller.
pub(crate) enum JumpOutcome {
    /// The Lead is not set up (or not ready): open the in-app setup flow.
    Setup,
    /// Activate this row target.
    Activate(RowTarget),
    /// A new group was pushed: refresh the model, persist the layout, relayout.
    Created,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str) -> TerminalRow {
        TerminalRow {
            id: 0,
            name: name.into(),
            kind: kind.into(),
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

    #[test]
    fn lead_row_finds_only_the_reserved_row() {
        assert!(lead_row(&[row("local", "local")]).is_none());
        assert!(lead_row(&[row("other", "lead")]).is_none());
        let rows = [row("local", "local"), row("lead", "lead")];
        assert_eq!(lead_row(&rows).map(|r| r.name.as_str()), Some("lead"));
    }

    #[test]
    fn restart_replaces_dead_lead_tabs_with_a_fresh_terminal() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut panes = crate::panes::Panes::new(tx);
        let old = crate::session::WorktreeGroup::terminal(LEAD_GROUP_NAME);
        let old_id = old.tabs[0].focused_pane;
        let mut session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![
                crate::session::WorktreeGroup::new(
                    "home",
                    crate::session::GroupKind::Home,
                    "/home",
                ),
                old,
            ],
            active: 0,
        };

        reset_lead_group(&mut session, &mut panes, &lead_choice());

        assert_eq!(session.active, 1);
        assert_eq!(session.worktrees[1].name, LEAD_GROUP_NAME);
        assert_eq!(session.worktrees[1].tabs.len(), 1);
        assert_ne!(session.worktrees[1].tabs[0].focused_pane, old_id);
    }

    #[test]
    fn plan_is_disabled_without_lead_config() {
        let cfg = thegn_core::config::Config::default();
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![],
            active: 0,
        };
        let facts = crate::lead::setup::Facts {
            dev_channel: true,
            file_preview: true,
            agents: vec![],
            file_agent: Some("claude".into()),
            file_host_network: false,
            daemon_enabled: true,
            bwrap: true,
            pasta: true,
            listener_up: false,
            logged_in: true,
            mcp_servers: vec![],
        };
        assert_eq!(
            plan_with_facts(&cfg, &session, &[], &facts),
            JumpPlan::Disabled
        );
    }

    #[test]
    fn plan_routes_an_existing_lead_without_a_listener_to_setup() {
        let cfg = thegn_core::config::Config {
            lead: Some(thegn_core::config_lead::LeadConfig::default()),
            ..Default::default()
        };
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![crate::session::WorktreeGroup::terminal("lead")],
            active: 0,
        };
        let facts = crate::lead::setup::Facts {
            dev_channel: true,
            file_preview: true,
            agents: vec![],
            file_agent: Some("claude".into()),
            file_host_network: false,
            daemon_enabled: true,
            bwrap: true,
            pasta: true,
            listener_up: false,
            logged_in: true,
            mcp_servers: vec![],
        };
        assert_eq!(
            plan_with_facts(&cfg, &session, &[], &facts),
            JumpPlan::Disabled
        );
    }

    #[test]
    fn ready_lead_entry_resolves_to_its_tab_from_another_group() {
        let cfg = thegn_core::config::Config {
            lead: Some(thegn_core::config_lead::LeadConfig::default()),
            ..Default::default()
        };
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![
                crate::session::WorktreeGroup::new(
                    "project",
                    crate::session::GroupKind::Home,
                    "/project",
                ),
                crate::session::WorktreeGroup::terminal("lead"),
            ],
            active: 0,
        };
        assert_eq!(
            plan(&cfg, &session, &[]),
            JumpPlan::Activate(RowTarget::Tab(1, 0))
        );
        let target = RowTarget::Tab(1, 0);
        assert!(is_lead_target(&target, &session));
        assert_eq!(session.worktrees[1].name, LEAD_GROUP_NAME);
    }

    #[test]
    fn setup_target_is_the_lead_group_only_while_unconfigured() {
        let lead = RowTarget::Workspace {
            repo_path: "terminal".into(),
            group: Some("lead".into()),
        };
        let other = RowTarget::Workspace {
            repo_path: "terminal".into(),
            group: Some("local".into()),
        };
        let off = thegn_core::config::Config::default();
        assert!(is_setup_target(&lead, &off));
        assert!(!is_setup_target(&other, &off));
        assert!(!is_setup_target(&RowTarget::Tab(0, 0), &off));
        let on = thegn_core::config::Config {
            lead: Some(thegn_core::config_lead::LeadConfig::default()),
            ..Default::default()
        };
        assert!(!is_setup_target(&lead, &on));
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![],
            active: 0,
        };
        assert!(is_lead_target(&lead, &session));
    }

    #[test]
    fn failed_lead_start_reentry_resolves_to_the_lead_tab() {
        let cfg = thegn_core::config::Config {
            lead: Some(thegn_core::config_lead::LeadConfig::default()),
            ..Default::default()
        };
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![
                crate::session::WorktreeGroup::new(
                    "project",
                    crate::session::GroupKind::Home,
                    "/project",
                ),
                crate::session::WorktreeGroup::terminal("lead"),
            ],
            active: 0,
        };
        let target = RowTarget::Tab(1, 0);
        // The error recovery click is another activation of the same row; the
        // jump plan resolves to the Lead tab that owns the failed launch.
        assert!(is_lead_target(&target, &session));
        assert_eq!(plan(&cfg, &session, &[]), JumpPlan::Activate(target));
        assert_eq!(session.worktrees[1].name, LEAD_GROUP_NAME);
    }

    #[test]
    fn lead_choice_is_the_reserved_terminal() {
        let c = lead_choice();
        assert_eq!((c.name.as_str(), c.kind.as_str()), ("lead", "lead"));
        assert!(is_lead_group(&c.name));
    }

    #[test]
    fn sidebar_lead_target_covers_resident_and_global_entries_only() {
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![crate::session::WorktreeGroup::terminal("lead")],
            active: 0,
        };
        assert!(is_lead_target(&RowTarget::Tab(0, 0), &session));
        assert!(is_lead_target(
            &RowTarget::Workspace {
                repo_path: "terminal".into(),
                group: Some("lead".into()),
            },
            &session,
        ));
        assert!(!is_lead_target(
            &RowTarget::Workspace {
                repo_path: "terminal".into(),
                group: Some("local".into()),
            },
            &session,
        ));
        assert!(!is_lead_target(
            &RowTarget::Workspace {
                repo_path: "/repo".into(),
                group: Some("lead".into()),
            },
            &session,
        ));
        assert!(!is_lead_target(&RowTarget::Tab(3, 0), &session));
    }
}
