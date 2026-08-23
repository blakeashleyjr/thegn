//! GitHub via the `gh` CLI — the **transport** half of the GitHub forge.
//!
//! This module is the one place in the workspace that runs `gh`; it is the
//! `GithubCli` implementation of [`crate::forge::Forge`] and nothing else may
//! name it (the `forge-leak` ratchet in `just lint` pins that). Models and
//! parsers live in [`crate::forge::model`]; the trait and its error type in
//! [`crate::forge`].
//!
//! Everything runs with `cwd = worktree` (locally, or over ssh on the remote
//! host) so `gh` auto-detects the repo from its remote.

pub use crate::forge::ForgeError as GhError;
use crate::forge::model::*;
use crate::remote::GitLoc;
use serde::Deserialize;

/// Run `gh <args>` with `cwd = worktree` (local, or over ssh on the remote host);
/// trimmed stdout on success, else a classified error.
pub fn gh_out(loc: &GitLoc, args: &[&str]) -> Result<String, GhError> {
    let out = loc.gh_command(args).output().map_err(spawn_err)?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    Err(classify(
        &String::from_utf8_lossy(&out.stderr).to_lowercase(),
    ))
}

/// Run `gh <args>` for its exit code (output discarded). Errors classified.
pub fn gh_run(loc: &GitLoc, args: &[&str]) -> Result<(), GhError> {
    let out = loc.gh_command(args).output().map_err(spawn_err)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(classify(
            &String::from_utf8_lossy(&out.stderr).to_lowercase(),
        ))
    }
}

/// Map a spawn failure (e.g. `gh` binary absent) onto the right `GhError`.
/// On a local worktree `gh_command` spawns `gh` directly, so a missing binary
/// surfaces here as `ErrorKind::NotFound` rather than through `classify`.
pub(crate) fn spawn_err(e: std::io::Error) -> GhError {
    if e.kind() == std::io::ErrorKind::NotFound {
        GhError::NotInstalled
    } else {
        GhError::Other(e.to_string())
    }
}

pub(crate) fn classify(stderr: &str) -> GhError {
    // Shell-shaped "not found" only — a bare "not found" also matches gh's REST
    // `Not Found (HTTP 404)`, so the NotInstalled patterns must be specific.
    if stderr.contains("command not found")
        || stderr.contains("no such file or directory")
        // A shell's "gh: not found" means the binary is missing — but gh's own
        // REST error "Not Found (HTTP 404)" also contains ": not found", so only
        // treat the bare form (no HTTP status) as NotInstalled.
        || (stderr.contains(": not found") && !stderr.contains("http"))
    {
        GhError::NotInstalled
    } else if stderr.contains("no pull requests found")
        || stderr.contains("no default remote repository")
        || stderr.contains("no open pull request")
        || stderr.contains("no pr ")
        || stderr.contains("http 404")
        || stderr.contains("not found (http")
    {
        // A 404 against a PR/repo endpoint means the target is gone, not that
        // gh is missing — fold it into the "no PR" state rather than Other.
        GhError::NoPr
    } else if stderr.contains("not logged")
        || stderr.contains("authentication")
        || stderr.contains("gh auth login")
        || stderr.contains("http 401")
    {
        GhError::NotAuthenticated
    } else if stderr.contains("rate limit") || stderr.contains("api rate") {
        GhError::RateLimited
    } else if stderr.contains("error connecting to")
        || stderr.contains("could not resolve host")
        || stderr.contains("no such host")
        || stderr.contains("connection refused")
        || stderr.contains("network is unreachable")
        || stderr.contains("i/o timeout")
        || stderr.contains("check your internet connection")
        || stderr.contains("tls handshake")
    {
        // Transient network failure — distinct from Other so the UI can show
        // "GitHub unreachable" and callers can circuit-break.
        GhError::Offline
    } else {
        GhError::Other(stderr.trim().to_string())
    }
}

const PR_FIELDS: &str = "number,title,state,url,isDraft,headRefName,headRefOid,baseRefName,\
                         mergeable,mergeStateStatus,reviewDecision,statusCheckRollup";

