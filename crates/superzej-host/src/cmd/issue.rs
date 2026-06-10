//! `superzej issue <action>` — GitHub Issue data + actions for a worktree.
//!
//! list / view / create / comment over `superzej_core::forge`.

use anyhow::Result;
use superzej_core::forge::{CreateIssueOpts, Forge, GitHubForge, Issue};
use superzej_core::remote::GitLoc;
use superzej_core::{msg, outln};

use crate::cmd::resolve_worktree;

/// Issue subcommands (mirrors the legacy `IssueAction`).
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List issues for the repository.
    List {
        #[arg(long)]
        worktree: Option<String>,
        /// Filter by state (open, closed, all).
        #[arg(long, default_value = "open")]
        state: String,
        #[arg(long)]
        json: bool,
    },
    /// View a specific issue.
    View {
        #[arg(long)]
        worktree: Option<String>,
        number: u64,
        #[arg(long)]
        json: bool,
    },
    /// Create a new issue.
    Create {
        #[arg(long)]
        worktree: Option<String>,
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// Comma-separated labels.
        #[arg(long)]
        labels: Option<String>,
    },
    /// Add a comment to an issue.
    Comment {
        #[arg(long)]
        worktree: Option<String>,
        number: u64,
        body: String,
    },
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::List {
            worktree,
            state,
            json,
        } => list_issues(worktree, state, json),
        Action::View {
            worktree,
            number,
            json,
        } => view_issue(worktree, number, json),
        Action::Create {
            worktree,
            title,
            body,
            labels,
        } => create_issue(worktree, title, body, labels),
        Action::Comment {
            worktree,
            number,
            body,
        } => comment_issue(worktree, number, body),
    }
}

fn get_forge() -> impl Forge {
    GitHubForge::new()
}

fn state_icon(state: &str) -> &'static str {
    match state {
        "OPEN" => "○",
        "CLOSED" => "●",
        _ => "◌",
    }
}

fn list_issues(worktree: Option<String>, state: String, json: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match get_forge().list_issues(&loc, &state) {
        Ok(issues) => {
            if json {
                outln!("{}", serde_json::to_string(&issues)?);
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
        outln!("No issues found");
        return;
    }
    for issue in issues {
        outln!(
            "{} #{} {}",
            state_icon(&issue.state),
            issue.number,
            issue.title
        );
    }
    outln!("\n{} issue(s)", issues.len());
}

fn view_issue(worktree: Option<String>, number: u64, json: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match get_forge().get_issue(&loc, number) {
        Ok(issue) => {
            if json {
                outln!("{}", serde_json::to_string(&issue)?);
            } else {
                print_issue(&issue);
            }
        }
        Err(e) => msg::die(&format!("view issue failed: {e}")),
    }
    Ok(())
}

fn print_issue(issue: &Issue) {
    outln!(
        "{} #{} {}",
        state_icon(&issue.state),
        issue.number,
        issue.title
    );
    outln!("{}", issue.url);
    if let Some(author) = &issue.author {
        outln!("Author: {author}");
    }
    if let Some(created) = &issue.created_at {
        outln!("Created: {created}");
    }
    if let Some(body) = &issue.body
        && !body.is_empty()
    {
        outln!("\n{body}");
    }
}

fn create_issue(
    worktree: Option<String>,
    title: String,
    body: Option<String>,
    labels: Option<String>,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let opts = CreateIssueOpts {
        title,
        body,
        labels: labels
            .map(|l| l.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default(),
    };
    match get_forge().create_issue(&loc, &opts) {
        Ok(issue) => outln!("Issue created: {}", issue.url),
        Err(e) => msg::die(&format!("create issue failed: {e}")),
    }
    Ok(())
}

fn comment_issue(worktree: Option<String>, number: u64, body: String) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match get_forge().issue_comment(&loc, number, &body) {
        Ok(()) => outln!("Comment added to issue #{number}"),
        Err(e) => msg::die(&format!("comment failed: {e}")),
    }
    Ok(())
}
