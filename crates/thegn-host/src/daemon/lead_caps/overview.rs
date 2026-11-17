//! `overview.get` (THE-2171): the Lead's one-call cross-workspace overview.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 3.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `overview.get`. Stub: `Unimplemented`.
pub(crate) async fn get(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("overview.get"))
}
