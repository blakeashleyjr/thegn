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
use crate::forge::checkout::{ForgeCheckoutScope, checkout_scope, checkout_scope_for_branch};
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

const PR_FIELDS: &str = "number,title,state,url,author,isDraft,headRefName,headRefOid,baseRefName,\
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
    // Name the PR explicitly. A bare `gh pr view` resolves it through the
    // branch's push/upstream config, which can land on ANOTHER branch's PR
    // (e.g. a worktree branch auto-tracking `origin/main`).
    let (selector, scope) = match number {
        // The PR queue can ask about a number from a location with no
        // checked-out branch. Keep that established path rooted at origin;
        // branch-scoped calls below carry the complete captured scope.
        Some(n) => (n.to_string(), Some(gh_repo_scope_origin(loc)?)),
        None => {
            let checkout = checkout_scope(loc)?;
            let number = branch_pr_number(loc, &checkout)?;
            (number.to_string(), Some(scope_for_checkout(&checkout)?))
        }
    };
    let mut args: Vec<String> = vec!["pr".into(), "view".into(), selector];
    if let Some(scope) = &scope {
        args.extend(["--repo".into(), scope.clone()]);
    }
    args.extend(["--json".into(), PR_FIELDS.into()]);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let json = gh_out(loc, &argv)?;
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
    if limit == 0 {
        return Ok(Vec::new());
    }
    let limit = limit.to_string();
    let origin = origin_identity(loc)?;
    let mut args = vec![
        "pr".to_string(),
        "list".to_string(),
        "--json".to_string(),
        "number,headRefName,state,url,isDraft".to_string(),
        "--limit".to_string(),
        limit,
    ];
    let scope = scope_for_identity(&origin)
        .ok_or(GhError::NotConfigured("origin repository scope".into()))?;
    args.extend(["--repo".to_string(), scope]);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let json = gh_out(loc, &argv)?;
    let rows: Vec<PrHeader> = serde_json::from_str(&json)
        .map_err(|_| GhError::NotConfigured("malformed gh PR list response".into()))?;
    if rows.iter().any(|row| {
        row.number == 0
            || row.head_ref.is_empty()
            || repo_identity_from_pr_url(&row.url).is_none_or(|identity| !identity.matches(&origin))
    }) {
        return Err(GhError::NotConfigured(
            "PR list response identity is malformed".into(),
        ));
    }
    Ok(rows)
}

/// The state (`OPEN`/`MERGED`/`CLOSED`) of the PR for `branch`, via
/// `gh pr view <branch> --json state`. `None` when there's no PR or `gh`
/// fails. Used by the on-merge auto-clean to resolve the precise outcome when a
/// branch drops out of the open-PR set (merged vs closed-without-merge).
pub fn pr_state_for_branch(loc: &GitLoc, branch: &str) -> Option<String> {
    let checkout = checkout_scope_for_branch(loc, branch).ok()?;
    let number = branch_pr_number(loc, &checkout).ok()?;
    let number = number.to_string();
    let mut args = vec!["pr", "view", number.as_str()];
    let scope = scope_for_checkout(&checkout).ok()?;
    args.extend(["--repo", scope.as_str()]);
    args.extend(["--json", "state"]);
    let json = gh_out(loc, &args).ok()?;
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
    nwo_from_remote_url(&loc.origin_url()?)
}

fn scope_for_identity(identity: &ForgeRepoIdentity) -> Option<String> {
    let path = identity.path.clone();
    (path.matches('/').count() == 1).then(|| {
        if identity.host.eq_ignore_ascii_case("github.com") {
            path
        } else {
            format!("{}/{}", identity.host, path)
        }
    })
}

fn gh_repo_scope_origin(loc: &GitLoc) -> Result<String, GhError> {
    let identity = origin_identity(loc)?;
    scope_for_identity(&identity).ok_or(GhError::NotConfigured("origin repository scope".into()))
}

fn origin_identity(loc: &GitLoc) -> Result<ForgeRepoIdentity, GhError> {
    repo_identity_from_remote_url(
        &loc.origin_url()
            .ok_or(GhError::NotConfigured("origin repository".into()))?,
    )
    .ok_or(GhError::NotConfigured("origin repository identity".into()))
}

