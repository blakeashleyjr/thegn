//! GitHub integration via the `gh` CLI. The native binary is the data/action
//! provider; the WASM panel plugin renders what we emit and triggers our action
//! subcommands (it can't shell out itself).
//!
//! Everything runs with `cwd = worktree` so `gh` auto-detects the repo from its
//! remote (mirrors how `util::git_out` uses `-C dir`). All failure modes the
//! panel cares about are mapped onto `PanelState` so the UI never crashes.

use crate::remote::GitLoc;
use serde::{Deserialize, Serialize};

/// Distinguishable `gh` failure modes.
#[derive(Debug)]
pub enum GhError {
    NotInstalled,
    NotAuthenticated,
    NoPr,
    RateLimited,
    /// Transient network failure (DNS, TCP connect, TLS). Separate from `Other`
    /// so the UI can show "GitHub unreachable" and callers can circuit-break.
    Offline,
    Other(String),
}

/// How to merge a PR.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    fn flag(self) -> &'static str {
        match self {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

/// Run `gh <args>` with `cwd = worktree` (local, or over ssh on the remote host);
/// trimmed stdout on success, else a classified error.
pub fn gh_out(loc: &GitLoc, args: &[&str]) -> Result<String, GhError> {
    let out = loc
        .gh_command(args)
        .output()
        .map_err(|e| GhError::Other(e.to_string()))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(classify(
        &String::from_utf8_lossy(&out.stderr).to_lowercase(),
    ))
}

/// Run `gh <args>` for its exit code (output discarded). Errors classified.
pub fn gh_run(loc: &GitLoc, args: &[&str]) -> Result<(), GhError> {
    let out = loc
        .gh_command(args)
        .output()
        .map_err(|e| GhError::Other(e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(classify(
            &String::from_utf8_lossy(&out.stderr).to_lowercase(),
        ))
    }
}

fn classify(stderr: &str) -> GhError {
    if stderr.contains("command not found")
        || stderr.contains("not found")
        || stderr.contains("no such file")
    {
        GhError::NotInstalled
    } else if stderr.contains("no pull requests found")
        || stderr.contains("no default remote repository")
        || stderr.contains("no open pull request")
        || stderr.contains("no pr ")
    {
        GhError::NoPr
    } else if stderr.contains("not logged")
        || stderr.contains("authentication")
        || stderr.contains("gh auth login")
        || stderr.contains("http 401")
    {
        GhError::NotAuthenticated
    } else if stderr.contains("rate limit") || stderr.contains("api rate") {
        GhError::RateLimited
    } else {
        GhError::Other(stderr.trim().to_string())
    }
}

impl GhError {
    fn message(&self) -> String {
        match self {
            GhError::NotInstalled => "gh CLI not installed".into(),
            GhError::NotAuthenticated => "gh not authenticated (run: gh auth login)".into(),
            GhError::NoPr => "no PR for this branch".into(),
            GhError::RateLimited => "GitHub API rate limited".into(),
            GhError::Offline => "GitHub unreachable".into(),
            GhError::Other(m) => m.clone(),
        }
    }

    /// Whether this is a transient network error (as opposed to a permanent
    /// config/auth issue). Used by the circuit breaker in `GhNative`.
    pub fn is_transient(&self) -> bool {
        matches!(self, GhError::Offline)
    }
}

// --- serde model ----------------------------------------------------------

/// The full panel feed for one worktree (flattened state + metadata).
/// Round-trips through the `pr_cache` table; every extension field is
/// `#[serde(default)]` so old cached rows keep deserializing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrPanel {
    #[serde(flatten)]
    pub state: PanelState,
    pub worktree: String,
    pub branch: String,
    pub fetched_at: i64,
    /// Review threads of the open PR (unresolved first), best-effort.
    #[serde(default)]
    pub threads: Vec<ReviewThreadRow>,
    /// Open repo issues (a small page), best-effort.
    #[serde(default)]
    pub issues: Vec<IssueRow>,
}

/// The per-worktree PR state, internally tagged by `kind` for the plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelState {
    NoGh,
    NotAuthenticated,
    NoPr,
    RateLimited,
    /// GitHub API was unreachable (network partition, no egress). Stale cached
    /// data may still be shown; the panel distinguishes this from a permanent
    /// error so the chrome can render "unreachable" rather than a raw error.
    Offline,
    Error {
        message: String,
    },
    Pr(Box<PrStatus>),
}

