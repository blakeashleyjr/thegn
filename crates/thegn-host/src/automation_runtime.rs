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

pub fn clear_session_origin(session: &str) {
    if let Some(origins) = SESSION_ORIGINS.get() {
        origins
            .lock()
            .expect("automation session origin lock")
            .remove(session);
    }
}

/// Whether a session an automation run of `rule_id` opened is still live.
/// `register_session_origin` records each automation-opened session and the
/// session-exit path removes it, so this is the in-process view of "the
/// previous run is still going" (`SkipReason::PreviousRunLive`).
#[allow(dead_code, reason = "consumed by b153 lanes 2 and 5 (PreviousRunLive)")]
pub(crate) fn rule_has_live_session(rule_id: &str) -> bool {
    SESSION_ORIGINS.get().is_some_and(|origins| {
        origins
            .lock()
            .expect("automation session origin lock")
            .values()
            .any(|origin| origin.rule_id == rule_id)
    })
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
    // Config-only gate (THE-2181): a configured Lead may author rules later,
    // so it arms the runtime even with no config rules. No DB read here.
    if !thegn_core::automation_authoring::runtime_armed(
        effective.enabled,
        effective.rules.len(),
        crate::lead::enabled(cfg),
    ) {
        *slot.lock().expect("automation sender lock") = None;
        return;
    }
    let Ok(rules) = effective.compiled_rules() else {
        tracing::warn!(target: "thegn::automation", "invalid automation config; runtime disabled");
        *slot.lock().expect("automation sender lock") = None;
        return;
    };
    let (tx, rx) = mpsc::channel(effective.queue_capacity);
    let cfg = Arc::new(cfg.clone());
    let thread_cfg = effective.clone();
    let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(thread_cfg.max_concurrent.max(1))
        .max_blocking_threads(thread_cfg.max_concurrent.max(1).saturating_mul(2))
        .enable_all()
        .build()
    else {
        tracing::error!(target: "thegn::automation", "failed to build automation runtime");
        return;
    };
    let spawn = thegn_core::thread_name::try_spawn_named("thegn-automate", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        rt.block_on(run(rx, cfg, rules, thread_cfg));
    });
    match spawn {
        Ok(_) => *slot.lock().expect("automation sender lock") = Some(tx),
        Err(error) => tracing::error!(
            target: "thegn::automation",
            %error,
            "failed to start automation runtime"
        ),
    }
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
        tracing::warn!(
            target: "thegn::automation",
            outcome = "dropped",
            "automation event queue full or closed"
        );
        let audit = audit_dropped(event);
        if audit.is_none() {
            pending_done();
        }
        return audit;
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
    thegn_core::thread_name::try_spawn_named("thegn-auto-drop", move || -> anyhow::Result<()> {
        let result = (|| -> anyhow::Result<()> {
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
                autonomy: None,
            })?;
            db.finish_automation_run(id, "dropped", Some("queue_overflow"), None, now)?;
            Ok(())
        })();
        if let Err(error) = &result {
            tracing::warn!(
                target: "thegn::automation",
                %error,
                outcome = "dropped",
                "failed to persist queue-overflow audit"
            );
        }
        pending_done();
        result
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
        let rules = with_lead_rules(&db, rules);
        let now = thegn_core::util::now();
        let admissions = admit_unless_paused(&db, &rules, &event, now)?;
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
                crate::daemon::lead_caps::feed::automation_run(&action.rule_id, run_id, outcome);
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

/// Admission behind the kill switch (THE-2192). Paused: evaluate purely and
/// audit every would-be plan as `SkipReason::Paused` through the same audit
/// table admission writes skips to, persist no ledger transition, and admit
/// nothing, so no action executes. A resumed runtime therefore still sees an
/// event it skipped while paused as fresh (`once_per_key` was not consumed).
fn admit_unless_paused(
    db: &thegn_core::db::Db,
    rules: &[thegn_core::automation::AutomationRule],
    event: &AutomationEvent,
    now: i64,
) -> anyhow::Result<Vec<AutomationAdmission>> {
    if thegn_core::automation_control::paused(db) {
        let audited = db.audit_paused_automation_event(rules, event, now)?;
        if audited > 0 {
            tracing::info!(
                target: "thegn::automation",
                event_key = %event.key.0,
                audited,
                outcome = "paused",
                "automations paused; matching rules skipped"
            );
        }
        return Ok(Vec::new());
    }
    db.admit_automation_event(rules, event, now)
}

