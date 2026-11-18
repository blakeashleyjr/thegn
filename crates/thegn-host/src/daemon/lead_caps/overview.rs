//! `overview.get` (THE-2171): the Lead's one-call cross-workspace overview.
//!
//! DB and cache only: no git, no subprocess. The daemon's session table is
//! snapshotted first (async lock, released) and the DB gather runs after, so
//! the sessions lock is never held across `with_db`. UI-only live state is not
//! visible here, so the activity facts are approximate and the reply says so.

use std::collections::HashMap;

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use thegn_core::attention::{ActivityKind, MqFacts, MqStatus, PrFacts, UnreadNote};
use thegn_core::db::Db;
use thegn_core::overview::{
    self, CalendarView, CiView, NotificationCounts, OverviewInputs, OverviewRequest, QueueView,
    RosterInput, RosterView, WorktreeInput,
};
use thegn_core::store::{
    CacheStore, CalendarStore, NotificationStore, WorkspaceStore, WorktreeAuxStore,
};

/// Per-worktree session facts the daemon knows: `(sessions, oldest start ms)`.
type SessionFacts = HashMap<String, (usize, i64)>;

/// Answer `overview.get`.
pub(crate) async fn get(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let req: OverviewRequest = decode(req)?;
    let facts: SessionFacts = {
        let sessions = svc.sessions.lock().await;
        let mut facts = SessionFacts::new();
        for entry in sessions.values() {
            if let Some(wt) = &entry.meta.worktree {
                let slot = facts.entry(wt.clone()).or_insert((0, i64::MAX));
                slot.0 += 1;
                slot.1 = slot.1.min(entry.meta.created_at_ms);
            }
        }
        facts
    };
    let now = thegn_core::util::now();
    let calendar_cfg = svc.config.calendar.clone();
    let limit = req.effective_limit();
    let inputs = svc
        .with_db(move |db| Ok(gather(db, &facts, &calendar_cfg, now, limit)))
        .await?;
    encode(&overview::assemble(inputs, &req, now))
}

/// Run one source read; a failure becomes a `partial` entry and the default.
fn section<T: Default>(
    errors: &mut Vec<String>,
    name: &str,
    read: impl FnOnce() -> anyhow::Result<T>,
) -> T {
    match read() {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("{name}: {e:#}"));
            T::default()
        }
    }
}

fn gather(
    db: &Db,
    facts: &SessionFacts,
    calendar_cfg: &thegn_core::config::CalendarConfig,
    now: i64,
    limit: usize,
) -> OverviewInputs {
    let mut errors = vec!["activity: daemon-only view".to_owned()];

    let rows = section(&mut errors, "worktrees", || db.worktrees());
    let unread = section(&mut errors, "notifications", || {
        db.get_unread_notifications()
    });
    let mq = section(&mut errors, "merge_queue", || db.list_merge_queue());
    let pr_rows = section(&mut errors, "pr_queue", || db.list_pr_queue());
    let pr_cache = section(&mut errors, "pr_cache", || {
        Ok(db
            .list_pr_cache()?
            .into_iter()
            .map(|(wt, json, _)| (wt, json))
            .collect::<HashMap<_, _>>())
    });

    let mut notes: HashMap<&str, Vec<UnreadNote>> = HashMap::new();
    let mut counts = NotificationCounts::default();
    for n in &unread {
        counts.unread += 1;
        *counts
            .by_kind
            .entry(n.kind.as_str().to_owned())
            .or_default() += 1;
        notes
            .entry(n.worktree_path.as_str())
            .or_default()
            .push(UnreadNote {
                kind: n.kind,
                at: n.created_at_ms,
            });
    }

    let mq_by_wt: HashMap<&str, &thegn_core::db::MergeQueueRow> =
        mq.iter().map(|r| (r.worktree.as_str(), r)).collect();

    let mut ci_failing = Vec::new();
    let mut worktrees = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut input = WorktreeInput {
            worktree: row.worktree.clone(),
            workspace: row.repo_root.clone(),
            branch: (!row.branch.is_empty()).then(|| row.branch.clone()),
            ..Default::default()
        };
        let my_notes = notes.remove(row.worktree.as_str()).unwrap_or_default();
        input.unread = my_notes.len();
        input.attention.unread = my_notes;
        if let Some((_, started_ms)) = facts.get(&row.worktree) {
            input.attention.activity = ActivityKind::Active;
            input.attention.has_agent = true;
            input.attention.activity_since = Some(started_ms / 1000);
        }
        input.attention.pr = pr_cache.get(&row.worktree).and_then(|json| pr_facts(json));
        if let Some(r) = mq_by_wt.get(row.worktree.as_str())
            && let Some(status) = MqStatus::parse(&r.status)
            && status != MqStatus::Landed
        {
            input.attention.merge_queue = Some(MqFacts {
                status,
                updated_at: r.updated_at,
            });
        }
        match db.get_ci_cache(&row.worktree) {
            Ok(Some((json, _))) => {
                if let Some(run) = latest_failing_run(&json) {
                    input.attention.ci_failing = true;
                    ci_failing.push(CiView {
                        worktree: row.worktree.clone(),
                        branch: run.branch.clone(),
                        run: run.name.clone(),
                        url: (!run.url.is_empty()).then(|| run.url.clone()),
                    });
                }
            }
            Ok(None) => {}
            Err(e) => errors.push(format!("ci_cache: {e:#}")),
        }
        worktrees.push(input);
    }

    let roster = section(&mut errors, "roster", || {
        Ok(db
            .list_dispatches()?
            .into_iter()
            .filter(|d| d.status.is_active())
            .map(|d| RosterInput {
                stage: d.stage.clone(),
                view: RosterView {
                    issue_id: d.issue_id,
                    worktree: d.worktree_path,
                    agent: d.agent_name,
                    status: d.status.as_str().to_owned(),
                    dispatched_at_ms: d.dispatched_at_ms,
                },
            })
            .collect::<Vec<_>>())
    });

    let merge_queue = mq
        .iter()
        .filter(|r| r.status != "landed")
        .map(|r| QueueView {
            id: r.worktree.clone(),
            worktree: Some(r.worktree.clone()),
            branch: r.branch.clone(),
            status: r.status.clone(),
            detail: r.error_detail.clone(),
            updated_at: r.updated_at,
        })
        .collect();
    let pr_queue = pr_rows
        .into_iter()
        .map(|r| QueueView {
            id: r.key,
            worktree: r.worktree,
            branch: r.branch,
            status: r.status,
            detail: r.detail,
            updated_at: 0,
        })
        .collect();

    let approvals_pending = section(&mut errors, "approvals", || {
        super::approvals::pending(db, now.saturating_mul(1000), limit)
    });
    let calendar_today = section(&mut errors, "calendar", || {
        calendar_today(db, calendar_cfg, now)
    });

    OverviewInputs {
        worktrees,
        roster,
        merge_queue,
        pr_queue,
        ci_failing,
        approvals_pending,
        notifications: counts,
        calendar_today,
        errors,
    }
}