/// One review thread, flattened to its first comment for the panel rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewThreadRow {
    pub author: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    /// First-comment excerpt (single line, capped).
    pub snippet: String,
    pub resolved: bool,
    #[serde(default)]
    pub created_at: String,
}

/// One open issue for the panel's ISSUES block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueRow {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Deserialized from `gh pr view --json …`, plus a computed checks rollup.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrStatus {
    pub number: u64,
    pub title: String,
    pub state: String, // OPEN | CLOSED | MERGED
    pub url: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub base_ref_name: String,
    #[serde(default)]
    pub mergeable: String,
    #[serde(default)]
    pub merge_state_status: String,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub status_check_rollup: Vec<CheckRun>,
    /// Computed by `pr_status` (ignored on input, emitted on output).
    #[serde(default, skip_deserializing)]
    pub checks: ChecksSummary,
}

/// One entry of `statusCheckRollup` — heterogeneous (CheckRun vs StatusContext).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String, // CheckRun: QUEUED | IN_PROGRESS | COMPLETED
    #[serde(default)]
    pub conclusion: Option<String>, // CheckRun: SUCCESS | FAILURE | …
    #[serde(default)]
    pub state: Option<String>, // StatusContext: SUCCESS | PENDING | FAILURE | ERROR
    #[serde(default)]
    pub workflow_name: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    /// RFC3339 start/finish stamps (CheckRun shape) — per-check durations.
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

impl CheckRun {
    /// Seconds the check ran (completed) or has been running (started only,
    /// measured against `now` epoch seconds). `None` without a start stamp.
    pub fn duration_secs(&self, now: i64) -> Option<i64> {
        let parse = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.timestamp())
        };
        let start = self.started_at.as_deref().and_then(parse)?;
        let end = self.completed_at.as_deref().and_then(parse).unwrap_or(now);
        Some((end - start).max(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Pass,
    Fail,
    Pending,
}

/// Normalize a check entry into pass/fail/pending (handles both shapes).
pub fn check_bucket(c: &CheckRun) -> Bucket {
    if let Some(con) = c.conclusion.as_deref() {
        return match con.to_uppercase().as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => Bucket::Pass,
            "" => Bucket::Pending,
            _ => Bucket::Fail, // FAILURE, TIMED_OUT, CANCELLED, ACTION_REQUIRED, …
        };
    }
    if let Some(st) = c.state.as_deref() {
        return match st.to_uppercase().as_str() {
            "SUCCESS" => Bucket::Pass,
            "FAILURE" | "ERROR" => Bucket::Fail,
            _ => Bucket::Pending, // PENDING, EXPECTED
        };
    }
    Bucket::Pending
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ChecksSummary {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub total: u32,
}

impl PrStatus {
    /// Recompute the checks rollup from `status_check_rollup`. The CLI path does
    /// this inline after deserializing; the octocrab native path (superzej-svc)
    /// calls this so both produce an identical summary.
    pub fn recompute_checks(&mut self) {
        self.checks = summarize(&self.status_check_rollup);
    }
}

fn summarize(runs: &[CheckRun]) -> ChecksSummary {
    let mut s = ChecksSummary::default();
    for r in runs {
        s.total += 1;
        match check_bucket(r) {
            Bucket::Pass => s.passed += 1,
            Bucket::Fail => s.failed += 1,
            Bucket::Pending => s.pending += 1,
        }
    }
    s
}

const PR_FIELDS: &str = "number,title,state,url,isDraft,headRefName,baseRefName,\
                         mergeable,mergeStateStatus,reviewDecision,statusCheckRollup";

/// Fetch the PR state for a worktree, mapping every failure mode to a PanelState.
pub fn pr_status(loc: &GitLoc) -> PrPanel {
    let branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_default();
    let state = match gh_out(loc, &["pr", "view", "--json", PR_FIELDS]) {
        Ok(json) => match serde_json::from_str::<PrStatus>(&json) {
            Ok(mut pr) => {
                pr.checks = summarize(&pr.status_check_rollup);
                PanelState::Pr(Box::new(pr))
            }
            Err(e) => PanelState::Error {
                message: format!("parse error: {e}"),
            },
        },
        Err(GhError::NotInstalled) => PanelState::NoGh,
        Err(GhError::NotAuthenticated) => PanelState::NotAuthenticated,
        Err(GhError::NoPr) => PanelState::NoPr,
        Err(GhError::RateLimited) => PanelState::RateLimited,
        Err(GhError::Offline) => PanelState::Offline,
        Err(GhError::Other(m)) => PanelState::Error { message: m },
    };
    PrPanel {
        state,
        worktree: loc.path(),
        branch,
        fetched_at: crate::util::now(),
        threads: Vec::new(),
        issues: Vec::new(),
    }
}

/// As [`pr_status`], plus best-effort review threads + open issues — the
/// background cache-refresh feed. Extra fetches never fail the panel: any
/// error just leaves the corresponding list empty.
pub fn pr_status_full(loc: &GitLoc) -> PrPanel {
    let mut panel = pr_status(loc);
    if let PanelState::Pr(pr) = &panel.state
        && let Some((owner, repo)) = owner_repo_from_url(&pr.url)
    {
        panel.threads = review_threads(loc, &owner, &repo, pr.number).unwrap_or_default();
    }
    panel.issues = issue_list(loc, 10).unwrap_or_default();
    panel
}

/// One open PR's identifying header — the per-branch PR-badge feed
/// (`gh pr list`), cached as a JSON array in `pr_branch_cache`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrHeader {
    pub number: u64,
    #[serde(rename = "headRefName")]
    pub head_ref: String,
    pub state: String,
    pub url: String,
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
}

