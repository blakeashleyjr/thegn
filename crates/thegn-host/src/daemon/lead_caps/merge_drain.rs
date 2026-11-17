//! `merge.drain` (THE-2172): start the merge-queue drain for a worktree's
//! repo. Spawns only on request.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 6.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `merge.drain`. Stub: `Unimplemented`.
pub(crate) async fn drain(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("merge.drain"))
}
