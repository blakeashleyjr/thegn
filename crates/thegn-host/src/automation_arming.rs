//! Lead rule arming (b155 6c): a Lead rule asking above its cap is stored
//! disarmed and raises an arming approval; it compiles as
//! `RuleOrigin::LeadApproved` only while a live arming approval matches the
//! current body digest (30-day expiry).
//!
//! Spine stubs: b155 lane 4 fills them. Nothing is armed until then.

use std::collections::BTreeSet;
use std::sync::Arc;

use thegn_core::db::Db;

use crate::daemon::service::DaemonService;

/// Re-check the rule against the approved digest and arm it.
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub(crate) async fn on_approved(
    _svc: &Arc<DaemonService>,
    _rule_id: &str,
    _sha: &str,
) -> anyhow::Result<()> {
    Ok(())
}

/// `(rule id, body digest)` pairs with a live arming approval.
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub(crate) fn armed_set(_db: &Db, _now_ms: i64) -> BTreeSet<(String, String)> {
    BTreeSet::new()
}

/// The trusted description of an arming approval, from the stored rule.
#[allow(dead_code, reason = "filled by b155 lane 4")]
pub(crate) fn describe(_db: &Db, _rule_id: &str, _sha: &str) -> Option<String> {
    None
}
