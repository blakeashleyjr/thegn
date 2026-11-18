//! The daemon stage executor (b155): event-driven, no timer. It is kicked by
//! a worker exit (after `pipeline_retry` stamps the row), the reaper's
//! `CloseDone`, a successful `dispatches.set_status` to done or
//! `dispatches.report`, an approved lane approval, and one reconcile pass at
//! daemon start. It acts only on what `pipeline_supervise::plan` returns.
//!
//! With `[pipeline.supervisor] enabled = false` (the default) or no
//! `[[pipeline.stages]]`, `install` does nothing and every kick returns at
//! once (a relaxed atomic load): the idle invariant.
//!
//! Spine stubs: b155 lane 2 fills them. Inert until then.

use std::sync::Arc;

use thegn_core::config::Config;

use super::service::DaemonService;

/// Start the executor when the supervisor is enabled and stages exist.
#[allow(dead_code, reason = "filled by b155 lane 2")]
pub(crate) fn install(_svc: Arc<DaemonService>, _cfg: Arc<Config>) {}

/// Whether the executor runs for this config.
#[allow(dead_code, reason = "filled by b155 lane 2")]
pub(crate) fn enabled(_cfg: &Config) -> bool {
    false
}

/// Re-plan the lane owning dispatch row `row_id`.
#[allow(dead_code, reason = "filled by b155 lane 2")]
pub(crate) fn kick_row(_row_id: i64) {}

/// Re-plan the lane whose worker ran in daemon session `session`.
#[allow(dead_code, reason = "filled by b155 lane 2")]
pub(crate) fn kick_session(_session: &str) {}

/// Re-plan every lane of tracker issue `issue_id`.
#[allow(dead_code, reason = "filled by b155 lane 2")]
pub(crate) fn kick_issue(_issue_id: &str) {}

/// Cancel every in-flight executor validation; returns how many stopped.
#[allow(dead_code, reason = "filled by b155 lane 2")]
pub(crate) fn cancel_all() -> usize {
    0
}

#[cfg(test)]
mod tests {
    #[test]
    fn stubs_are_inert() {
        let cfg = thegn_core::config::Config::default();
        assert!(!super::enabled(&cfg));
        super::kick_row(1);
        super::kick_session("s");
        super::kick_issue("linear:THE-1");
        assert_eq!(super::cancel_all(), 0);
    }
}
