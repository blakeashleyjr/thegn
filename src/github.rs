//! GitHub integration via the `gh` CLI. The native binary is the data/action
//! provider; the WASM panel plugin renders what we emit and triggers our action
//! subcommands (it can't shell out itself).
//!
//! Everything runs with `cwd = worktree` so `gh` auto-detects the repo from its
//! remote (mirrors how `util::git_out` uses `-C dir`). All failure modes the
//! panel cares about are mapped onto `PanelState` so the UI never crashes.

use crate::util;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

/// Distinguishable `gh` failure modes.
#[derive(Debug)]
pub enum GhError {
    NotInstalled,
    NotAuthenticated,
    NoPr,
    RateLimited,
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

/// Run `gh <args>` with `cwd = dir`; trimmed stdout on success, else a
/// classified error.
pub fn gh_out(dir: &Path, args: &[&str]) -> Result<String, GhError> {
    if !util::have("gh") {
        return Err(GhError::NotInstalled);
    }
    let out = Command::new("gh")
        .current_dir(dir)
        .args(args)
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
pub fn gh_run(dir: &Path, args: &[&str]) -> Result<(), GhError> {
    if !util::have("gh") {
        return Err(GhError::NotInstalled);
    }
    let out = Command::new("gh")
        .current_dir(dir)
        .args(args)
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
    if stderr.contains("no pull requests found")
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
            GhError::Other(m) => m.clone(),
        }
    }
}

// --- serde model ----------------------------------------------------------

/// The full panel feed for one worktree (flattened state + metadata).
#[derive(Debug, Clone, Serialize)]
pub struct PrPanel {
    #[serde(flatten)]
    pub state: PanelState,
    pub worktree: String,
    pub branch: String,
    pub fetched_at: i64,
}

/// The per-worktree PR state, internally tagged by `kind` for the plugin.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PanelState {
    NoGh,
    NotAuthenticated,
    NoPr,
    RateLimited,
    Error { message: String },
    Pr(Box<PrStatus>),
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
pub fn pr_status(worktree: &Path) -> PrPanel {
    let branch =
        util::git_out(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let state = match gh_out(worktree, &["pr", "view", "--json", PR_FIELDS]) {
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
        Err(GhError::Other(m)) => PanelState::Error { message: m },
    };
    PrPanel {
        state,
        worktree: worktree.to_string_lossy().into_owned(),
        branch,
        fetched_at: util::now(),
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

pub fn create_pr(worktree: &Path, o: &CreateOpts) -> Result<String, GhError> {
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
    gh_out(worktree, &refs)
}

pub fn open_pr(worktree: &Path) -> Result<(), GhError> {
    gh_run(worktree, &["pr", "view", "--web"])
}

pub fn approve_pr(worktree: &Path, body: Option<&str>) -> Result<(), GhError> {
    let mut args = vec!["pr", "review", "--approve"];
    if let Some(b) = body {
        args.push("--body");
        args.push(b);
    }
    gh_run(worktree, &args)
}

pub fn merge_pr(
    worktree: &Path,
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
    gh_run(worktree, &args)
}

/// Print review comments / reviews as JSON.
pub fn reviews(worktree: &Path) -> Result<String, GhError> {
    gh_out(
        worktree,
        &["pr", "view", "--json", "reviews,latestReviews,comments"],
    )
}

/// Re-run failed workflow runs for the worktree's branch. Returns the count.
pub fn rerun_failed_checks(worktree: &Path) -> Result<u32, GhError> {
    let branch = util::git_out(worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok_or_else(|| GhError::Other("could not resolve branch".into()))?;
    // Enumerate this branch's workflow runs and re-run any that failed.
    let json = gh_out(
        worktree,
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
            if gh_run(worktree, &["run", "rerun", &id, "--failed"]).is_ok() {
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

    fn cr(status: &str, conclusion: Option<&str>, state: Option<&str>) -> CheckRun {
        CheckRun {
            name: "ci".into(),
            status: status.into(),
            conclusion: conclusion.map(String::from),
            state: state.map(String::from),
            workflow_name: None,
            details_url: None,
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
