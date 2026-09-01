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
                        })?;
                        let reason = format!("{reason:?}").to_ascii_lowercase();
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

    fn start_automation_run(&self, run: &NewAutomationRun) -> Result<i64> {
        self.conn().execute(
            "INSERT INTO automation_runs \
               (rule_id,event_id,event_key,trigger_kind,event_summary,action_cap, \
                action_summary,outcome,started_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,'started',?8)",
            params![
                run.rule_id,
                run.event_id,
                run.event_key,
                run.trigger_kind,
                run.event_summary,
                run.action_cap,
                run.action_summary,
                run.started_at,
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
                    action_summary,outcome,skip_reason,error,started_at,finished_at \
             FROM automation_runs \
             WHERE (?1 IS NULL OR rule_id=?1) \
             ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![rule_id, limit], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn prune_automation_runs(&self, retain_per_rule: usize) -> Result<usize> {
        let retain = i64::try_from(retain_per_rule.clamp(1, MAX_RETENTION_PER_RULE))
            .unwrap_or(MAX_RETENTION_PER_RULE as i64);
        let deleted = self.conn().execute(
            "DELETE FROM automation_runs AS old \
             WHERE (SELECT COUNT(*) FROM automation_runs AS newer \
                    WHERE newer.rule_id=old.rule_id \
                      AND (newer.started_at > old.started_at \
                           OR (newer.started_at = old.started_at AND newer.id > old.id))) >= ?1",
            params![retain],
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
    })
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
