//! Bounded off-loop automation evaluator and executor.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use thegn_core::automation::AutomationEvent;
use thegn_core::config::Config;
use thegn_core::store::{AutomationAdmission, AutomationStore, NewAutomationRun};
use tokio::sync::mpsc;

static SENDER: OnceLock<Mutex<Option<mpsc::Sender<AutomationEvent>>>> = OnceLock::new();
static PENDING: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
static SESSION_ORIGINS: OnceLock<
    Mutex<BTreeMap<String, thegn_core::automation::AutomationOrigin>>,
> = OnceLock::new();

pub fn register_session_origin(session: &str, origin: thegn_core::automation::AutomationOrigin) {
    SESSION_ORIGINS
        .get_or_init(Default::default)
        .lock()
        .expect("automation session origin lock")
        .insert(session.to_string(), origin);
}

fn session_origin(session: &str) -> Option<thegn_core::automation::AutomationOrigin> {
    SESSION_ORIGINS
        .get_or_init(Default::default)
        .lock()
        .expect("automation session origin lock")
        .get(session)
        .cloned()
}

fn take_session_origin(session: &str) -> Option<thegn_core::automation::AutomationOrigin> {
    SESSION_ORIGINS
        .get_or_init(Default::default)
        .lock()
        .expect("automation session origin lock")
        .remove(session)
}

pub fn install(cfg: &Config) {
    crate::automation_events::install_route_config(cfg);
    let effective = cfg.effective_automations();
    let slot = SENDER.get_or_init(|| Mutex::new(None));
    if !effective.enabled || effective.rules.is_empty() {
        *slot.lock().expect("automation sender lock") = None;
        return;
    }
    let Ok(rules) = effective.compiled_rules() else {
        tracing::warn!(target: "thegn::automation", "invalid automation config; runtime disabled");
        *slot.lock().expect("automation sender lock") = None;
        return;
    };
    let (tx, rx) = mpsc::channel(effective.queue_capacity);
    *slot.lock().expect("automation sender lock") = Some(tx);
    let cfg = Arc::new(cfg.clone());
    let thread_cfg = effective.clone();
    let _ = std::thread::Builder::new()
        .name("automation-runtime".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(thread_cfg.max_concurrent.max(1))
                .max_blocking_threads(thread_cfg.max_concurrent.max(1).saturating_mul(2))
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(run(rx, cfg, rules, thread_cfg));
        });
}

pub fn submit(event: AutomationEvent) {
    let Some(slot) = SENDER.get() else { return };
    let Some(tx) = slot.lock().expect("automation sender lock").clone() else {
        return;
    };
    let _ = try_submit(&tx, event);
}

fn try_submit(
    tx: &mpsc::Sender<AutomationEvent>,
    event: AutomationEvent,
) -> Option<std::thread::JoinHandle<anyhow::Result<()>>> {
    pending_add();
    if let Err(error) = tx.try_send(event) {
        let event = error.into_inner();
        pending_done();
        tracing::warn!(
            target: "thegn::automation",
            outcome = "dropped",
            "automation event queue full or closed"
        );
        return audit_dropped(event);
    }
    None
}

/// Bounded transient-producer handoff. A one-shot CLI calls this after it has
/// emitted, so successful exit means every accepted event reached a terminal
/// audit outcome.
pub fn drain(timeout: Duration) -> bool {
    let (lock, ready) = PENDING.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let pending = lock.lock().expect("automation pending lock");
    let (pending, _) = ready
        .wait_timeout_while(pending, timeout, |count| *count != 0)
        .expect("automation pending wait");
    *pending == 0
}

fn pending_add() {
    let (lock, _) = PENDING.get_or_init(|| (Mutex::new(0), Condvar::new()));
    *lock.lock().expect("automation pending lock") += 1;
}

fn pending_done() {
    let (lock, ready) = PENDING.get_or_init(|| (Mutex::new(0), Condvar::new()));
    let mut count = lock.lock().expect("automation pending lock");
    *count = count.saturating_sub(1);
    if *count == 0 {
        ready.notify_all();
    }
}

