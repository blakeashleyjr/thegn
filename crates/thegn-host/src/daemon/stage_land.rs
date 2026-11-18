//! The executor's terminal step (b155 5c): land a lane through the merge
//! queue when `thegn_core::stage_land::unattended_land` passes, otherwise
//! raise the land approval with the failed check as the reason. "Stop and
//! ask" is the outcome of every doubt.
//!
//! Spine stubs: b155 lane 3 fills them. Every terminal holds until then.

use std::sync::Arc;

use thegn_core::config::Config;
use thegn_core::pipeline_supervise::{EscalationReason, LaneFacts, SuperviseAction};

use super::service::DaemonService;

/// One lane at its terminal decision, as the executor gathered it.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) struct LaneCtx {
    pub issue_id: String,
    pub row: i64,
    pub stage: String,
    pub worktree: String,
    pub head_sha: String,
    pub facts: LaneFacts,
    /// Dispatch row ids from the lane's root to `row`.
    pub chain: Vec<i64>,
}

/// What the terminal step did.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) enum TerminalOutcome {
    Landed { drain_started: bool },
    AskedApproval { reason: String },
    Held { reason: String },
}

/// Handle a terminal `Enqueue` or `Escalate{AwaitingApproval}`.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) async fn on_terminal(
    _svc: &Arc<DaemonService>,
    _cfg: &Arc<Config>,
    _ctx: &LaneCtx,
    _action: &SuperviseAction,
) -> anyhow::Result<TerminalOutcome> {
    Ok(TerminalOutcome::Held {
        reason: "land is not implemented".into(),
    })
}

/// Handle any other `Escalate`.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) async fn on_escalate(
    _svc: &Arc<DaemonService>,
    _cfg: &Arc<Config>,
    _ctx: &LaneCtx,
    _why: &EscalationReason,
) -> anyhow::Result<()> {
    Ok(())
}
