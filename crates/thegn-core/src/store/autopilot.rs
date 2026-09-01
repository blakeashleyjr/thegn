//! Store seam for the issue-autopilot claim journal.

use crate::autopilot::{AutopilotIssueKey, AutopilotState, AutopilotSummary};
use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed(Box<AutopilotSummary>),
    AlreadyClaimed,
    AtCapacity,
    AttemptsExhausted,
}

pub trait AutopilotStore {
    /// Atomically claim an issue, enforcing the unique issue identity and the
    /// repository's active-run and attempt budgets.
    fn claim_autopilot(
        &self,
        key: &AutopilotIssueKey,
        repo_root: &str,
        max_concurrent: u32,
        max_attempts: u32,
        now: i64,
    ) -> Result<ClaimOutcome>;

    fn get_autopilot_run(&self, id: i64) -> Result<Option<AutopilotSummary>>;
    fn list_autopilot_runs(&self, repo_root: &str, limit: usize) -> Result<Vec<AutopilotSummary>>;
    fn transition_autopilot(
        &self,
        id: i64,
        expected: AutopilotState,
        next: AutopilotState,
        reason: Option<&str>,
        pr_number: Option<u64>,
        now: i64,
    ) -> Result<bool>;
    fn attach_autopilot_dispatch(&self, id: i64, dispatch_id: i64, now: i64) -> Result<bool>;
    fn set_autopilot_worktree(
        &self,
        id: i64,
        worktree: &str,
        branch: &str,
        base_branch: &str,
        now: i64,
    ) -> Result<bool>;
    fn set_autopilot_pr(
        &self,
        id: i64,
        number: u64,
        head: &str,
        url: &str,
        now: i64,
    ) -> Result<bool>;
    fn find_autopilot_by_pr(
        &self,
        repo_root: &str,
        number: u64,
    ) -> Result<Option<AutopilotSummary>>;
}
