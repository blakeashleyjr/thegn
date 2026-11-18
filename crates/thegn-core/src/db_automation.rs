//! SQLite implementation of the automation state/audit seam (schema v64).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};

use crate::automation::EventKey;
use crate::db::Db;
use crate::store::{
    AutomationAdmission, AutomationRunRow, AutomationStateRow, AutomationStore, NewAutomationRun,
};

const MAX_AUDIT_QUERY: usize = 1_000;
const MAX_RETENTION_PER_RULE: usize = 10_000;
/// A started run older than this is presumed orphaned by a crash and no
/// longer blocks its rule (`automation_run_in_flight`).
const IN_FLIGHT_WINDOW_SECS: i64 = 6 * 3_600;

impl AutomationStore for Db {
    fn automation_state(&self, rule_id: &str) -> Result<Option<AutomationStateRow>> {
        self.conn()
            .query_row(
                "SELECT rule_id, enabled_override, last_fired_at, recent_fires_json, \
                        action_fires_json, once_keys_json, updated_at \
                 FROM automation_state WHERE rule_id=?1",
                params![rule_id],
                |row| {
                    let enabled: Option<i64> = row.get(1)?;
                    let recent: String = row.get(3)?;
                    let actions: String = row.get(4)?;
                    let keys: String = row.get(5)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        enabled.map(|value| value != 0),
                        row.get::<_, Option<i64>>(2)?,
                        recent,
                        actions,
                        keys,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(rule_id, enabled_override, last_fired_at, recent, actions, keys, updated_at)| {
                    Ok(AutomationStateRow {
                        rule_id,
                        enabled_override,
                        last_fired_at,
                        recent_fires: serde_json::from_str(&recent)
                            .context("automation_state.recent_fires_json")?,
                        action_fires: serde_json::from_str::<BTreeMap<String, Vec<i64>>>(&actions)
                            .context("automation_state.action_fires_json")?,
                        once_keys: serde_json::from_str::<BTreeSet<EventKey>>(&keys)
                            .context("automation_state.once_keys_json")?,
                        updated_at,
                    })
                },
            )
            .transpose()
    }

    fn put_automation_state(&self, state: &AutomationStateRow) -> Result<()> {
        let recent = serde_json::to_string(&state.recent_fires)?;
        let actions = serde_json::to_string(&state.action_fires)?;
        let keys = serde_json::to_string(&state.once_keys)?;
        self.conn().execute(
            "INSERT INTO automation_state \
               (rule_id, enabled_override, last_fired_at, recent_fires_json, \
                action_fires_json, once_keys_json, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7) \
             ON CONFLICT(rule_id) DO UPDATE SET \
               enabled_override=excluded.enabled_override, \
               last_fired_at=excluded.last_fired_at, \
               recent_fires_json=excluded.recent_fires_json, \
               action_fires_json=excluded.action_fires_json, \
               once_keys_json=excluded.once_keys_json, \
               updated_at=excluded.updated_at",
            params![
                state.rule_id,
                state.enabled_override.map(i64::from),
                state.last_fired_at,
                recent,
                actions,
                keys,
                state.updated_at,
            ],
        )?;
        Ok(())
    }

    fn admit_automation_event(
        &self,
        rules: &[crate::automation::AutomationRule],
        event: &crate::automation::AutomationEvent,
        now: i64,
    ) -> Result<Vec<AutomationAdmission>> {
        self.conn().execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<Vec<AutomationAdmission>> {
            let mut state = crate::automation::EvaluationState::new();
            for rule in rules {
                if let Some(row) = self.automation_state(&rule.id)? {
                    state.insert(
                        rule.id.clone(),
                        crate::automation::RuleState {
                            enabled_override: row.enabled_override,
                            last_fired_at: row.last_fired_at,
                            recent_fires: row.recent_fires,
                            action_fires: row.action_fires,
                            once_keys: row.once_keys,
                        },
                    );
                }
            }
            let decisions = crate::automation::evaluate(rules, event, &state, now);
            let mut admitted = Vec::with_capacity(decisions.len());
            for decision in decisions {
                match decision {
                    crate::automation::EvaluationDecision::Planned(action) => {
                        self.put_automation_state(&AutomationStateRow {
                            rule_id: action.transition.rule_id.clone(),
                            enabled_override: action.transition.state.enabled_override,
                            last_fired_at: action.transition.state.last_fired_at,
                            recent_fires: action.transition.state.recent_fires.clone(),
                            action_fires: action.transition.state.action_fires.clone(),
                            once_keys: action.transition.state.once_keys.clone(),
                            updated_at: now,
                        })?;
                        let run_id = self.start_automation_run(&NewAutomationRun {
                            rule_id: action.rule_id.clone(),
                            event_id: event.id.clone(),
                            event_key: action.event_key.0.clone(),
                            trigger_kind: event.kind.as_str().into(),
                            event_summary: bounded(event.message.as_deref().unwrap_or_default()),
                            action_cap: action.cap.clone(),
                            action_summary: bounded(&format!(
                                "{} {:?}",
                                action.cap,
                                action.params.keys()
                            )),
                            started_at: now,
                            autonomy: Some(action.autonomy.as_str().to_string()),
                        })?;
                        admitted.push(AutomationAdmission::Planned {
                            action: Box::new(action),
                            run_id,
                        });
                    }
                    crate::automation::EvaluationDecision::Skipped {
                        rule_id,
                        event_key,
                        reason,
                    } => {
                        let run_id = self.start_automation_run(&NewAutomationRun {
                            rule_id,
                            event_id: event.id.clone(),
                            event_key: event_key.0,
                            trigger_kind: event.kind.as_str().into(),
                            event_summary: bounded(event.message.as_deref().unwrap_or_default()),
                            action_cap: String::new(),
                            action_summary: String::new(),
                            started_at: now,
                            autonomy: None,
                        })?;
                        let reason = skip_reason_label(&reason);
                        self.finish_automation_run(run_id, "skipped", Some(&reason), None, now)?;
                        admitted.push(AutomationAdmission::Skipped { run_id });
                    }
                }
            }
            Ok(admitted)
        })();
        match result {
            Ok(admitted) => {
                self.conn().execute_batch("COMMIT")?;
                Ok(admitted)
            }
            Err(error) => {
                let _ = self.conn().execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn audit_skipped_automation_event(
        &self,
        rules: &[crate::automation::AutomationRule],
        event: &crate::automation::AutomationEvent,
        now: i64,
        skip: crate::automation::SkipReason,
    ) -> Result<usize> {
        let mut state = crate::automation::EvaluationState::new();
        for rule in rules {
            if let Some(row) = self.automation_state(&rule.id)? {
                state.insert(
                    rule.id.clone(),
                    crate::automation::RuleState {
                        enabled_override: row.enabled_override,
                        last_fired_at: row.last_fired_at,
                        recent_fires: row.recent_fires,
                        action_fires: row.action_fires,
                        once_keys: row.once_keys,
                    },
                );
            }
        }
        let decisions = crate::automation::skipped_decisions(
            crate::automation::evaluate(rules, event, &state, now),
            skip,
        );
        let mut written = 0;
        for decision in decisions {
            let crate::automation::EvaluationDecision::Skipped {
                rule_id,
                event_key,
                reason,
            } = decision
            else {
                // skipped_decisions never returns a plan.
                continue;
            };
            let run_id = self.start_automation_run(&NewAutomationRun {
                rule_id,
                event_id: event.id.clone(),
                event_key: event_key.0,
                trigger_kind: event.kind.as_str().into(),
                event_summary: bounded(event.message.as_deref().unwrap_or_default()),
                action_cap: String::new(),
                action_summary: String::new(),
                started_at: now,
                autonomy: None,
            })?;
            self.finish_automation_run(
                run_id,
                "skipped",
                Some(&skip_reason_label(&reason)),
                None,
                now,
            )?;
            written += 1;
        }
        Ok(written)
    }

    fn automation_run_in_flight(&self, rule_id: &str, now: i64) -> Result<bool> {
        let since = now.saturating_sub(IN_FLIGHT_WINDOW_SECS);
        let live: i64 = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM automation_runs \
               WHERE rule_id=?1 AND finished_at IS NULL AND started_at > ?2)",
            params![rule_id, since],
            |row| row.get(0),
        )?;
        Ok(live != 0)
    }

    fn start_automation_run(&self, run: &NewAutomationRun) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO automation_runs \
               (rule_id,event_id,event_key,trigger_kind,event_summary,action_cap, \
                action_summary,outcome,started_at,autonomy) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,'started',?8,?9)",
            params![
                run.rule_id,
                run.event_id,
                run.event_key,
                run.trigger_kind,
                run.event_summary,
                run.action_cap,
                run.action_summary,
                run.started_at,
                run.autonomy,
            ],
        )?;
        Ok(self.conn().last_insert_rowid())
    }

    fn finish_automation_run(
        &self,
        id: i64,
        outcome: &str,
        skip_reason: Option<&str>,
        error: Option<&str>,
        finished_at: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE automation_runs SET outcome=?2, skip_reason=?3, error=?4, finished_at=?5 \
             WHERE id=?1",
            params![id, outcome, skip_reason, error, finished_at],
        )?;
        Ok(())
    }

    fn automation_runs(
        &self,
        rule_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AutomationRunRow>> {
        let limit =
            i64::try_from(limit.clamp(1, MAX_AUDIT_QUERY)).unwrap_or(MAX_AUDIT_QUERY as i64);
        let mut stmt = self.conn().prepare(
            "SELECT id,rule_id,event_id,event_key,trigger_kind,event_summary,action_cap, \
                    action_summary,outcome,skip_reason,error,started_at,finished_at,autonomy \
             FROM automation_runs \
             WHERE (?1 IS NULL OR rule_id=?1) \
             ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![rule_id, limit], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn automation_budget_usage(
        &self,
        now_ms: i64,
    ) -> Result<crate::automation_budget::BudgetUsage> {
        let conn = self.conn();
        // A lane is active while its status says so OR while its session has
        // not exited: a Lead can `dispatches.set_status` a row to done while
        // the worker is still running, and that must not free a lane slot.
        let active: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT issue_id) FROM agent_dispatches \
                 WHERE automation_rule IS NOT NULL \
                   AND (status IN ({}) \
                        OR (session_id IS NOT NULL AND session_id <> '' \
                            AND exited_at_ms IS NULL))",
                sql_list(&ACTIVE_DISPATCH_TOKENS)
            ),
            [],
            |row| row.get(0),
        )?;
        let since_secs = (now_ms / 1000).saturating_sub(DAY_SECS);
        // A `pipeline.start` run still in flight (`started`) is about to open
        // a lane with no dispatch row yet: it holds a lane slot.
        let lanes_in_flight: i64 = conn.query_row(
            "SELECT COUNT(*) FROM automation_runs \
             WHERE action_cap = 'pipeline.start' AND outcome = 'started' \
               AND COALESCE(autonomy,'') <> 'propose' AND started_at >= ?1",
            params![since_secs],
            |row| row.get(0),
        )?;
        // automation_runs.started_at is unix seconds (see `now()` callers and
        // the six-hour IN_FLIGHT_WINDOW_SECS); agent_dispatches is ms. Runs
        // still in flight (`started`) count as well as succeeded ones, so
        // several rules on one event (or concurrent events) cannot each see
        // the same free slot and together spawn past the limit.
        let runs: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM automation_runs \
                 WHERE action_cap IN ({}) AND outcome IN ('started','succeeded') \
                   AND COALESCE(autonomy,'') <> 'propose' AND started_at >= ?1",
                sql_list(&SPAWN_CAPS)
            ),
            params![since_secs],
            |row| row.get(0),
        )?;
        let advanced: i64 = conn.query_row(
            "SELECT COUNT(*) FROM agent_dispatches \
             WHERE automation_rule IS NOT NULL AND parent_id IS NOT NULL \
               AND dispatched_at_ms >= ?1",
            params![now_ms.saturating_sub(DAY_MS)],
            |row| row.get(0),
        )?;
        let clamp = |n: i64| u32::try_from(n.max(0)).unwrap_or(u32::MAX);
        Ok(crate::automation_budget::BudgetUsage {
            active_lanes: clamp(active.saturating_add(lanes_in_flight)),
            spawns_24h: clamp(runs.saturating_add(advanced)),
        })
    }

    fn prune_automation_runs(&self, retain_per_rule: usize) -> Result<usize> {
        let retain = i64::try_from(retain_per_rule.clamp(1, MAX_RETENTION_PER_RULE))
            .unwrap_or(MAX_RETENTION_PER_RULE as i64);
        // Never prune a row the budget still counts (6e): a spawn row inside
        // the rolling 24 h window, or a run still in flight. Pruning one would
        // free a budget slot that was really spent.
        let deleted = self.conn().execute(
            &format!(
                "DELETE FROM automation_runs AS old \
                 WHERE (SELECT COUNT(*) FROM automation_runs AS newer \
                        WHERE newer.rule_id=old.rule_id \
                          AND (newer.started_at > old.started_at \
                               OR (newer.started_at = old.started_at AND newer.id > old.id))) >= ?1 \
                   AND old.outcome <> 'started' \
                   AND NOT (old.action_cap IN ({}) \
                            AND old.started_at >= CAST(strftime('%s','now') AS INTEGER) - ?2)",
                sql_list(&SPAWN_CAPS)
            ),
            params![retain, DAY_SECS],
        )?;
        Ok(deleted)
    }
}

