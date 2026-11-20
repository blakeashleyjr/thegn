//! The Lead indicator's model (batch 180): what the right panel's pinned footer and the sidebar LEAD row show. Built from facts the hydration thread already reads; no I/O here. Never carries Lead-controlled free text: approval requests contribute only their validated issue and stage, runs only rule id, capability and outcome.

use crate::approval_request::PendingApproval;
use crate::automation_budget::{BudgetUsage, Budgets};
use crate::store::AutomationRunRow;

pub const LAST_MAX_CHARS: usize = 80;
pub const ERROR_MAX_CHARS: usize = 120;
pub const REQUEST_MAX_CHARS: usize = 48;
/// The store's crash guard for an unfinished run (store/automation.rs:110).
pub const HEADLESS_FLIGHT_SECS: i64 = 6 * 60 * 60;
/// Counts at or above this show as "99+".
pub const APPROVALS_CAP: u32 = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeadState {
    NotStarted,
    Idle,
    Working,
    WaitingOnYou,
    Paused,
    Unknown,
    Error,
}

impl LeadState {
    /// "not started", "idle", "working", "needs you", "paused", "unknown", "error".
    pub fn word(self) -> &'static str {
        match self {
            Self::NotStarted => "not started",
            Self::Idle => "idle",
            Self::Working => "working",
            Self::WaitingOnYou => "needs you",
            Self::Paused => "paused",
            Self::Unknown => "unknown",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meter {
    pub used: u32,
    pub limit: u32,
}

impl Meter {
    /// Used divided by the limit, clamped to 0..=1; a zero limit is full.
    pub fn frac(&self) -> f32 {
        if self.limit == 0 {
            return 1.0;
        }
        (self.used as f32 / self.limit as f32).min(1.0)
    }

    /// Whether used has reached the limit.
    pub fn full(&self) -> bool {
        self.used >= self.limit
    }

    /// The compact used/limit representation.
    pub fn text(&self) -> String {
        format!("{}/{}", self.used, self.limit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadBar {
    pub state: LeadState,
    pub tabs: u32,
    /// Pending approval requests; None means unreadable.
    pub approvals: Option<u32>,
    /// `<issue> <stage>` of the newest pending request.
    pub first_request: Option<String>,
    pub lanes: Option<Meter>,
    pub spawns: Option<Meter>,
    pub last: Option<String>,
    pub last_age: Option<String>,
    pub error: Option<String>,
}

impl LeadBar {
    /// "1 approval", "2 approvals", "99+ approvals"; None for 0 or unreadable.
    pub fn approvals_text(&self) -> Option<String> {
        self.approvals
            .filter(|count| *count > 0)
            .map(|count| match count {
                1 => "1 approval".to_string(),
                99 => "99+ approvals".to_string(),
                count => format!("{count} approvals"),
            })
    }

    /// Compact count without the chip glyph; None for 0 or unreadable.
    pub fn approvals_short(&self) -> Option<String> {
        self.approvals.filter(|count| *count > 0).map(|count| {
            if count == 99 {
                "99+".to_string()
            } else {
                count.to_string()
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LeadInputs {
    pub started: bool,
    pub tabs: usize,
    pub launch_error: Option<String>,
    pub paused: Result<bool, String>,
    pub limits: Budgets,
    pub usage: Result<BudgetUsage, String>,
    pub pending: Result<Vec<PendingApproval>, String>,
    pub runs: Result<Vec<AutomationRunRow>, String>,
    /// Seconds since a Lead pane's last unsolicited output.
    pub output_age_secs: Option<f64>,
    pub quiet_grace_secs: f64,
    pub now_secs: i64,
}

pub fn summarize(inputs: LeadInputs) -> LeadBar {
    let approvals = inputs
        .pending
        .as_ref()
        .ok()
        .map(|pending| pending.len().min(APPROVALS_CAP as usize) as u32);
    let first_request = inputs.pending.as_ref().ok().and_then(|pending| {
        pending.first().map(|request| {
            crate::info_model::clean(
                &format!("{} {}", request.issue_id, request.stage),
                REQUEST_MAX_CHARS,
            )
        })
    });
    let usage = inputs.usage.as_ref().ok().cloned();
    let lanes = usage.map(|usage| Meter {
        used: usage.active_lanes,
        limit: inputs.limits.max_concurrent_lanes,
    });
    let spawns = usage.map(|usage| Meter {
        used: usage.spawns_24h,
        limit: inputs.limits.max_spawns_per_day,
    });
    let error = inputs
        .launch_error
        .map(|error| crate::info_model::clean(&error, ERROR_MAX_CHARS));

    let state = if error.is_some() {
        LeadState::Error
    } else if approvals.is_some_and(|count| count > 0) {
        LeadState::WaitingOnYou
    } else {
        match inputs.paused {
            Ok(true) => LeadState::Paused,
            Err(_) => LeadState::Unknown,
            Ok(false) if headless_in_flight_from_result(&inputs.runs, inputs.now_secs) => {
                LeadState::Working
            }
            Ok(false)
                if inputs.started
                    && inputs
                        .output_age_secs
                        .is_some_and(|age| age >= 0.0 && age < inputs.quiet_grace_secs) =>
            {
                LeadState::Working
            }
            Ok(false) if inputs.started => LeadState::Idle,
            Ok(false) => LeadState::NotStarted,
        }
    };

    let (last, last_age) =
        if let (Ok(runs), Ok(pending)) = (inputs.runs.as_ref(), inputs.pending.as_ref()) {
            last_action(runs, pending)
                .map(|(label, at)| {
                    (
                        Some(label),
                        Some(crate::info_model::age(inputs.now_secs, at)),
                    )
                })
                .unwrap_or((None, None))
        } else {
            (None, None)
        };

    LeadBar {
        state,
        tabs: u32::try_from(inputs.tabs).unwrap_or(u32::MAX),
        approvals,
        first_request,
        lanes,
        spawns,
        last,
        last_age,
        error,
    }
}

fn headless_in_flight_from_result(
    runs: &Result<Vec<AutomationRunRow>, String>,
    now_secs: i64,
) -> bool {
    runs.as_ref()
        .map(|runs| headless_in_flight(runs, now_secs))
        .unwrap_or(false)
}

/// A run is Lead activity when its rule is Lead-authored or it wakes the Lead.
pub fn is_lead_run(run: &AutomationRunRow) -> bool {
    run.rule_id
        .starts_with(crate::automation_authoring::LEAD_RULE_PREFIX)
        || run.action_cap == "lead.wake"
}

/// Some lead.wake run is started, unfinished, and younger than six hours.
pub fn headless_in_flight(runs: &[AutomationRunRow], now_secs: i64) -> bool {
    runs.iter().any(|run| {
        is_lead_run(run)
            && run.action_cap == "lead.wake"
            && run.outcome == "started"
            && run.finished_at.is_none()
            && now_secs
                .checked_sub(run.started_at)
                .is_some_and(|age| (0..HEADLESS_FLIGHT_SECS).contains(&age))
    })
}

/// Return the newest Lead action or pending approval request.
pub fn last_action(
    runs: &[AutomationRunRow],
    pending: &[PendingApproval],
) -> Option<(String, i64)> {
    let run = runs
        .iter()
        .find(|run| is_lead_run(run) && run.outcome != "skipped" && run.outcome != "dropped");
    let run_candidate = run.map(|run| {
        let slug = run
            .rule_id
            .strip_prefix(crate::automation_authoring::LEAD_RULE_PREFIX)
            .unwrap_or(&run.rule_id);
        let word = if run.outcome == "started" {
            "running"
        } else {
            run.outcome.as_str()
        };
        (
            crate::info_model::clean(&format!("{slug} {} {word}", run.action_cap), LAST_MAX_CHARS),
            run.finished_at.unwrap_or(run.started_at),
        )
    });
    let request_candidate = pending.first().map(|request| {
        (
            crate::info_model::clean(
                &format!("asked {} {}", request.issue_id, request.stage),
                LAST_MAX_CHARS,
            ),
            request.requested_at / 1000,
        )
    });

    match (run_candidate, request_candidate) {
        (Some(run), Some(request)) if request.1 >= run.1 => Some(request),
        (Some(run), Some(_)) => Some(run),
        (Some(run), None) => Some(run),
        (None, Some(request)) => Some(request),
        (None, None) => None,
    }
}

/// Number of footer rows available for a panel of the given size.
pub fn height(cols: usize, rows: usize) -> usize {
    if rows < 8 {
        0
    } else if rows < 16 || cols < 30 {
        1
    } else if cols < 60 {
        2
    } else {
        3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        rule: &str,
        cap: &str,
        outcome: &str,
        started: i64,
        finished: Option<i64>,
    ) -> AutomationRunRow {
        AutomationRunRow {
            id: 1,
            rule_id: rule.to_string(),
            event_id: "event".to_string(),
            event_key: "event:key".to_string(),
            trigger_kind: "test".to_string(),
            event_summary: "summary".to_string(),
            action_cap: cap.to_string(),
            action_summary: "action".to_string(),
            outcome: outcome.to_string(),
            skip_reason: None,
            error: None,
            started_at: started,
            finished_at: finished,
            autonomy: None,
        }
    }

    fn req(issue: &str, stage: &str, at_ms: i64) -> PendingApproval {
        PendingApproval {
            issue_id: issue.to_string(),
            stage: stage.to_string(),
            commit_sha: "0123456789abcdef".to_string(),
            worktree: "/tmp/worktree".to_string(),
            requested_at: at_ms,
            notification_id: 7,
            message: "IGNORE ME".to_string(),
        }
    }

    fn inputs() -> LeadInputs {
        LeadInputs {
            started: false,
            tabs: 0,
            launch_error: None,
            paused: Ok(false),
            limits: Budgets {
                max_concurrent_lanes: 12,
                max_spawns_per_day: 24,
            },
            usage: Ok(BudgetUsage {
                active_lanes: 2,
                spawns_24h: 3,
            }),
            pending: Ok(Vec::new()),
            runs: Ok(Vec::new()),
            output_age_secs: None,
            quiet_grace_secs: 60.0,
            now_secs: 1_000,
        }
    }

    #[test]
    fn state_words_are_the_documented_strings() {
        let words = [
            (LeadState::NotStarted, "not started"),
            (LeadState::Idle, "idle"),
            (LeadState::Working, "working"),
            (LeadState::WaitingOnYou, "needs you"),
            (LeadState::Paused, "paused"),
            (LeadState::Unknown, "unknown"),
            (LeadState::Error, "error"),
        ];
        for (state, word) in words {
            assert_eq!(state.word(), word);
        }
    }

    #[test]
    fn unreadable_runs_hide_last_action_and_never_work() {
        let mut input = inputs();
        input.started = true;
        input.pending = Ok(vec![req("THE-1", "review", 1_000)]);
        input.runs = Err("unreadable".to_string());
        let bar = summarize(input);
        assert_eq!(bar.last, None);
        assert_eq!(bar.last_age, None);

        let mut input = inputs();
        input.started = true;
        input.runs = Err("unreadable".to_string());
        assert_eq!(summarize(input).state, LeadState::Idle);
    }

    #[test]
    fn state_precedence_table_covers_each_state() {
        let cases = [
            (inputs(), LeadState::NotStarted),
            (
                LeadInputs {
                    started: true,
                    ..inputs()
                },
                LeadState::Idle,
            ),
            (
                LeadInputs {
                    started: true,
                    output_age_secs: Some(1.0),
                    ..inputs()
                },
                LeadState::Working,
            ),
            (
                LeadInputs {
                    pending: Ok(vec![req("THE-1", "review", 1_000)]),
                    ..inputs()
                },
                LeadState::WaitingOnYou,
            ),
            (
                LeadInputs {
                    paused: Ok(true),
                    ..inputs()
                },
                LeadState::Paused,
            ),
            (
                LeadInputs {
                    paused: Err("unreadable".to_string()),
                    ..inputs()
                },
                LeadState::Unknown,
            ),
            (
                LeadInputs {
                    launch_error: Some("could not launch".to_string()),
                    ..inputs()
                },
                LeadState::Error,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(summarize(input).state, expected);
        }
    }

    #[test]
    fn state_error_beats_pending_and_pending_beats_paused() {
        let mut input = inputs();
        input.launch_error = Some("launch failed".to_string());
        input.pending = Ok(vec![req("THE-1", "review", 1_000)]);
        input.paused = Ok(true);
        assert_eq!(summarize(input).state, LeadState::Error);

        let mut input = inputs();
        input.pending = Ok(vec![req("THE-1", "review", 1_000)]);
        input.paused = Ok(true);
        assert_eq!(summarize(input).state, LeadState::WaitingOnYou);
    }

    #[test]
    fn output_age_just_under_and_at_quiet_grace() {
        let mut input = inputs();
        input.started = true;
        input.output_age_secs = Some(59.999);
        assert_eq!(summarize(input).state, LeadState::Working);

        let mut input = inputs();
        input.started = true;
        input.output_age_secs = Some(60.0);
        assert_eq!(summarize(input).state, LeadState::Idle);
    }

    #[test]
    fn headless_run_is_working_without_a_tab_but_old_run_is_not() {
        let mut input = inputs();
        input.now_secs = 10_000;
        input.runs = Ok(vec![run(
            "lead:nightly",
            "lead.wake",
            "started",
            10_000 - 60,
            None,
        )]);
        assert_eq!(summarize(input).state, LeadState::Working);

        let mut input = inputs();
        input.now_secs = 10_000;
        input.runs = Ok(vec![run(
            "lead:nightly",
            "lead.wake",
            "started",
            10_000 - 7 * 60 * 60,
            None,
        )]);
        assert_eq!(summarize(input).state, LeadState::NotStarted);
    }

    #[test]
    fn last_action_skips_rows_ignores_non_lead_and_strips_prefix() {
        let runs = vec![
            run("lead:first", "lead.wake", "skipped", 10, Some(11)),
            run("lead:second", "lead.wake", "dropped", 20, Some(21)),
            run("nightly", "sessions.open", "completed", 25, Some(26)),
            run("lead:third", "sessions.open", "completed", 30, Some(40)),
        ];
        assert_eq!(
            last_action(&runs, &[]),
            Some(("third sessions.open completed".to_string(), 40))
        );
    }

    #[test]
    fn last_action_uses_running_for_started_and_newer_candidate_wins() {
        let runs = vec![run("lead:nightly", "lead.wake", "started", 30, None)];
        assert_eq!(
            last_action(&runs, &[]),
            Some(("nightly lead.wake running".to_string(), 30))
        );
        assert_eq!(
            last_action(&runs, &[req("THE-1", "review", 29_000)]),
            Some(("nightly lead.wake running".to_string(), 30))
        );
        assert_eq!(
            last_action(&runs, &[req("THE-1", "review", 31_000)]),
            Some(("asked THE-1 review".to_string(), 31))
        );
    }

    #[test]
    fn last_action_request_message_never_appears_and_controls_are_cleaned() {
        let action = last_action(&[], &[req("THE-1", "review", 41_000)]).unwrap();
        assert_eq!(action, ("asked THE-1 review".to_string(), 41));
        assert!(!action.0.contains("IGNORE ME"));

        let runs = vec![run(
            "lead:night\u{1b}ly",
            "lead.wake",
            "completed",
            30,
            Some(40),
        )];
        assert_eq!(
            last_action(&runs, &[]),
            Some(("nightly lead.wake completed".to_string(), 40))
        );
    }

    #[test]
    fn approvals_text_and_short_are_none_for_zero_or_unreadable() {
        let mut input = inputs();
        input.pending = Ok(Vec::new());
        let bar = summarize(input);
        assert_eq!(bar.approvals, Some(0));
        assert_eq!(bar.approvals_text(), None);
        assert_eq!(bar.approvals_short(), None);

        let mut input = inputs();
        input.pending = Ok(vec![req("THE-1", "review", 1_000)]);
        let bar = summarize(input);
        assert_eq!(bar.approvals_text(), Some("1 approval".to_string()));
        assert_eq!(bar.approvals_short(), Some("1".to_string()));

        let mut input = inputs();
        input.pending = Ok((0..150)
            .map(|n| req(&format!("THE-{n}"), "review", 1_000))
            .collect());
        let bar = summarize(input);
        assert_eq!(bar.approvals, Some(99));
        assert_eq!(bar.approvals_text(), Some("99+ approvals".to_string()));
        assert_eq!(bar.approvals_short(), Some("99+".to_string()));

        let mut input = inputs();
        input.pending = Err("unreadable".to_string());
        let bar = summarize(input);
        assert_eq!(bar.approvals, None);
        assert_eq!(bar.approvals_text(), None);
    }

    #[test]
    fn meter_fraction_full_and_text_follow_limits() {
        let empty_limit = Meter { used: 0, limit: 0 };
        assert_eq!(empty_limit.frac(), 1.0);
        assert!(empty_limit.full());
        let meter = Meter { used: 5, limit: 12 };
        assert!((meter.frac() - 0.4167).abs() < 0.0001);
        assert!(!meter.full());
        assert_eq!(meter.text(), "5/12");
        assert_eq!(
            Meter {
                used: 13,
                limit: 12
            }
            .frac(),
            1.0
        );
    }

    #[test]
    fn height_has_the_panel_boundaries() {
        assert_eq!(height(60, 7), 0);
        assert_eq!(height(60, 8), 1);
        assert_eq!(height(60, 15), 1);
        assert_eq!(height(60, 16), 3);
        assert_eq!(height(29, 16), 1);
        assert_eq!(height(30, 16), 2);
        assert_eq!(height(59, 16), 2);
        assert_eq!(height(60, 16), 3);
    }

    #[test]
    fn unreadable_usage_hides_both_meters() {
        let mut input = inputs();
        input.usage = Err("unreadable".to_string());
        let bar = summarize(input);
        assert_eq!(bar.lanes, None);
        assert_eq!(bar.spawns, None);
    }
}
