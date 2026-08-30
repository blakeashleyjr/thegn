//! PR-review handoff policy.
//!
//! Selection and live-pane discovery stay separate from rendering.  The live
//! pane search is deliberately scoped to the active worktree's own tabs and
//! checks the actual foreground process rather than the focused pane.

use thegn_core::agent_task::{TaskKind, TaskVars};
use thegn_core::config::Config;
use thegn_core::review::{PrReviewSnapshot, format_review_feedback};

use crate::chrome::FrameModel;
use crate::focus::FocusState;
use crate::hydrate::RefreshKind;
use crate::panes::Panes;
use crate::session::{Session, Tab};
use tokio::sync::mpsc::UnboundedSender;

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
    let queue = review_queue(session, cfg);
    headless_target(cfg, &queue)
}

/// Resolve the repository overlay without running git on the compositor loop.
/// Session group names are `<workspace-slug>/<branch>`; `repo_pr_queue` needs
/// only that slug to select the already-loaded trusted workspace overlay.
fn review_queue(session: &Session, cfg: &Config) -> thegn_core::config::PrQueueConfig {
    let slug = session
        .active_group()
        .and_then(|group| group.name.split('/').next())
        .filter(|slug| !slug.is_empty());
    cfg.repo_pr_queue(std::path::Path::new(slug.unwrap_or("repo")))
}

fn headless_target(cfg: &Config, queue: &thegn_core::config::PrQueueConfig) -> PaneTarget {
    if !queue.agent_command.trim().is_empty() {
        return thegn_core::agent_task::resolve_agent(cfg, "", &queue.agent_command)
            .map(|command| PaneTarget::Headless { command })
            .unwrap_or(PaneTarget::None);
    }
    let agent = if queue.agent.trim().is_empty() {
        match cfg.default_agent_name() {
            Some(agent) => agent,
            None => return PaneTarget::None,
        }
    } else {
        queue.agent.as_str()
    };
    thegn_core::agent_task::resolve_agent(cfg, agent, &queue.agent_command)
        .map(|command| PaneTarget::Headless { command })
        .unwrap_or(PaneTarget::None)
}