/// Fetch the PR state for a worktree, mapping every failure mode to a PanelState.
pub fn pr_status(loc: &GitLoc) -> PrPanel {
    pr_status_of(loc, None)
}

/// As [`pr_status`], but for an explicitly named pull request.
///
/// The PR queue tracks entries by number — including ones with no local
/// checkout — so it cannot rely on `gh` inferring the PR from the current
/// branch. Shares the error mapping so both paths report failures identically.
pub fn pr_status_for(loc: &GitLoc, number: u64) -> PrPanel {
    pr_status_of(loc, Some(number))
}

fn pr_status_of(loc: &GitLoc, number: Option<u64>) -> PrPanel {
    let branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_default();
    PrPanel::from_result(pr_status_raw(loc, number), loc.path(), branch)
}

/// `gh pr view [<n>] --json …` as a `Result` — the forge-trait shape.
pub fn pr_status_raw(loc: &GitLoc, number: Option<u64>) -> Result<PrStatus, GhError> {
    let num = number.map(|n| n.to_string());
    let mut args: Vec<&str> = vec!["pr", "view"];
    if let Some(n) = num.as_deref() {
        args.push(n);
    }
    args.extend_from_slice(&["--json", PR_FIELDS]);
    let json = gh_out(loc, &args)?;
    serde_json::from_str::<PrStatus>(&json).map_err(|e| GhError::Other(format!("parse error: {e}")))
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

/// [`pr_status_for`] plus that PR's review threads. The PR queue's fetch: it
/// needs threads to classify "changes requested" and to feed the review agent,
/// but never the repo's issue list.
pub fn pr_status_with_threads(loc: &GitLoc, number: u64) -> PrPanel {
    let mut panel = pr_status_for(loc, number);
    if let PanelState::Pr(pr) = &panel.state
        && let Some((owner, repo)) = owner_repo_from_url(&pr.url)
    {
        panel.threads = review_threads(loc, &owner, &repo, pr.number).unwrap_or_default();
    }
    panel
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

/// Open the PR belonging to `branch` in the browser
/// (`gh pr view <branch> --web`) — the fallback when no cached URL exists.
pub fn open_pr_for_branch(loc: &GitLoc, branch: &str) -> Result<(), GhError> {
    gh_run(loc, &["pr", "view", branch, "--web"])
}

const THREADS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
reviewThreads(first:20){nodes{isResolved comments(first:1){nodes{\
author{login} path line body createdAt}}}}}}}";

/// Fetch the viewer's GitHub notification threads (`gh api notifications` —
/// unread, participating). The caller filters to @mentions for this repo via
/// [`parse_mention_notifications`]. Subprocess seam; throttled by the caller.
pub fn fetch_gh_notifications(loc: &GitLoc) -> Result<String, GhError> {
    gh_out(
        loc,
        &["api", "notifications?participating=true&per_page=50"],
    )
}

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

const CONVERSATION_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){\
comments(first:100){nodes{author{login} body createdAt}}\
reviews(first:100){nodes{author{login} state body submittedAt}}\
reviewThreads(first:100){nodes{id isResolved path line \
comments(first:50){nodes{author{login} body createdAt diffHunk}}}}}}}";

/// Fetch the deep conversation feed (comments + reviews + review threads) in one
/// `gh api graphql` round trip.
pub fn conversation(
    loc: &GitLoc,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<PrConversation, GhError> {
    let num = number.to_string();
    let owner_arg = format!("owner={owner}");
    let name_arg = format!("name={repo}");
    let num_arg = format!("number={num}");
    let query_arg = format!("query={CONVERSATION_QUERY}");
    let json = gh_out(
        loc,
        &[
            "api", "graphql", "-f", &query_arg, "-f", &owner_arg, "-f", &name_arg, "-F", &num_arg,
        ],
    )?;
    let v: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| GhError::Other(format!("parse error: {e}")))?;
    Ok(parse_conversation(&v))
}

/// Fetch the PR's unified diff (`gh pr diff`) and parse it into a [`PrDiff`].
pub fn pr_diff(loc: &GitLoc) -> Result<PrDiff, GhError> {
    let raw = gh_out(loc, &["pr", "diff"])?;
    Ok(parse_unified_diff(&raw))
}

