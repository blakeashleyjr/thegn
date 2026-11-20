//! The Lead indicator's facts, gathered on the hydration thread (batch 180): DB reads and two process-global cells, never on the event loop.

use std::sync::{Mutex, OnceLock};

use thegn_core::lead_bar::{LeadBar, LeadInputs};
use thegn_core::store::AutomationStore;

const PENDING_FETCH: usize = 99;
const RUNS_FETCH: usize = 20;

fn failure_cell() -> &'static Mutex<Option<String>> {
    static CELL: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Record the outcome of a Lead tab launch: Err sets the refusal text, Ok clears it.
pub(crate) fn note_launch<T>(result: &anyhow::Result<T>) {
    let mut failure = failure_cell().lock().unwrap_or_else(|p| p.into_inner());
    *failure = result.as_ref().err().map(|e| format!("{e:#}"));
}

pub(crate) fn launch_failure() -> Option<String> {
    failure_cell()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// None unless `crate::lead::enabled(cfg)` and not `crate::e2e_freeze::active()`.
pub(crate) fn gather(
    cfg: &thegn_core::config::Config,
    db: &thegn_core::db::Db,
    session: &crate::session::Session,
    now_ms: i64,
) -> Option<LeadBar> {
    if !crate::lead::enabled(cfg) || crate::e2e_freeze::active() {
        return None;
    }

    let (started, tabs) = session
        .worktrees
        .iter()
        .find(|g| {
            crate::lead::group::is_lead_group(&g.name)
                && g.kind == crate::session::GroupKind::Terminal
        })
        .map_or((false, 0), |g| (true, g.tabs.len()));
    let paused = thegn_core::automation_control::paused_strict(db).map_err(|e| format!("{e:#}"));
    let limits = thegn_core::automation_budget::Budgets::from_config(&cfg.effective_automations());
    let usage = db
        .automation_budget_usage(now_ms)
        .map_err(|e| format!("{e:#}"));
    let pending = crate::daemon::lead_caps::approvals::pending_with_ttl(
        db,
        now_ms,
        cfg.pipeline.supervisor.approval_ttl_secs,
        PENDING_FETCH,
    )
    .map_err(|e| format!("{e:#}"));
    let runs = db
        .automation_runs(None, RUNS_FETCH)
        .map_err(|e| format!("{e:#}"));

    Some(thegn_core::lead_bar::summarize(LeadInputs {
        started,
        tabs,
        launch_error: launch_failure(),
        paused,
        limits,
        usage,
        pending,
        runs,
        output_age_secs: crate::agent_output::snapshot_lead_output()
            .map(|stamp| now_ms as f64 / 1000.0 - stamp),
        quiet_grace_secs: cfg.activity.quiet_grace(),
        now_secs: now_ms / 1000,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn configured() -> thegn_core::config::Config {
        thegn_core::config::Config {
            lead: Some(thegn_core::config_lead::LeadConfig::default()),
            ..Default::default()
        }
    }

    #[test]
    fn gather_is_absent_without_lead_config() {
        let _guard = test_guard();
        let db = thegn_core::db::Db::open_memory().unwrap();
        assert!(
            gather(
                &thegn_core::config::Config::default(),
                &db,
                &crate::session::Session::default(),
                1_000,
            )
            .is_none()
        );
    }

    #[test]
    fn gather_reports_not_started_and_default_budget() {
        let _guard = test_guard();
        let db = thegn_core::db::Db::open_memory().unwrap();
        let bar = gather(
            &configured(),
            &db,
            &crate::session::Session::default(),
            1_000,
        )
        .unwrap();
        assert_eq!(bar.state, thegn_core::lead_bar::LeadState::NotStarted);
        assert_eq!(bar.approvals, Some(0));
        assert_eq!(
            bar.lanes,
            Some(thegn_core::lead_bar::Meter { used: 0, limit: 2 })
        );
    }

    #[test]
    fn gather_reports_paused() {
        let _guard = test_guard();
        let db = thegn_core::db::Db::open_memory().unwrap();
        thegn_core::automation_control::set_paused(&db, true).unwrap();
        let bar = gather(
            &configured(),
            &db,
            &crate::session::Session::default(),
            1_000,
        )
        .unwrap();
        assert_eq!(bar.state, thegn_core::lead_bar::LeadState::Paused);
    }

    #[test]
    fn launch_failure_is_serialized_and_cleared() {
        let _guard = test_guard();

        note_launch(&Err::<(), _>(anyhow::anyhow!("no bwrap")));
        assert_eq!(launch_failure().as_deref(), Some("no bwrap"));
        note_launch(&Ok::<(), anyhow::Error>(()));
        assert_eq!(launch_failure(), None);
    }
}