fn audit_dropped(event: AutomationEvent) -> Option<std::thread::JoinHandle<anyhow::Result<()>>> {
    std::thread::Builder::new()
        .name("automation-drop-audit".into())
        .spawn(move || -> anyhow::Result<()> {
            let db = thegn_core::db::Db::open()?;
            let now = thegn_core::util::now();
            let id = db.start_automation_run(&NewAutomationRun {
                rule_id: "__queue__".into(),
                event_id: event.id,
                event_key: event.key.0,
                trigger_kind: event.kind.as_str().into(),
                event_summary: bounded(event.message.as_deref().unwrap_or_default()),
                action_cap: String::new(),
                action_summary: String::new(),
                started_at: now,
            })?;
            db.finish_automation_run(id, "dropped", Some("queue_overflow"), None, now)?;
            Ok(())
        })
        .ok()
}

async fn run(
    mut rx: mpsc::Receiver<AutomationEvent>,
    cfg: Arc<Config>,
    rules: Vec<thegn_core::automation::AutomationRule>,
    settings: thegn_core::config_automations::AutomationsConfig,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(settings.max_concurrent));
    while let Some(event) = rx.recv().await {
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            break;
        };
        let cfg = Arc::clone(&cfg);
        let rules = rules.clone();
        let settings = settings.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(error) = process_event(cfg, rules, settings, event).await {
                tracing::warn!(target: "thegn::automation", error = %error, outcome = "failed", "automation event processing failed");
            }
            pending_done();
        });
    }
}

async fn process_event(
    cfg: Arc<Config>,
    rules: Vec<thegn_core::automation::AutomationRule>,
    settings: thegn_core::config_automations::AutomationsConfig,
    event: AutomationEvent,
) -> anyhow::Result<()> {
    let (event, admissions) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = thegn_core::db::Db::open()?;
        let mut event = event;
        enrich_event(&db, &mut event);
        event
            .validate_required_facts()
            .map_err(anyhow::Error::msg)?;
        let now = thegn_core::util::now();
        let admissions = db.admit_automation_event(&rules, &event, now)?;
        Ok((event, admissions))
    })
    .await??;

    for admission in admissions {
        match admission {
            AutomationAdmission::Skipped { .. } => {}
            AutomationAdmission::Planned { action, run_id } => {
                let action = *action;
                tracing::info!(
                    target: "thegn::automation",
                    rule = %action.rule_id,
                    event_key = %action.event_key.0,
                    cap = %action.cap,
                    run_id,
                    outcome = "fired",
                    "automation action dispatched"
                );
                let result = tokio::time::timeout(
                    Duration::from_secs(settings.action_timeout_secs.saturating_add(5)),
                    crate::automation_executor::execute(
                        Arc::clone(&cfg),
                        event.clone(),
                        action.clone(),
                        run_id,
                        settings.action_timeout_secs,
                    ),
                )
                .await;
                let (outcome, error) = match result {
                    Ok(Ok(())) => ("succeeded", None),
                    Ok(Err(error))
                        if error
                            .downcast_ref::<crate::automation_executor::ActionTimedOut>()
                            .is_some() =>
                    {
                        ("timed_out", Some(bounded(&format!("{error:#}"))))
                    }
                    Ok(Err(error)) => ("failed", Some(bounded(&format!("{error:#}")))),
                    Err(_) => (
                        "timed_out",
                        Some(format!("deadline {}s", settings.action_timeout_secs)),
                    ),
                };
                let error_for_store = error.clone();
                let retention = settings.audit_retention_per_rule;
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let db = thegn_core::db::Db::open()?;
                    db.finish_automation_run(
                        run_id,
                        outcome,
                        None,
                        error_for_store.as_deref(),
                        thegn_core::util::now(),
                    )?;
                    let _ = db.prune_automation_runs(retention);
                    Ok(())
                })
                .await??;
                tracing::info!(
                    target: "thegn::automation",
                    rule = %action.rule_id,
                    event_key = %action.event_key.0,
                    cap = %action.cap,
                    run_id,
                    outcome,
                    "automation action completed"
                );
                if outcome != "succeeded" {
                    let message = format!(
                        "{}: {}",
                        action.rule_id,
                        error.unwrap_or_else(|| outcome.into())
                    );
                    let worktree = event.worktree.clone().unwrap_or_default();
                    let root_event_id = event
                        .origin
                        .as_ref()
                        .map_or_else(|| event.id.clone(), |o| o.root_event_id.clone());
                    let rule_id = action.rule_id.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        let db = thegn_core::db::Db::open()?;
                        crate::automation_events::emit_with_facts(
                            &db,
                            "automation_failed",
                            &format!("automation:{rule_id}"),
                            &bounded(&message),
                            &worktree,
                            crate::automation_events::EventFacts {
                                origin: Some(thegn_core::automation::AutomationOrigin {
                                    root_event_id,
                                    rule_id,
                                    run_id: run_id.to_string(),
                                }),
                                ..Default::default()
                            },
                        )?;
                        Ok(())
                    })
                    .await??;
                } else if action.cap != "notify.push" {
                    let worktree = event.worktree.clone().unwrap_or_default();
                    let root_event_id = event
                        .origin
                        .as_ref()
                        .map_or_else(|| event.id.clone(), |o| o.root_event_id.clone());
                    let rule_id = action.rule_id.clone();
                    let cap = action.cap.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        let db = thegn_core::db::Db::open()?;
                        crate::automation_events::emit_with_facts(
                            &db,
                            "automation",
                            &format!("automation:{rule_id}"),
                            &format!("{rule_id}: {cap} succeeded"),
                            &worktree,
                            crate::automation_events::EventFacts {
                                origin: Some(thegn_core::automation::AutomationOrigin {
                                    root_event_id,
                                    rule_id,
                                    run_id: run_id.to_string(),
                                }),
                                ..Default::default()
                            },
                        )?;
                        Ok(())
                    })
                    .await??;
                }
            }
        }
    }
    Ok(())
}