/// Parse the cached/CLI JSON array of PR headers (empty on any mismatch).
pub fn parse_pr_headers(json: &str) -> Vec<PrHeader> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The repo's open PRs, one header per branch
/// (`gh pr list --json … --limit <n>`).
pub fn pr_list(loc: &GitLoc, limit: usize) -> Result<Vec<PrHeader>, GhError> {
    let limit = limit.to_string();
    let json = gh_out(
        loc,
        &[
            "pr",
            "list",
            "--json",
            "number,headRefName,state,url,isDraft",
            "--limit",
            &limit,
        ],
    )?;
    Ok(parse_pr_headers(&json))
}

/// The state (`OPEN`/`MERGED`/`CLOSED`) of the PR for `branch`, via
/// `gh pr view <branch> --json state`. `None` when there's no PR or `gh`
/// fails. Used by the on-merge auto-clean to resolve the precise outcome when a
/// branch drops out of the open-PR set (merged vs closed-without-merge).
pub fn pr_state_for_branch(loc: &GitLoc, branch: &str) -> Option<String> {
    let json = gh_out(loc, &["pr", "view", branch, "--json", "state"]).ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    v.get("state")?.as_str().map(str::to_string)
}

/// One PR from a cross-repo `gh search prs` — the unified "My Work" feed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrSearchRow {
    pub number: u64,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub repository: PrSearchRepo,
}

/// The `repository` object in a `gh search prs` row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PrSearchRepo {
    #[serde(rename = "nameWithOwner", default)]
    pub name_with_owner: String,
}

/// Parse the JSON array from `gh search prs --json …` (empty on mismatch).
pub fn parse_pr_search(json: &str) -> Vec<PrSearchRow> {
    serde_json::from_str(json).unwrap_or_default()
}