/// The open PR's attention facts from a cached panel row.
fn pr_facts(json: &str) -> Option<PrFacts> {
    use thegn_core::forge::model::{PanelState, PrPanel};
    let panel: PrPanel = serde_json::from_str(json).ok()?;
    match panel.state {
        PanelState::Pr(pr) => PrFacts::from_status(&pr),
        _ => None,
    }
}

/// The newest cached CI run, if it is failing.
fn latest_failing_run(json: &str) -> Option<thegn_core::ci::CiRun> {
    let cache = thegn_core::ci::decode_run_cache(json)?;
    cache
        .runs
        .into_iter()
        .next()
        .filter(|r| r.state == thegn_core::ci::CiState::Fail)
}

fn time_text(t: &thegn_core::calendar::EventTime) -> String {
    use thegn_core::calendar::EventTime as T;
    match t {
        T::Date { date } => date.to_string(),
        T::Zoned { local, .. } => local.to_string(),
        T::Instant { at } => at.to_rfc3339(),
    }
}

fn calendar_today(
    db: &Db,
    cfg: &thegn_core::config::CalendarConfig,
    now: i64,
) -> anyhow::Result<Vec<CalendarView>> {
    use crate::daemon::calendar_api;
    let home = calendar_api::home_zone(cfg);
    let today = chrono::DateTime::from_timestamp(now, 0)
        .ok_or_else(|| anyhow::anyhow!("clock out of range"))?
        .with_timezone(&home)
        .date_naive();
    let window = thegn_core::calendar::CalendarWindow::new(today, today, home)
        .ok_or_else(|| anyhow::anyhow!("today's window is not representable"))?;
    let rows = db.get_calendar_events(window.start_ms, window.end_exclusive_ms, &[])?;
    let reply = calendar_api::events_from_rows(rows, today, today, home)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(reply
        .events
        .iter()
        .map(|e| CalendarView {
            title: e.title.clone(),
            start: time_text(&e.start),
            end: time_text(&e.end),
            all_day: e.start.is_all_day(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::issue::NewDispatch;

    #[test]
    fn gather_reports_a_seeded_worktree_unread_note_and_active_dispatch() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open_memory().expect("db");
        db.put_worktree("tab", "/repo", "/repo/wt1", "feat", None, None)
            .expect("worktree");
        db.put_notification("agent_attention", "i", "needs you", "/repo/wt1")
            .expect("note");
        db.put_agent_dispatch(NewDispatch {
            stage: Some("code"),
            ..NewDispatch::new("linear:T-1", "/repo/wt1", "coder")
        })
        .expect("dispatch");

        let facts = SessionFacts::new();
        let cfg = thegn_core::config::CalendarConfig::default();
        let inputs = gather(&db, &facts, &cfg, 1_700_000_000, 50);
        let reply = overview::assemble(inputs, &OverviewRequest::default(), 1_700_000_000);

        assert_eq!(reply.worktrees.len(), 1);
        assert_eq!(reply.worktrees[0].worktree, "/repo/wt1");
        assert_eq!(reply.worktrees[0].tier, "blocked");
        assert_eq!(reply.worktrees[0].unread, 1);
        assert_eq!(reply.notifications.unread, 1);
        assert_eq!(reply.roster["code"].len(), 1);
        assert!(reply.partial.iter().any(|p| p.starts_with("activity:")));
    }
}
