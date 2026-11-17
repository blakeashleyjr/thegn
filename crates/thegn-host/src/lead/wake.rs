//! `lead.wake`: run a bounded headless Lead turn for a rule (THE-2197,
//! filled by b153 lane 6). See `thegn_core::lead_wake` for the contract: the
//! rule's prompt renders against `LEAD_WAKE_TEMPLATE_VARS` (never `{message}`)
//! and untrusted event text rides only in a delimited event block.

use std::sync::Arc;

use thegn_core::automation::{AutomationEvent, PlannedAction};
use thegn_core::config::Config;

#[allow(dead_code, unused_variables, reason = "filled by b153 lane 6")]
pub(crate) async fn run(
    cfg: Arc<Config>,
    event: &AutomationEvent,
    action: &PlannedAction,
    timeout_secs: u64,
) -> anyhow::Result<()> {
    anyhow::bail!("lead.wake is not implemented yet")
}
