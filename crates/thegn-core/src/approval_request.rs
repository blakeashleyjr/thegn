//! Approval requests (`approvals.request` / `approvals.list` /
//! `approvals.resolve`, THE-2172).
//!
//! A request only asks: it is an `agent_attention` notification row naming the
//! lane, stage and commit. Granting stays an Admin act outside every agent door
//! (see [`crate::pipeline_approval`]: no supervisor path may grant).
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 6. The two
//! types below are final.

use serde::{Deserialize, Serialize};

/// One outstanding approval request: a human has been asked to approve
/// `stage` of `issue_id` at `commit_sha` and has not answered yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// Tracker issue id (`"<provider>:<key>"`), the lane's identity.
    pub issue_id: String,
    /// The stage whose output awaits approval.
    pub stage: String,
    /// The commit the approval would cover.
    pub commit_sha: String,
    /// The worktree the stage runs in (empty when unknown).
    pub worktree: String,
    /// When the request was made (epoch ms).
    pub requested_at: i64,
    /// The `agent_attention` notification row that carries the request.
    pub notification_id: i64,
    /// The human-readable request text.
    pub message: String,
}

/// One recorded approval as `approvals.list` reports it: the record plus its
/// evaluated state token ([`crate::pipeline_approval::ApprovalState::token`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    /// The recorded approval.
    #[serde(flatten)]
    pub approval: crate::pipeline_approval::Approval,
    /// `live`, `revoked`, `superseded`, `expired`, … as evaluated now.
    pub state: String,
}
