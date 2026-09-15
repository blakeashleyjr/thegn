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

    fn validate_extra_flags(&self) -> Result<(), IssueError> {
        let mut iter = self.extra_flags.iter();
        while let Some(flag) = iter.next() {
            let inline = flag
                .strip_prefix("--repo=")
                .or_else(|| flag.strip_prefix("-R="));
            if let Some(repo) = inline {
                super::identity::github_repo(repo).map_err(IssueError::Parse)?;
            } else if flag == "--repo" || flag == "-R" {
                let repo = iter.next().ok_or_else(|| {
                    IssueError::Parse("GitHub --repo/-R flag requires owner/repo".into())
                })?;
                super::identity::github_repo(repo).map_err(IssueError::Parse)?;
            }
        }
        Ok(())
    }

    /// Resolve the repository authority that this invocation actually asked
    /// `gh` to use. A scoped filter is part of the invocation, and a later
    /// configured `--repo` flag wins because it is appended later to argv.
    /// Response URLs are checked against this admitted authority; they never
    /// get to choose their own host.
    fn effective_repo(&self, filter_repo: Option<&str>) -> Result<Option<String>, IssueError> {
        self.validate_extra_flags()?;
        let mut selected = filter_repo
            .filter(|repo| !repo.is_empty())
            .map(str::to_owned);
        let mut flags = self.extra_flags.iter();
        while let Some(flag) = flags.next() {
            if let Some(repo) = flag
                .strip_prefix("--repo=")
                .or_else(|| flag.strip_prefix("-R="))
            {
                selected = Some(repo.to_owned());
            } else if flag == "--repo" || flag == "-R" {
                selected = Some(
                    flags
                        .next()
                        .ok_or_else(|| {
                            IssueError::Parse("GitHub --repo/-R flag requires owner/repo".into())
                        })?
                        .to_owned(),
                );
            }
        }
        if let Some(repo) = selected.as_deref() {
            super::identity::github_repo(repo).map_err(IssueError::Parse)?;
        }
        Ok(selected)
    }

    fn effective_host(&self, filter_repo: Option<&str>) -> Result<Option<String>, IssueError> {
        let repo = self.effective_repo(filter_repo)?;
        self.host_for_repo(repo.as_deref())
    }

    fn host_for_repo(&self, repo: Option<&str>) -> Result<Option<String>, IssueError> {
        if let Some(repo) = repo {
            let parts: Vec<&str> = repo.split('/').collect();
            if let [host, _, _] = parts.as_slice() {
                return Ok(Some((*host).to_owned()));
            }
        }
        let host = std::env::var("GH_HOST").ok().filter(|h| !h.is_empty());
        if let Some(host) = host.as_deref() {
            super::identity::github_host_for_url(host).map_err(IssueError::Parse)?;
        }
        Ok(host)
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
    s.and_then(|s| match chrono::DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Some(dt),
        Err(_) => None,
    })
    .map(|dt| dt.timestamp_millis())
    .unwrap_or(0)
}

/// Extract `owner/repo` from a GitHub issue/PR URL.  The authority is parsed
/// structurally so a lookalike such as `github.com.attacker/…` cannot become a
/// repository. `GH_HOST` keeps the `gh` CLI's enterprise host convention.
fn issue_repo_number_from_url(
    url: &str,
    configured_host: Option<&str>,
) -> Option<(String, String)> {
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => return None,
    };
    // Query and fragment are part of a public browse URL; they do not
    // change the admitted authority or repository path.
    if !matches!(parsed.scheme(), "https" | "http")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.port().is_some()
    {
        return None;
    }
    let host = parsed.host_str()?;
    let expected = configured_host.unwrap_or("github.com").trim();
    if super::identity::github_host_for_url(expected).is_err()
        || !host.eq_ignore_ascii_case(expected)
    {
        return None;
    }
    let parts: Vec<&str> = parsed.path_segments()?.collect();
    if parts.len() != 4 || !matches!(parts[2], "issues" | "pull") {
        return None;
    }
    if parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let repo = if configured_host.is_some() && !host.eq_ignore_ascii_case("github.com") {
        format!("{host}/{}/{}", parts[0], parts[1])
    } else {
        format!("{}/{}", parts[0], parts[1])
    };
    if super::identity::github_repo(&repo).is_err()
        || super::identity::github_number(parts[3]).is_err()
    {
        return None;
    }
    Some((repo, parts[3].to_string()))
}

#[cfg(test)]
fn repo_from_url_with_host(url: &str, configured_host: Option<&str>) -> Option<String> {
    issue_repo_number_from_url(url, configured_host).map(|(repo, _)| repo)
}

fn validated_repo_number_from_url(
    url: &str,
    expected_host: Option<&str>,
) -> Result<(String, String), IssueError> {
    issue_repo_number_from_url(url, expected_host).ok_or_else(|| {
        IssueError::Parse("GitHub issue URL is not a valid configured host/repo issue route".into())
    })
}