/// The config rules plus the enabled, still-valid Lead-authored rules
/// (THE-2181). Every stored row is re-validated and `merge_rules` filters the
/// action allowlist again, so a stored rule can never reach `merge.add` or
/// `tools.run`. A DB error keeps the config rules only.
fn with_lead_rules(
    db: &thegn_core::db::Db,
    rules: Vec<thegn_core::automation::AutomationRule>,
) -> Vec<thegn_core::automation::AutomationRule> {
    use thegn_core::store::AutomationRuleStore;
    let rows = match db.lead_rules() {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "thegn::automation",
                %error,
                "failed to read Lead-authored automation rules; using config rules only"
            );
            return rules;
        }
    };
    if rows.is_empty() {
        return rules;
    }
    let config_names: Vec<String> = rules.iter().map(|rule| rule.id.clone()).collect();
    let (lead, invalid) = thegn_core::automation_authoring::compile_lead_rows(&rows, &config_names);
    for (rule, errors) in invalid {
        tracing::warn!(
            target: "thegn::automation",
            rule = %rule,
            errors = %errors.join("; "),
            "skipping an invalid Lead-authored automation rule"
        );
    }
    thegn_core::automation_authoring::merge_rules(rules, lead)
}

fn bounded(value: &str) -> String {
    value.chars().take(512).collect()
}

fn same_worktree(stored: &str, worktree: &str, resolved: Option<&str>) -> bool {
    stored == worktree
        || resolved.is_some_and(|resolved| {
            crate::monitor_pipeline::resolved_worktree(stored).as_deref() == Some(resolved)
        })
}

fn enrich_event(db: &thegn_core::db::Db, event: &mut AutomationEvent) {
    use thegn_core::store::{NotificationStore, WorkspaceStore};
    let Some(worktree) = event.worktree.as_deref() else {
        return;
    };
    let resolved = crate::monitor_pipeline::resolved_worktree(worktree);
    let row = db.worktrees().ok().and_then(|rows| {
        if let Some(exact) = rows.iter().find(|row| row.worktree == worktree) {
            Some(exact.clone())
        } else {
            let mut matches = rows.into_iter().filter(|row| {
                std::path::Path::new(&row.worktree).is_absolute()
                    && !thegn_core::remote::GitLoc::from_db(&row.worktree, Some(&row.location))
                        .is_remote()
                    && same_worktree(&row.worktree, worktree, resolved.as_deref())
            });
            let first = matches.next()?;
            if matches.all(|row| row.repo_root == first.repo_root) {
                Some(first)
            } else {
                tracing::debug!(
                    target: "thegn::automation",
                    worktree,
                    "worktree alias matches registry rows from multiple repositories"
                );
                None
            }
        }
    });
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
        event.agent_role = if let Some(session) = event.session_id.as_deref() {
            db.dispatch_by_session(session)
                .ok()
                .flatten()
                .filter(|dispatch| {
                    same_worktree(&dispatch.worktree_path, worktree, resolved.as_deref())
                })
                .and_then(|dispatch| dispatch.stage)
        } else {
            db.dispatch_for_worktree(worktree)
                .ok()
                .flatten()
                .or_else(|| {
                    resolved
                        .as_deref()
                        .filter(|resolved| *resolved != worktree)
                        .and_then(|resolved| db.dispatch_for_worktree(resolved).ok().flatten())
                })
                .and_then(|id| db.get_dispatch(id).ok().flatten())
                .and_then(|dispatch| dispatch.stage)
        };
    }
}

