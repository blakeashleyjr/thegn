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
    /// `[lead]` is absent or the channel is stable: open the in-app setup.
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
    match plan(cfg, session, db_terminals) {
        JumpPlan::Disabled => JumpOutcome::Setup,
        JumpPlan::Activate(t) => JumpOutcome::Activate(t),
        JumpPlan::Create => {
            // Configured but not logged in, or the daemon's Lead listener is
            // not up: a Lead opened now would only fail, so finish setup first.
            let host = super::setup::RealHost { cfg };
            if super::setup::incomplete(&super::setup::Host::facts(&host)) {
                return JumpOutcome::Setup;
            }
            let choice = lead_choice();
            crate::handlers::terminal::persist(&choice, tx, waker);
            crate::handlers::terminal::push_terminal_group(session, panes, &choice);
            JumpOutcome::Created
        }
    }
}

/// Whether activating `target` is the sidebar's "Lead — set up" row: the Lead
/// group's target while no `[lead]` is configured.
pub(crate) fn is_setup_target(target: &RowTarget, cfg: &thegn_core::config::Config) -> bool {
    !super::enabled(cfg)
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
    fn plan_is_disabled_without_lead_config() {
        let cfg = thegn_core::config::Config::default();
        let session = crate::session::Session {
            id: "s".into(),
            worktrees: vec![],
            active: 0,
        };
        assert_eq!(plan(&cfg, &session, &[]), JumpPlan::Disabled);
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
    }

    #[test]
    fn lead_choice_is_the_reserved_terminal() {
        let c = lead_choice();
        assert_eq!((c.name.as_str(), c.kind.as_str()), ("lead", "lead"));
        assert!(is_lead_group(&c.name));
    }
}
