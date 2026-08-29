//! PR-review handoff policy.
//!
//! Selection and live-pane discovery stay separate from rendering.  The live
//! pane search is deliberately scoped to the active worktree's own tabs and
//! checks the actual foreground process rather than the focused pane.

use thegn_core::agent_task::{TaskKind, TaskVars};
use thegn_core::config::Config;
use thegn_core::review::{PrReviewSnapshot, format_review_feedback};

use crate::panes::Panes;
use crate::session::{Session, Tab};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewSelection {
    Selected(String),
    AllUnresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneTarget {
    Live(u32),
    Headless { command: String },
    None,
}

pub(crate) fn feedback(snapshot: &PrReviewSnapshot, selection: &ReviewSelection) -> String {
    match selection {
        ReviewSelection::Selected(id) => snapshot
            .conversation
            .threads
            .iter()
            .find(|thread| thread.id == *id)
            .map(|thread| format_review_feedback(snapshot, Some(thread)))
            .unwrap_or_default(),
        ReviewSelection::AllUnresolved => format_review_feedback(snapshot, None),
    }
}

/// Inspect only panes belonging to the active worktree's session tabs.
pub(crate) fn live_agent_pane(session: &Session, panes: &Panes, cfg: &Config) -> Option<u32> {
    let group = session.active_group()?;
    for tab in &group.tabs {
        if let Some(id) = tab_agent_pane(tab, panes, cfg) {
            return Some(id);
        }
    }
    None
}

fn tab_agent_pane(tab: &Tab, panes: &Panes, cfg: &Config) -> Option<u32> {
    tab.center.pane_ids().into_iter().find(|id| {
        panes
            .table
            .get(id)
            .and_then(|pane| pane.foreground_program())
            .is_some_and(|program| is_agent_program(&program, cfg))
    })
}

fn is_agent_program(program: &str, cfg: &Config) -> bool {
    cfg.activity.is_agent_program(program)
        || cfg.agents.iter().any(|agent| {
            crate::pane::agent_program_name(&agent.command, &agent.name)
                .eq_ignore_ascii_case(program)
        })
}

pub(crate) fn target(session: &Session, panes: &Panes, cfg: &Config) -> PaneTarget {
    if let Some(id) = live_agent_pane(session, panes, cfg) {
        return PaneTarget::Live(id);
    }
    let Some(agent) = cfg.default_agent_name() else {
        return PaneTarget::None;
    };
    let repo = crate::hydrate::active_tab_path(session);
    let repo_root = thegn_core::repo::main_worktree(&repo).unwrap_or(repo);
    let queue = cfg.repo_pr_queue(&repo_root);
    thegn_core::agent_task::resolve_agent(cfg, agent, &queue.agent_command)
        .map(|command| PaneTarget::Headless { command })
        .unwrap_or(PaneTarget::None)
}

pub(crate) fn vars(
    snapshot: &PrReviewSnapshot,
    title: &str,
    url: &str,
    worktree: &str,
    feedback: String,
) -> TaskVars {
    TaskVars::new()
        .set("branch", &snapshot.branch)
        .set("base", "")
        .set("worktree", worktree)
        .set("pr_number", snapshot.pr_number.to_string())
        .set("pr_url", url)
        .set("pr_title", title)
        .set("threads", feedback)
}

pub(crate) fn task_kind() -> TaskKind {
    TaskKind::PrReview
}
