//! GitHub Issues backend via the `gh` CLI.
//!
//! Uses the same subprocess pattern as `superzej_core::github` — always works
//! as long as `gh` is authenticated, even without native octocrab credentials.

use serde::Deserialize;
use std::process::Command;
use superzej_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::{IssueBackend, IssueError};

pub struct GitHubIssuesBackend {
    extra_flags: Vec<String>,
}

impl GitHubIssuesBackend {
    pub fn new(extra_flags: Vec<String>) -> Self {
        GitHubIssuesBackend { extra_flags }
    }

    fn gh(&self, args: &[&str]) -> Result<String, IssueError> {
        let mut cmd = Command::new("gh");
        cmd.args(args);
        let out = cmd
            .output()
            .map_err(|e| IssueError::Subprocess(e.to_string()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(IssueError::Subprocess(stderr.into_owned()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

// ---- JSON shapes from `gh issue list --json` --------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhIssue {
    number: u64,
    title: String,
    state: String,
    body: Option<String>,
    #[serde(default)]
    assignees: Vec<GhUser>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    url: String,
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct GhUser {
    login: String,
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhComment {
    body: String,
    author: Option<GhActor>,
    created_at: Option<String>,
}

#[derive(Deserialize)]
struct GhActor {
    login: String,
}

fn parse_ms(s: Option<&str>) -> i64 {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn gh_issue_to_domain(gi: GhIssue) -> Issue {
    let status = match gi.state.as_str() {
        "CLOSED" => IssueStatus::Done,
        _ => IssueStatus::Todo,
    };
    Issue {
        id: format!("github:{}", gi.number),
        number: gi.number.to_string(),
        provider: "github".into(),
        title: gi.title,
        body: gi.body,
        status,
        priority: IssuePriority::None,
        assignees: gi.assignees.into_iter().map(|u| u.login).collect(),
        labels: gi.labels.into_iter().map(|l| l.name).collect(),
        url: gi.url,
        branch_hint: None,
        updated_at_ms: parse_ms(gi.updated_at.as_deref()),
        ..Default::default()
    }
}

const GH_LIST_FIELDS: &str = "number,title,state,body,assignees,labels,url,updatedAt";

#[allow(async_fn_in_trait)]
impl IssueBackend for GitHubIssuesBackend {
    fn provider_id(&self) -> &'static str {
        "github"
    }

    async fn list_issues(&self, filter: &IssueFilter) -> Result<Vec<Issue>, IssueError> {
        let limit_str = filter.limit.to_string();
        let mut args: Vec<&str> = vec![
            "issue",
            "list",
            "--json",
            GH_LIST_FIELDS,
            "--limit",
            &limit_str,
        ];
        if filter.assignee_me {
            args.extend(["--assignee", "@me"]);
        }
        // Include extra flags configured by the user.
        let extra: Vec<&str> = self.extra_flags.iter().map(|s| s.as_str()).collect();
        args.extend(extra);

        let json = self.gh(&args)?;
        let issues: Vec<GhIssue> =
            serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
        Ok(issues.into_iter().map(gh_issue_to_domain).collect())
    }

    async fn get_issue(&self, id: &str) -> Result<IssueDetail, IssueError> {
        let number = id.strip_prefix("github:").unwrap_or(id);
        let json = self.gh(&[
            "issue",
            "view",
            number,
            "--json",
            "number,title,state,body,assignees,labels,url,updatedAt,comments",
        ])?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct GhIssueDetail {
            #[serde(flatten)]
            issue: GhIssue,
            #[serde(default)]
            comments: Vec<GhComment>,
        }
        let detail: GhIssueDetail =
            serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
        let comments = detail
            .comments
            .into_iter()
            .map(|c| IssueComment {
                author: c
                    .author
                    .map(|a| a.login)
                    .unwrap_or_else(|| "unknown".into()),
                body: c.body,
                created_at_ms: parse_ms(c.created_at.as_deref()),
            })
            .collect();
        Ok(IssueDetail {
            issue: gh_issue_to_domain(detail.issue),
            comments,
        })
    }

    async fn create_issue(&self, draft: &IssueDraft) -> Result<Issue, IssueError> {
        let mut args = vec!["issue", "create", "--title", &draft.title];
        let body_val;
        if let Some(body) = &draft.body {
            body_val = body.clone();
            args.extend(["--body", &body_val]);
        } else {
            args.extend(["--body", ""]);
        }
        // gh issue create prints the URL; fetch the number from it.
        let url = self.gh(&args)?.trim().to_string();
        let number = url
            .rsplit('/')
            .next()
            .ok_or_else(|| IssueError::Parse("unexpected gh issue create output".into()))?
            .to_string();
        let json = self.gh(&["issue", "view", &number, "--json", GH_LIST_FIELDS])?;
        let gi: GhIssue =
            serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
        Ok(gh_issue_to_domain(gi))
    }

    async fn update_issue(&self, id: &str, patch: &IssuePatch) -> Result<Issue, IssueError> {
        let number = id.strip_prefix("github:").unwrap_or(id);

        // Handle status (open / close).
        if let Some(status) = patch.status {
            let sub = match status {
                IssueStatus::Done | IssueStatus::Cancelled => "close",
                _ => "reopen",
            };
            self.gh(&["issue", sub, number])?;
        }

        // Handle title update.
        if let Some(title) = &patch.title {
            self.gh(&["issue", "edit", number, "--title", title])?;
        }

        // Re-fetch the updated issue.
        let json = self.gh(&["issue", "view", number, "--json", GH_LIST_FIELDS])?;
        let gi: GhIssue =
            serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
        Ok(gh_issue_to_domain(gi))
    }

    async fn search(&self, query_str: &str, limit: usize) -> Result<Vec<Issue>, IssueError> {
        let limit_str = limit.to_string();
        let json = self.gh(&[
            "issue",
            "list",
            "--search",
            query_str,
            "--json",
            GH_LIST_FIELDS,
            "--limit",
            &limit_str,
        ])?;
        let issues: Vec<GhIssue> =
            serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
        Ok(issues.into_iter().map(gh_issue_to_domain).collect())
    }
}
