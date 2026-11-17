//! Lead-authored automation rules: `automations.put` / `rm` / `enable` /
//! `runs` (THE-2172), plus the two hooks `automations.list` and
//! `automations.test` call to see Lead rules beside config rules.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 5.

use super::JsonResult;
use crate::daemon::service::DaemonService;
use thegn_svc::control::ControlError;

/// Answer `automations.put`. Stub: `Unimplemented`.
pub(crate) async fn put(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("automations.put"))
}

/// Answer `automations.rm`; config rules answer `Conflict`. Stub: `Unimplemented`.
pub(crate) async fn rm(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("automations.rm"))
}

/// Answer `automations.enable`; config rules answer `Conflict`. Stub: `Unimplemented`.
pub(crate) async fn enable(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("automations.enable"))
}

/// Answer `automations.runs`. Stub: `Unimplemented`.
pub(crate) async fn runs(_svc: &DaemonService, _req: serde_json::Value) -> JsonResult {
    Err(ControlError::Unimplemented("automations.runs"))
}

/// The Lead-authored rules, appended after the config rows by
/// `automations.list`. Stub: none.
pub(crate) async fn list_lead_rules(
    _svc: &DaemonService,
) -> thegn_svc::control::ControlResult<Vec<thegn_svc::control::AutomationRuleInfo>> {
    Ok(Vec::new())
}

/// A Lead-authored rule by name, the `automations.test` fallback when no config
/// rule matches. Stub: none.
pub(crate) async fn find_rule(
    _svc: &DaemonService,
    _name: &str,
) -> thegn_svc::control::ControlResult<Option<thegn_core::automation::AutomationRule>> {
    Ok(None)
}
