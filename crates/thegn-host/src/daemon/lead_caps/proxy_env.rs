//! `mcp_proxy.profile_env` (b155 THE-2222): a `[mcp_proxy.profiles]` entry's
//! resolved server env, for `thegn mcp proxy --profile` inside the Lead
//! sandbox. Returned only to a Lead-labelled token for `[lead] mcp_profile`,
//! or to an admin caller.
//!
//! Spine stub: b155 lane 9 fills it. Unimplemented until then.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::{ControlError, ProfileEnvCaller};

/// Answer `mcp_proxy.profile_env` (`{profile}`).
#[allow(dead_code, reason = "filled by b155 lane 9")]
pub(crate) async fn profile_env(
    _svc: &DaemonService,
    _req: serde_json::Value,
    _caller: ProfileEnvCaller,
) -> JsonResult {
    Err(ControlError::Unimplemented("mcp_proxy.profile_env"))
}
