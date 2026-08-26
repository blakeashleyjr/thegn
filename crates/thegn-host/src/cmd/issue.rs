//! `thegn issue <action>` — GitHub Issue data + actions for a worktree.
//!
//! list / view / create / comment over `thegn_core::forge`.

use anyhow::Result;
use thegn_core::forge::{CreateIssueOpts, ForgeIssue as Issue};
use thegn_core::remote::GitLoc;
use thegn_core::{msg, outln};

use crate::cmd::resolve_worktree;

/// Issue subcommands (mirrors the legacy `IssueAction`).
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List issues for the repository.
    ///
    /// Two modes. Default: the worktree's **forge** (`gh`) issues, filtered by
    /// `--state`. Passing `--status` (or `--limit`) switches to the configured
    /// **tracker** (`[issues]` — Linear/Jira/Kaneo/GitHub-tracker), the
    /// provider-agnostic model a supervisor batches over (THE-57).
    List {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Forge mode: filter by state (open, closed, all).
        #[arg(long, default_value = "open")]
        state: String,
        /// Tracker mode: comma-separated statuses
        /// (backlog,todo,in_progress,done,cancelled). Selects the tracker path.
        #[arg(long)]
        status: Option<String>,
        /// Tracker mode: cap the number of issues returned.
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// View a specific issue.
    View {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        number: u64,
        #[arg(long)]
        json: bool,
    },
    /// Create a new issue.
    Create {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        title: String,
        #[arg(long)]
        body: Option<String>,
        /// Comma-separated labels.
        #[arg(long)]
        labels: Option<String>,
    },
    /// Add a comment to an issue.
    Comment {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        number: u64,
        body: String,
    },
}

pub fn run(cfg: &thegn_core::config::Config, action: Action) -> Result<()> {
    match action {
        Action::List {
            target,
            state,
            status,
            limit,
            json,
        } => {
            // `--status`/`--limit` select the tracker path; otherwise the
            // historical forge (`gh`) listing.
            if status.is_some() || limit.is_some() {
                list_tracker_issues(cfg, status, limit.unwrap_or(0), json)
            } else {
                list_issues(target.worktree, state, json)
            }
        }
        Action::View {
            target,
            number,
            json,
        } => view_issue(target.worktree, number, json),
        Action::Create {
            target,
            title,
            body,
            labels,
        } => create_issue(target.worktree, title, body, labels),
        Action::Comment {
            target,
            number,
            body,
        } => comment_issue(target.worktree, number, body),
    }
}

/// The worktree's forge (the process `ForgeSet`, routed by origin host).
fn forges() -> std::sync::Arc<thegn_svc::forge::ForgeSet> {
    crate::forge_handle::get()
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
    match forges().for_loc(&loc).issue_list(&loc, &state) {
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

/// Tracker-mode `issue list` (THE-57): the configured `[issues]` provider,
/// filtered by status/limit and emitted machine-readable — the door a
/// supervisor lists its next batch through. Provider-agnostic: the same
/// `IssueRouter` the panel and the control plane use.
fn list_tracker_issues(
    cfg: &thegn_core::config::Config,
    status: Option<String>,
    limit: usize,
    json: bool,
) -> Result<()> {
    let router = thegn_svc::issue::IssueRouter::from_config(&cfg.issues);
    if !router.is_configured() {
        msg::die("no issue tracker configured (set [issues] providers/accounts)");
    }
    let statuses = status
        .as_deref()
        .map(parse_tracker_statuses)
        .unwrap_or_default();
    let filter = thegn_core::issue::IssueFilter {
        statuses,
        limit,
        ..Default::default()
    };
    let rt = tokio::runtime::Runtime::new()?;
    let mut issues = match rt.block_on(router.list_issues(&filter)) {
        Ok(v) => v,
        Err(e) => msg::die(&format!("list issues failed: {e}")),
    };
    if limit > 0 && issues.len() > limit {
        issues.truncate(limit);
    }
    if json {
        outln!("{}", serde_json::to_string(&issues)?);
    } else if issues.is_empty() {
        outln!("No issues found");
    } else {
        for i in &issues {
            outln!("{} {} {}", i.status.glyph(), i.number, i.title);
        }
    }
    Ok(())
}

/// Parse a comma-separated tracker status list (unknown names dropped).
fn parse_tracker_statuses(s: &str) -> Vec<thegn_core::issue::IssueStatus> {
    use thegn_core::issue::IssueStatus::*;
    s.split(',')
        .filter_map(|p| match p.trim() {
            "backlog" => Some(Backlog),
            "todo" => Some(Todo),
            "in_progress" => Some(InProgress),
            "done" => Some(Done),
            "cancelled" => Some(Cancelled),
            _ => None,
        })
        .collect()
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
    match forges().for_loc(&loc).issue_get(&loc, number) {
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
    match forges().for_loc(&loc).issue_create(&loc, &opts) {
        Ok(issue) => outln!("Issue created: {}", issue.url),
        Err(e) => msg::die(&format!("create issue failed: {e}")),
    }
    Ok(())
}

fn comment_issue(worktree: Option<String>, number: u64, body: String) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).issue_comment(&loc, number, &body) {
        Ok(()) => outln!("Comment added to issue #{number}"),
        Err(e) => msg::die(&format!("comment failed: {e}")),
    }
    Ok(())
}
