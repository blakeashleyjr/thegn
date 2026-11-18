//! The daemon's `schedule_due` producer (THE-2193).
//!
//! A sibling of the `worktree_idle` timer in `subscribe_daemon_events`: one
//! daemon task, spawned only when at least one enabled rule has a schedule,
//! sleeping until the single earliest due time (`automation_schedule::next_due`)
//! and submitting `automation_schedule::schedule_event` through
//! `automation_runtime::submit`. It exits when no scheduled rule remains.
//! `rearm` (called by `automations.put`/`rm`/`enable`) respawns or nudges it;
//! `armed` keeps `idle_exit_loop` from reaping a daemon that has cron work.
//! The UI process never produces `schedule_due`.
//!
//! No catch-up: due times are always computed from the wall clock now, and an
//! `every` schedule is anchored at task start, so a daemon restart restarts
//! the interval. A run still in flight (or a live automation-opened session)
//! makes the tick `skipped_live` instead of stacking another.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thegn_core::automation_schedule::{Schedule, next_due, schedule_event};
use thegn_core::config::Config;
use thegn_core::store::AutomationStore;
use tokio::sync::Notify;

static CONFIG: Mutex<Option<Arc<Config>>> = Mutex::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);
static ARMED: AtomicBool = AtomicBool::new(false);
static REARM: Notify = Notify::const_new();

/// Start the producer if any enabled rule has a schedule. Idle invariant: with
/// no scheduled rule the task exits after one rule load.
pub(crate) fn spawn(cfg: &Config) {
    *CONFIG.lock().expect("schedule config lock") = Some(Arc::new(cfg.clone()));
    start();
}

fn start() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    ARMED.store(true, Ordering::SeqCst);
    if RUNNING.swap(true, Ordering::SeqCst) {
        REARM.notify_one();
        return;
    }
    handle.spawn(run());
}

/// Re-read the rule set after a rule write and respawn or nudge the producer.
/// A no-op when `spawn` was never called (this process is not the daemon).
pub(crate) fn rearm() {
    if RUNNING.load(Ordering::SeqCst) {
        REARM.notify_one();
    } else if CONFIG.lock().expect("schedule config lock").is_some() {
        start();
    }
}

/// Whether a schedule producer is live (the daemon is not idle while it is).
pub(crate) fn armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

/// Rules whose next due time is at or before `now`.
fn due_rules(next: &BTreeMap<String, i64>, now: i64) -> Vec<String> {
    next.iter()
        .filter(|(_, due)| **due <= now)
        .map(|(id, _)| id.clone())
        .collect()
}

fn load_rules(cfg: &Config) -> Vec<(String, Schedule)> {
    let effective = cfg.effective_automations();
    if !thegn_core::automation_authoring::runtime_armed(
        effective.enabled,
        effective.rules.len(),
        crate::lead::enabled(cfg),
    ) {
        return Vec::new();
    }
    let Ok(rules) = effective.compiled_rules() else {
        return Vec::new();
    };
    let rules = match thegn_core::db::Db::open() {
        Ok(db) => crate::automation_runtime::with_lead_rules(&db, rules),
        Err(_) => rules,
    };
    rules
        .into_iter()
        .filter(|rule| {
            rule.enabled && rule.event == thegn_core::automation::AutomationEventKind::ScheduleDue
        })
        .filter_map(|rule| rule.schedule.map(|s| (rule.id, s)))
        .collect()
}

async fn reload() -> Vec<(String, Schedule)> {
    let Some(cfg) = CONFIG.lock().expect("schedule config lock").clone() else {
        return Vec::new();
    };
    tokio::task::spawn_blocking(move || load_rules(&cfg))
        .await
        .unwrap_or_default()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn compute_next(rules: &[(String, Schedule)], now: i64, anchor: i64) -> BTreeMap<String, i64> {
    rules
        .iter()
        .filter_map(|(id, s)| next_due(s, now, anchor, &chrono::Local).map(|due| (id.clone(), due)))
        .collect()
}

async fn run() {
    let anchor = now_ms() / 1000;
    let mut rules = reload().await;
    loop {
        let now = now_ms() / 1000;
        let next = compute_next(&rules, now, anchor);
        let Some(earliest) = next.values().copied().min() else {
            // Nothing scheduled (or nothing ever due): exit. A rearm that
            // raced the exit is picked up by the zero-wait permit check.
            ARMED.store(false, Ordering::SeqCst);
            RUNNING.store(false, Ordering::SeqCst);
            let raced = tokio::time::timeout(Duration::ZERO, REARM.notified())
                .await
                .is_ok();
            if raced && !RUNNING.swap(true, Ordering::SeqCst) {
                ARMED.store(true, Ordering::SeqCst);
                rules = reload().await;
                continue;
            }
            return;
        };
        ARMED.store(true, Ordering::SeqCst);
        let wait_ms = (earliest.saturating_mul(1000) - now_ms()).max(0) as u64 + 25;
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(wait_ms)) => {
                fire(&next, now_ms() / 1000).await;
            }
            () = REARM.notified() => {
                rules = reload().await;
            }
        }
    }
}

async fn fire(next: &BTreeMap<String, i64>, now: i64) {
    for rule in due_rules(next, now) {
        let due = next[&rule];
        let id = rule.clone();
        let live = tokio::task::spawn_blocking(move || {
            crate::automation_runtime::rule_has_live_session(&id)
                || thegn_core::db::Db::open()
                    .and_then(|db| db.automation_run_in_flight(&id, now))
                    .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if live {
            tracing::info!(
                target: "thegn::automation",
                rule = %rule,
                outcome = "skipped_live",
                "scheduled tick skipped: previous run still live"
            );
            continue;
        }
        crate::automation_runtime::submit(schedule_event(&rule, due));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_rules_selects_at_or_before_now() {
        let next: BTreeMap<String, i64> = [("a", 100), ("b", 101), ("c", 99)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        assert_eq!(
            due_rules(&next, 100),
            vec!["a".to_string(), "c".to_string()]
        );
        assert!(due_rules(&next, 98).is_empty());
    }

    #[test]
    fn rearm_without_spawn_is_a_no_op() {
        rearm();
        assert!(!armed());
        assert!(!RUNNING.load(Ordering::SeqCst));
    }
}
