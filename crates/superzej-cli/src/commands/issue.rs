//! `superzej issue <action>` — manage issues for a worktree.

use crate::cli::IssueAction;
use crate::config::Config;
use crate::commands::resolve_worktree;
use crate::msg;
use anyhow::Result;
use superzej_core::forge::models::CreateIssueOpts;
use superzej_core::remote::GitLoc;

pub fn run(_cfg: &Config, action: IssueAction) -> Result<()> {
    match action {
        IssueAction::List { worktree, state } => list(worktree, &state),
        IssueAction::View { worktree, issue } => view(worktree, issue),
        IssueAction::Create {
            worktree,
            title,
            body,
            label,
        } => create(worktree, title, body, label),
        IssueAction::Comment {
            worktree,
            issue,
            body,
        } => comment(worktree, issue, &body),
    }
}

fn list(worktree: Option<String>, state: &str) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = superzej_core::forge::get_forge_for_loc(&loc)
        .ok_or_else(|| anyhow::anyhow!("No supported forge detected"))?;

    let issues = forge
        .list_issues(&loc, state)
        .map_err(|e| anyhow::anyhow!("issue list failed: {}", e.message()))?;

    // Output JSON for the plugin/panel consumption
    println!("{}", serde_json::to_string_pretty(&issues)?);
    Ok(())
}

fn view(worktree: Option<String>, issue_num: u64) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = superzej_core::forge::get_forge_for_loc(&loc)
        .ok_or_else(|| anyhow::anyhow!("No supported forge detected"))?;

    let issue = forge
        .get_issue(&loc, issue_num)
        .map_err(|e| anyhow::anyhow!("issue view failed: {}", e.message()))?;
    println!("{}", serde_json::to_string_pretty(&issue)?);
    Ok(())
}

fn create(
    worktree: Option<String>,
    title: String,
    body: Option<String>,
    labels: Vec<String>,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = superzej_core::forge::get_forge_for_loc(&loc)
        .ok_or_else(|| anyhow::anyhow!("No supported forge detected"))?;

    let opts = CreateIssueOpts {
        title,
        body,
        labels,
    };
    let issue = forge
        .create_issue(&loc, &opts)
        .map_err(|e| anyhow::anyhow!("issue create failed: {}", e.message()))?;

    msg::info(&format!("Issue #{} created: {}", issue.number, issue.url));
    Ok(())
}

fn comment(worktree: Option<String>, issue_num: u64, body: &str) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = superzej_core::forge::get_forge_for_loc(&loc)
        .ok_or_else(|| anyhow::anyhow!("No supported forge detected"))?;

    forge
        .issue_comment(&loc, issue_num, body)
        .map_err(|e| anyhow::anyhow!("issue comment failed: {}", e.message()))?;
    msg::info("Comment posted");
    Ok(())
}