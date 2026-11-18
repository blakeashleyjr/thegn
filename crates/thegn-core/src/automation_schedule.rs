//! `schedule_due` producer inputs: cron / interval schedules (THE-2193).
//!
//! Pure: the daemon producer (thegn-host `automation_schedule`) passes the
//! clock and the time zone in. Schedules persist nothing; "no catch-up" means
//! the next due time is always computed from now.
//!
//! The parser and evaluator are filled by b153 lane 2: 5-field cron (`*`,
//! lists, ranges, steps, month and weekday names) in the host's local zone,
//! and `every = "<n>s|m|h|d"` (60 s to 7 d) anchored at producer start.

use serde::{Deserialize, Serialize};

use crate::automation::{AutomationEvent, AutomationEventKind};

/// A rule's schedule: a cron expression or a fixed interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Schedule {
    Cron(CronSpec),
    Every { secs: u64 },
}

/// A parsed 5-field cron expression. `source` is the text it was parsed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronSpec {
    pub source: String,
}

/// Parse a rule's `schedule` (cron) or `every` (interval). `Ok(None)` when
/// neither is set; exclusivity is the config validator's job.
#[allow(dead_code, reason = "filled by b153 lane 2")]
pub fn parse_schedule(cron: Option<&str>, every: Option<&str>) -> Result<Option<Schedule>, String> {
    if cron.is_some() || every.is_some() {
        return Err("schedule_due is not implemented yet".into());
    }
    Ok(None)
}

/// The next due instant (unix seconds), strictly after `now_secs`. `anchor_secs`
/// is the producer start an `Every` schedule counts from.
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 2")]
pub fn next_due<Tz: chrono::TimeZone>(
    s: &Schedule,
    now_secs: i64,
    anchor_secs: i64,
    tz: &Tz,
) -> Option<i64> {
    None
}

/// The event a due schedule produces. Targeted at its own rule only, keyed by
/// the due instant so a re-fire of the same slot coalesces.
pub fn schedule_event(rule_id: &str, due_secs: i64) -> AutomationEvent {
    let mut event = AutomationEvent::bare(
        AutomationEventKind::ScheduleDue,
        format!("schedule:{rule_id}:{due_secs}"),
        due_secs,
    );
    event.target_rule = Some(rule_id.to_string());
    event.source_ref = Some(format!("schedule:{rule_id}"));
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_event_targets_its_rule_and_keys_by_due_time() {
        let event = schedule_event("nightly", 1_700_000_000);
        assert_eq!(event.kind, AutomationEventKind::ScheduleDue);
        assert_eq!(event.key.0, "schedule:nightly:1700000000");
        assert_eq!(event.target_rule.as_deref(), Some("nightly"));
        assert_eq!(event.occurred_at, 1_700_000_000);
        assert_ne!(event.id, schedule_event("nightly", 1_700_000_060).id);
    }

    #[test]
    fn no_schedule_parses_to_none() {
        assert_eq!(parse_schedule(None, None), Ok(None));
    }
}