/// Post a PR-level comment (`gh pr comment --body <body>`).
pub fn comment_pr(loc: &GitLoc, body: &str) -> Result<(), GhError> {
    gh_run(loc, &["pr", "comment", "--body", body])
}

/// Submit a review with an explicit state + optional body. `gh` requires a body
/// for `--request-changes` and `--comment`; we surface that as a clear error
/// rather than a raw `gh` failure.
pub fn submit_review(loc: &GitLoc, state: ReviewState, body: Option<&str>) -> Result<(), GhError> {
    let body = body.map(str::trim).filter(|b| !b.is_empty());
    if matches!(state, ReviewState::RequestChanges | ReviewState::Comment) && body.is_none() {
        return Err(GhError::Other(
            "a review body is required for request-changes / comment".into(),
        ));
    }
    let mut args = vec!["pr", "review", state.flag()];
    if let Some(b) = body {
        args.push("--body");
        args.push(b);
    }
    gh_run(loc, &args)
}

const THREAD_REPLY_MUTATION: &str = "mutation($threadId:ID!,$body:String!){\
addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$threadId,body:$body}){\
comment{id}}}";

/// Reply to an existing review thread via the GraphQL mutation (the CLI has no
/// thread-reply verb). `thread_id` is the review-thread node id.
pub fn reply_to_thread(loc: &GitLoc, thread_id: &str, body: &str) -> Result<(), GhError> {
    let query_arg = format!("query={THREAD_REPLY_MUTATION}");
    let id_arg = format!("threadId={thread_id}");
    let body_arg = format!("body={body}");
    gh_run(
        loc,
        &[
            "api", "graphql", "-f", &query_arg, "-f", &id_arg, "-f", &body_arg,
        ],
    )
}

