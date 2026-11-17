//! The outward-action gate (b155 6d): a Lead-confined `issues.comment` /
//! `issues.update` needs a live owner approval bound to the call's digest
//! (`thegn_core::outward_gate::approval_ids`). Without one the daemon raises
//! it and refuses with "needs owner approval ... retry the identical call";
//! with one it lets the call through once and revokes it.
//! `[lead] outward_allow` opts a cap out.
//!
//! Spine stub: b155 lane 4 fills it. Fails closed until then.

use crate::daemon::service::DaemonService;
use thegn_svc::control::{ControlError, ControlResult, LeadCaller};

/// Admit (or refuse) one outward call by a Lead-confined caller.
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub(crate) async fn gate(
    _svc: &DaemonService,
    _cap: &str,
    _target: &str,
    _body: serde_json::Value,
    _caller: &LeadCaller,
) -> ControlResult<()> {
    Err(ControlError::FailedPrecondition(
        "outward actions need owner approval".into(),
    ))
}
