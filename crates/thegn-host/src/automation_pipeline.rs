//! `pipeline.start`: an `issue_matched` rule starting the stage pipeline for
//! the matched issue (THE-2195, filled by b153 lane 5).
//!
//! A rule-only action (`automation::RULE_ONLY_ACTIONS`). The effective
//! autonomy rides on the planned action: `propose` raises an approval and
//! starts nothing; `review`/`merge` start the first stage and record the
//! ceiling on the dispatch row (`set_dispatch_autonomy`), which the stage
//! executor enforces at land.

use std::sync::Arc;

use thegn_core::automation::{AutomationEvent, PlannedAction};
use thegn_core::config::Config;

#[allow(dead_code, unused_variables, reason = "filled by b153 lane 5")]
pub(crate) async fn start(
    cfg: Arc<Config>,
    event: &AutomationEvent,
    action: &PlannedAction,
    origin: thegn_svc::control::AutomationOrigin,
) -> anyhow::Result<()> {
    anyhow::bail!("pipeline.start is not implemented yet")
}
