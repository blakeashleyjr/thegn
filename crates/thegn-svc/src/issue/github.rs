//! GitHub Issues backend via the `gh` CLI.
//!
//! Uses the same subprocess pattern as `thegn_core::github` — always works
//! as long as `gh` is authenticated, even without native octocrab credentials.

use serde::Deserialize;
use std::process::Command;
use thegn_core::issue::{
    Issue, IssueComment, IssueDetail, IssueDraft, IssueFilter, IssuePatch, IssuePriority,
    IssueStatus,
};

use super::{IssueBackend, IssueError};
use futures_util::future::BoxFuture;

pub struct GitHubIssuesBackend {
    extra_flags: Vec<String>,
    /// Working directory for `gh` invocations. Without it, any call that lacks
    /// an explicit `--repo` resolves against the *process* cwd — one fixed repo
    /// for the whole session — so callers should anchor the backend to the
    /// worktree they're fetching for.
    dir: Option<std::path::PathBuf>,
}

impl GitHubIssuesBackend {
    pub fn new(extra_flags: Vec<String>) -> Self {
        GitHubIssuesBackend {
            extra_flags,
            dir: None,
        }
    }

    /// Anchor `gh` invocations to `dir` (see the field doc).
    pub fn set_dir(&mut self, dir: Option<std::path::PathBuf>) {
        self.dir = dir;
    }

