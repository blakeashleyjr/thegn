use crate::remote::GitLoc;
use serde::{Deserialize, Serialize};

/// Distinguishable `gh` (or other forge CLI) failure modes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub url: String,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateIssueOpts {
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

#[derive(Debug)]
pub enum ForgeError {
    NotInstalled,
    NotAuthenticated,
    NoPr,
    RateLimited,
    Other(String),
}

impl ForgeError {
    pub fn message(&self) -> String {
        match self {
            ForgeError::NotInstalled => "Forge CLI not installed".into(),
            ForgeError::NotAuthenticated => "Forge CLI not authenticated".into(),
            ForgeError::NoPr => "no PR for this branch".into(),
            ForgeError::RateLimited => "Forge API rate limited".into(),
            ForgeError::Other(m) => m.clone(),
        }
    }
}

/// How to merge a PR.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
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

/// Options for `create_pr`.
pub struct CreateOpts {
    pub title: Option<String>,
    pub body: Option<String>,
    pub base: Option<String>,
    pub draft: bool,
    pub web: bool,
    pub fill: bool,
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
    NoForgeCli,
    NotAuthenticated,
    NoPr,
    RateLimited,
    Error { message: String },
    Pr(Box<PrStatus>),
}

/// Deserialized from `gh pr view --json …` (or similar), plus a computed checks rollup.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
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
    #[serde(default)]
    pub linked_issue: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ChecksSummary {
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub total: u32,
}

impl PrStatus {
    /// Recompute the checks rollup from `status_check_rollup`.
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
