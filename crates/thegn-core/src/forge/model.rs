//! The forge model: every pull-request / check / review / issue / diff type
//! the host renders, plus the pure parsers that produce them from provider
//! JSON. No subprocess, no network — this is the half of the forge seam the
//! 95% coverage gate covers, and the only half host code should import types
//! from (`thegn_core::forge::model`).
//!
//! `PrPanel` is also the `pr_cache` wire shape: every extension field is
//! `#[serde(default)]` so cached rows keep deserializing across releases.

use super::ForgeError;
use serde::{Deserialize, Serialize};

/// How to merge a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

impl MergeMethod {
    pub fn flag(self) -> &'static str {
        match self {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        }
    }
}

/// The state to submit a PR review as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ReviewState {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewState {
    pub fn flag(self) -> &'static str {
        match self {
            ReviewState::Approve => "--approve",
            ReviewState::RequestChanges => "--request-changes",
            ReviewState::Comment => "--comment",
        }
    }
}

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
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    /// The head commit SHA — the `commit_id` an inline review comment anchors to.
    #[serde(default)]
    pub head_ref_oid: String,
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
    /// this inline after deserializing; the octocrab native path (thegn-svc)
    /// calls this so both produce an identical summary.
    pub fn recompute_checks(&mut self) {
        self.checks = summarize(&self.status_check_rollup);
    }
}

pub fn summarize(runs: &[CheckRun]) -> ChecksSummary {
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

impl PrPanel {
    /// Fold a forge answer into the panel feed — the one place a transport
    /// error becomes a `PanelState`. `threads`/`issues` start empty; the
    /// [`Forge::pr_panel`](super::Forge::pr_panel) composition fills them.
    pub fn from_result(
        result: Result<PrStatus, ForgeError>,
        worktree: String,
        branch: String,
    ) -> PrPanel {
        let state = match result {
            Ok(mut pr) => {
                pr.checks = summarize(&pr.status_check_rollup);
                PanelState::Pr(Box::new(pr))
            }
            Err(ForgeError::NotInstalled) => PanelState::NoGh,
            Err(ForgeError::NotAuthenticated) => PanelState::NotAuthenticated,
            Err(ForgeError::NoPr) => PanelState::NoPr,
            Err(ForgeError::RateLimited) => PanelState::RateLimited,
            Err(ForgeError::Offline) => PanelState::Offline,
            Err(e @ (ForgeError::NotConfigured(_) | ForgeError::Unsupported(_))) => {
                PanelState::Error {
                    message: e.describe(),
                }
            }
            Err(ForgeError::Other(m)) => PanelState::Error { message: m },
        };
        PrPanel {
            state,
            worktree,
            branch,
            fetched_at: crate::util::now(),
            threads: Vec::new(),
            issues: Vec::new(),
        }
    }
}

/// Parse the cached/CLI JSON array of PR headers (empty on any mismatch).
pub fn parse_pr_headers(json: &str) -> Vec<PrHeader> {
    serde_json::from_str(json).unwrap_or_default()
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

/// Parse the REST notifications payload down to this repo's @mention threads:
/// `(source_ref, message)` rows, where `source_ref` is
/// `ghn:<thread id>:<updated_at>` — stable per mention event, so the
/// emit-once store dedupe fires exactly once per (re-)mention. Pure.
pub fn parse_mention_notifications(json: &str, nwo: &str) -> Vec<(String, String)> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(items) = v.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|n| {
            n.pointer("/reason").and_then(|r| r.as_str()) == Some("mention")
                && n.pointer("/repository/full_name").and_then(|r| r.as_str()) == Some(nwo)
        })
        .filter_map(|n| {
            let id = n.pointer("/id")?.as_str()?;
            let updated = n
                .pointer("/updated_at")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            let title = n
                .pointer("/subject/title")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            let kind = n
                .pointer("/subject/type")
                .and_then(|t| t.as_str())
                .unwrap_or("thread");
            Some((
                format!("ghn:{id}:{updated}"),
                format!("mentioned in {kind}: {title}"),
            ))
        })
        .collect()
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
// --- deep PR view model ----------------------------------------------------

/// One PR-level issue comment (the Conversation timeline, non-inline).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrComment {
    pub author: String,
    pub body: String,
    #[serde(default)]
    pub created_at: String,
    /// GraphQL node id (reply/edit targeting); empty when unknown.
    #[serde(default)]
    pub id: String,
}

