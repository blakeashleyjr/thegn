//! The `issue_matched` producer edge (THE-2194, filled by b153 lane 3).
//!
//! Called from the existing tracker-refresh background job, and only when an
//! enabled `issue_matched` rule exists (config or DB). It diffs the cached and
//! fresh issue lists with `thegn_core::automation_issue::matched_candidates`,
//! projects facts (`facts_from_issue`) and submits `issue_event`s through
//! `automation_runtime::submit`, at most
//! `automation_issue::MAX_ISSUE_EVENTS_PER_REFRESH` per refresh. Phase 3 moves
//! the call site into the daemon unchanged.

use thegn_core::config::Config;
use thegn_core::issue::Issue;

/// One tracker refresh's inputs.
#[allow(dead_code, reason = "filled by b153 lane 3")]
pub(crate) struct IssueRefresh<'a> {
    pub cfg: &'a Config,
    pub repo_key: &'a str,
    pub provider: &'a str,
    pub account: &'a str,
    pub assignee_me: bool,
    pub had_cache: bool,
}

/// Submit an `issue_matched` event for each matched candidate.
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 3")]
pub(crate) fn emit_matched(
    db: &thegn_core::db::Db,
    ctx: &IssueRefresh<'_>,
    old: &[Issue],
    new: &[Issue],
) {
}
