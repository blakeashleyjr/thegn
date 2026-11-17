//! `notifications.list` / `notifications.ack` (THE-2172).
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 4.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `notifications.list`. Stub: `Unimplemented`.
pub(crate) async fn list(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("notifications.list"))
}

/// Answer `notifications.ack`; exactly one of `id`, `all`, `worktree`. Stub: `Unimplemented`.
pub(crate) async fn ack(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("notifications.ack"))
}
