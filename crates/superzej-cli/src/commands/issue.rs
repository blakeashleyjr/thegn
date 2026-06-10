//! `superzej issue <action>` — GitHub Issue data + actions for a worktree.
//!
//! Provides list, view, create, and comment operations for GitHub issues.

use crate::cli::IssueAction;
use crate::commands::resolve_worktree;
use crate::config::Config;
use crate::forge::{CreateIssueOpts, Forge, GitHubForge, Issue};
use crate::msg;
use crate::remote::GitLoc;
use anyhow::Result;

pub fn run(_cfg: &Config, action: IssueAction) -> Result<()> {
    match action {
        IssueAction::List {
            worktree,
            state,
            json,
        } => list_issues(worktree, state, json),
        IssueAction::View {
            worktree,
            number,
            json,
        } => view_issue(worktree, number, json),
        IssueAction::Create {
            worktree,
            title,
            body,
            labels,
        } => create_issue(worktree, title, body, labels),
        IssueAction::Comment {
            worktree,
            number,
            body,
        } => comment_issue(worktree, number, body),
    }
}

fn get_forge() -> impl Forge {
    GitHubForge::new()
}

fn list_issues(worktree: Option<String>, state: String, json: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = get_forge();

    match forge.list_issues(&loc, &state) {
        Ok(issues) => {
            if json {
                crate::outln!("{}", serde_json::to_string(&issues)?);
            } else {
                print_issues(&issues);
            }
        }
        Err(e) => msg::die(&format!("list issues failed: {e}")),
    }
    Ok(())
}

fn print_issues(issues: &[Issue]) {
    if issues.is_empty() {
        crate::outln!("No issues found");
        return;
    }
    for issue in issues {
        let state_icon = match issue.state.as_str() {
            "OPEN" => "○",
            "CLOSED" => "●",
            _ => "◌",
        };
        crate::outln!("{} #{} {}", state_icon, issue.number, issue.title);
    }
    crate::outln!("\n{} issue(s)", issues.len());
}

fn view_issue(worktree: Option<String>, number: u64, json: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = get_forge();

    match forge.get_issue(&loc, number) {
        Ok(issue) => {
            if json {
                crate::outln!("{}", serde_json::to_string(&issue)?);
            } else {
                print_issue(&issue);
            }
        }
        Err(e) => msg::die(&format!("view issue failed: {e}")),
    }
    Ok(())
}

fn print_issue(issue: &Issue) {
    let state_icon = match issue.state.as_str() {
        "OPEN" => "○",
        "CLOSED" => "●",
        _ => "◌",
    };
    crate::outln!("{} #{} {}", state_icon, issue.number, issue.title);
    crate::outln!("{}", issue.url);
    if let Some(author) = &issue.author {
        crate::outln!("Author: {}", author);
    }
    if let Some(created) = &issue.created_at {
        crate::outln!("Created: {}", created);
    }
    if let Some(body) = &issue.body {
        if !body.is_empty() {
            crate::outln!("\n{}", body);
        }
    }
}

fn create_issue(
    worktree: Option<String>,
    title: String,
    body: Option<String>,
    labels: Option<String>,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = get_forge();

    let opts = CreateIssueOpts {
        title,
        body,
        labels: labels
            .map(|l| l.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default(),
    };

    match forge.create_issue(&loc, &opts) {
        Ok(issue) => {
            crate::outln!("Issue created: {}", issue.url);
        }
        Err(e) => msg::die(&format!("create issue failed: {e}")),
    }
    Ok(())
}

fn comment_issue(worktree: Option<String>, number: u64, body: String) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forge = get_forge();

    match forge.issue_comment(&loc, number, &body) {
        Ok(()) => crate::outln!("Comment added to issue #{}", number),
        Err(e) => msg::die(&format!("comment failed: {e}")),
    }
    Ok(())
}