/// Resolve a branch through `gh pr list --head`, then use its number for all
/// subsequent operations. This removes the numeric-branch ambiguity of the
/// positional `gh pr view <selector>` grammar and scopes the lookup to the
/// origin repository.
fn scope_for_checkout(checkout: &ForgeCheckoutScope) -> Result<String, GhError> {
    scope_for_identity(&checkout.base).ok_or(GhError::NotConfigured(
        "repository scope is not owner/repo".into(),
    ))
}

fn branch_pr_number(loc: &GitLoc, checkout: &ForgeCheckoutScope) -> Result<u64, GhError> {
    let branch = checkout.branch.as_str();
    let scope = scope_for_checkout(checkout)?;
    let json = gh_out(
        loc,
        &[
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,state,headRefName,headRepository,headRepositoryOwner",
            "--limit",
            "20",
            "--repo",
            &scope,
        ],
    )?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&json)
        .map_err(|_| GhError::Other("malformed gh PR list response".into()))?;
    let expected = &checkout.head;
    let complete_window = rows.len() < 20;
    let matched = rows
        .into_iter()
        .filter(|row| {
            row.get("headRefName").and_then(|v| v.as_str()) == Some(branch)
                && cli_head_repo_path(row)
                    .is_some_and(|repo| repo.eq_ignore_ascii_case(&expected.path))
        })
        .min_by_key(|row| row.get("state").and_then(|v| v.as_str()) != Some("OPEN"));
    match matched {
        Some(row) => row
            .get("number")
            .and_then(|number| number.as_u64())
            .filter(|number| *number > 0)
            .ok_or(GhError::NotConfigured(
                "matched PR omitted its number".into(),
            )),
        None if complete_window => Err(GhError::NoPr),
        None => Err(GhError::NotConfigured(
            "PR response window did not prove branch absence".into(),
        )),
    }
}

fn cli_head_repo_path(row: &serde_json::Value) -> Option<String> {
    row.pointer("/headRepository/nameWithOwner")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| {
            Some(format!(
                "{}/{}",
                row.pointer("/headRepositoryOwner/login")?.as_str()?,
                row.pointer("/headRepository/name")?.as_str()?
            ))
        })
}

/// Open the PR belonging to `branch` in the browser. Resolve the branch via a
/// scoped `--head` list first, then pass the verified number to `gh pr view`.
pub fn open_pr_for_branch(loc: &GitLoc, branch: &str) -> Result<(), GhError> {
    let checkout = checkout_scope_for_branch(loc, branch)?;
    let number = branch_pr_number(loc, &checkout)?;
    let number = number.to_string();
    let scope = scope_for_checkout(&checkout)?;
    let mut args = vec!["pr", "view", number.as_str()];
    args.extend(["--repo", scope.as_str()]);
    args.push("--web");
    gh_run(loc, &args)
}

#[cfg(test)]
mod branch_resolution_tests {
    use super::{cli_head_repo_path, pr_status_raw};
    use crate::remote::GitLoc;
    use std::path::Path;

    fn git(dir: &Path, args: &[&str]) {
        // See the note in forge/checkout.rs: a fixture that inherits the
        // developer's `commit.gpgSign = true` blocks on gpg-agent and fails
        // ~60s later, so the suite's result depends on a cached passphrase.
        let status = crate::util::git_cmd(dir)
            .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed: {status}");
    }

    #[test]
    fn cli_head_identity_accepts_both_documented_json_shapes() {
        let compact = serde_json::json!({
            "headRepository": {"nameWithOwner": "user/fork"}
        });
        assert_eq!(cli_head_repo_path(&compact).as_deref(), Some("user/fork"));
        let expanded = serde_json::json!({
            "headRepository": {"name": "fork"},
            "headRepositoryOwner": {"login": "user"}
        });
        assert_eq!(cli_head_repo_path(&expanded).as_deref(), Some("user/fork"));
        assert_eq!(cli_head_repo_path(&serde_json::json!({})), None);
    }

