//! Prompt-first, TUI side: finish a sandbox's deferred steps after its shell is
//! up, one status-line message per step, with a notice on failure.

use anyhow::Result;
use thegn_core::config::Config;
use thegn_core::provision_journal::{self as journal, AgentGate, DeferredState, Journal};

use crate::agent::{DeferredHandoff, ProvisionState, ProvisionStepView};

const MAX_DISPLAY_CHARS: usize = 200;

pub(crate) fn finish_blocking(
    cfg: &Config,
    worktree: &str,
    handoff: DeferredHandoff,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
) -> Result<()> {
    match handoff.lease {
        Some(lease) => crate::agent::provision_worktree_mode(
            cfg,
            worktree,
            crate::agent::ProvisionMode::Deferred(lease),
            &mut *progress,
        )
        .map(|_| ()),
        None => crate::agent::provision_worktree(cfg, worktree, &mut *progress).map(|_| ()),
    }
}

pub(crate) fn finish_in_background(cfg: &Config, worktree: &str, handoff: DeferredHandoff) {
    let id = handoff.sandbox.clone();
    let Some(lease) = handoff.lease else {
        return;
    };
    let mut last = None;
    let result = finish_blocking(
        cfg,
        worktree,
        DeferredHandoff {
            sandbox: id.clone(),
            lease: Some(lease),
        },
        &mut |views| {
            if let Some(text) = status_text(views, &mut last)
                && let Some(state) = crate::notify::global()
            {
                state.emit_status(&text);
            }
        },
    );
    match finish_notice(&result, journal::read(&id).as_ref(), worktree) {
        Notice::Status(text) => {
            if let Some(state) = crate::notify::global() {
                state.emit_status(&text);
            }
        }
        Notice::Warn(text) => crate::launch_notice::post(worktree, &text),
    }
}

pub(crate) fn ensure_agents_ready(cfg: &Config, worktree: &str, choice: &str) -> Result<()> {
    if !journal::is_agent_choice(cfg, choice) {
        return Ok(());
    }
    let id = match crate::provision_gate::provider_target(cfg, worktree) {
        Ok(Some((_, id))) => id,
        Ok(None) => return Ok(()),
        Err(error) => {
            let detail = clean(&format!("{error:#}"));
            tracing::debug!(
                target: "thegn::provider",
                worktree = %worktree,
                %detail,
                "could not resolve deferred provisioning target"
            );
            return Ok(());
        }
    };
    let gate = journal::agent_gate(journal::read(&id).as_ref());
    match gate {
        AgentGate::Go => Ok(()),
        AgentGate::GoWithWarning => {
            let warning = journal::read(&id)
                .as_ref()
                .map(|entry| failed_text(entry))
                .unwrap_or_else(|| "background setup previously failed".to_string());
            if let Some(state) = crate::notify::global() {
                state.emit_status(&warning);
            }
            Ok(())
        }
        AgentGate::Wait => {
            let choice = clean(choice);
            if let Some(state) = crate::notify::global() {
                state.emit_status(&format!("{choice} starts when background setup finishes"));
            }
            crate::agent::provision_worktree(cfg, worktree, |_| {})?;
            if journal::agent_gate(journal::read(&id).as_ref()) == AgentGate::Wait {
                anyhow::bail!(
                    "background setup for {worktree} did not finish (cancelled, or the sandbox is gone); reopen it or run `thegn env up --worktree {worktree}`"
                );
            }
            Ok(())
        }
    }
}

pub(crate) fn show_line(id: &str) -> Option<String> {
    show_line_for(journal::read(id).as_ref())
}

fn status_text(views: &[ProvisionStepView], last: &mut Option<String>) -> Option<String> {
    let active = views
        .iter()
        .find(|view| view.state == ProvisionState::Active)
        .map(|view| clean(&view.label))?;
    if last.as_deref() == Some(active.as_str()) {
        return None;
    }
    *last = Some(active.clone());
    Some(format!("background setup: {active}…"))
}

#[derive(Debug, PartialEq, Eq)]
enum Notice {
    Status(String),
    Warn(String),
}

