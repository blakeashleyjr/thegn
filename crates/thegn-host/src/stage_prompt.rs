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

/// Bind the ten `agent_task::STAGE_VARS` for one stage dispatch (THE-88:
/// `{row}` joins the nine from THE-86 — a worker must know its row id so it
/// can call `thegn dispatch report {row} --text …`). The single place the
/// CLI assembles them, so the render step is unit-testable without a client
/// or a daemon.
pub(crate) fn stage_task_vars(
    facts: &IssueFacts,
    branch: &str,
    worktree: &str,
    stage: &str,
    artifact: &str,
    parent_artifact: &str,
    row_id: i64,
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
        .set("row", row_id.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::agent_task::render_prompt;

    #[test]
    fn stage_task_vars_binds_row_alongside_the_other_nine() {
        // THE-88: a worker must know its row id so it can file the
        // `thegn dispatch report {row} --text …` handoff the Lead reads.
        // `agent_task::STAGE_VARS` already admits the name; this pins the
        // binding so a future edit cannot silently drop it.
        let facts = IssueFacts {
            number: "THE-88".into(),
            title: "pipeline token efficiency".into(),
            body: "data".into(),
            url: "https://example.test/THE-88".into(),
        };
        let vars = stage_task_vars(
            &facts,
            "tg/the-88",
            "/wt/the-88",
            "code",
            ".thegn/pipeline/THE-88/code/7.md",
            ".thegn/pipeline/THE-88/architect/3.md",
            7,
        );
        // The ten expected keys are present, in insertion-stable order.
        let names = vars.names();
        for key in [
            "issue_number",
            "issue_title",
            "issue_body",
            "issue_url",
            "branch",
            "worktree",
            "stage",
            "artifact",
            "parent_artifact",
            "row",
        ] {
            assert!(names.contains(&key), "missing {key} in {names:?}");
        }
        assert_eq!(vars.get("row"), Some("7"));
        // And the renderer substitutes it like every other stage var.
        let rendered = render_prompt("row={row} stage={stage}", &vars).expect("renders");
        assert_eq!(rendered, "row=7 stage=code");
    }
}