/// Split an issue id back into `(Some(owner/repo), number)`. Accepts both the
/// scoped `github:owner/repo#42` form (carries the repo so get/close/edit hit
/// the right repo) and the legacy bare `github:42` / `42` form (no repo).
fn split_id(id: &str) -> Result<(Option<&str>, &str), IssueError> {
    let body = id.strip_prefix("github:").unwrap_or(id);
    match body.rsplit_once('#') {
        Some((repo, number)) if !repo.is_empty() => {
            super::identity::github_repo(repo).map_err(IssueError::Parse)?;
            super::identity::github_number(number).map_err(IssueError::Parse)?;
            Ok((Some(repo), number))
        }
        Some(_) => Err(IssueError::Parse("malformed scoped GitHub issue id".into())),
        None => {
            super::identity::github_number(body).map_err(IssueError::Parse)?;
            Ok((None, body))
        }
    }
}

fn gh_issue_to_domain_with_host(
    gi: GhIssue,
    configured_host: Option<&str>,
) -> Result<Issue, IssueError> {
    let number = gi.number.to_string();
    super::identity::github_number(&number).map_err(IssueError::Parse)?;
    let status = match gi.state.as_str() {
        "CLOSED" => IssueStatus::Done,
        _ => IssueStatus::Todo,
    };
    // Carry owner/repo in the id (`github:owner/repo#N`) so later get/update/
    // search can pass `--repo` and never resolve `gh` against the process cwd —
    // which could close the wrong repo's issue. An unparseable response URL is
    // rejected above rather than downgraded to an unscoped number.
    let (repo, url_number) = issue_repo_number_from_url(&gi.url, configured_host)
        .ok_or_else(|| IssueError::Parse("GitHub issue URL has invalid authority/route".into()))?;
    if url_number != number {
        return Err(IssueError::Parse(
            "GitHub issue number does not match its response URL".into(),
        ));
    }
    Ok(Issue {
        id: format!("github:{repo}#{number}"),
        number: number.clone(),
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
    })
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
            let expected_host = self.effective_host(filter.repo.as_deref())?;
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
                super::identity::github_repo(repo).map_err(IssueError::Parse)?;
                args.extend(["--repo", repo]);
            }
            // Include extra flags configured by the user.
            self.validate_extra_flags()?;
            let extra: Vec<&str> = self.extra_flags.iter().map(|s| s.as_str()).collect();
            args.extend(extra);

            let json = self.gh(&args)?;
            let issues: Vec<GhIssue> =
                serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
            issues
                .into_iter()
                .map(|issue| gh_issue_to_domain_with_host(issue, expected_host.as_deref()))
                .collect()
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            let (repo, number) = split_id(id)?;
            let expected_host = self.host_for_repo(repo)?;
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
                issue: gh_issue_to_domain_with_host(detail.issue, expected_host.as_deref())?,
                comments,
            })
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let expected_host = self.host_for_repo(None)?;
            let mut args = vec!["issue", "create", "--title", &draft.title];
            let body_val;
            if let Some(body) = &draft.body {
                body_val = body.clone();
                args.extend(["--body", &body_val]);
            } else {
                args.extend(["--body", ""]);
            }
            // Creation currently follows gh's anchored-directory contract;
            // account/draft repository precedence is tracked by THE-315.
            // Validate the printed URL and retain its exact repository for
            // the follow-up view, with no malformed-output cwd fallback.
            let url = self.gh(&args)?.trim().to_string();
            let (repo, number) = validated_repo_number_from_url(&url, expected_host.as_deref())?;
            let json = self.gh(&[
                "issue",
                "view",
                &number,
                "--repo",
                &repo,
                "--json",
                GH_LIST_FIELDS,
            ])?;
            let gi: GhIssue =
                serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
            gh_issue_to_domain_with_host(gi, expected_host.as_deref())
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let (repo, number) = split_id(id)?;
            let expected_host = self.host_for_repo(repo)?;
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
            gh_issue_to_domain_with_host(gi, expected_host.as_deref())
        })
    }

    fn search<'a>(
        &'a self,
        query_str: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let expected_host = self.effective_host(None)?;
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
            self.validate_extra_flags()?;
            let extra: Vec<&str> = self.extra_flags.iter().map(|s| s.as_str()).collect();
            args.extend(extra);
            let json = self.gh(&args)?;
            let issues: Vec<GhIssue> =
                serde_json::from_str(&json).map_err(|e| IssueError::Parse(e.to_string()))?;
            issues
                .into_iter()
                .map(|issue| gh_issue_to_domain_with_host(issue, expected_host.as_deref()))
                .collect()
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
        let issue = gh_issue_to_domain_with_host(gi, Some("github.com")).unwrap();
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
            repo_from_url_with_host("https://github.com/o/r/issues/42", Some("github.com"))
                .as_deref(),
            Some("o/r")
        );
        assert_eq!(
            repo_from_url_with_host(
                "https://github.com/my-org/my.repo/issues/1",
                Some("github.com")
            )
            .as_deref(),
            Some("my-org/my.repo")
        );
        // A configured enterprise host is retained in the scoped repository
        // identity so later `gh --repo` calls preserve the authority.
        assert_eq!(
            repo_from_url_with_host("https://example.com/o/r/issues/1", Some("github.com")),
            None
        );
        assert_eq!(
            repo_from_url_with_host("not a url", Some("github.com")),
            None
        );
        assert_eq!(
            repo_from_url_with_host("https://github.com/o", Some("github.com")),
            None
        );
        assert_eq!(
            repo_from_url_with_host("https://github.com:443/o/r/issues/1", Some("github.com")),
            Some("o/r".into())
        );
        assert_eq!(
            repo_from_url_with_host("https://github.com:8443/o/r/issues/1", Some("github.com")),
            None
        );
        assert_eq!(
            repo_from_url_with_host(
                "https://github.com/o/r/issues/1?x=1#comment",
                Some("github.com")
            ),
            Some("o/r".into())
        );
        assert_eq!(
            repo_from_url_with_host("https://github.com/o/r/tree/1", Some("github.com")),
            None
        );
        assert_eq!(
            repo_from_url_with_host("https://ghe.example/o/r/issues/1", Some("ghe.example")),
            Some("ghe.example/o/r".into())
        );
        assert_eq!(
            repo_from_url_with_host("https://ghe.example/o//r/issues/1", Some("ghe.example")),
            None
        );
    }

    #[test]
    fn explicit_enterprise_scope_admits_response_without_ambient_gh_host() {
        let backend = GitHubIssuesBackend::new(vec!["--repo".into(), "ghe.example/o/r".into()]);
        assert_eq!(
            backend.effective_host(None).unwrap().as_deref(),
            Some("ghe.example")
        );
        let unconfigured = GitHubIssuesBackend::new(Vec::new());
        assert_eq!(
            unconfigured
                .effective_host(Some("ghe.example/o/r"))
                .unwrap(),
            Some("ghe.example".into())
        );
        assert_eq!(
            unconfigured.host_for_repo(Some("ghe.example/o/r")).unwrap(),
            Some("ghe.example".into())
        );
        let gi: GhIssue = serde_json::from_value(json!({
            "number": 7,
            "title": "enterprise",
            "state": "OPEN",
            "url": "https://ghe.example/o/r/issues/7?view=full#top"
        }))
        .unwrap();
        let issue = gh_issue_to_domain_with_host(gi, Some("ghe.example")).unwrap();
        assert_eq!(issue.id, "github:ghe.example/o/r#7");
        let wrong: GhIssue = serde_json::from_value(json!({
            "number": 7,
            "title": "wrong host",
            "state": "OPEN",
            "url": "https://github.com/o/r/issues/7"
        }))
        .unwrap();
        assert!(gh_issue_to_domain_with_host(wrong, Some("ghe.example")).is_err());
    }

    #[test]
    fn extra_repo_flag_forms_are_checked() {
        let mut backend = GitHubIssuesBackend::new(vec!["-R".into(), "o/r".into()]);
        assert!(backend.validate_extra_flags().is_ok());
        backend.extra_flags = vec!["--repo=o/r".into()];
        assert!(backend.validate_extra_flags().is_ok());
        backend.extra_flags = vec!["--repo".into(), "ghe.example/o/.github".into()];
        assert!(backend.validate_extra_flags().is_ok());
        backend.extra_flags = vec!["-R=../r".into()];
        assert!(backend.validate_extra_flags().is_err());
    }

    #[test]
    fn split_id_round_trips_scoped_and_bare_ids() {
        // Scoped id: repo is recovered for `--repo`, number is bare.
        assert_eq!(split_id("github:o/r#42").unwrap(), (Some("o/r"), "42"));
        // Legacy bare ids (with or without prefix) carry no repo.
        assert_eq!(split_id("github:42").unwrap(), (None, "42"));
        assert_eq!(split_id("42").unwrap(), (None, "42"));
        // An id built from a real issue round-trips through split_id.
        let gi: GhIssue = serde_json::from_value(json!({
            "number": 99,
            "title": "t",
            "state": "OPEN",
            "url": "https://github.com/acme/widgets/issues/99"
        }))
        .unwrap();
        let id = gh_issue_to_domain_with_host(gi, Some("github.com"))
            .unwrap()
            .id;
        assert_eq!(id, "github:acme/widgets#99");
        assert_eq!(split_id(&id).unwrap(), (Some("acme/widgets"), "99"));
        assert!(split_id("github:acme/widgets#0").is_err());
        assert!(split_id("github:acme/widgets#42#43").is_err());
    }

    #[test]
    fn malformed_response_url_does_not_downgrade_to_bare_id() {
        let gi: GhIssue = serde_json::from_value(json!({
            "number": 42,
            "title": "bad authority",
            "state": "OPEN",
            "url": "https://github.com.attacker/o/r/issues/42"
        }))
        .unwrap();
        assert!(gh_issue_to_domain_with_host(gi, Some("github.com")).is_err());
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
        let issue = gh_issue_to_domain_with_host(gi, Some("github.com")).unwrap();
        assert_eq!(issue.status, IssueStatus::Done);
        assert_eq!(issue.body, None);
        assert!(issue.assignees.is_empty());
        assert!(issue.labels.is_empty());
        assert_eq!(issue.updated_at_ms, 0, "missing updatedAt ⇒ 0");
    }
}
