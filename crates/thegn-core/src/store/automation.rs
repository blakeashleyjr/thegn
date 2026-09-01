//! Automation throttle state and bounded audit-log persistence seam.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::automation::EventKey;
use crate::automation::{AutomationEvent, AutomationRule, PlannedAction};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AutomationStateRow {
    pub rule_id: String,
    pub enabled_override: Option<bool>,
    pub last_fired_at: Option<i64>,
    pub recent_fires: Vec<i64>,
    pub action_fires: BTreeMap<String, Vec<i64>>,
    pub once_keys: BTreeSet<EventKey>,
    pub updated_at: i64,
}

/// Metadata written before dispatch. Summaries are bounded by the caller and
/// never contain secrets or full prompts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewAutomationRun {
    pub rule_id: String,
    pub event_id: String,
    pub event_key: String,
    pub trigger_kind: String,
    pub event_summary: String,
    pub action_cap: String,
    pub action_summary: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AutomationRunRow {
    pub id: i64,
    pub rule_id: String,
    pub event_id: String,
    pub event_key: String,
    pub trigger_kind: String,
    pub event_summary: String,
    pub action_cap: String,
    pub action_summary: String,
    pub outcome: String,
    pub skip_reason: Option<String>,
    pub error: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationAdmission {
    Planned {
        action: Box<PlannedAction>,
        run_id: i64,
    },
    Skipped {
        run_id: i64,
    },
}

/// Object-safe synchronous store. Callers keep SQLite work off the render loop.
pub trait AutomationStore {
    fn automation_state(&self, rule_id: &str) -> Result<Option<AutomationStateRow>>;
    fn put_automation_state(&self, state: &AutomationStateRow) -> Result<()>;

    /// Cross-process admission boundary. Evaluation, every accepted state
    /// transition, and its pre-dispatch audit row commit in one write
    /// transaction before any action is launched.
    fn admit_automation_event(
        &self,
        rules: &[AutomationRule],
        event: &AutomationEvent,
        now: i64,
    ) -> Result<Vec<AutomationAdmission>>;

    /// Insert the pre-dispatch audit row and return its run id.
    fn start_automation_run(&self, run: &NewAutomationRun) -> Result<i64>;

    /// Complete a started row. `outcome` is the stable fired/skipped/dropped/
    /// succeeded/timed_out/failed vocabulary owned by the runtime.
    fn finish_automation_run(
        &self,
        id: i64,
        outcome: &str,
        skip_reason: Option<&str>,
        error: Option<&str>,
        finished_at: i64,
    ) -> Result<()>;

    /// Newest-first, capped internally even when the caller asks for more.
    fn automation_runs(&self, rule_id: Option<&str>, limit: usize)
    -> Result<Vec<AutomationRunRow>>;

    /// Keep at most `retain_per_rule` newest rows for each stable rule id.
    fn prune_automation_runs(&self, retain_per_rule: usize) -> Result<usize>;
}
