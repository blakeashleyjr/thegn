//! Approved `propose` starts (b155): an `automation_proposals` row waits for
//! the owner; on approval the pipeline starts at `review` (never `merge`)
//! after re-validating the stored proposal; on rejection or expiry the row is
//! marked and nothing starts.
//!
//! Spine stubs: b155 lane 3 fills them. Nothing starts until then.

use std::sync::Arc;

use thegn_core::db::Db;

use crate::daemon::service::DaemonService;

/// Start the proposal waiting on approval `(issue, stage, sha)`.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) async fn on_approved(
    _svc: &Arc<DaemonService>,
    _issue: &str,
    _stage: &str,
    _sha: &str,
) -> anyhow::Result<()> {
    Ok(())
}

/// Mark the proposal waiting on approval `(issue, stage, sha)` rejected.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) async fn on_rejected(
    _svc: &Arc<DaemonService>,
    _issue: &str,
    _stage: &str,
    _sha: &str,
) -> anyhow::Result<()> {
    Ok(())
}

/// The trusted description of a proposal approval, from the stored row.
#[allow(dead_code, reason = "filled by b155 lane 3")]
pub(crate) fn describe(_db: &Db, _issue: &str, _stage: &str, _sha: &str) -> Option<String> {
    None
}