pub(crate) fn vars(
    snapshot: &PrReviewSnapshot,
    base: &str,
    title: &str,
    url: &str,
    worktree: &str,
    feedback: String,
) -> TaskVars {
    TaskVars::new()
        .set("branch", &snapshot.branch)
        .set("base", base)
        .set("worktree", worktree)
        .set("pr_number", snapshot.pr_number.to_string())
        .set("pr_url", url)
        .set("pr_title", title)
        .set("threads", feedback)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch(
    session: &mut Session,
    panes: &mut Panes,
    focus: &mut FocusState,
    cfg: &Config,
    model: &mut FrameModel,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
    snapshot: PrReviewSnapshot,
    selection: ReviewSelection,
    title: &str,
    url: &str,
    base: &str,
) {
    let text = feedback(&snapshot, &selection);
    if text.is_empty() {
        model.status = "no review feedback selected".into();
        return;
    }
    match target(session, panes, cfg) {
        PaneTarget::Live(id) => {
            let result = panes
                .table
                .get_mut(&id)
                .map(|pane| crate::run::paste_text_into_pane(pane, &text));
            match result {
                Some(Ok(())) => {
                    if let Some(group) = session.active_group_mut()
                        && let Some(tab) = group
                            .tabs
                            .iter_mut()
                            .find(|tab| tab.center.pane_ids().contains(&id))
                    {
                        tab.focused_pane = id;
                    }
                    focus.zone = crate::focus::Zone::Center;
                    model.status = "review feedback pasted (not submitted)".into();
                }
                Some(Err(e)) => model.status = format!("review handoff failed: {e}"),
                None => model.status = "agent pane closed before handoff".into(),
            }
        }
        PaneTarget::Headless { command } => {
            let refresh_tx = refresh_tx.clone();
            let waker = waker.clone();
            let queue = review_queue(session, cfg);
            let worktree = crate::hydrate::active_tab_path(session)
                .to_string_lossy()
                .into_owned();
            let sandbox = match crate::agent_run::agent_floor_gate(
                cfg,
                &worktree,
                queue.agent_sandbox,
                queue.agent_isolation_floor,
                queue.agent_on_floor_miss,
            ) {
                crate::agent_run::AgentDispatch::Run(sandbox) => sandbox,
                crate::agent_run::AgentDispatch::RunDegraded(sandbox, warning) => {
                    thegn_core::msg::warn(&warning);
                    sandbox
                }
                crate::agent_run::AgentDispatch::InfraHold(reason) => {
                    model.status = format!("review handoff blocked: {reason}");
                    return;
                }
            };
            let vars = vars(&snapshot, base, title, url, &worktree, text);
            let prompt = match thegn_core::agent_task::render_prompt(
                queue.prompts.resolve(TaskKind::PrReview),
                &vars,
            ) {
                Ok(prompt) => prompt,
                Err(error) => {
                    model.status = format!("review handoff template invalid: {error}");
                    return;
                }
            };
            tokio::task::spawn_blocking(move || {
                let ok = crate::agent_run::run(&crate::agent_run::AgentTaskRun {
                    kind: TaskKind::PrReview,
                    worktree: &worktree,
                    prompt: &prompt,
                    command_template: &command,
                    vars: &vars,
                    timeout_secs: queue.agent_timeout_secs,
                    sandbox,
                });
                if !ok {
                    thegn_core::msg::warn("PR review agent handoff failed");
                }
                if refresh_tx.send(RefreshKind::Pr).is_ok()
                    && let Err(error) = waker.wake()
                {
                    tracing::debug!(%error, "review handoff refresh wake failed");
                }
            });
            model.status = "review feedback sent to headless agent".into();
        }
        PaneTarget::None => {
            model.status = "no live agent pane or configured headless agent".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::forge::model::{PrComment, PrConversation, ReviewThread};

    fn snapshot() -> PrReviewSnapshot {
        PrReviewSnapshot {
            pr_number: 27,
            branch: "feature/review".into(),
            head_oid: "deadbeef".into(),
            conversation: PrConversation {
                threads: vec![
                    ReviewThread {
                        id: "one".into(),
                        path: "src/lib.rs".into(),
                        line: Some(9),
                        comments: vec![PrComment {
                            author: "reviewer".into(),
                            body: "fix this".into(),
                            ..PrComment::default()
                        }],
                        ..ReviewThread::default()
                    },
                    ReviewThread {
                        id: "two".into(),
                        path: "src/main.rs".into(),
                        line: Some(4),
                        comments: vec![PrComment {
                            author: "reviewer".into(),
                            body: "also this".into(),
                            ..PrComment::default()
                        }],
                        ..ReviewThread::default()
                    },
                ],
                ..PrConversation::default()
            },
            ..PrReviewSnapshot::default()
        }
    }

    #[test]
    fn selected_feedback_contains_only_the_selected_thread() {
        let text = feedback(&snapshot(), &ReviewSelection::Selected("one".into()));
        assert!(text.contains("fix this"));
        assert!(!text.contains("also this"));
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn headless_target_uses_the_resolved_repo_queue_command() {
        let cfg = Config::default();
        let mut queue = cfg.pr_queue.clone();
        queue.agent_command = "repo-agent --prompt {prompt}".into();

        assert_eq!(
            headless_target(&cfg, &queue),
            PaneTarget::Headless {
                command: "repo-agent --prompt {prompt}".into()
            }
        );
    }

    #[test]
    fn review_queue_uses_the_active_groups_workspace_overlay() {
        let mut cfg = Config::default();
        let mut workspace = thegn_core::config::WorkspaceConfig::default();
        workspace.pr_queue.agent_command = Some("workspace-agent {prompt}".into());
        cfg.workspace.insert("widget".into(), workspace);
        let mut session = Session::default();
        session.add_group(crate::session::WorktreeGroup::new(
            "widget/feature",
            crate::session::GroupKind::Branch,
            "/worktrees/widget-feature",
        ));

        assert_eq!(
            review_queue(&session, &cfg).agent_command,
            "workspace-agent {prompt}"
        );
    }
}