fn bounded(value: &str) -> String {
    value.chars().take(512).collect()
}

fn enrich_event(db: &thegn_core::db::Db, event: &mut AutomationEvent) {
    use thegn_core::store::{NotificationStore, WorkspaceStore};
    let Some(worktree) = event.worktree.as_deref() else {
        return;
    };
    let row = db
        .worktrees()
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.worktree == worktree));
    if let Some(row) = row {
        if event.repo.is_none() {
            event.repo = Some(row.repo_root.clone());
        }
        if event.branch.is_none() && !row.branch.is_empty() {
            event.branch = Some(row.branch);
        }
        if event.workspace.is_none() {
            event.workspace = db.workspaces().ok().and_then(|rows| {
                rows.into_iter()
                    .find(|workspace| workspace.repo_path == row.repo_root)
                    .map(|workspace| workspace.name)
            });
        }
    }
    if event.agent_role.is_none() {
        event.agent_role = db.list_dispatches().ok().and_then(|rows| {
            rows.into_iter()
                .filter(|dispatch| dispatch.worktree_path == worktree)
                .filter(|dispatch| {
                    event
                        .session_id
                        .as_ref()
                        .is_none_or(|session| dispatch.session_id.as_ref() == Some(session))
                })
                .max_by_key(|dispatch| dispatch.dispatched_at_ms)
                .and_then(|dispatch| dispatch.stage)
        });
    }
}