/// THE-2021: after the last daemon session of a provider-located worktree
/// exits, pull the sandbox's commits back once, on the blocking pool (never
/// the event task). Single-flight per worktree. The summary goes to
/// `thegn::git_audit`; a kept ref or a failure is also submitted as one
/// automation fact.
fn spawn_exit_pull_back(boot: &Arc<Config>, worktree: &str, other_live_sessions: usize) {
    if other_live_sessions > 0 {
        return;
    }
    let boot = Arc::clone(boot);
    let worktree = worktree.to_string();
    tokio::task::spawn_blocking(move || {
        use thegn_core::store::WorkspaceStore;
        let flight = crate::provider_git::try_begin_pull(&worktree);
        let location_is_provider = thegn_core::db::Db::open()
            .and_then(|db| db.location_for(&worktree))
            .is_ok_and(|stored| {
                crate::provider_git::is_provider_location(&worktree, stored.as_deref())
            });
        if !crate::provider_git::should_pull_on_exit(
            location_is_provider,
            other_live_sessions,
            flight.is_none(),
        ) {
            return;
        }
        // The currently admitted config (hot reloads included), else boot.
        let admitted = crate::config_startup::display();
        let cfg = admitted.as_deref().map_or(boot.as_ref(), |a| a.config());
        let notice = match crate::provider_git::pull_back_if_provider(cfg, &worktree) {
            Ok(None) => None,
            Ok(Some(report)) => {
                tracing::info!(target: "thegn::git_audit", worktree = %worktree, summary = %report.summary(), "session-exit pull-back");
                match &report.outcome {
                    crate::provider_git::PullOutcome::Diverged { kept_ref, .. }
                    | crate::provider_git::PullOutcome::LocalDirty { kept_ref } => {
                        Some(format!("pull-back kept {kept_ref}: {}", report.summary()))
                    }
                    _ => None,
                }
            }
            Err(error) => {
                tracing::warn!(target: "thegn::git_audit", worktree = %worktree, error = %format!("{error:#}"), "session-exit pull-back failed");
                Some(format!("pull-back failed: {error:#}"))
            }
        };
        drop(flight);
        if let Some(notice) = notice {
            crate::automation_events::submit_fact(
                thegn_core::automation::AutomationEventKind::Notification,
                format!("worktree:{worktree}:pull-back"),
                Some(worktree.clone()),
                Some(bounded(&notice)),
                crate::automation_events::EventFacts::default(),
            );
        }
    });
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
    let boot = Arc::new(cfg.clone());
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
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    };
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
                            if let Some(wt) = worktree.as_deref() {
                                let others = sessions.values().filter(|(other, _, _)| other.as_deref() == Some(wt)).count();
                                spawn_exit_pull_back(&boot, wt, others);
                            }
                            let origin = take_session_origin(session);
                            let succeeded = code.unwrap_or(1) == 0;
                            if !((succeeded && final_state == "done") || (!succeeded && error_was_active)) {
                                crate::automation_events::submit_fact(
                                    if succeeded { AutomationEventKind::AgentFinished } else { AutomationEventKind::AgentFailed },
                                    format!("session:{session}:exit"),
                                    worktree.clone(),
                                    Some(format!("session exited ({})", code.map_or_else(|| "unknown".into(), |c| c.to_string()))),
                                    crate::automation_events::EventFacts { session_id: Some(session.clone()), origin: origin.clone(), ..Default::default() },
                                );
                            }
                            // Every exit, whatever the role or final state.
                            submit(thegn_core::automation::session_exited_event(
                                session,
                                worktree,
                                *code,
                                origin,
                                thegn_core::util::now(),
                            ));
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
                            issue: None,
                            exit_code: None,
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
            issue: None,
            exit_code: None,
        };
        enrich_event(&db, &mut event);
        assert_eq!(event.workspace.as_deref(), Some("product"));
        assert_eq!(event.repo.as_deref(), Some("/repo"));
        assert_eq!(event.branch.as_deref(), Some("tg/feature"));
        assert_eq!(event.agent_role.as_deref(), Some("code"));
    }

    fn event_for_dispatch(worktree: &str, session_id: Option<&str>) -> AutomationEvent {
        let id = "dispatch-event";
        AutomationEvent {
            id: id.into(),
            occurred_at: 1,
            key: thegn_core::automation::EventKey(id.into()),
            kind: thegn_core::automation::AutomationEventKind::AgentFinished,
            target_rule: None,
            workspace: None,
            repo: None,
            worktree: Some(worktree.into()),
            branch: None,
            agent_role: None,
            notification_kind: None,
            priority: None,
            source_ref: None,
            message: None,
            session_id: session_id.map(str::to_owned),
            pr_checks_passed: None,
            pr_review_requested: None,
            pr_merged: None,
            origin: None,
            issue: None,
            exit_code: None,
        }
    }

    fn put_dispatch(db: &thegn_core::db::Db, worktree: &str, session_id: &str, stage: &str) -> i64 {
        db.put_agent_dispatch(thegn_core::issue::NewDispatch {
            issue_id: "THE-259",
            worktree_path: worktree,
            agent_name: "codex",
            stage: Some(stage),
            parent_id: None,
            session_id: Some(session_id),
            artifact_path: None,
            chunk_path: None,
        })
        .unwrap()
    }

    #[test]
    fn worktree_enrichment_matches_a_registered_worktree_through_a_symlink_alias() {
        let temp = thegn_core::testenv::symlinked_tempdir();
        if temp.path() == temp.real_path() {
            return;
        }
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "product", "repo").unwrap();
        db.put_worktree(
            "product/feature",
            "/repo",
            &temp.real_path().display().to_string(),
            "tg/feature",
            None,
            None,
        )
        .unwrap();

        let mut event = event_for_dispatch(&temp.path().display().to_string(), None);
        enrich_event(&db, &mut event);
        assert_eq!(event.workspace.as_deref(), Some("product"));
        assert_eq!(event.repo.as_deref(), Some("/repo"));
        assert_eq!(event.branch.as_deref(), Some("tg/feature"));
    }

    #[test]
    fn worktree_enrichment_does_not_resolve_remote_registry_paths_locally() {
        let temp = thegn_core::testenv::symlinked_tempdir();
        if temp.path() == temp.real_path() {
            return;
        }
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_workspace("/repo", "product", "repo").unwrap();
        let remote_location = thegn_core::remote::GitLoc::remote_db_string(
            "example.invalid",
            22,
            false,
            &temp.real_path().display().to_string(),
        );
        db.put_worktree(
            "product/remote",
            "/repo",
            &temp.real_path().display().to_string(),
            "tg/remote",
            Some(&remote_location),
            None,
        )
        .unwrap();

        let mut event = event_for_dispatch(&temp.path().display().to_string(), None);
        enrich_event(&db, &mut event);
        assert_eq!(event.repo, None);
        assert_eq!(event.branch, None);
        assert_eq!(event.workspace, None);
    }

    #[test]
    fn session_dispatch_enrichment_accepts_its_row_under_an_alias_spelling() {
        let temp = thegn_core::testenv::symlinked_tempdir();
        if temp.path() == temp.real_path() {
            return;
        }
        let db = thegn_core::db::Db::open_memory().unwrap();
        put_dispatch(
            &db,
            &temp.real_path().display().to_string(),
            "s-alias",
            "code",
        );

        let mut event = event_for_dispatch(&temp.path().display().to_string(), Some("s-alias"));
        enrich_event(&db, &mut event);
        assert_eq!(event.agent_role.as_deref(), Some("code"));
    }

    #[test]
    fn worktree_dispatch_enrichment_falls_back_to_the_resolved_path() {
        let temp = thegn_core::testenv::symlinked_tempdir();
        if temp.path() == temp.real_path() {
            return;
        }
        let db = thegn_core::db::Db::open_memory().unwrap();
        put_dispatch(
            &db,
            &temp.real_path().display().to_string(),
            "s-any",
            "review",
        );

        let mut event = event_for_dispatch(&temp.path().display().to_string(), None);
        enrich_event(&db, &mut event);
        assert_eq!(event.agent_role.as_deref(), Some("review"));
    }

    #[test]
    fn session_dispatch_enrichment_rejects_an_alias_of_another_worktree() {
        let a = thegn_core::testenv::symlinked_tempdir();
        let b = thegn_core::testenv::symlinked_tempdir();
        if a.path() == a.real_path() || b.path() == b.real_path() {
            return;
        }
        let db = thegn_core::db::Db::open_memory().unwrap();
        put_dispatch(&db, &b.real_path().display().to_string(), "s-x", "wrong");

        let mut event = event_for_dispatch(&a.path().display().to_string(), Some("s-x"));
        enrich_event(&db, &mut event);
        assert_eq!(event.agent_role, None);
    }

    #[test]
    fn ambiguous_alias_registry_match_enriches_nothing() {
        let temp = thegn_core::testenv::symlinked_tempdir();
        if temp.path() == temp.real_path() {
            return;
        }
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree(
            "t/one",
            "/repo-one",
            &temp.path().display().to_string(),
            "b1",
            None,
            None,
        )
        .unwrap();
        db.put_worktree(
            "t/two",
            "/repo-two",
            &temp.real_path().display().to_string(),
            "b2",
            None,
            None,
        )
        .unwrap();
        let event_path = format!("{}/.", temp.real_path().display());

        let mut event = event_for_dispatch(&event_path, None);
        enrich_event(&db, &mut event);
        assert_eq!(event.repo, None);
        assert_eq!(event.branch, None);
    }

    #[test]
    fn session_dispatch_enrichment_uses_its_row_instead_of_a_newer_worktree_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open_at(&dir.path().join("automation.db")).unwrap();
        put_dispatch(&db, "/wt", "session-requested", "requested-stage");
        put_dispatch(&db, "/wt", "other-session", "newer-stage");

        let mut event = event_for_dispatch("/wt", Some("session-requested"));
        enrich_event(&db, &mut event);
        assert_eq!(event.agent_role.as_deref(), Some("requested-stage"));
    }

    #[test]
    fn session_dispatch_enrichment_rejects_a_session_row_in_another_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open_at(&dir.path().join("automation.db")).unwrap();
        put_dispatch(&db, "/wt", "session-requested", "older-stage");
        put_dispatch(&db, "/wt/other", "session-requested", "wrong-stage");

        let mut event = event_for_dispatch("/wt", Some("session-requested"));
        enrich_event(&db, &mut event);
        assert_eq!(event.agent_role, None);
    }

    #[test]
    fn lead_rules_merge_after_config_and_never_escalate() {
        use thegn_core::config_automations::{AutomationActionConfig, AutomationRuleConfig};
        use thegn_core::store::AutomationRuleStore;
        let rule = |name: &str, cap: &str, field: &str| {
            let mut then = AutomationActionConfig {
                cap: cap.into(),
                ..AutomationActionConfig::default()
            };
            match field {
                "body" => then.body = Some("done".into()),
                "name" => then.name = Some("deploy".into()),
                _ => {}
            }
            AutomationRuleConfig {
                name: name.into(),
                when: "agent_finished".into(),
                then,
                ..AutomationRuleConfig::default()
            }
        };
        let config = vec![
            thegn_core::config_automations::compile_one(&rule("cfg", "tools.run", "name")).unwrap(),
        ];
        let db = thegn_core::db::Db::open_memory().unwrap();
        assert_eq!(with_lead_rules(&db, config.clone()), config);
        for (id, cap, field) in [
            ("lead:ok", "notify.push", "body"),
            ("lead:tool", "tools.run", "name"),
            ("lead:merge", "merge.add", ""),
        ] {
            let body = serde_json::to_string(&rule(id, cap, field)).unwrap();
            db.put_lead_rule(id, &body, "lead", 1).unwrap();
        }
        let merged = with_lead_rules(&db, config.clone());
        let ids: Vec<&str> = merged.iter().map(|rule| rule.id.as_str()).collect();
        assert_eq!(ids, ["cfg", "lead:ok"]);
        db.set_lead_rule_enabled("lead:ok", false).unwrap();
        assert_eq!(with_lead_rules(&db, config.clone()), config);
    }

    fn notify_rule(id: &str) -> thegn_core::automation::AutomationRule {
        thegn_core::automation::AutomationRule {
            id: id.into(),
            enabled: true,
            event: thegn_core::automation::AutomationEventKind::AgentFinished,
            predicate: thegn_core::automation::AutomationPredicate::default(),
            action: thegn_core::automation::ActionTemplate {
                cap: "notify.push".into(),
                params: BTreeMap::from([("body".into(), "done".into())]),
            },
            debounce_secs: 0,
            idle_secs: None,
            once_per_key: true,
            max_per_hour: 30,
            max_action_per_hour: 30,
            schedule: None,
            autonomy: thegn_core::automation_autonomy::Autonomy::Review,
        }
    }

    #[test]
    fn a_paused_runtime_admits_nothing_and_audits_paused() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        let rules = vec![notify_rule("r")];
        let event = event_for_dispatch("/wt", None);
        thegn_core::automation_control::set_paused(&db, true).unwrap();
        let admitted = admit_unless_paused(&db, &rules, &event, 100).unwrap();
        assert!(admitted.is_empty(), "{admitted:?}");
        let rows = db.automation_runs(Some("r"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].outcome, "skipped");
        assert_eq!(rows[0].skip_reason.as_deref(), Some("paused"));

        thegn_core::automation_control::set_paused(&db, false).unwrap();
        let admitted = admit_unless_paused(&db, &rules, &event, 101).unwrap();
        assert!(matches!(
            admitted.as_slice(),
            [AutomationAdmission::Planned { .. }]
        ));
    }

    #[test]
    fn live_session_tracking_is_per_rule() {
        let origin = |rule: &str| thegn_core::automation::AutomationOrigin {
            root_event_id: "root".into(),
            rule_id: rule.into(),
            run_id: "1".into(),
        };
        register_session_origin("live-test-session", origin("live-test-rule"));
        assert!(rule_has_live_session("live-test-rule"));
        assert!(!rule_has_live_session("live-test-other-rule"));
        clear_session_origin("live-test-session");
        assert!(!rule_has_live_session("live-test-rule"));
    }

    #[test]
    fn runtime_gate_arms_for_a_configured_lead_without_config_rules() {
        assert!(thegn_core::automation_authoring::runtime_armed(
            true, 0, true
        ));
        assert!(!thegn_core::automation_authoring::runtime_armed(
            true, 0, false
        ));
        assert!(!thegn_core::automation_authoring::runtime_armed(
            false, 0, true
        ));
    }

    #[test]
    fn bounded_queue_overflow_writes_a_durable_dropped_audit() {
        let _state = crate::testenv::IsolatedState::new();
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
            issue: None,
            exit_code: None,
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
            issue: None,
            exit_code: None,
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
