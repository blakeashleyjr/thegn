//! Stage-prompt composition — the helpers ONE change owns so the CLI dispatch
//! path (`cmd/session.rs::open_stage`) and the daemon's transport-retry
//! relaunch path (`daemon/pipeline_retry.rs`) render a stage's prompt
//! identically. One seamer, two callers (THE-86 chunk 2).
//!
//! Moved verbatim from `cmd/session.rs` (where they lived since THE-76); the
//! CLI module re-imports them, so no behaviour changes — only the home.

use anyhow::Result;
use thegn_core::agent_task::{TaskVars, render_prompt, template_vars};

/// The tracker facts a stage prompt may reference. Empty strings when the
/// template does not read the tracker — a stage that does not need the issue
/// must not require a configured tracker either.
pub(crate) struct IssueFacts {
    pub(crate) number: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) url: String,
}

impl IssueFacts {
    /// Issue facts for a row whose tracker lookup is unavailable or
    /// unnecessary: the local number is derivable, the tracker-backed fields
    /// stay empty (the render is still correct — the template simply reads
    /// empty strings).
    pub(crate) fn number_only(number: String) -> Self {
        Self {
            number,
            title: String::new(),
            body: String::new(),
            url: String::new(),
        }
    }
}

/// Bind the nine `agent_task::STAGE_VARS` for one stage dispatch. The single
/// place the CLI assembles them, so the render step is unit-testable without
/// a client or a daemon.
pub(crate) fn stage_task_vars(
    facts: &IssueFacts,
    branch: &str,
    worktree: &str,
    stage: &str,
    artifact: &str,
    parent_artifact: &str,
) -> TaskVars {
    TaskVars::new()
        .set("issue_number", facts.number.as_str())
        .set("issue_title", facts.title.as_str())
        .set("issue_body", facts.body.as_str())
        .set("issue_url", facts.url.as_str())
        .set("branch", branch)
        .set("worktree", worktree)
        .set("stage", stage)
        .set("artifact", artifact)
        .set("parent_artifact", parent_artifact)
}

/// Whether the template reads a tracker-backed var (`{issue_title}`,
/// `{issue_body}`, `{issue_url}`) — the caller consults the tracker only then,
/// so a stage that does not need the issue never needs the tracker either.
pub(crate) fn needs_tracker(template: &str) -> bool {
    let referenced = template_vars(template).unwrap_or_default();
    ["issue_title", "issue_body", "issue_url"]
        .iter()
        .any(|v| referenced.iter().any(|r| r == v))
}

/// Render one stage's prompt and refuse an empty one — the step `open_stage`
/// performs between the roster insert and the session open, shared verbatim
/// with the daemon's relaunch path. A blank render is refused, never launched:
/// an empty task leaves a worker sitting on a blank pane.
pub(crate) fn render_stage(stage_name: &str, template: &str, vars: &TaskVars) -> Result<String> {
    let prompt = render_prompt(template, vars)
        .map_err(|e| anyhow::anyhow!("stage '{stage_name}' prompt template is invalid: {e}"))?;
    if prompt.trim().is_empty() {
        anyhow::bail!(
            "stage '{stage_name}' rendered an empty prompt — an empty task would \
             leave the worker sitting on a blank pane"
        );
    }
    Ok(prompt)
}