    #[test]
    fn fake_gh_resolves_numeric_branch_in_scoped_fork() {
        let dir = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let gh = bin.path().join("gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
case "$1 $2" in
  "pr list") printf '%s\n' "$@" >> "$FAKE_GH_LOG"; printf '%s\n' '[{"number":42,"state":"OPEN","headRefName":"123","headRepository":{"nameWithOwner":"user/fork"}}]' ;;
  "pr view") printf '%s\n' "$@" >> "$FAKE_GH_LOG"; printf '%s\n' '{"number":42,"title":"scope","state":"OPEN","url":"https://github.com/org/base/pull/42","headRefName":"123","baseRefName":"main"}' ;;
esac
"#,
        )
        .unwrap();
        if crate::fsperm::make_executable_for_test(&gh).is_err() {
            return;
        }
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "scope-test"]);
        git(
            dir.path(),
            &["config", "user.email", "scope@example.invalid"],
        );
        std::fs::write(dir.path().join("README"), "scope\n").unwrap();
        git(dir.path(), &["add", "README"]);
        git(dir.path(), &["commit", "-qm", "scope"]);
        git(dir.path(), &["branch", "-M", "123"]);
        git(
            dir.path(),
            &["remote", "add", "origin", "git@github.com:user/fork.git"],
        );
        git(
            dir.path(),
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/org/base.git",
            ],
        );
        git(dir.path(), &["config", "branch.123.remote", "upstream"]);
        git(dir.path(), &["config", "branch.123.pushRemote", "origin"]);

        let old_path = std::env::var_os("PATH").unwrap_or_default();
        let path = format!("{}:{}", bin.path().display(), old_path.to_string_lossy());
        let log = bin.path().join("args");
        let log = log.to_string_lossy().into_owned();
        let _env = crate::testenv::EnvGuard::set(&[("PATH", &path), ("FAKE_GH_LOG", &log)]);
        let result = pr_status_raw(&GitLoc::Local(dir.path().to_path_buf()), None);
        let status = result.unwrap();
        assert_eq!(status.number, 42);
        assert_eq!(status.head_ref_name, "123");
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("--repo\norg/base\n"));
    }
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

const THREAD_REPLY_RESOLVE_MUTATION: &str = "mutation($threadId:ID!,$body:String!){\
reply:addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$threadId,body:$body}){\
comment{id}} resolve:resolveReviewThread(input:{threadId:$threadId}){thread{id isResolved}}}";
const MAX_REVIEW_RESOLVE_REPLY_CHARS: usize = 4 * 1024;

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