fn finish_notice(result: &Result<()>, journal: Option<&Journal>, worktree: &str) -> Notice {
    if journal.is_some_and(|entry| entry.state == DeferredState::Cancelled) {
        return Notice::Status("background setup cancelled".to_string());
    }
    if let Some(entry) = journal.filter(|entry| entry.state == DeferredState::Failed) {
        return Notice::Warn(failure_text(entry, worktree));
    }
    if let Err(error) = result {
        let entry = journal.map(|entry| (entry.step.as_str(), entry.detail.as_str()));
        let (step, detail) = entry.unwrap_or(("background setup", ""));
        let detail = if detail.trim().is_empty() {
            clean(&format!("{error:#}"))
        } else {
            clean(detail)
        };
        return Notice::Warn(format!(
            "background setup failed at {}: {}; reopen the worktree or run `thegn env up --worktree {}` to retry",
            clean_or(step, "background setup"),
            clean_or(&detail, "unknown error"),
            clean(worktree),
        ));
    }
    Notice::Status("background setup finished; coding agents are ready".to_string())
}

fn show_line_for(journal: Option<&Journal>) -> Option<String> {
    let entry = journal?;
    match entry.state {
        DeferredState::Pending => Some(
            "setup:     background steps pending (coding agents, agent logins, snapshot)"
                .to_string(),
        ),
        DeferredState::Failed => Some(format!(
            "setup:     background steps failed at {}: {}; `thegn env up` retries",
            clean_or(&entry.step, "background setup"),
            clean_or(&entry.detail, "unknown error"),
        )),
        DeferredState::Cancelled => {
            Some("setup:     background steps cancelled; the next open resumes".to_string())
        }
    }
}

fn failed_text(entry: &Journal) -> String {
    format!(
        "background setup failed at {}: {}",
        clean_or(&entry.step, "background setup"),
        clean_or(&entry.detail, "unknown error")
    )
}

fn failure_text(entry: &Journal, worktree: &str) -> String {
    format!(
        "{}; reopen the worktree or run `thegn env up --worktree {}` to retry",
        failed_text(entry),
        clean(worktree)
    )
}

fn clean_or(value: &str, fallback: &str) -> String {
    let value = clean(value);
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_DISPLAY_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(state: DeferredState, step: &str, detail: &str) -> Journal {
        Journal {
            v: 1,
            worktree: "/tmp/worktree".to_string(),
            env: "cloud".to_string(),
            state,
            step: step.to_string(),
            detail: detail.to_string(),
            updated_ms: 1,
        }
    }

    #[test]
    fn status_text_reports_only_new_active_labels() {
        let mut last = None;
        let active = |label: &str| ProvisionStepView {
            label: label.to_string(),
            state: ProvisionState::Active,
            detail: None,
        };
        assert_eq!(
            status_text(&[active("Install")], &mut last),
            Some("background setup: Install…".to_string())
        );
        assert_eq!(status_text(&[active("Install")], &mut last), None);
        assert_eq!(
            status_text(&[active("Login")], &mut last),
            Some("background setup: Login…".to_string())
        );
    }

    #[test]
    fn finish_notice_table() {
        assert_eq!(
            finish_notice(&Ok(()), None, "/wt"),
            Notice::Status("background setup finished; coding agents are ready".to_string())
        );
        assert!(matches!(
            finish_notice(&Ok(()), Some(&journal(DeferredState::Failed, "agents", "nope")), "/wt"),
            Notice::Warn(text) if text.contains("failed at agents: nope")
        ));
        assert_eq!(
            finish_notice(
                &Ok(()),
                Some(&journal(DeferredState::Cancelled, "", "")),
                "/wt"
            ),
            Notice::Status("background setup cancelled".to_string())
        );
    }

    #[test]
    fn show_line_for_table_strips_control_characters() {
        assert!(
            show_line_for(Some(&journal(DeferredState::Pending, "", "")))
                .is_some_and(|line| line.contains("background steps pending"))
        );
        let failed = journal(DeferredState::Failed, "ag\u{1b}[31m\u{7}", "bad\nnews");
        assert_eq!(
            show_line_for(Some(&failed)),
            Some(
                "setup:     background steps failed at ag[31m: badnews; `thegn env up` retries"
                    .to_string()
            )
        );
        assert!(
            show_line_for(Some(&journal(DeferredState::Cancelled, "", "")))
                .is_some_and(|line| line.contains("cancelled"))
        );
    }

    #[test]
    fn shell_choice_needs_no_io() {
        assert!(ensure_agents_ready(&Config::default(), "/not-a-worktree", "shell").is_ok());
    }
}