/// Subscribe once at the daemon service edge. Activity transitions are already
/// coalesced by `SessionActor`; this adapter never inspects PTY text.
pub fn subscribe_daemon_events(
    mut rx: tokio::sync::broadcast::Receiver<Arc<thegn_core::control_wire::EventFrame>>,
    cfg: &Config,
) {
    use thegn_core::automation::AutomationEventKind;
    use thegn_core::control_wire::EventFrame;
    use thegn_svc::control::SessionActivityEvent;

    let idle_rules: Vec<(String, u64)> = cfg
        .effective_automations()
        .compiled_rules()
        .unwrap_or_default()
        .into_iter()
        .filter(|rule| rule.enabled && rule.event == AutomationEventKind::WorktreeIdle)
        .filter_map(|rule| rule.idle_secs.map(|after| (rule.id, after)))
        .collect();
    tokio::spawn(async move {
        let mut sessions = BTreeMap::<String, (Option<String>, bool, String)>::new();
        let mut idle_deadlines = BTreeMap::<
            (String, String),
            (
                tokio::time::Instant,
                Option<thegn_core::automation::AutomationOrigin>,
            ),
        >::new();
        loop {
            let next_idle = idle_deadlines.values().map(|(deadline, _)| *deadline).min();
            tokio::select! {
                frame = rx.recv() => {
                    let Ok(frame) = frame else { continue };
                    match frame.as_ref() {
                        EventFrame::Activity { json } => {
                            let Ok(activity) = serde_json::from_str::<SessionActivityEvent>(json) else { continue };
                            let previous_error = sessions.get(&activity.session).is_some_and(|(_, error, _)| *error);
                            let origin = session_origin(&activity.session);
                            sessions.insert(activity.session.clone(), (activity.worktree.clone(), activity.error_active, activity.state.clone()));
                            let facts = crate::automation_events::EventFacts { session_id: Some(activity.session.clone()), origin: origin.clone(), ..Default::default() };
                            if activity.state == "blocked" {
                                crate::automation_events::submit_fact(AutomationEventKind::AgentNeedsYou, format!("session:{}:blocked:{}", activity.session, activity.since_ms), activity.worktree.clone(), activity.message.clone(), facts.clone());
                            }
                            if activity.state == "done" {
                                crate::automation_events::submit_fact(AutomationEventKind::AgentFinished, format!("session:{}:done:{}", activity.session, activity.since_ms), activity.worktree.clone(), activity.message.clone(), facts.clone());
                            }
                            if activity.error_active && !previous_error {
                                crate::automation_events::submit_fact(AutomationEventKind::AgentFailed, format!("session:{}:error:{}", activity.session, activity.since_ms), activity.worktree.clone(), activity.message.clone(), facts);
                            }
                            if let Some(worktree) = activity.worktree {
                                let elapsed_ms = thegn_core::util::now()
                                    .saturating_mul(1_000)
                                    .saturating_sub(activity.since_ms)
                                    .max(0) as u64;
                                for (rule_id, after) in &idle_rules {
                                    let remaining_ms = after.saturating_mul(1_000).saturating_sub(elapsed_ms);
                                    idle_deadlines.insert(
                                        (worktree.clone(), rule_id.clone()),
                                        (tokio::time::Instant::now() + Duration::from_millis(remaining_ms), origin.clone()),
                                    );
                                }
                            }
                        }
                        EventFrame::SessionExit { session, code } => {
                            let (worktree, error_was_active, final_state) = sessions
                                .remove(session)
                                .map_or((None, false, String::new()), |(worktree, error, state)| (worktree, error, state));
                            let origin = take_session_origin(session);
                            let succeeded = code.unwrap_or(1) == 0;
                            if !((succeeded && final_state == "done") || (!succeeded && error_was_active)) {
                                crate::automation_events::submit_fact(
                                    if succeeded { AutomationEventKind::AgentFinished } else { AutomationEventKind::AgentFailed },
                                    format!("session:{session}:exit"),
                                    worktree,
                                    Some(format!("session exited ({})", code.map_or_else(|| "unknown".into(), |c| c.to_string()))),
                                    crate::automation_events::EventFacts { session_id: Some(session.clone()), origin, ..Default::default() },
                                );
                            }
                        }
                        _ => {}
                    }
                }
                _ = async {
                    if let Some(deadline) = next_idle { tokio::time::sleep_until(deadline).await } else { std::future::pending::<()>().await }
                } => {
                    let now = tokio::time::Instant::now();
                    let due: Vec<(String, String)> = idle_deadlines.iter().filter(|(_, (deadline, _))| *deadline <= now).map(|(key, _)| key.clone()).collect();
                    for (worktree, rule_id) in due {
                        let origin = idle_deadlines.remove(&(worktree.clone(), rule_id.clone())).and_then(|(_, origin)| origin);
                        let occurred_at = thegn_core::util::now();
                        submit(AutomationEvent {
                            id: format!("worktree:{worktree}:idle:{rule_id}:{occurred_at}"),
                            occurred_at,
                            key: thegn_core::automation::EventKey(format!("worktree:{worktree}:idle:{rule_id}")),
                            kind: AutomationEventKind::WorktreeIdle,
                            target_rule: Some(rule_id),
                            workspace: None,
                            repo: None,
                            worktree: Some(worktree),
                            branch: None,
                            agent_role: None,
                            notification_kind: None,
                            priority: None,
                            source_ref: None,
                            message: None,
                            session_id: None,
                            pr_checks_passed: None,
                            pr_review_requested: None,
                            pr_merged: None,
                            origin,
                        });
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::{NotificationStore, WorkspaceStore};

    #[test]
    fn summaries_are_bounded() {
        assert_eq!(bounded(&"x".repeat(600)).len(), 512);
    }

    #[test]
    fn daemon_session_fact_is_enriched_from_workspace_and_dispatch_records() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "product", "repo").unwrap();
        db.put_worktree("product/feature", "/repo", "/wt", "tg/feature", None, None)
            .unwrap();
        db.put_agent_dispatch(thegn_core::issue::NewDispatch {
            issue_id: "THE-21",
            worktree_path: "/wt",
            agent_name: "codex",
            stage: Some("code"),
            parent_id: None,
            session_id: Some("session-1"),
            artifact_path: None,
            chunk_path: None,
        })
        .unwrap();
        let mut event = AutomationEvent {
            id: "session-1:done".into(),
            occurred_at: 1,
            key: thegn_core::automation::EventKey("session-1:done".into()),
            kind: thegn_core::automation::AutomationEventKind::AgentFinished,
            target_rule: None,
            workspace: None,
            repo: None,
            worktree: Some("/wt".into()),
            branch: None,
            agent_role: None,
            notification_kind: None,
            priority: None,
            source_ref: None,
            message: None,
            session_id: Some("session-1".into()),
            pr_checks_passed: None,
            pr_review_requested: None,
            pr_merged: None,
            origin: None,
        };
        enrich_event(&db, &mut event);
        assert_eq!(event.workspace.as_deref(), Some("product"));
        assert_eq!(event.repo.as_deref(), Some("/repo"));
        assert_eq!(event.branch.as_deref(), Some("tg/feature"));
        assert_eq!(event.agent_role.as_deref(), Some("code"));
    }

    #[test]
    fn bounded_queue_overflow_writes_a_durable_dropped_audit() {
        let state = tempfile::tempdir().unwrap();
        let _env =
            crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state.path().to_str().unwrap())]);
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(AutomationEvent {
            id: "occupy".into(),
            occurred_at: 1,
            key: thegn_core::automation::EventKey("occupy".into()),
            kind: thegn_core::automation::AutomationEventKind::DiskLow,
            target_rule: None,
            workspace: None,
            repo: None,
            worktree: None,
            branch: None,
            agent_role: None,
            notification_kind: None,
            priority: None,
            source_ref: None,
            message: None,
            session_id: None,
            pr_checks_passed: None,
            pr_review_requested: None,
            pr_merged: None,
            origin: None,
        })
        .unwrap();
        let dropped = AutomationEvent {
            id: "dropped-event".into(),
            occurred_at: 2,
            key: thegn_core::automation::EventKey("dropped".into()),
            kind: thegn_core::automation::AutomationEventKind::DiskLow,
            target_rule: None,
            workspace: None,
            repo: None,
            worktree: None,
            branch: None,
            agent_role: None,
            notification_kind: None,
            priority: None,
            source_ref: None,
            message: Some("low disk".into()),
            session_id: None,
            pr_checks_passed: None,
            pr_review_requested: None,
            pr_merged: None,
            origin: None,
        };
        try_submit(&tx, dropped)
            .expect("overflow starts durable audit")
            .join()
            .expect("audit thread")
            .unwrap();
        let rows = thegn_core::db::Db::open()
            .and_then(|db| db.automation_runs(Some("__queue__"), 10))
            .unwrap();
        assert!(
            rows.iter().any(|row| {
                row.event_id == "dropped-event"
                    && row.outcome == "dropped"
                    && row.skip_reason.as_deref() == Some("queue_overflow")
            }),
            "{rows:?}"
        );
    }
}
