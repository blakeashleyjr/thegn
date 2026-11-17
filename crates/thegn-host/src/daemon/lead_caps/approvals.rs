//! `approvals.list` / `approvals.request` / `approvals.resolve` (THE-2172).
//! A request is an `agent_attention` notification row; resolving is Admin.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 6.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `approvals.list`. Stub: `Unimplemented`.
pub(crate) async fn list(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("approvals.list"))
}

/// Answer `approvals.request`. Stub: `Unimplemented`.
pub(crate) async fn request(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("approvals.request"))
}

/// Answer `approvals.resolve` (Admin). Stub: `Unimplemented`.
pub(crate) async fn resolve(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("approvals.resolve"))
}

/// The outstanding approval requests, newest first, for the overview. Stub:
/// none.
#[allow(dead_code, reason = "filled by b152 lane 6")]
pub(crate) fn pending(
    _db: &thegn_core::db::Db,
    _now_ms: i64,
    _limit: usize,
) -> anyhow::Result<Vec<thegn_core::approval_request::PendingApproval>> {
    Ok(Vec::new())
}
