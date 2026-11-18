//! `automations.halt` (b155 6f): the operator's kill switch. Kills every live
//! session carrying an automation origin or opened by a Lead-confined caller,
//! cancels in-flight executor validations and any headless Lead turn, and
//! reports what it stopped. Admin scope, HTTP + CLI only.
//!
//! Spine stub: b155 lane 7 fills it. Unimplemented until then.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `automations.halt` (`{keep_sessions}`).
#[allow(dead_code, reason = "filled by b155 lane 7")]
pub(crate) async fn halt(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("automations.halt"))
}