/// Reply to and resolve one GitHub review thread in a single GraphQL request.
/// GraphQL mutation fields execute serially, so a reply failure prevents the
/// resolve field from running. The reply is bounded again at the provider edge.
pub fn resolve_review_thread(
    loc: &GitLoc,
    thread_id: &str,
    bounded_reply: &str,
) -> Result<(), GhError> {
    let reply: String = bounded_reply
        .chars()
        .filter(|character| !matches!(*character as u32, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f))
        .take(MAX_REVIEW_RESOLVE_REPLY_CHARS)
        .collect();
    let query_arg = format!("query={THREAD_REPLY_RESOLVE_MUTATION}");
    let id_arg = format!("threadId={thread_id}");
    let body_arg = format!("body={reply}");
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

fn run_list_args(branch: &str) -> Vec<String> {
    vec![
        "run".into(),
        "list".into(),
        "--branch".into(),
        branch.into(),
        "--json".into(),
        "databaseId,conclusion,headSha".into(),
        "--limit".into(),
        "20".into(),
    ]
}

fn failed_run_ids(json: &str, expected_head: &str) -> Result<Vec<u64>, GhError> {
    #[derive(Deserialize)]
    struct Run {
        #[serde(rename = "databaseId")]
        database_id: u64,
        conclusion: Option<String>,
        #[serde(rename = "headSha")]
        head_sha: Option<String>,
    }

    let runs: Vec<Run> = serde_json::from_str(json)
        .map_err(|error| GhError::Other(format!("could not parse workflow runs: {error}")))?;
    Ok(runs
        .into_iter()
        .filter(|run| {
            run.head_sha.as_deref() == Some(expected_head)
                && matches!(
                    run.conclusion
                        .as_deref()
                        .map(|value| value.to_uppercase())
                        .as_deref(),
                    Some("FAILURE")
                        | Some("TIMED_OUT")
                        | Some("CANCELLED")
                        | Some("STARTUP_FAILURE")
                )
        })
        .map(|run| run.database_id)
        .collect())
}

fn rerun_args(id: u64) -> Vec<String> {
    vec![
        "run".into(),
        "rerun".into(),
        id.to_string(),
        "--failed".into(),
    ]
}

/// Re-run failed workflow runs pinned to the PR's current head commit. A total
/// rerun failure is returned as an error; partial success returns its count.
fn rerun_failed_for_pr<F, R>(
    number: Option<u64>,
    mut output: F,
    mut rerun: R,
) -> Result<u32, GhError>
where
    F: FnMut(&[&str]) -> Result<String, GhError>,
    R: FnMut(&[&str]) -> Result<(), GhError>,
{
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PrHead {
        head_ref_name: String,
        head_ref_oid: String,
    }

    let mut pr_args = vec!["pr".to_owned(), "view".to_owned()];
    if let Some(number) = number {
        pr_args.push(number.to_string());
    }
    pr_args.extend(["--json".to_owned(), "headRefName,headRefOid".to_owned()]);
    let pr_refs: Vec<&str> = pr_args.iter().map(String::as_str).collect();
    let pr_json = output(&pr_refs)?;
    let pr_head: PrHead = serde_json::from_str(&pr_json)
        .map_err(|error| GhError::Other(format!("could not parse PR head: {error}")))?;
    if pr_head.head_ref_name.trim().is_empty() || pr_head.head_ref_oid.trim().is_empty() {
        return Err(GhError::Other("PR has an empty head branch or SHA".into()));
    }

    let list_args = run_list_args(&pr_head.head_ref_name);
    let list_refs: Vec<&str> = list_args.iter().map(String::as_str).collect();
    let run_json = output(&list_refs)?;
    let ids = failed_run_ids(&run_json, &pr_head.head_ref_oid)?;
    let mut count = 0;
    let mut first_error: Option<GhError> = None;
    for id in ids {
        let args = rerun_args(id);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match rerun(&refs) {
            Ok(()) => count += 1,
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    if count == 0
        && let Some(error) = first_error
    {
        return Err(error);
    }
    Ok(count)
}

#[cfg(test)]
fn rerun_failed_for_number<F, R>(number: u64, output: F, rerun: R) -> Result<u32, GhError>
where
    F: FnMut(&[&str]) -> Result<String, GhError>,
    R: FnMut(&[&str]) -> Result<(), GhError>,
{
    rerun_failed_for_pr(Some(number), output, rerun)
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

#[path = "github_authorship.rs"]
mod authorship_transport;

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
        Ok(crate::github::pr_state_for_branch(loc, branch))
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
        expected_head: Option<&str>,
        auto: bool,
    ) -> Result<(), GhError> {
        let args = merge_args(pr, method, delete_branch, expected_head, auto);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
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
        expected_head: Option<&str>,
    ) -> Result<(), GhError> {
        match (pr, expected_head) {
            (PrRef::Current, Some(_)) => Err(GhError::Unsupported("submit_review expected head")),
            (PrRef::Current, None) => submit_review(loc, state, body),
            (PrRef::Number(number), Some(head)) => {
                let repo = self.repo_ref(loc).ok_or_else(|| {
                    GhError::Other(
                        "could not resolve the PR's repository; nothing was submitted".into(),
                    )
                })?;
                let args = review_api_args(&repo, number, head, state, body)?;
                let args = args.iter().map(String::as_str).collect::<Vec<_>>();
                gh_run(loc, &args)
            }
            (PrRef::Number(n), None) => {
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
    fn resolve_review_thread(
        &self,
        loc: &GitLoc,
        thread_id: &str,
        bounded_reply: &str,
    ) -> Result<(), GhError> {
        resolve_review_thread(loc, thread_id, bounded_reply)
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
    fn rerun_failed(&self, loc: &GitLoc, pr: PrRef) -> Result<u32, GhError> {
        match pr {
            PrRef::Current => {
                rerun_failed_for_pr(None, |args| gh_out(loc, args), |args| gh_run(loc, args))
            }
            PrRef::Number(number) => rerun_failed_for_pr(
                Some(number),
                |args| gh_out(loc, args),
                |args| gh_run(loc, args),
            ),
        }
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

    fn pr_authorship(
        &self,
        loc: &GitLoc,
        number: u64,
    ) -> Result<crate::forge::authorship::PrAuthorship, GhError> {
        authorship_transport::fetch(loc, self.id(), number)
    }
}

fn merge_args(
    pr: PrRef,
    method: MergeMethod,
    delete_branch: bool,
    expected_head: Option<&str>,
    auto: bool,
) -> Vec<String> {
    let mut args = vec!["pr".into(), "merge".into()];
    if let Some(number) = pr.number() {
        args.push(number.to_string());
    }
    args.push(method.flag().into());
    if delete_branch {
        args.push("--delete-branch".into());
    }
    if let Some(head) = expected_head {
        args.push("--match-head-commit".to_owned());
        args.push(head.to_owned());
    }
    if auto {
        args.push("--auto".into());
    }
    args
}

fn review_api_args(
    repo: &RepoRef,
    number: u64,
    head: &str,
    state: ReviewState,
    body: Option<&str>,
) -> Result<Vec<String>, GhError> {
    if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GhError::Other("PR head is not a valid commit id".into()));
    }
    if number == 0 {
        return Err(GhError::Other("PR number must be greater than zero".into()));
    }
    if !matches!(state, ReviewState::Approve) && body.is_none_or(|value| value.trim().is_empty()) {
        return Err(GhError::Other(
            "a body is required for request-changes / comment reviews".into(),
        ));
    }

    let mut args = vec![
        "api".into(),
        "-X".into(),
        "POST".into(),
        format!("repos/{}/{}/pulls/{number}/reviews", repo.owner, repo.repo),
        "-f".into(),
        format!("commit_id={head}"),
        "-f".into(),
        format!("event={}", state.api_event()),
    ];
    if let Some(body) = body {
        args.push("-f".into());
        args.push(format!("body={body}"));
    }
    Ok(args)
}
#[cfg(test)]
mod numbered_rerun_tests {
    use super::GhError;
    use super::{
        failed_run_ids, rerun_args, rerun_failed_for_number, rerun_failed_for_pr, run_list_args,
    };

    #[test]
    fn numbered_rerun_uses_pr_branch_and_only_matching_head_runs() {
        let mut commands = Vec::<Vec<String>>::new();
        let mut reruns = Vec::<Vec<String>>::new();
        let count = rerun_failed_for_number(
            42,
            |args| {
                commands.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                if args.first() == Some(&"pr") {
                    Ok(r#"{"headRefName":"feature/pr-42","headRefOid":"abc123"}"#.into())
                } else {
                    Ok(r#"[
                        {"databaseId":1,"conclusion":"FAILURE","headSha":"abc123"},
                        {"databaseId":2,"conclusion":"TIMED_OUT","headSha":"other"},
                        {"databaseId":3,"conclusion":"SUCCESS","headSha":"abc123"}
                    ]"#
                    .into())
                }
            },
            |args| {
                reruns.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(count, 1);
        assert_eq!(
            commands,
            vec![
                ["pr", "view", "42", "--json", "headRefName,headRefOid"]
                    .map(str::to_owned)
                    .to_vec(),
                run_list_args("feature/pr-42"),
            ]
        );
        assert_eq!(reruns, vec![rerun_args(1)]);
        assert!(!commands.iter().flatten().any(|arg| arg == "rev-parse"));
    }

    #[test]
    fn numbered_rerun_rejects_missing_head_proof_and_malformed_runs() {
        let error = rerun_failed_for_number(
            42,
            |_| Ok(r#"{"headRefName":"","headRefOid":"abc123"}"#.into()),
            |_| panic!("must not rerun without a proven PR head"),
        );
        assert!(error.is_err());
        assert!(failed_run_ids("not-json", "abc123").is_err());
    }

    #[test]
    fn numbered_rerun_propagates_pr_lookup_failure_without_listing_runs() {
        let error = rerun_failed_for_number(
            42,
            |_| Err(GhError::Other("lookup failed".into())),
            |_| panic!("must not rerun after lookup failure"),
        );
        assert!(error.is_err());
    }

    #[test]
    fn current_rerun_resolves_the_current_pr_head_and_reruns_only_its_failed_runs() {
        let mut commands = Vec::<Vec<String>>::new();
        let mut reruns = Vec::<Vec<String>>::new();
        let count = rerun_failed_for_pr(
            None,
            |args| {
                commands.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                if args.first() == Some(&"pr") {
                    Ok(r#"{"headRefName":"feature/pr-42","headRefOid":"abc123"}"#.into())
                } else {
                    Ok(r#"[
                        {"databaseId":1,"conclusion":"FAILURE","headSha":"abc123"},
                        {"databaseId":2,"conclusion":"TIMED_OUT","headSha":"other"},
                        {"databaseId":3,"conclusion":"SUCCESS","headSha":"abc123"}
                    ]"#
                    .into())
                }
            },
            |args| {
                reruns.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(count, 1);
        assert_eq!(
            commands,
            vec![
                ["pr", "view", "--json", "headRefName,headRefOid"]
                    .map(str::to_owned)
                    .to_vec(),
                run_list_args("feature/pr-42"),
            ]
        );
        assert_eq!(reruns, vec![rerun_args(1)]);
        assert!(!commands.iter().flatten().any(|arg| arg == "rev-parse"));
    }

    #[test]
    fn current_rerun_propagates_pr_lookup_failure_without_listing_runs() {
        let mut output_calls = 0;
        let error = rerun_failed_for_pr(
            None,
            |_| {
                output_calls += 1;
                Err(GhError::NoPr)
            },
            |_| panic!("must not rerun after PR lookup failure"),
        );

        assert_eq!(error, Err(GhError::NoPr));
        assert_eq!(output_calls, 1);
    }

    #[test]
    fn rerun_reports_the_first_error_when_every_rerun_fails() {
        let mut rerun_calls = 0;
        let result = rerun_failed_for_pr(
            Some(42),
            |args| {
                if args.first() == Some(&"pr") {
                    Ok(r#"{"headRefName":"feature/pr-42","headRefOid":"abc123"}"#.into())
                } else {
                    Ok(r#"[
                        {"databaseId":1,"conclusion":"FAILURE","headSha":"abc123"},
                        {"databaseId":2,"conclusion":"FAILURE","headSha":"abc123"}
                    ]"#
                    .into())
                }
            },
            |_| {
                rerun_calls += 1;
                if rerun_calls == 1 {
                    Err(GhError::RateLimited)
                } else {
                    Err(GhError::Offline)
                }
            },
        );

        assert_eq!(result, Err(GhError::RateLimited));
        assert_eq!(rerun_calls, 2);
    }

    #[test]
    fn rerun_counts_partial_success() {
        let mut rerun_calls = 0;
        let result = rerun_failed_for_pr(
            Some(42),
            |args| {
                if args.first() == Some(&"pr") {
                    Ok(r#"{"headRefName":"feature/pr-42","headRefOid":"abc123"}"#.into())
                } else {
                    Ok(r#"[
                        {"databaseId":1,"conclusion":"FAILURE","headSha":"abc123"},
                        {"databaseId":2,"conclusion":"FAILURE","headSha":"abc123"}
                    ]"#
                    .into())
                }
            },
            |_| {
                rerun_calls += 1;
                if rerun_calls == 1 {
                    Err(GhError::Offline)
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(result, Ok(1));
        assert_eq!(rerun_calls, 2);
    }

    #[test]
    fn current_rerun_rejects_a_malformed_run_list() {
        let mut commands = Vec::<Vec<String>>::new();
        let result = rerun_failed_for_pr(
            None,
            |args| {
                commands.push(args.iter().map(|arg| (*arg).to_owned()).collect());
                if args.first() == Some(&"pr") {
                    Ok(r#"{"headRefName":"feature/pr-42","headRefOid":"abc123"}"#.into())
                } else {
                    Ok("not-json".into())
                }
            },
            |_| panic!("must not rerun malformed workflow runs"),
        );

        assert!(result.is_err());
        assert_eq!(commands.len(), 2);
    }
}

#[cfg(test)]
mod merge_args_tests {
    use super::merge_args;
    use crate::forge::PrRef;
    use crate::forge::model::MergeMethod;

    #[test]
    fn merge_args_carry_match_head_commit() {
        assert_eq!(
            merge_args(
                PrRef::Number(42),
                MergeMethod::Squash,
                true,
                Some("deadbeef"),
                true,
            ),
            vec![
                "pr".to_owned(),
                "merge".to_owned(),
                "42".to_owned(),
                "--squash".to_owned(),
                "--delete-branch".to_owned(),
                "--match-head-commit".to_owned(),
                "deadbeef".to_owned(),
                "--auto".to_owned(),
            ]
        );
        assert_eq!(
            merge_args(PrRef::Number(42), MergeMethod::Squash, true, None, true),
            vec![
                "pr".to_owned(),
                "merge".to_owned(),
                "42".to_owned(),
                "--squash".to_owned(),
                "--delete-branch".to_owned(),
                "--auto".to_owned(),
            ]
        );
    }
}

#[cfg(test)]
mod review_args_tests {
    use super::{GithubCli, review_api_args};
    use crate::forge::Forge;
    use crate::forge::model::ReviewState;
    use crate::forge::{ForgeError, RepoRef};
    use crate::remote::GitLoc;
    use std::path::PathBuf;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn pinned_review_posts_commit_id_and_mapped_event() {
        let repo = RepoRef {
            owner: "o".into(),
            repo: "r".into(),
        };
        for (state, body, event) in [
            (ReviewState::Approve, None, "APPROVE"),
            (ReviewState::Approve, Some("looks good"), "APPROVE"),
            (
                ReviewState::RequestChanges,
                Some("please revise"),
                "REQUEST_CHANGES",
            ),
            (ReviewState::Comment, Some("note"), "COMMENT"),
        ] {
            let mut expected = vec![
                "api".to_owned(),
                "-X".to_owned(),
                "POST".to_owned(),
                "repos/o/r/pulls/7/reviews".to_owned(),
                "-f".to_owned(),
                format!("commit_id={HEAD}"),
                "-f".to_owned(),
                format!("event={event}"),
            ];
            if let Some(body) = body {
                expected.push("-f".to_owned());
                expected.push(format!("body={body}"));
            }
            assert_eq!(
                review_api_args(&repo, 7, HEAD, state, body).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn pinned_review_refuses_a_malformed_head_or_missing_body() {
        let repo = RepoRef {
            owner: "o".into(),
            repo: "r".into(),
        };
        for head in ["", "1234567", "0123456789abcdef0123456789abcdef0123456g"] {
            assert!(matches!(
                review_api_args(&repo, 7, head, ReviewState::Approve, None),
                Err(ForgeError::Other(_))
            ));
        }
        assert!(matches!(
            review_api_args(&repo, 0, HEAD, ReviewState::Approve, None),
            Err(ForgeError::Other(_))
        ));
        for body in [None, Some("   ")] {
            assert!(matches!(
                review_api_args(&repo, 7, HEAD, ReviewState::Comment, body),
                Err(ForgeError::Other(_))
            ));
        }
    }

    #[test]
    fn current_pr_with_expected_head_is_unsupported() {
        let forge = GithubCli::new();
        let loc = GitLoc::Local(PathBuf::from("/nonexistent"));
        assert!(matches!(
            forge.submit_review(
                &loc,
                crate::forge::PrRef::Current,
                ReviewState::Approve,
                None,
                Some(HEAD)
            ),
            Err(ForgeError::Unsupported("submit_review expected head"))
        ));
    }
}