/// Post an inline review comment on a specific new-side line via the REST API
/// (`gh api POST …/pulls/{n}/comments`). `commit_id` is the PR head SHA
/// ([`PrStatus::head_ref_oid`]).
#[allow(clippy::too_many_arguments)]
pub fn add_line_comment(
    loc: &GitLoc,
    owner: &str,
    repo: &str,
    number: u64,
    commit_id: &str,
    path: &str,
    line: u64,
    body: &str,
) -> Result<(), GhError> {
    let endpoint = format!("repos/{owner}/{repo}/pulls/{number}/comments");
    let body_arg = format!("body={body}");
    let commit_arg = format!("commit_id={commit_id}");
    let path_arg = format!("path={path}");
    let line_arg = format!("line={line}");
    gh_run(
        loc,
        &[
            "api",
            "-X",
            "POST",
            &endpoint,
            "-f",
            &body_arg,
            "-f",
            &commit_arg,
            "-f",
            &path_arg,
            "-F",
            &line_arg,
            "-f",
            "side=RIGHT",
        ],
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

// ---------------------------------------------------------------------------
// The `gh` CLI as a Forge
// ---------------------------------------------------------------------------

use crate::forge::{
    CreateIssueOpts, Forge, ForgeCaps, ForgeIssue, LineComment, Mention, PrRef, PrRole, RepoRef,
};
use crate::seam::{Availability, Probe, ProbeReport};

/// GitHub (or GitHub Enterprise) through the user's `gh` CLI: reuses its
/// auth (keyring, enterprise hosts) and runs with `cwd = worktree`, locally or
/// over ssh. The permanent fallback layer under `GithubNative`.
#[derive(Debug, Clone, Default)]
pub struct GithubCli {
    /// `"ghe"` for an enterprise entry, else `"github"`.
    pub enterprise: bool,
}

impl GithubCli {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Probe for GithubCli {
    fn probe(&self) -> ProbeReport {
        let availability = match crate::util::which_path("gh") {
            Some(_) => Availability::Ready,
            None => Availability::Unavailable("`gh` not found on PATH".into()),
        };
        ProbeReport::new("forge", self.id(), availability)
            .with_caps(&self.caps())
            .note("GitHub via `gh` (reuses `gh auth`)")
    }
}

const ISSUE_FIELDS: &str = "number,title,body,state,url,author,createdAt,updatedAt";

impl Forge for GithubCli {
    fn id(&self) -> &'static str {
        if self.enterprise { "ghe" } else { "github" }
    }
    fn caps(&self) -> ForgeCaps {
        ForgeCaps::ALL
    }
    fn repo_ref(&self, loc: &GitLoc) -> Option<RepoRef> {
        origin_nwo(loc)
            .and_then(|nwo| {
                nwo.split_once('/')
                    .map(|(o, r)| (o.to_string(), r.to_string()))
            })
            .map(|(owner, repo)| RepoRef { owner, repo })
    }
    fn pr_status(&self, loc: &GitLoc, pr: PrRef) -> Result<PrStatus, GhError> {
        pr_status_raw(loc, pr.number())
    }
    fn pr_list(&self, loc: &GitLoc, limit: usize) -> Result<Vec<PrHeader>, GhError> {
        pr_list(loc, limit)
    }
    fn pr_state_for_branch(&self, loc: &GitLoc, branch: &str) -> Result<Option<String>, GhError> {
        let json = match gh_out(loc, &["pr", "view", branch, "--json", "state"]) {
            Ok(j) => j,
            Err(GhError::NoPr) => return Ok(None),
            Err(e) => return Err(e),
        };
        let v: serde_json::Value =
            serde_json::from_str(&json).map_err(|e| GhError::Other(e.to_string()))?;
        Ok(v.get("state").and_then(|s| s.as_str()).map(str::to_string))
    }
    fn search_prs(
        &self,
        loc: &GitLoc,
        role: PrRole,
        repo: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PrSearchRow>, GhError> {
        let flag = match role {
            PrRole::ReviewRequested => "--review-requested=@me",
            PrRole::Author => "--author=@me",
        };
        search_prs(loc, flag, repo, limit)
    }
    fn review_threads(
        &self,
        loc: &GitLoc,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ReviewThreadRow>, GhError> {
        review_threads(loc, &repo.owner, &repo.repo, number)
    }
    fn conversation(
        &self,
        loc: &GitLoc,
        repo: &RepoRef,
        number: u64,
    ) -> Result<PrConversation, GhError> {
        conversation(loc, &repo.owner, &repo.repo, number)
    }
    fn reviews_json(&self, loc: &GitLoc) -> Result<String, GhError> {
        reviews(loc)
    }
    fn pr_diff(&self, loc: &GitLoc, pr: PrRef) -> Result<PrDiff, GhError> {
        match pr {
            PrRef::Current => pr_diff(loc),
            PrRef::Number(n) => {
                let raw = gh_out(loc, &["pr", "diff", &n.to_string()])?;
                Ok(parse_unified_diff(&raw))
            }
        }
    }
    fn pr_diff_raw(&self, loc: &GitLoc, pr: PrRef) -> Result<String, GhError> {
        match pr {
            PrRef::Current => gh_out(loc, &["pr", "diff"]),
            PrRef::Number(n) => gh_out(loc, &["pr", "diff", &n.to_string()]),
        }
    }
    fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, GhError> {
        create_pr(loc, opts)
    }
    fn merge_pr(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), GhError> {
        let num = pr.number().map(|n| n.to_string());
        let mut args: Vec<&str> = vec!["pr", "merge"];
        if let Some(n) = num.as_deref() {
            args.push(n);
        }
        args.push(method.flag());
        if delete_branch {
            args.push("--delete-branch");
        }
        if auto {
            args.push("--auto");
        }
        gh_run(loc, &args)
    }
    fn set_draft(&self, loc: &GitLoc, pr: PrRef, draft: bool) -> Result<(), GhError> {
        let num = pr.number().map(|n| n.to_string());
        let mut args: Vec<&str> = vec!["pr", "ready"];
        if let Some(n) = num.as_deref() {
            args.push(n);
        }
        if draft {
            args.push("--undo");
        }
        gh_run(loc, &args)
    }
    fn set_auto_merge(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        enable: bool,
        method: MergeMethod,
    ) -> Result<(), GhError> {
        let num = pr.number().map(|n| n.to_string());
        let mut args: Vec<&str> = vec!["pr", "merge"];
        if let Some(n) = num.as_deref() {
            args.push(n);
        }
        if enable {
            args.push("--auto");
            args.push(method.flag());
        } else {
            args.push("--disable-auto");
        }
        gh_run(loc, &args)
    }
    fn comment(&self, loc: &GitLoc, pr: PrRef, body: &str) -> Result<(), GhError> {
        let num = pr.number().map(|n| n.to_string());
        let mut args: Vec<&str> = vec!["pr", "comment"];
        if let Some(n) = num.as_deref() {
            args.push(n);
        }
        args.extend_from_slice(&["--body", body]);
        gh_run(loc, &args)
    }
    fn submit_review(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        state: ReviewState,
        body: Option<&str>,
    ) -> Result<(), GhError> {
        match pr {
            PrRef::Current => submit_review(loc, state, body),
            PrRef::Number(n) => {
                if !matches!(state, ReviewState::Approve) && body.is_none() {
                    return Err(GhError::Other(
                        "a body is required for request-changes / comment reviews".into(),
                    ));
                }
                let n = n.to_string();
                let mut args: Vec<&str> = vec!["pr", "review", &n, state.flag()];
                if let Some(b) = body {
                    args.extend_from_slice(&["--body", b]);
                }
                gh_run(loc, &args)
            }
        }
    }
    fn reply_thread(&self, loc: &GitLoc, thread_id: &str, body: &str) -> Result<(), GhError> {
        reply_to_thread(loc, thread_id, body)
    }
    fn add_line_comment(&self, loc: &GitLoc, c: LineComment<'_>) -> Result<(), GhError> {
        add_line_comment(
            loc,
            &c.repo.owner,
            &c.repo.repo,
            c.number,
            c.commit_id,
            c.path,
            c.line,
            c.body,
        )
    }
    fn rerun_failed(&self, loc: &GitLoc, _pr: PrRef) -> Result<u32, GhError> {
        rerun_failed_checks(loc)
    }
    fn issue_rows(&self, loc: &GitLoc, limit: usize) -> Result<Vec<IssueRow>, GhError> {
        issue_list(loc, limit)
    }
    fn issue_list(&self, loc: &GitLoc, state: &str) -> Result<Vec<ForgeIssue>, GhError> {
        let json = gh_out(
            loc,
            &["issue", "list", "--json", ISSUE_FIELDS, "--state", state],
        )?;
        serde_json::from_str(&json).map_err(|e| GhError::Other(format!("parse error: {e}")))
    }
    fn issue_get(&self, loc: &GitLoc, number: u64) -> Result<ForgeIssue, GhError> {
        let json = gh_out(
            loc,
            &["issue", "view", &number.to_string(), "--json", ISSUE_FIELDS],
        )?;
        serde_json::from_str(&json).map_err(|e| GhError::Other(format!("parse error: {e}")))
    }
    fn issue_create(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<ForgeIssue, GhError> {
        let mut args: Vec<String> = vec![
            "issue".into(),
            "create".into(),
            "--title".into(),
            opts.title.clone(),
        ];
        if let Some(body) = &opts.body {
            args.push("--body".into());
            args.push(body.clone());
        }
        if !opts.labels.is_empty() {
            args.push("--label".into());
            args.push(opts.labels.join(","));
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let url = gh_out(loc, &refs)?;
        // `gh issue create` prints the new issue's URL.
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| GhError::Other(format!("unexpected issue URL: {url}")))?;
        self.issue_get(loc, number)
    }
    fn issue_comment(&self, loc: &GitLoc, number: u64, body: &str) -> Result<(), GhError> {
        gh_run(
            loc,
            &["issue", "comment", &number.to_string(), "--body", body],
        )
    }
    fn mentions(&self, loc: &GitLoc, repo: &RepoRef) -> Result<Vec<Mention>, GhError> {
        let json = fetch_gh_notifications(loc)?;
        Ok(parse_mention_notifications(&json, &repo.nwo()))
    }
    fn open_in_browser(&self, loc: &GitLoc, branch: Option<&str>) -> Result<(), GhError> {
        match branch {
            Some(b) => open_pr_for_branch(loc, b),
            None => open_pr(loc),
        }
    }
    fn whoami(&self, loc: &GitLoc) -> Result<String, GhError> {
        gh_out(loc, &["api", "user", "--jq", ".login"])
    }
}