/// PR search for the unified work feed. `role_flag` is a single `gh search prs`
/// selector such as `"--review-requested=@me"` or `"--author=@me"`; results are
/// restricted to open PRs. When `repo` is `Some("owner/repo")` the search is
/// scoped to that repository (the default, repo-scoped feed); `None` searches
/// across every repo the user touches (the "all" toggle). `loc` supplies the
/// `gh` invocation context.
pub fn search_prs(
    loc: &GitLoc,
    role_flag: &str,
    repo: Option<&str>,
    limit: usize,
) -> Result<Vec<PrSearchRow>, GhError> {
    let limit = limit.to_string();
    let mut args: Vec<String> = vec![
        "search".into(),
        "prs".into(),
        role_flag.into(),
        "--state=open".into(),
        "--json".into(),
        "number,title,url,state,repository".into(),
        "--limit".into(),
        limit,
    ];
    if let Some(nwo) = repo.filter(|r| !r.is_empty()) {
        args.push(format!("--repo={nwo}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let json = gh_out(loc, &argv)?;
    Ok(parse_pr_search(&json))
}

/// The `owner/repo` (nameWithOwner) of a worktree's `origin` remote, or `None`
/// when there is no origin or it is not a recognizable forge URL. Used to scope
/// the "My Work" feed / PR search to the current repository.
pub fn origin_nwo(loc: &GitLoc) -> Option<String> {
    let out = loc
        .git_command(&["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    nwo_from_remote_url(&url)
}

/// Parse `owner/repo` from any git remote URL form: `https://host/owner/repo`,
/// `ssh://git@host/owner/repo`, or the scp-like `git@host:owner/repo` — with an
/// optional trailing `.git`. Forge-host agnostic (mirrors [`owner_repo_from_url`]).
pub fn nwo_from_remote_url(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    // Drop the scheme+host (`scheme://host/…`) or the scp `git@host:` prefix,
    // leaving the `owner/repo[/…]` path.
    let path = if let Some((_, rest)) = s.split_once("://") {
        rest.split_once('/').map(|(_, p)| p)?
    } else if let Some((_, rest)) = s.split_once(':') {
        rest
    } else {
        return None;
    };
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Open the PR belonging to `branch` in the browser
/// (`gh pr view <branch> --web`) — the fallback when no cached URL exists.
pub fn open_pr_for_branch(loc: &GitLoc, branch: &str) -> Result<(), GhError> {
    gh_run(loc, &["pr", "view", branch, "--web"])
}

/// `(owner, repo)` from a GitHub PR/issue/repo URL
/// (`https://github.com/OWNER/REPO[/...]`). Forge-host agnostic: any host
/// with the same path shape parses.
pub fn owner_repo_from_url(url: &str) -> Option<(String, String)> {
    let rest = url.split("://").nth(1)?;
    let mut parts = rest.split('/');
    let _host = parts.next()?;
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

const THREADS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
reviewThreads(first:20){nodes{isResolved comments(first:1){nodes{\
author{login} path line body createdAt}}}}}}}";

/// Fetch the PR's review threads via `gh api graphql` (the `pr view` JSON
/// fields don't expose threads).
pub fn review_threads(
    loc: &GitLoc,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Vec<ReviewThreadRow>, GhError> {
    let num = number.to_string();
    let owner_arg = format!("owner={owner}");
    let name_arg = format!("name={repo}");
    let num_arg = format!("number={num}");
    let query_arg = format!("query={THREADS_QUERY}");
    let json = gh_out(
        loc,
        &[
            "api", "graphql", "-f", &query_arg, "-f", &owner_arg, "-f", &name_arg, "-F", &num_arg,
        ],
    )?;
    Ok(parse_review_threads(&json))
}

/// Parse the GraphQL reviewThreads response into rows (unresolved first).
pub fn parse_review_threads(json: &str) -> Vec<ReviewThreadRow> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let nodes = v
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(|n| n.as_array());
    let Some(nodes) = nodes else {
        return Vec::new();
    };
    let mut rows: Vec<ReviewThreadRow> = nodes
        .iter()
        .filter_map(|t| {
            let resolved = t
                .get("isResolved")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let c = t.pointer("/comments/nodes/0")?;
            let body = c.get("body").and_then(|s| s.as_str()).unwrap_or_default();
            let snippet: String = body
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            Some(ReviewThreadRow {
                author: c
                    .pointer("/author/login")
                    .and_then(|s| s.as_str())
                    .unwrap_or("?")
                    .to_string(),
                path: c
                    .get("path")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
                line: c.get("line").and_then(|n| n.as_u64()),
                snippet,
                resolved,
                created_at: c
                    .get("createdAt")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect();
    rows.sort_by_key(|r| r.resolved);
    rows
}

/// Fetch a small page of open issues (`gh issue list --json …`).
pub fn issue_list(loc: &GitLoc, limit: usize) -> Result<Vec<IssueRow>, GhError> {
    let limit = limit.to_string();
    let json = gh_out(
        loc,
        &[
            "issue",
            "list",
            "--json",
            "number,title,labels",
            "--limit",
            &limit,
        ],
    )?;
    Ok(parse_issue_list(&json))
}

/// Parse `gh issue list --json number,title,labels` output.
pub fn parse_issue_list(json: &str) -> Vec<IssueRow> {
    let v: Vec<serde_json::Value> = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    v.iter()
        .filter_map(|i| {
            Some(IssueRow {
                number: i.get("number")?.as_u64()?,
                title: i.get("title")?.as_str()?.to_string(),
                labels: i
                    .get("labels")
                    .and_then(|l| l.as_array())
                    .map(|l| {
                        l.iter()
                            .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

// --- actions --------------------------------------------------------------

/// Options for `create_pr`.
pub struct CreateOpts {
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub draft: bool,
    pub web: bool,
    pub fill: bool,
}

pub fn create_pr(loc: &GitLoc, o: &CreateOpts) -> Result<String, GhError> {
    let mut args: Vec<String> = vec!["pr".into(), "create".into()];
    if o.fill {
        args.push("--fill".into());
    }
    if o.draft {
        args.push("--draft".into());
    }
    if o.web {
        args.push("--web".into());
    }
    if let Some(t) = &o.title {
        args.push("--title".into());
        args.push(t.clone());
    }
    if let Some(b) = &o.body {
        args.push("--body".into());
        args.push(b.clone());
    }
    if let Some(b) = &o.base {
        args.push("--base".into());
        args.push(b.clone());
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    gh_out(loc, &refs)
}

pub fn open_pr(loc: &GitLoc) -> Result<(), GhError> {
    gh_run(loc, &["pr", "view", "--web"])
}

pub fn approve_pr(loc: &GitLoc, body: Option<&str>) -> Result<(), GhError> {
    let mut args = vec!["pr", "review", "--approve"];
    if let Some(b) = body {
        args.push("--body");
        args.push(b);
    }
    gh_run(loc, &args)
}

pub fn merge_pr(
    loc: &GitLoc,
    method: MergeMethod,
    delete_branch: bool,
    auto: bool,
) -> Result<(), GhError> {
    let mut args = vec!["pr", "merge", method.flag()];
    if delete_branch {
        args.push("--delete-branch");
    }
    if auto {
        args.push("--auto");
    }
    gh_run(loc, &args)
}

/// Convert the worktree's PR to a draft (`draft = true`) or mark it ready for
/// review (`draft = false`). Ported from the forge-features work.
pub fn set_draft_pr(loc: &GitLoc, draft: bool) -> Result<(), GhError> {
    let flag = if draft { "--undo" } else { "" };
    // `gh pr ready` marks ready; `gh pr ready --undo` converts back to draft.
    let mut args = vec!["pr", "ready"];
    if !flag.is_empty() {
        args.push(flag);
    }
    gh_run(loc, &args)
}

/// Enable (`enable = true`) or disable auto-merge for the worktree's PR.
pub fn set_auto_merge(loc: &GitLoc, enable: bool) -> Result<(), GhError> {
    let args = if enable {
        vec!["pr", "merge", "--auto", "--squash"]
    } else {
        vec!["pr", "merge", "--disable-auto"]
    };
    gh_run(loc, &args)
}

/// Print review comments / reviews as JSON.
pub fn reviews(loc: &GitLoc) -> Result<String, GhError> {
    gh_out(
        loc,
        &["pr", "view", "--json", "reviews,latestReviews,comments"],
    )
}

/// Re-run failed workflow runs for the worktree's branch. Returns the count.
pub fn rerun_failed_checks(loc: &GitLoc) -> Result<u32, GhError> {
    let branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .ok_or_else(|| GhError::Other("could not resolve branch".into()))?;
    // Enumerate this branch's workflow runs and re-run any that failed.
    let json = gh_out(
        loc,
        &[
            "run",
            "list",
            "--branch",
            &branch,
            "--json",
            "databaseId,conclusion",
            "--limit",
            "20",
        ],
    )?;
    #[derive(Deserialize)]
    struct Run {
        #[serde(rename = "databaseId")]
        database_id: u64,
        conclusion: Option<String>,
    }
    let runs: Vec<Run> = serde_json::from_str(&json).unwrap_or_default();
    let mut count = 0;
    for r in runs {
        if matches!(
            r.conclusion.as_deref().map(|s| s.to_uppercase()).as_deref(),
            Some("FAILURE") | Some("TIMED_OUT") | Some("CANCELLED") | Some("STARTUP_FAILURE")
        ) {
            let id = r.database_id.to_string();
            if gh_run(loc, &["run", "rerun", &id, "--failed"]).is_ok() {
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Short human-readable description of an error (for CLI output).
pub fn describe(e: &GhError) -> String {
    e.message()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_search_reads_repo_with_owner() {
        let json = r#"[
            {"number":42,"title":"Fix bug","url":"https://github.com/acme/widget/pull/42",
             "state":"OPEN","repository":{"name":"widget","nameWithOwner":"acme/widget"}},
            {"number":7,"title":"Docs","url":"https://github.com/acme/site/pull/7",
             "state":"OPEN","repository":{"nameWithOwner":"acme/site"}}
        ]"#;
        let rows = parse_pr_search(json);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 42);
        assert_eq!(rows[0].repository.name_with_owner, "acme/widget");
        assert_eq!(rows[1].title, "Docs");
        // Malformed input degrades to empty, never panics.
        assert!(parse_pr_search("not json").is_empty());
    }

    fn cr(status: &str, conclusion: Option<&str>, state: Option<&str>) -> CheckRun {
        CheckRun {
            name: "ci".into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
            state: state.map(String::from),
            workflow_name: None,
            details_url: None,
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn check_duration_from_stamps() {
        let mut c = cr("COMPLETED", Some("SUCCESS"), None);
        assert_eq!(c.duration_secs(0), None); // no start stamp
        c.started_at = Some("2026-06-11T10:00:00Z".into());
        c.completed_at = Some("2026-06-11T10:02:41Z".into());
        assert_eq!(c.duration_secs(0), Some(161));
        // Running check: measured against `now`.
        c.completed_at = None;
        let start = chrono::DateTime::parse_from_rfc3339("2026-06-11T10:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(c.duration_secs(start + 72), Some(72));
        // Clock skew never yields negative durations.
        assert_eq!(c.duration_secs(start - 100), Some(0));
        // Garbage stamps degrade to None.
        c.started_at = Some("not-a-date".into());
        assert_eq!(c.duration_secs(0), None);
    }

    #[test]
    fn owner_repo_parses_pr_and_repo_urls() {
        assert_eq!(
            owner_repo_from_url("https://github.com/acme/superzej/pull/142"),
            Some(("acme".into(), "superzej".into()))
        );
        assert_eq!(
            owner_repo_from_url("https://github.com/acme/superzej"),
            Some(("acme".into(), "superzej".into()))
        );
        assert_eq!(
            owner_repo_from_url("https://ghe.corp.example/org/repo/pull/1"),
            Some(("org".into(), "repo".into()))
        );
        assert_eq!(owner_repo_from_url("https://github.com/onlyowner"), None);
        assert_eq!(owner_repo_from_url("not a url"), None);
        assert_eq!(owner_repo_from_url(""), None);
    }

    #[test]
    fn nwo_from_remote_url_handles_https_ssh_and_scp_forms() {
        assert_eq!(
            nwo_from_remote_url("https://github.com/acme/superzej.git").as_deref(),
            Some("acme/superzej")
        );
        assert_eq!(
            nwo_from_remote_url("https://github.com/acme/superzej").as_deref(),
            Some("acme/superzej")
        );
        assert_eq!(
            nwo_from_remote_url("ssh://git@github.com/acme/superzej.git").as_deref(),
            Some("acme/superzej")
        );
        assert_eq!(
            nwo_from_remote_url("git@github.com:acme/superzej.git").as_deref(),
            Some("acme/superzej")
        );
        assert_eq!(
            nwo_from_remote_url("git@ghe.corp.example:org/repo").as_deref(),
            Some("org/repo")
        );
        assert_eq!(
            nwo_from_remote_url("git@github.com:onlyowner").as_deref(),
            None
        );
        assert_eq!(nwo_from_remote_url("not a url"), None);
        assert_eq!(nwo_from_remote_url(""), None);
    }

    #[test]
    fn parse_review_threads_flattens_and_sorts_unresolved_first() {
        let json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[
            {"isResolved":true,"comments":{"nodes":[
                {"author":{"login":"dev"},"path":"session.rs","line":9,
                 "body":"resolved earlier","createdAt":"2026-06-11T08:00:00Z"}]}},
            {"isResolved":false,"comments":{"nodes":[
                {"author":{"login":"mira"},"path":"session.rs","line":42,
                 "body":"ttl from cfg\nsecond line ignored","createdAt":"2026-06-11T11:43:00Z"}]}},
            {"isResolved":false,"comments":{"nodes":[]}}
        ]}}}}}"#;
        let rows = parse_review_threads(json);
        // The empty-comments thread is dropped; unresolved sorts first.
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].resolved);
        assert_eq!(rows[0].author, "mira");
        assert_eq!(rows[0].path, "session.rs");
        assert_eq!(rows[0].line, Some(42));
        assert_eq!(rows[0].snippet, "ttl from cfg");
        assert!(rows[1].resolved);
        // Garbage and shape misses degrade to empty.
        assert!(parse_review_threads("not json").is_empty());
        assert!(parse_review_threads("{}").is_empty());
    }

    #[test]
    fn parse_issue_list_extracts_labels() {
        let json = r#"[
            {"number":98,"title":"panel flicker on resize",
             "labels":[{"name":"P1"},{"name":"bug"}]},
            {"number":87,"title":"document keymap layer","labels":[]},
            {"bogus":true}
        ]"#;
        let rows = parse_issue_list(json);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].number, 98);
        assert_eq!(rows[0].labels, vec!["P1".to_string(), "bug".to_string()]);
        assert!(rows[1].labels.is_empty());
        assert!(parse_issue_list("nope").is_empty());
        assert!(parse_issue_list("[]").is_empty());
    }

    #[test]
    fn pr_panel_round_trips_with_and_without_extension_fields() {
        // A fresh panel serializes; old cached JSON (no threads/issues keys)
        // still deserializes thanks to serde defaults.
        let panel = PrPanel {
            state: PanelState::NoPr,
            worktree: "/wt".into(),
            branch: "main".into(),
            fetched_at: 1,
            threads: vec![ReviewThreadRow {
                author: "mira".into(),
                path: "a.rs".into(),
                line: Some(1),
                snippet: "s".into(),
                resolved: false,
                created_at: String::new(),
            }],
            issues: vec![IssueRow {
                number: 5,
                title: "t".into(),
                labels: vec![],
            }],
        };
        let json = serde_json::to_string(&panel).unwrap();
        let back: PrPanel = serde_json::from_str(&json).unwrap();
        assert_eq!(back.threads.len(), 1);
        assert_eq!(back.issues[0].number, 5);

        let legacy = r#"{"kind":"no_pr","worktree":"/wt","branch":"main","fetched_at":1}"#;
        let back: PrPanel = serde_json::from_str(legacy).unwrap();
        assert!(matches!(back.state, PanelState::NoPr));
        assert!(back.threads.is_empty() && back.issues.is_empty());

        // A full Pr state with checks round-trips through the cache too.
        let pr_json = r#"{"kind":"pr","number":142,"title":"session cache","state":"OPEN",
            "url":"https://github.com/a/r/pull/142","isDraft":false,
            "statusCheckRollup":[{"name":"build","status":"COMPLETED","conclusion":"SUCCESS",
            "startedAt":"2026-06-11T10:00:00Z","completedAt":"2026-06-11T10:01:00Z"}],
            "worktree":"/wt","branch":"feat","fetched_at":2}"#;
        let back: PrPanel = serde_json::from_str(pr_json).unwrap();
        match &back.state {
            PanelState::Pr(pr) => {
                assert_eq!(pr.number, 142);
                assert_eq!(pr.status_check_rollup[0].duration_secs(0), Some(60));
            }
            other => panic!("expected Pr, got {other:?}"),
        }
    }

    #[test]
    fn buckets_handle_both_shapes() {
        // CheckRun shape (conclusion).
        assert_eq!(
            check_bucket(&cr("COMPLETED", Some("SUCCESS"), None)),
            Bucket::Pass
        );
        assert_eq!(
            check_bucket(&cr("COMPLETED", Some("FAILURE"), None)),
            Bucket::Fail
        );
        assert_eq!(
            check_bucket(&cr("IN_PROGRESS", None, None)),
            Bucket::Pending
        );
        // StatusContext shape (state).
        assert_eq!(check_bucket(&cr("", None, Some("SUCCESS"))), Bucket::Pass);
        assert_eq!(
            check_bucket(&cr("", None, Some("PENDING"))),
            Bucket::Pending
        );
        assert_eq!(check_bucket(&cr("", None, Some("ERROR"))), Bucket::Fail);
    }

    #[test]
    fn parses_gh_pr_view_and_summarizes() {
        let json = r#"{
            "number": 42, "title": "Add thing", "state": "OPEN",
            "url": "https://example/pr/42", "isDraft": false,
            "headRefName": "sz/add-thing", "baseRefName": "main",
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "reviewDecision": "APPROVED",
            "statusCheckRollup": [
                {"name":"build","status":"COMPLETED","conclusion":"SUCCESS"},
                {"name":"test","status":"COMPLETED","conclusion":"FAILURE"},
                {"name":"lint","status":"IN_PROGRESS"},
                {"context":"legacy","state":"PENDING"}
            ]
        }"#;
        let mut pr: PrStatus = serde_json::from_str(json).expect("parse");
        pr.checks = summarize(&pr.status_check_rollup);
        assert_eq!(pr.number, 42);
        assert_eq!(pr.checks.total, 4);
        assert_eq!(pr.checks.passed, 1);
        assert_eq!(pr.checks.failed, 1);
        assert_eq!(pr.checks.pending, 2);
    }

    #[test]
    fn panel_state_serializes_with_kind_tag() {
        let panel = PrPanel {
            state: PanelState::NoPr,
            worktree: "/tmp/wt".into(),
            branch: "sz/x".into(),
            fetched_at: 0,
            threads: Vec::new(),
            issues: Vec::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&panel).unwrap();
        assert_eq!(v["kind"], "no_pr");
        assert_eq!(v["branch"], "sz/x");
    }

    #[test]
    fn pr_variant_flattens_for_the_panel() {
        let json = r#"{"number":7,"title":"x","state":"OPEN","url":"u",
            "isDraft":false,"headRefName":"sz/x","baseRefName":"main",
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"APPROVED",
            "statusCheckRollup":[{"name":"b","status":"COMPLETED","conclusion":"SUCCESS"}]}"#;
        let mut pr: PrStatus = serde_json::from_str(json).unwrap();
        pr.checks = summarize(&pr.status_check_rollup);
        let panel = PrPanel {
            state: PanelState::Pr(Box::new(pr)),
            worktree: "/tmp/wt".into(),
            branch: "sz/x".into(),
            fetched_at: 0,
            threads: Vec::new(),
            issues: Vec::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&panel).unwrap();
        // The plugin reads these flattened keys.
        assert_eq!(v["kind"], "pr");
        assert_eq!(v["number"], 7);
        assert_eq!(v["reviewDecision"], "APPROVED");
        assert_eq!(v["checks"]["passed"], 1);
        assert_eq!(v["branch"], "sz/x");
    }
}