fn run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AutomationRunRow> {
    Ok(AutomationRunRow {
        id: row.get(0)?,
        rule_id: row.get(1)?,
        event_id: row.get(2)?,
        event_key: row.get(3)?,
        trigger_kind: row.get(4)?,
        event_summary: row.get(5)?,
        action_cap: row.get(6)?,
        action_summary: row.get(7)?,
        outcome: row.get(8)?,
        skip_reason: row.get(9)?,
        error: row.get(10)?,
        started_at: row.get(11)?,
        finished_at: row.get(12)?,
        autonomy: row.get(13)?,
    })
}

/// The dispatch statuses [`crate::issue::AgentDispatchStatus::is_active`]
/// holds, as stored tokens. Pinned against `is_active` by a test.
pub(crate) const ACTIVE_DISPATCH_TOKENS: [&str; 5] =
    ["queued", "spawning", "running", "waiting_human", "pr_open"];

/// The automation actions whose succeeded runs count as spawns (6e).
const SPAWN_CAPS: [&str; 3] = ["pipeline.start", "sessions.open", "lead.wake"];

const DAY_MS: i64 = 86_400_000;
const DAY_SECS: i64 = 86_400;

fn sql_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|item| format!("'{item}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The audit label of a skip reason: the variant name lowercased (`paused`,
/// `debounced`, `onceperkey`), the vocabulary the audit log has always used.
fn skip_reason_label(reason: &crate::automation::SkipReason) -> String {
    format!("{reason:?}").to_ascii_lowercase()
}

fn bounded(value: &str) -> String {
    value.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission_event(
        id: &str,
        key: &str,
        target_rule: Option<&str>,
    ) -> crate::automation::AutomationEvent {
        crate::automation::AutomationEvent {
            id: id.into(),
            occurred_at: 10_000,
            key: EventKey(key.into()),
            kind: crate::automation::AutomationEventKind::Notification,
            target_rule: target_rule.map(str::to_string),
            workspace: None,
            repo: None,
            worktree: None,
            branch: None,
            agent_role: None,
            notification_kind: Some(crate::notification::NotificationKind::TestFailed),
            priority: Some(crate::notification::Priority::Alert),
            source_ref: Some("test".into()),
            message: Some("failed".into()),
            session_id: None,
            pr_checks_passed: None,
            pr_review_requested: None,
            pr_merged: None,
            origin: None,
            issue: None,
            exit_code: None,
        }
    }

    fn admission_rule(id: &str) -> crate::automation::AutomationRule {
        crate::automation::AutomationRule {
            id: id.into(),
            enabled: true,
            event: crate::automation::AutomationEventKind::Notification,
            predicate: crate::automation::AutomationPredicate::default(),
            action: crate::automation::ActionTemplate {
                cap: "notify.push".into(),
                params: BTreeMap::from([("body".into(), "{message}".into())]),
            },
            debounce_secs: 0,
            idle_secs: None,
            once_per_key: false,
            max_per_hour: 30,
            max_action_per_hour: 30,
            schedule: None,
            autonomy: crate::automation_autonomy::Autonomy::Review,
        }
    }

    fn race_admissions(
        rules: Vec<crate::automation::AutomationRule>,
        left: crate::automation::AutomationEvent,
        right: crate::automation::AutomationEvent,
    ) -> (usize, Vec<AutomationRunRow>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thegn.db");
        drop(Db::open_at(&path).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = [left, right]
            .into_iter()
            .map(|event| {
                let path = path.clone();
                let barrier = barrier.clone();
                let rules = rules.clone();
                std::thread::spawn(move || {
                    let db = Db::open_at(&path).unwrap();
                    barrier.wait();
                    db.admit_automation_event(&rules, &event, 10_000).unwrap()
                })
            })
            .collect();
        barrier.wait();
        let planned = handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .filter(|admission| matches!(admission, AutomationAdmission::Planned { .. }))
            .count();
        let rows = Db::open_at(&path)
            .unwrap()
            .automation_runs(None, 20)
            .unwrap();
        (planned, rows)
    }

    #[test]
    fn active_dispatch_tokens_match_is_active() {
        use crate::issue::AgentDispatchStatus as S;
        let all = [
            S::Queued,
            S::Spawning,
            S::Running,
            S::WaitingHuman,
            S::PrOpen,
            S::Merged,
            S::Abandoned,
            S::Done,
            S::Failed,
            S::Unknown,
        ];
        let active: Vec<&str> = all
            .into_iter()
            .filter(|s| s.is_active())
            .map(|s| s.as_str())
            .collect();
        assert_eq!(active, super::ACTIVE_DISPATCH_TOKENS);
    }

    fn budget_dispatch(
        db: &Db,
        issue: &str,
        status: &str,
        rule: Option<&str>,
        parent: Option<i64>,
        at_ms: i64,
    ) -> i64 {
        use crate::store::NotificationStore;
        let id = db
            .put_agent_dispatch(crate::issue::NewDispatch {
                issue_id: issue,
                worktree_path: "/wt/budget",
                agent_name: "codex",
                stage: None,
                parent_id: None,
                session_id: None,
                artifact_path: None,
                chunk_path: None,
            })
            .unwrap();
        db.conn()
            .execute(
                "UPDATE agent_dispatches SET status=?1, automation_rule=?2, parent_id=?3, \
                 dispatched_at_ms=?4 WHERE id=?5",
                params![status, rule, parent, at_ms, id],
            )
            .unwrap();
        id
    }

    #[test]
    fn budget_usage_counts_active_lanes_and_day_spawns() {
        let db = Db::open_memory().unwrap();
        let now_ms: i64 = 10 * 86_400_000;
        let now = now_ms / 1000;
        assert_eq!(
            db.automation_budget_usage(now_ms).unwrap(),
            crate::automation_budget::BudgetUsage::default()
        );
        // Lanes: two rows of THE-1 count once; a manual row and a done row do not.
        let root = budget_dispatch(
            &db,
            "THE-1",
            "running",
            Some("r"),
            None,
            now_ms - 5 * 86_400_000,
        );
        budget_dispatch(
            &db,
            "THE-1",
            "waiting_human",
            Some("r"),
            Some(root),
            now_ms - 1_000,
        );
        budget_dispatch(
            &db,
            "THE-2",
            "queued",
            Some("r"),
            None,
            now_ms - 3 * 86_400_000,
        );
        budget_dispatch(&db, "THE-3", "running", None, None, now_ms);
        budget_dispatch(&db, "THE-4", "done", Some("r"), Some(root), now_ms - 2_000);
        // An advance older than 24 h is out of the window.
        budget_dispatch(
            &db,
            "THE-5",
            "failed",
            Some("r"),
            Some(root),
            now_ms - 86_400_001,
        );
        // Runs: succeeded spawns in the window count; propose, failed, other
        // caps and old runs do not.
        let mut counted = 0;
        for (cap, outcome, autonomy, at, counts) in [
            (
                "pipeline.start",
                "succeeded",
                Some("review"),
                now - 60,
                true,
            ),
            ("sessions.open", "succeeded", None, now - 60, true),
            ("lead.wake", "succeeded", None, now - 86_400, true),
            (
                "pipeline.start",
                "succeeded",
                Some("propose"),
                now - 60,
                false,
            ),
            ("pipeline.start", "failed", Some("review"), now - 60, false),
            ("notify.push", "succeeded", None, now - 60, false),
            ("sessions.open", "succeeded", None, now - 86_401, false),
        ] {
            let mut r = run("r", &format!("e{at}{cap}{outcome}"), at);
            r.action_cap = cap.into();
            r.autonomy = autonomy.map(str::to_string);
            let id = db.start_automation_run(&r).unwrap();
            db.finish_automation_run(id, outcome, None, None, at)
                .unwrap();
            counted += usize::from(counts);
        }
        let usage = db.automation_budget_usage(now_ms).unwrap();
        assert_eq!(usage.active_lanes, 2, "THE-1 and THE-2");
        // 3 counted runs + 2 parent-linked advances inside 24 h.
        assert_eq!(counted, 3);
        assert_eq!(usage.spawns_24h, 5);
    }

    #[test]
    fn budget_usage_counts_in_flight_runs_and_unexited_sessions() {
        let db = Db::open_memory().unwrap();
        let now_ms: i64 = 10 * 86_400_000;
        let now = now_ms / 1000;
        // A started (in-flight) pipeline.start holds a spawn and a lane slot;
        // a started sessions.open holds a spawn only.
        for (cap, autonomy) in [("pipeline.start", Some("review")), ("sessions.open", None)] {
            let mut r = run("r", &format!("inflight-{cap}"), now - 30);
            r.action_cap = cap.into();
            r.autonomy = autonomy.map(str::to_string);
            db.start_automation_run(&r).unwrap();
        }
        // A roster row marked done whose session has not exited is active.
        let id = budget_dispatch(&db, "THE-9", "done", Some("r"), None, now_ms - 1_000);
        db.conn()
            .execute(
                "UPDATE agent_dispatches SET session_id='s9', exited_at_ms=NULL WHERE id=?1",
                params![id],
            )
            .unwrap();
        // The same shape with an exited session is not.
        let gone = budget_dispatch(&db, "THE-10", "done", Some("r"), None, now_ms - 1_000);
        db.conn()
            .execute(
                "UPDATE agent_dispatches SET session_id='s10', exited_at_ms=?1 WHERE id=?2",
                params![now_ms - 500, gone],
            )
            .unwrap();
        let usage = db.automation_budget_usage(now_ms).unwrap();
        assert_eq!(usage.active_lanes, 2, "THE-9 plus the in-flight lane");
        assert_eq!(usage.spawns_24h, 2, "both in-flight runs");
    }

    #[test]
    fn prune_keeps_spawn_rows_inside_the_budget_window_and_in_flight_runs() {
        let db = Db::open_memory().unwrap();
        let now = crate::util::now();
        // Recent spawn rows: never pruned even past the retention count.
        for n in 0..3 {
            let mut r = run("spawner", &format!("recent{n}"), now - 60 + n);
            r.action_cap = "pipeline.start".into();
            r.autonomy = Some("review".into());
            let id = db.start_automation_run(&r).unwrap();
            db.finish_automation_run(id, "succeeded", None, None, now)
                .unwrap();
        }
        // An old in-flight row is kept too.
        let mut old_started = run("spawner", "old-started", now - 3 * 86_400);
        old_started.action_cap = "notify.push".into();
        db.start_automation_run(&old_started).unwrap();
        // An old finished spawn row outside the window may go.
        let mut old = run("spawner", "old", now - 2 * 86_400);
        old.action_cap = "pipeline.start".into();
        let id = db.start_automation_run(&old).unwrap();
        db.finish_automation_run(id, "succeeded", None, None, now - 2 * 86_400)
            .unwrap();
        assert_eq!(db.prune_automation_runs(1).unwrap(), 1);
        let left: Vec<String> = db
            .automation_runs(Some("spawner"), 99)
            .unwrap()
            .into_iter()
            .map(|row| row.event_id)
            .collect();
        assert_eq!(left.len(), 4, "{left:?}");
        assert!(!left.iter().any(|e| e == "old"), "{left:?}");
    }

    fn run(rule: &str, event: &str, at: i64) -> NewAutomationRun {
        NewAutomationRun {
            rule_id: rule.into(),
            event_id: event.into(),
            event_key: format!("key:{event}"),
            trigger_kind: "notification".into(),
            event_summary: "bounded event".into(),
            action_cap: "notify.push".into(),
            action_summary: "bounded action".into(),
            started_at: at,
            autonomy: None,
        }
    }

    #[test]
    fn automation_state_roundtrips() {
        let db = Db::open_memory().unwrap();
        let state = AutomationStateRow {
            rule_id: "r1".into(),
            enabled_override: Some(false),
            last_fired_at: Some(99),
            recent_fires: vec![1, 99],
            action_fires: BTreeMap::from([("notify.push".into(), vec![99])]),
            once_keys: BTreeSet::from([EventKey("k1".into())]),
            updated_at: 100,
        };
        db.put_automation_state(&state).unwrap();
        assert_eq!(db.automation_state("r1").unwrap(), Some(state));
        assert_eq!(db.automation_state("missing").unwrap(), None);
    }

    #[test]
    fn audit_start_finish_filter_and_retention_are_bounded() {
        let db = Db::open_memory().unwrap();
        for (rule, event, at) in [
            ("r1", "e1", 1),
            ("r1", "e2", 2),
            ("r1", "e3", 3),
            ("r2", "e4", 4),
        ] {
            let id = db.start_automation_run(&run(rule, event, at)).unwrap();
            db.finish_automation_run(id, "succeeded", None, None, at + 1)
                .unwrap();
        }
        let r1 = db.automation_runs(Some("r1"), 99).unwrap();
        assert_eq!(
            r1.iter()
                .map(|row| row.event_id.as_str())
                .collect::<Vec<_>>(),
            ["e3", "e2", "e1"]
        );
        assert!(r1.iter().all(|row| row.outcome == "succeeded"));

        assert_eq!(db.prune_automation_runs(2).unwrap(), 1);
        assert_eq!(db.automation_runs(Some("r1"), 99).unwrap().len(), 2);
        assert_eq!(db.automation_runs(Some("r2"), 99).unwrap().len(), 1);
    }

    #[test]
    fn audit_failure_fields_roundtrip() {
        let db = Db::open_memory().unwrap();
        let id = db.start_automation_run(&run("r", "e", 10)).unwrap();
        db.finish_automation_run(id, "skipped", Some("debounced"), Some("detail"), 11)
            .unwrap();
        let row = &db.automation_runs(None, 1).unwrap()[0];
        assert_eq!(row.skip_reason.as_deref(), Some("debounced"));
        assert_eq!(row.error.as_deref(), Some("detail"));
        assert_eq!(row.finished_at, Some(11));
    }

    #[test]
    fn admission_records_the_planned_autonomy() {
        let db = Db::open_memory().unwrap();
        let mut rule = admission_rule("auto");
        rule.autonomy = crate::automation_autonomy::Autonomy::Propose;
        let admitted = db
            .admit_automation_event(&[rule], &admission_event("e1", "k", None), 10_000)
            .unwrap();
        assert!(matches!(admitted[0], AutomationAdmission::Planned { .. }));
        let row = &db.automation_runs(Some("auto"), 1).unwrap()[0];
        assert_eq!(row.autonomy.as_deref(), Some("propose"));
    }

    #[test]
    fn paused_audit_skips_every_plan_and_persists_no_state() {
        let db = Db::open_memory().unwrap();
        let mut once = admission_rule("once");
        once.once_per_key = true;
        let rules = vec![admission_rule("plain"), once];
        let written = db
            .audit_paused_automation_event(&rules, &admission_event("e1", "k", None), 10_000)
            .unwrap();
        assert_eq!(written, 2);
        let rows = db.automation_runs(None, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.outcome == "skipped"
            && row.skip_reason.as_deref() == Some("paused")
            && row.finished_at == Some(10_000)));
        // No ledger transition: the once key was never consumed, so a resumed
        // runtime still admits the same event.
        assert_eq!(db.automation_state("once").unwrap(), None);
        let admitted = db
            .admit_automation_event(&rules, &admission_event("e2", "k", None), 10_001)
            .unwrap();
        assert!(
            admitted
                .iter()
                .all(|admission| matches!(admission, AutomationAdmission::Planned { .. }))
        );
    }

    #[test]
    fn run_in_flight_counts_only_recent_unfinished_rows() {
        let db = Db::open_memory().unwrap();
        let now = 100_000;
        assert!(!db.automation_run_in_flight("r", now).unwrap());
        let live = db.start_automation_run(&run("r", "e1", now - 60)).unwrap();
        assert!(db.automation_run_in_flight("r", now).unwrap());
        assert!(!db.automation_run_in_flight("other", now).unwrap());
        db.finish_automation_run(live, "succeeded", None, None, now)
            .unwrap();
        assert!(!db.automation_run_in_flight("r", now).unwrap());
        // An unfinished row older than six hours is a crash orphan.
        db.start_automation_run(&run("r", "e2", now - 6 * 3_600))
            .unwrap();
        assert!(!db.automation_run_in_flight("r", now).unwrap());
        db.start_automation_run(&run("r", "e3", now - 6 * 3_600 + 1))
            .unwrap();
        assert!(db.automation_run_in_flight("r", now).unwrap());
    }

    #[test]
    fn v64_schema_is_additive_and_verified() {
        let db = Db::open_memory().unwrap();
        db.conn()
            .execute_batch(
                "CREATE TABLE existing_cache (id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
             INSERT INTO existing_cache VALUES (1, 'kept');",
            )
            .unwrap();
        crate::db_migrate::migrate_v64(db.conn()).unwrap();
        crate::db_migrate::verify_v64_schema(db.conn()).unwrap();
        let kept: String = db
            .conn()
            .query_row("SELECT value FROM existing_cache WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "kept");
    }

    #[test]
    fn concurrent_once_debounce_rule_and_action_rates_cannot_double_admit() {
        let mut once = admission_rule("once");
        once.once_per_key = true;
        let (planned, rows) = race_admissions(
            vec![once],
            admission_event("once-a", "same", None),
            admission_event("once-b", "same", None),
        );
        assert_eq!(planned, 1);
        assert_eq!(
            rows.iter().filter(|row| row.outcome == "started").count(),
            1
        );

        let mut debounce = admission_rule("debounce");
        debounce.debounce_secs = 60;
        assert_eq!(
            race_admissions(
                vec![debounce],
                admission_event("debounce-a", "a", None),
                admission_event("debounce-b", "b", None),
            )
            .0,
            1
        );

        let mut rule_rate = admission_rule("rule-rate");
        rule_rate.max_per_hour = 1;
        assert_eq!(
            race_admissions(
                vec![rule_rate],
                admission_event("rate-a", "a", None),
                admission_event("rate-b", "b", None),
            )
            .0,
            1
        );

        let mut action_a = admission_rule("action-a");
        let mut action_b = admission_rule("action-b");
        action_a.max_action_per_hour = 1;
        action_b.max_action_per_hour = 1;
        assert_eq!(
            race_admissions(
                vec![action_a, action_b],
                admission_event("action-a", "a", Some("action-a")),
                admission_event("action-b", "b", Some("action-b")),
            )
            .0,
            1
        );
    }
}
