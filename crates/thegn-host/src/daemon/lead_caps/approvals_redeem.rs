//! `approvals.redeem` (b155 6a): the owner's phone answers an approval with
//! the single-use nonce its push carried. Write scope, HTTP + CLI only, and
//! refused for a Lead-confined caller; it also needs the push inbox's HMAC
//! secret (`[notifications.push.inbox] allow` must list it).
//!
//! Spine stub: b155 lane 6 fills it. Unimplemented until then.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `approvals.redeem` (`{nonce, decision}`).
#[allow(dead_code, reason = "filled by b155 lane 6")]
pub(crate) async fn redeem(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("approvals.redeem"))
}