/// One submitted review (APPROVED / CHANGES_REQUESTED / COMMENTED / DISMISSED).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrReview {
    pub author: String,
    pub state: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub submitted_at: String,
}

/// A full review thread — the deep-view form ([`ReviewThreadRow`] is the
/// flattened panel-summary form). Carries the thread node id so the view can
/// reply, every comment, and the anchoring diff hunk for inline context.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewThread {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub resolved: bool,
    #[serde(default)]
    pub comments: Vec<PrComment>,
    #[serde(default)]
    pub diff_hunk: String,
}

/// The full conversation feed for the deep view (one GraphQL round trip).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrConversation {
    #[serde(default)]
    pub comments: Vec<PrComment>,
    #[serde(default)]
    pub reviews: Vec<PrReview>,
    #[serde(default)]
    pub threads: Vec<ReviewThread>,
}

/// A parsed unified diff for the Files tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrDiff {
    pub files: Vec<DiffFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffFile {
    /// New-side path (`b/…`); refined from the `+++` header.
    pub path: String,
    /// Old-side path (`a/…`); `None` for added files.
    #[serde(default)]
    pub old_path: Option<String>,
    /// True for a mode-160000 gitlink; its body is metadata, not selectable
    /// source text.
    #[serde(default)]
    pub is_submodule: bool,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    /// The new-side (RIGHT) line number — GitHub's anchor for an inline comment
    /// on an added/context line. `None` for deletions.
    #[serde(default)]
    pub new_lineno: Option<u64>,
    #[serde(default)]
    pub old_lineno: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

/// Parse a unified diff (as printed by `gh pr diff`) into a structured [`PrDiff`],
/// tracking per-line old/new line numbers so the Files tab can anchor inline
/// comments to the new-side line GitHub expects. Robust to partial/odd input:
/// anything it can't classify is skipped rather than panicking.
pub fn parse_unified_diff(raw: &str) -> PrDiff {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut old_no = 0u64;
    let mut new_no = 0u64;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            files.push(DiffFile {
                path: git_header_path(rest),
                old_path: None,
                is_submodule: false,
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue; // preamble before any `diff --git`
        };
        if line.starts_with("index ")
            && line
                .split_whitespace()
                .any(|part| part.eq_ignore_ascii_case("160000"))
        {
            file.is_submodule = true;
        }
        if line.starts_with("new file mode 160000")
            || line.starts_with("deleted file mode 160000")
            || line.starts_with("old mode 160000")
            || line.starts_with("new mode 160000")
            || line
                .strip_prefix('+')
                .or_else(|| line.strip_prefix('-'))
                .is_some_and(|body| body.starts_with("Subproject commit "))
        {
            file.is_submodule = true;
        }
        if let Some(p) = line.strip_prefix("--- ") {
            file.old_path = strip_ab(p);
            continue;
        }
        if let Some(p) = line.strip_prefix("+++ ") {
            if let Some(np) = strip_ab(p) {
                file.path = np;
            }
            continue;
        }
        if line.starts_with("@@") {
            if let Some((os, ns)) = parse_hunk_header(line) {
                old_no = os;
                new_no = ns;
            }
            file.hunks.push(DiffHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        // Body lines only count inside a hunk.
        let Some(hunk) = file.hunks.last_mut() else {
            continue;
        };
        let kind = match line.as_bytes().first() {
            Some(b'+') => DiffLineKind::Add,
            Some(b'-') => DiffLineKind::Del,
            Some(b' ') => DiffLineKind::Context,
            _ => continue, // `\ No newline at end of file`, stray lines, etc.
        };
        let text = line[1..].to_string();
        let (old_lineno, new_lineno) = match kind {
            DiffLineKind::Context => {
                let pair = (Some(old_no), Some(new_no));
                old_no += 1;
                new_no += 1;
                pair
            }
            DiffLineKind::Add => {
                let n = Some(new_no);
                new_no += 1;
                (None, n)
            }
            DiffLineKind::Del => {
                let o = Some(old_no);
                old_no += 1;
                (o, None)
            }
        };
        hunk.lines.push(DiffLine {
            kind,
            text,
            new_lineno,
            old_lineno,
        });
    }
    PrDiff { files }
}

/// `a/PATH b/PATH` → the new-side (`b/`) path; the `+++` header refines it later.
/// Split on the ` b/` separator (from the right) rather than the first space, so
/// paths containing spaces survive — binary/rename-only files emit no `+++`
/// header to correct a mis-split, so this is the final path for them.
fn git_header_path(rest: &str) -> String {
    if let Some((_, b)) = rest.rsplit_once(" b/") {
        return b.to_string();
    }
    // Fallback for odd input (no ` b/`): strip the `a/`|`b/` prefix off the first
    // token (single-path input has no space either).
    let first = rest.split_once(' ').map(|(a, _)| a).unwrap_or(rest);
    strip_ab(first).unwrap_or_else(|| first.to_string())
}

/// Strip the `a/`/`b/` prefix from a `---`/`+++` operand; `None` for `/dev/null`.
fn strip_ab(operand: &str) -> Option<String> {
    // git may append a tab + metadata; take the leading path token.
    let path = operand.split('\t').next().unwrap_or(operand).trim();
    if path == "/dev/null" {
        return None;
    }
    let p = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    (!p.is_empty()).then(|| p.to_string())
}

/// Parse `@@ -old_start[,n] +new_start[,n] @@ …` → `(old_start, new_start)`.
fn parse_hunk_header(line: &str) -> Option<(u64, u64)> {
    let inner = line.trim_start_matches('@').trim();
    let mut old_start = None;
    let mut new_start = None;
    for tok in inner.split_whitespace() {
        if let Some(rest) = tok.strip_prefix('-') {
            old_start = rest.split(',').next().and_then(|n| n.parse().ok());
        } else if let Some(rest) = tok.strip_prefix('+') {
            new_start = rest.split(',').next().and_then(|n| n.parse().ok());
            break;
        }
    }
    Some((old_start?, new_start?))
}

/// Parse a GraphQL conversation response (`CONVERSATION_QUERY`) into a
/// [`PrConversation`]. Accepts either the full `{data:{repository:{pullRequest}}}`
/// envelope or a bare `pullRequest` object (for tests).
pub fn parse_conversation(v: &serde_json::Value) -> PrConversation {
    let pr = v.pointer("/data/repository/pullRequest").unwrap_or(v);
    let node_array = |ptr: &str| {
        pr.pointer(ptr)
            .and_then(|n| n.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let comments = node_array("/comments/nodes")
        .iter()
        .map(comment_from_node)
        .collect();
    let reviews = node_array("/reviews/nodes")
        .iter()
        .filter_map(review_from_node)
        .collect();
    let threads = node_array("/reviewThreads/nodes")
        .iter()
        .map(thread_from_node)
        .collect();
    PrConversation {
        comments,
        reviews,
        threads,
    }
}

fn json_str(v: &serde_json::Value, ptr: &str) -> String {
    v.pointer(ptr)
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string()
}

fn comment_from_node(n: &serde_json::Value) -> PrComment {
    PrComment {
        author: json_str(n, "/author/login"),
        body: json_str(n, "/body"),
        created_at: json_str(n, "/createdAt"),
        id: json_str(n, "/id"),
    }
}

fn review_from_node(n: &serde_json::Value) -> Option<PrReview> {
    let state = json_str(n, "/state");
    let body = json_str(n, "/body");
    // Drop the empty `COMMENTED` envelope reviews that only carry inline
    // thread comments (surfaced under `threads` instead) — they'd be noise.
    if state.eq_ignore_ascii_case("COMMENTED") && body.trim().is_empty() {
        return None;
    }
    Some(PrReview {
        author: json_str(n, "/author/login"),
        state,
        body,
        submitted_at: json_str(n, "/submittedAt"),
    })
}

fn thread_from_node(n: &serde_json::Value) -> ReviewThread {
    let comments: Vec<PrComment> = n
        .pointer("/comments/nodes")
        .and_then(|c| c.as_array())
        .map(|nodes| nodes.iter().map(comment_from_node).collect())
        .unwrap_or_default();
    ReviewThread {
        id: json_str(n, "/id"),
        path: json_str(n, "/path"),
        line: n.get("line").and_then(|x| x.as_u64()),
        resolved: n
            .get("isResolved")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        diff_hunk: json_str(n, "/comments/nodes/0/diffHunk"),
        comments,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{GhError, classify, spawn_err, submit_review};
    use crate::remote::GitLoc;
    use crate::seam::SeamError;

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
            owner_repo_from_url("https://github.com/acme/thegn/pull/142"),
            Some(("acme".into(), "thegn".into()))
        );
        assert_eq!(
            owner_repo_from_url("https://github.com/acme/thegn"),
            Some(("acme".into(), "thegn".into()))
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
            nwo_from_remote_url("https://github.com/acme/thegn.git").as_deref(),
            Some("acme/thegn")
        );
        assert_eq!(
            nwo_from_remote_url("https://github.com/acme/thegn").as_deref(),
            Some("acme/thegn")
        );
        assert_eq!(
            nwo_from_remote_url("ssh://git@github.com/acme/thegn.git").as_deref(),
            Some("acme/thegn")
        );
        assert_eq!(
            nwo_from_remote_url("git@github.com:acme/thegn.git").as_deref(),
            Some("acme/thegn")
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
    fn parse_mention_notifications_filters_reason_and_repo() {
        let json = r#"[
            {"id":"11","reason":"mention","updated_at":"2026-08-20T10:00:00Z",
             "repository":{"full_name":"o/r"},
             "subject":{"title":"fix the panel","type":"PullRequest"}},
            {"id":"12","reason":"review_requested","updated_at":"2026-08-20T10:00:00Z",
             "repository":{"full_name":"o/r"},
             "subject":{"title":"other","type":"PullRequest"}},
            {"id":"13","reason":"mention","updated_at":"2026-08-20T11:00:00Z",
             "repository":{"full_name":"other/repo"},
             "subject":{"title":"elsewhere","type":"Issue"}}
        ]"#;
        let rows = parse_mention_notifications(json, "o/r");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "ghn:11:2026-08-20T10:00:00Z");
        assert!(rows[0].1.contains("fix the panel"), "{}", rows[0].1);
        // Garbage / non-array payloads parse to nothing.
        assert!(parse_mention_notifications("not json", "o/r").is_empty());
        assert!(parse_mention_notifications("{}", "o/r").is_empty());
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
            "headRefName": "tg/add-thing", "baseRefName": "main",
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
    fn parse_unified_diff_tracks_line_numbers_and_paths() {
        // Built from a line array so leading spaces on context lines survive
        // (a `\`-continuation in a string literal would strip them).
        let raw = [
            "diff --git a/src/foo.rs b/src/foo.rs",
            "index 1234567..89abcde 100644",
            "--- a/src/foo.rs",
            "+++ b/src/foo.rs",
            "@@ -10,4 +10,5 @@ fn existing() {",
            " ctx one",
            "-removed line",
            "+added line a",
            "+added line b",
            " ctx two",
            "diff --git a/new.txt b/new.txt",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/new.txt",
            "@@ -0,0 +1,1 @@",
            "+hello",
            "\\ No newline at end of file",
        ]
        .join("\n");
        let diff = parse_unified_diff(&raw);
        assert_eq!(diff.files.len(), 2);

        let f0 = &diff.files[0];
        assert_eq!(f0.path, "src/foo.rs");
        assert_eq!(f0.old_path.as_deref(), Some("src/foo.rs"));
        assert_eq!(f0.hunks.len(), 1);
        let lines = &f0.hunks[0].lines;
        // ctx one: old 10 / new 10
        assert_eq!(lines[0].kind, DiffLineKind::Context);
        assert_eq!(lines[0].old_lineno, Some(10));
        assert_eq!(lines[0].new_lineno, Some(10));
        // removed: old 11 / new None
        assert_eq!(lines[1].kind, DiffLineKind::Del);
        assert_eq!(lines[1].old_lineno, Some(11));
        assert_eq!(lines[1].new_lineno, None);
        // added a: old None / new 11
        assert_eq!(lines[2].kind, DiffLineKind::Add);
        assert_eq!(lines[2].old_lineno, None);
        assert_eq!(lines[2].new_lineno, Some(11));
        // added b: new 12
        assert_eq!(lines[3].new_lineno, Some(12));
        assert_eq!(lines[3].text, "added line b");
        // ctx two: old 12 (was at 11, del bumped it) / new 13
        assert_eq!(lines[4].kind, DiffLineKind::Context);
        assert_eq!(lines[4].old_lineno, Some(12));
        assert_eq!(lines[4].new_lineno, Some(13));

        // Added file: /dev/null old side → old_path None; new line anchored at 1.
        let f1 = &diff.files[1];
        assert_eq!(f1.path, "new.txt");
        assert_eq!(f1.old_path, None);
        assert_eq!(f1.hunks[0].lines[0].new_lineno, Some(1));

        // Garbage degrades to an empty diff, never panics.
        assert!(
            parse_unified_diff("not a diff\nrandom text")
                .files
                .is_empty()
        );
        // A round-trip through serde preserves the structure.
        let json = serde_json::to_string(&diff).unwrap();
        assert_eq!(serde_json::from_str::<PrDiff>(&json).unwrap(), diff);
    }

    #[test]
    fn git_header_path_keeps_spaces_in_binary_paths() {
        // Binary / rename-only diffs emit no `+++` header, so `git_header_path`
        // is the final path. A filename with spaces must not be split on the
        // first space (regression: old code yielded "shot.png b/docs/...").
        assert_eq!(
            git_header_path("a/docs/screen shot.png b/docs/screen shot.png"),
            "docs/screen shot.png"
        );
        assert_eq!(git_header_path("a/src/foo.rs b/src/foo.rs"), "src/foo.rs");
        // Pure rename: old and new paths differ, both with spaces.
        assert_eq!(
            git_header_path("a/old name.txt b/new name.txt"),
            "new name.txt"
        );
        // Odd input with no ` b/` falls back to the leading-token behavior.
        assert_eq!(git_header_path("a/only.txt"), "only.txt");
    }

    #[test]
    fn unified_diff_marks_gitlinks_as_non_text_submodules() {
        let diff = parse_unified_diff(
            "diff --git a/vendor/lib b/vendor/lib\nindex aaaaaaa..bbbbbbb 160000\n--- a/vendor/lib\n+++ b/vendor/lib\n@@ -1 +1 @@\n-Subproject commit aaaaaaa\n+Subproject commit bbbbbbb\n",
        );
        assert_eq!(diff.files.len(), 1);
        assert!(diff.files[0].is_submodule);
    }

    #[test]
    fn classify_distinguishes_not_installed_404_and_offline() {
        // Shell "command not found" → NotInstalled.
        assert!(matches!(
            classify("gh: command not found"),
            GhError::NotInstalled
        ));
        assert!(matches!(
            classify("/bin/sh: gh: not found"),
            GhError::NotInstalled
        ));
        assert!(matches!(
            classify("no such file or directory"),
            GhError::NotInstalled
        ));
        // A REST 404 must NOT be misread as NotInstalled — gh prints it as
        // `Not Found (HTTP 404)`, whose lowercase contains "not found".
        assert!(matches!(
            classify("gh: not found (http 404)"),
            GhError::NoPr
        ));
        // Offline: real gh network stderr classifies as Offline, not Other.
        assert!(matches!(
            classify("error connecting to api.github.com\ncheck your internet connection"),
            GhError::Offline
        ));
        assert!(matches!(
            classify("dial tcp: lookup api.github.com: no such host"),
            GhError::Offline
        ));
        assert!(matches!(
            classify("could not resolve host: api.github.com"),
            GhError::Offline
        ));
        assert!(classify("error connecting to api.github.com").is_transient());
        // Still-correct existing branches.
        assert!(matches!(
            classify("no pull requests found for branch"),
            GhError::NoPr
        ));
        assert!(matches!(classify("http 401"), GhError::NotAuthenticated));
        assert!(matches!(
            classify("api rate limit exceeded"),
            GhError::RateLimited
        ));
        assert!(matches!(classify("some other failure"), GhError::Other(_)));
    }

    #[test]
    fn spawn_err_maps_enoent_to_not_installed() {
        use std::io::{Error, ErrorKind};
        assert!(matches!(
            spawn_err(Error::from(ErrorKind::NotFound)),
            GhError::NotInstalled
        ));
        assert!(matches!(
            spawn_err(Error::from(ErrorKind::PermissionDenied)),
            GhError::Other(_)
        ));
    }

    #[test]
    fn parse_conversation_reads_comments_reviews_and_threads() {
        let json = r#"{"data":{"repository":{"pullRequest":{
            "comments":{"nodes":[
                {"author":{"login":"alice"},"body":"top-level comment","createdAt":"2026-06-11T10:00:00Z"}
            ]},
            "reviews":{"nodes":[
                {"author":{"login":"bob"},"state":"APPROVED","body":"LGTM","submittedAt":"2026-06-11T11:00:00Z"},
                {"author":{"login":"bot"},"state":"COMMENTED","body":"","submittedAt":"2026-06-11T11:05:00Z"}
            ]},
            "reviewThreads":{"nodes":[
                {"id":"THREAD_1","isResolved":false,"path":"src/x.rs","line":42,
                 "comments":{"nodes":[
                    {"author":{"login":"carol"},"body":"nit here","createdAt":"2026-06-11T09:00:00Z",
                     "diffHunk":"@@ -1 +1 @@\n-old\n+new"}
                 ]}}
            ]}
        }}}}"#;
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        let conv = parse_conversation(&v);
        assert_eq!(conv.comments.len(), 1);
        assert_eq!(conv.comments[0].author, "alice");
        // The empty COMMENTED envelope review is dropped; only the real one stays.
        assert_eq!(conv.reviews.len(), 1);
        assert_eq!(conv.reviews[0].state, "APPROVED");
        assert_eq!(conv.threads.len(), 1);
        assert_eq!(conv.threads[0].id, "THREAD_1");
        assert_eq!(conv.threads[0].line, Some(42));
        assert_eq!(conv.threads[0].comments[0].body, "nit here");
        assert!(conv.threads[0].diff_hunk.contains("+new"));

        // Bare pullRequest object (no envelope) also parses.
        let bare = v.pointer("/data/repository/pullRequest").unwrap();
        assert_eq!(parse_conversation(bare).comments.len(), 1);
        // Garbage → empty, never panics.
        assert!(
            parse_conversation(&serde_json::json!({}))
                .comments
                .is_empty()
        );
    }

    #[test]
    fn submit_review_requires_body_for_non_approve() {
        let loc = GitLoc::for_worktree(std::path::Path::new("/nonexistent"));
        // request-changes / comment without a body fail before touching `gh`.
        assert!(matches!(
            submit_review(&loc, ReviewState::RequestChanges, None),
            Err(GhError::Other(_))
        ));
        assert!(matches!(
            submit_review(&loc, ReviewState::Comment, Some("   ")),
            Err(GhError::Other(_))
        ));
    }

    #[test]
    fn panel_state_serializes_with_kind_tag() {
        let panel = PrPanel {
            state: PanelState::NoPr,
            worktree: "/tmp/wt".into(),
            branch: "tg/x".into(),
            fetched_at: 0,
            threads: Vec::new(),
            issues: Vec::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&panel).unwrap();
        assert_eq!(v["kind"], "no_pr");
        assert_eq!(v["branch"], "tg/x");
    }

    #[test]
    fn pr_variant_flattens_for_the_panel() {
        let json = r#"{"number":7,"title":"x","state":"OPEN","url":"u",
            "isDraft":false,"headRefName":"tg/x","baseRefName":"main",
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","reviewDecision":"APPROVED",
            "statusCheckRollup":[{"name":"b","status":"COMPLETED","conclusion":"SUCCESS"}]}"#;
        let mut pr: PrStatus = serde_json::from_str(json).unwrap();
        pr.checks = summarize(&pr.status_check_rollup);
        let panel = PrPanel {
            state: PanelState::Pr(Box::new(pr)),
            worktree: "/tmp/wt".into(),
            branch: "tg/x".into(),
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
        assert_eq!(v["branch"], "tg/x");
    }

    #[test]
    fn from_result_maps_every_error_to_a_state() {
        let mk = |r| PrPanel::from_result(r, "/w".into(), "b".into());
        assert!(matches!(
            mk(Err(ForgeError::NotInstalled)).state,
            PanelState::NoGh
        ));
        assert!(matches!(
            mk(Err(ForgeError::NotAuthenticated)).state,
            PanelState::NotAuthenticated
        ));
        assert!(matches!(mk(Err(ForgeError::NoPr)).state, PanelState::NoPr));
        assert!(matches!(
            mk(Err(ForgeError::RateLimited)).state,
            PanelState::RateLimited
        ));
        assert!(matches!(
            mk(Err(ForgeError::Offline)).state,
            PanelState::Offline
        ));
        assert!(matches!(
            mk(Err(ForgeError::Other("x".into()))).state,
            PanelState::Error { message } if message == "x"
        ));
        assert!(matches!(
            mk(Err(ForgeError::Unsupported("op"))).state,
            PanelState::Error { .. }
        ));
        assert!(matches!(
            mk(Err(ForgeError::NotConfigured("t"))).state,
            PanelState::Error { .. }
        ));
        let ok = mk(Ok(PrStatus {
            number: 1,
            ..Default::default()
        }));
        assert!(matches!(ok.state, PanelState::Pr(p) if p.number == 1));
        assert_eq!(ok.worktree, "/w");
        assert_eq!(ok.branch, "b");
    }
}
