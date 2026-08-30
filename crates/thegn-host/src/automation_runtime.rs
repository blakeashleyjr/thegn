//! Bounded off-loop automation evaluator and executor.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use thegn_core::automation::{AutomationEvent, EvaluationDecision, EvaluationState, RuleState};
use thegn_core::config::Config;
use thegn_core::store::{AutomationStateRow, AutomationStore, NewAutomationRun};
use tokio::sync::mpsc;

static SENDER: OnceLock<Mutex<Option<mpsc::Sender<AutomationEvent>>>> = OnceLock::new();
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
    if let Err(error) = tx.try_send(event) {
        tracing::warn!(
            target: "thegn::automation",
            outcome = "dropped",
            reason = %error,
            "automation event queue full or closed"
        );
    }
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
        });
    }
}

async fn process_event(
    cfg: Arc<Config>,
    rules: Vec<thegn_core::automation::AutomationRule>,
    settings: thegn_core::config_automations::AutomationsConfig,
    event: AutomationEvent,
) -> anyhow::Result<()> {
    let rule_ids: Vec<String> = rules.iter().map(|rule| rule.id.clone()).collect();
    let event_for_evaluation = event.clone();
    let (decisions, now) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = thegn_core::db::Db::open()?;
        let mut state = EvaluationState::new();
        for rule_id in rule_ids {
            if let Some(row) = db.automation_state(&rule_id)? {
                state.insert(rule_id, row_to_state(row));
            }
        }
        let now = thegn_core::util::now();
        Ok((
            thegn_core::automation::evaluate(&rules, &event_for_evaluation, &state, now),
            now,
        ))
    })
    .await??;

    for decision in decisions {
        match decision {
            EvaluationDecision::Skipped {
                rule_id,
                event_key,
                reason,
            } => {
                let event_clone = event.clone();
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let db = thegn_core::db::Db::open()?;
                    let id = db.start_automation_run(&NewAutomationRun {
                        rule_id: rule_id.clone(),
                        event_id: event_clone.id,
                        event_key: event_key.0,
                        trigger_kind: event_clone.kind.as_str().into(),
                        event_summary: bounded(event_clone.message.as_deref().unwrap_or_default()),
                        action_cap: String::new(),
                        action_summary: String::new(),
                        started_at: now,
                    })?;
                    let reason = format!("{reason:?}").to_ascii_lowercase();
                    db.finish_automation_run(id, "skipped", Some(&reason), None, now)?;
                    Ok(())
                })
                .await??;
            }
            EvaluationDecision::Planned(action) => {
                let action_for_store = action.clone();
                let event_for_store = event.clone();
                let run_id = tokio::task::spawn_blocking(move || -> anyhow::Result<i64> {
                    let db = thegn_core::db::Db::open()?;
                    db.put_automation_state(&AutomationStateRow {
                        rule_id: action_for_store.transition.rule_id.clone(),
                        enabled_override: action_for_store.transition.state.enabled_override,
                        last_fired_at: action_for_store.transition.state.last_fired_at,
                        recent_fires: action_for_store.transition.state.recent_fires.clone(),
                        action_fires: action_for_store.transition.state.action_fires.clone(),
                        once_keys: action_for_store.transition.state.once_keys.clone(),
                        updated_at: now,
                    })?;
                    db.start_automation_run(&NewAutomationRun {
                        rule_id: action_for_store.rule_id.clone(),
                        event_id: event_for_store.id,
                        event_key: action_for_store.event_key.0.clone(),
                        trigger_kind: event_for_store.kind.as_str().into(),
                        event_summary: bounded(
                            event_for_store.message.as_deref().unwrap_or_default(),
                        ),
                        action_cap: action_for_store.cap.clone(),
                        action_summary: bounded(&format!(
                            "{} {:?}",
                            action_for_store.cap,
                            action_for_store.params.keys()
                        )),
                        started_at: now,
                    })
                })
                .await??;
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
                    Duration::from_secs(settings.action_timeout_secs),
                    crate::automation_executor::execute(
                        Arc::clone(&cfg),
                        event.clone(),
                        action.clone(),
                        run_id,
                    ),
                )
                .await;
                let (outcome, error) = match result {
                    Ok(Ok(())) => ("succeeded", None),
                    Ok(Err(error)) => ("failed", Some(bounded(&format!("{error:#}")))),
                    Err(_) => (
                        "timed_out",
                        Some(format!("deadline {}s", settings.action_timeout_secs)),
                    ),
                };
                let error_for_store = error.clone();
                tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                    let db = thegn_core::db::Db::open()?;
                    db.finish_automation_run(
                        run_id,
                        outcome,
                        None,
                        error_for_store.as_deref(),
                        thegn_core::util::now(),
                    )?;
                    let _ = db.prune_automation_runs(settings.audit_retention_per_rule);
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
                    let db = tokio::task::spawn_blocking(thegn_core::db::Db::open).await??;
                    crate::automation_events::emit_with_facts(
                        &db,
                        "automation_failed",
                        &format!("automation:{}", action.rule_id),
                        &bounded(&message),
                        event.worktree.as_deref().unwrap_or_default(),
                        crate::automation_events::EventFacts {
                            origin: Some(thegn_core::automation::AutomationOrigin {
                                root_event_id: event
                                    .origin
                                    .as_ref()
                                    .map_or_else(|| event.id.clone(), |o| o.root_event_id.clone()),
                                rule_id: action.rule_id,
                                run_id: run_id.to_string(),
                            }),
                            ..Default::default()
                        },
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn row_to_state(row: AutomationStateRow) -> RuleState {
    RuleState {
        enabled_override: row.enabled_override,
        last_fired_at: row.last_fired_at,
        recent_fires: row.recent_fires,
        action_fires: row.action_fires,
        once_keys: row.once_keys,
    }
}

fn bounded(value: &str) -> String {
    value.chars().take(512).collect()
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

    let idle_after = cfg
        .effective_automations()
        .compiled_rules()
        .unwrap_or_default()
        .into_iter()
        .filter(|rule| rule.enabled && rule.event == AutomationEventKind::WorktreeIdle)
        .map(|rule| rule.debounce_secs.max(60))
        .min();
    tokio::spawn(async move {
        let mut sessions = BTreeMap::<String, (Option<String>, bool, String)>::new();
        let mut idle_deadlines = BTreeMap::<
            String,
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
                            if let (Some(after), Some(worktree)) = (idle_after, activity.worktree) {
                                let elapsed_ms = thegn_core::util::now()
                                    .saturating_mul(1_000)
                                    .saturating_sub(activity.since_ms)
                                    .max(0) as u64;
                                let remaining_ms = after.saturating_mul(1_000).saturating_sub(elapsed_ms);
                                idle_deadlines.insert(worktree, (tokio::time::Instant::now() + Duration::from_millis(remaining_ms), origin));
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
                    let due: Vec<String> = idle_deadlines.iter().filter(|(_, (deadline, _))| *deadline <= now).map(|(worktree, _)| worktree.clone()).collect();
                    for worktree in due {
                        let origin = idle_deadlines.remove(&worktree).and_then(|(_, origin)| origin);
                        crate::automation_events::submit_fact(AutomationEventKind::WorktreeIdle, format!("worktree:{worktree}:idle"), Some(worktree), None, crate::automation_events::EventFacts { origin, ..Default::default() });
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_are_bounded() {
        assert_eq!(bounded(&"x".repeat(600)).len(), 512);
    }
}