    fn gh(&self, args: &[&str]) -> Result<String, IssueError> {
        let mut cmd = Command::new("gh");
        cmd.args(args);
        if let Some(dir) = &self.dir {
            cmd.current_dir(dir);
        }
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

/// Extract `owner/repo` from a GitHub issue/PR URL
/// (`https://github.com/owner/repo/issues/42` → `owner/repo`). Returns `None`
/// for URLs that don't match, so the id falls back to the bare-number form.
fn repo_from_url(url: &str) -> Option<String> {
    let rest = url.split("github.com/").nth(1)?;
    let mut parts = rest.trim_start_matches('/').split('/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// Split an issue id back into `(Some(owner/repo), number)`. Accepts both the
/// scoped `github:owner/repo#42` form (carries the repo so get/close/edit hit
/// the right repo) and the legacy bare `github:42` / `42` form (no repo).
fn split_id(id: &str) -> (Option<&str>, &str) {
    let body = id.strip_prefix("github:").unwrap_or(id);
    match body.rsplit_once('#') {
        Some((repo, number)) if !repo.is_empty() => (Some(repo), number),
        _ => (None, body),
    }
}

fn gh_issue_to_domain(gi: GhIssue) -> Issue {
    let status = match gi.state.as_str() {
        "CLOSED" => IssueStatus::Done,
        _ => IssueStatus::Todo,
    };
    // Carry owner/repo in the id (`github:owner/repo#N`) so later get/update/
    // search can pass `--repo` and never resolve `gh` against the process cwd —
    // which could close the wrong repo's issue. Falls back to the bare-number
    // form when the URL is unparseable.
    let id = match repo_from_url(&gi.url) {
        Some(repo) => format!("github:{repo}#{}", gi.number),
        None => format!("github:{}", gi.number),
    };
    Issue {
        id,
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
        // GitHub issues have no native due-date field (milestones carry one,
        // but a milestone date is not an issue deadline) — `due_at_ms` stays
        // `None`, so the `overdue` notification kind never fires for GitHub.
        ..Default::default()
    }
}

const GH_LIST_FIELDS: &str = "number,title,state,body,assignees,labels,url,updatedAt";

impl IssueBackend for GitHubIssuesBackend {
    fn provider_id(&self) -> &'static str {
        "github"
    }

    fn caps(&self) -> super::IssueCaps {
        super::IssueCaps::default()
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
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
            // Scope to a single repo (the repo-scoped "My Work" feed). Without this,
            // `gh issue list` falls back to the process cwd's repo, which is not the
            // active worktree — so unscoped fetches leak issues from other repos.
            if let Some(repo) = filter.repo.as_deref().filter(|r| !r.is_empty()) {
                args.extend(["--repo", repo]);
            }
            // Include extra flags configured by the user.
            let extra: Vec<&str> = self.extra_flags.iter().map(|s| s.as_str()).collect();
            args.extend(extra);

            let json = self.gh(&args)?;
            let issues: Vec<GhIssue> =
                serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
            Ok(issues.into_iter().map(gh_issue_to_domain).collect())
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            let (repo, number) = split_id(id);
            let mut args: Vec<&str> = vec![
                "issue",
                "view",
                number,
                "--json",
                "number,title,state,body,assignees,labels,url,updatedAt,comments",
            ];
            if let Some(repo) = repo {
                args.extend(["--repo", repo]);
            }
            let json = self.gh(&args)?;
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
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
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
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let (repo, number) = split_id(id);
            // Scope every mutation to the issue's own repo — without `--repo`, `gh`
            // resolves against the process cwd and can close/edit the wrong repo's
            // issue #N.
            let repo_flag: Vec<&str> = match repo {
                Some(r) => vec!["--repo", r],
                None => vec![],
            };

            // Handle status (open / close).
            if let Some(status) = patch.status {
                let sub = match status {
                    IssueStatus::Done | IssueStatus::Cancelled => "close",
                    _ => "reopen",
                };
                let mut args = vec!["issue", sub, number];
                args.extend_from_slice(&repo_flag);
                self.gh(&args)?;
            }

            // Handle title update.
            if let Some(title) = &patch.title {
                let mut args = vec!["issue", "edit", number, "--title", title];
                args.extend_from_slice(&repo_flag);
                self.gh(&args)?;
            }

            // Re-fetch the updated issue.
            let mut args = vec!["issue", "view", number, "--json", GH_LIST_FIELDS];
            args.extend_from_slice(&repo_flag);
            let json = self.gh(&args)?;
            let gi: GhIssue =
                serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
            Ok(gh_issue_to_domain(gi))
        })
    }

    fn search<'a>(
        &'a self,
        query_str: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let limit_str = limit.to_string();
            let mut args: Vec<&str> = vec![
                "issue",
                "list",
                "--search",
                query_str,
                "--json",
                GH_LIST_FIELDS,
                "--limit",
                &limit_str,
            ];
            // Apply the user's extra flags (e.g. `--repo owner/repo`) so search is
            // scoped the same way list_issues is, rather than falling back to cwd.
            let extra: Vec<&str> = self.extra_flags.iter().map(|s| s.as_str()).collect();
            args.extend(extra);
            let json = self.gh(&args)?;
            let issues: Vec<GhIssue> =
                serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
            Ok(issues.into_iter().map(gh_issue_to_domain).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_ms_valid_and_invalid() {
        assert_eq!(parse_ms(Some("1970-01-01T00:00:05Z")), 5000);
        assert_eq!(parse_ms(Some("nope")), 0);
        assert_eq!(parse_ms(None), 0);
    }

    #[test]
    fn issue_to_domain_open_maps_to_todo() {
        let gi: GhIssue = serde_json::from_value(json!({
            "number": 42,
            "title": "Open bug",
            "state": "OPEN",
            "body": "steps to repro",
            "assignees": [{ "login": "octocat" }],
            "labels": [{ "name": "bug" }, { "name": "p2" }],
            "url": "https://github.com/o/r/issues/42",
            "updatedAt": "1970-01-01T00:00:06Z"
        }))
        .unwrap();
        let issue = gh_issue_to_domain(gi);
        // The id now carries owner/repo so mutations can pass `--repo`.
        assert_eq!(issue.id, "github:o/r#42");
        assert_eq!(issue.number, "42");
        assert_eq!(issue.provider, "github");
        assert_eq!(issue.title, "Open bug");
        assert_eq!(issue.body.as_deref(), Some("steps to repro"));
        assert_eq!(issue.status, IssueStatus::Todo);
        assert_eq!(issue.priority, IssuePriority::None);
        assert_eq!(issue.assignees, vec!["octocat".to_string()]);
        assert_eq!(issue.labels, vec!["bug".to_string(), "p2".to_string()]);
        assert_eq!(issue.updated_at_ms, 6000);
    }

    #[test]
    fn repo_from_url_extracts_owner_repo() {
        assert_eq!(
            repo_from_url("https://github.com/o/r/issues/42").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            repo_from_url("https://github.com/my-org/my.repo/issues/1").as_deref(),
            Some("my-org/my.repo")
        );
        // Enterprise / non-github.com host or malformed URL ⇒ no repo (falls
        // back to bare-number id).
        assert_eq!(repo_from_url("https://example.com/o/r/issues/1"), None);
        assert_eq!(repo_from_url("not a url"), None);
        assert_eq!(repo_from_url("https://github.com/o"), None);
    }

    #[test]
    fn split_id_round_trips_scoped_and_bare_ids() {
        // Scoped id: repo is recovered for `--repo`, number is bare.
        assert_eq!(split_id("github:o/r#42"), (Some("o/r"), "42"));
        // Legacy bare ids (with or without prefix) carry no repo.
        assert_eq!(split_id("github:42"), (None, "42"));
        assert_eq!(split_id("42"), (None, "42"));
        // An id built from a real issue round-trips through split_id.
        let gi: GhIssue = serde_json::from_value(json!({
            "number": 99,
            "title": "t",
            "state": "OPEN",
            "url": "https://github.com/acme/widgets/issues/99"
        }))
        .unwrap();
        let id = gh_issue_to_domain(gi).id;
        assert_eq!(id, "github:acme/widgets#99");
        assert_eq!(split_id(&id), (Some("acme/widgets"), "99"));
    }

    #[test]
    fn issue_to_domain_closed_maps_to_done() {
        let gi: GhIssue = serde_json::from_value(json!({
            "number": 7,
            "title": "Closed",
            "state": "CLOSED",
            "url": "https://github.com/o/r/issues/7"
        }))
        .unwrap();
        let issue = gh_issue_to_domain(gi);
        assert_eq!(issue.status, IssueStatus::Done);
        assert_eq!(issue.body, None);
        assert!(issue.assignees.is_empty());
        assert!(issue.labels.is_empty());
        assert_eq!(issue.updated_at_ms, 0, "missing updatedAt ⇒ 0");
    }
}
