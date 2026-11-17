//! Lead-authored automation rules: `automations.put` / `rm` / `enable` /
//! `runs` (THE-2181), plus the two hooks `automations.list` and
//! `automations.test` call to see Lead rules beside config rules.
//!
//! The escalation boundary: these verbs are Write-scoped (`runs` is Read), so a
//! rule stored here may only target `notify.push` or `sessions.open`
//! (`thegn_core::automation_authoring`). Config rules stay read-only: `rm` and
//! `enable` on one answer `Conflict`.

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use thegn_core::automation_authoring::{
    self as authoring, AutomationRunsQuery, AutomationRunsReply, LEAD_AUTHOR, LeadRuleEnable,
    LeadRuleEnableReply, LeadRulePut, LeadRulePutReply, LeadRuleRm, LeadRuleRmReply,
};
use thegn_core::store::{AutomationRuleStore, AutomationStore, LeadRuleLimitReached};
use thegn_svc::control::{AutomationRuleInfo, ControlError, ControlResult};

/// The trusted config's rule names, as written. Read from the raw rule list
/// so a config that fails validation still reserves its names.
fn config_names(svc: &DaemonService) -> Vec<String> {
    svc.config
        .effective_automations()
        .rules
        .iter()
        .map(|rule| rule.name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

/// `(automations enabled, runtime armed)` for the daemon's config.
fn runtime_state(svc: &DaemonService) -> (bool, bool) {
    let effective = svc.config.effective_automations();
    let armed = authoring::runtime_armed(
        effective.enabled,
        effective.rules.len(),
        crate::lead::enabled(&svc.config),
    );
    (effective.enabled, armed)
}

/// `Conflict` when `name` is a config rule; otherwise its `lead:` id.
fn lead_target(svc: &DaemonService, name: &str) -> ControlResult<String> {
    let name = name.trim();
    if config_names(svc).iter().any(|config| config == name) {
        return Err(ControlError::Conflict("config rules are read-only".into()));
    }
    authoring::lead_rule_id(name).map_err(ControlError::InvalidArgument)
}

/// Answer `automations.put`: decode `{rule}`, validate, store.
pub(crate) async fn put(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let LeadRulePut { rule } = decode(req)?;
    let rule = authoring::decode_lead_rule(rule)
        .map_err(|errors| ControlError::InvalidArgument(errors.join("\n")))?;
    let compiled = authoring::validate_lead_rule(&rule, &config_names(svc))
        .map_err(|errors| ControlError::InvalidArgument(errors.join("\n")))?;
    let stored = authoring::normalize(&rule, &compiled.id);
    let body =
        serde_json::to_string(&stored).map_err(|error| ControlError::Internal(error.into()))?;
    let id = compiled.id.clone();
    let enabled = compiled.enabled;
    let outcome = svc
        .with_db(move |db| {
            let created = match db.put_lead_rule(&id, &body, LEAD_AUTHOR, thegn_core::util::now()) {
                Ok(created) => created,
                Err(error) if error.downcast_ref::<LeadRuleLimitReached>().is_some() => {
                    return Ok(Err(LeadRuleLimitReached));
                }
                Err(error) => return Err(error),
            };
            db.set_lead_rule_enabled(&id, enabled)?;
            Ok(Ok(created))
        })
        .await?;
    let created = outcome.map_err(|limit| ControlError::ResourceExhausted(limit.to_string()))?;
    // A stored rule may add or change a schedule (THE-2193).
    crate::automation_schedule::rearm();
    let (automations_enabled, armed) = runtime_state(svc);
    encode(&LeadRulePutReply {
        id: compiled.id,
        created,
        inert_reason: authoring::lead_inert_reason(
            automations_enabled,
            armed,
            enabled,
            Some(compiled.event),
        ),
    })
}

/// Answer `automations.rm`; config rules answer `Conflict`, unknown names
/// `NotFound`.
pub(crate) async fn rm(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let LeadRuleRm { name } = decode(req)?;
    let id = lead_target(svc, &name)?;
    let target = id.clone();
    let removed = svc.with_db(move |db| db.delete_lead_rule(&target)).await?;
    if !removed {
        return Err(ControlError::NotFound(format!("automation rule {name}")));
    }
    crate::automation_schedule::rearm();
    encode(&LeadRuleRmReply { id, removed })
}

/// Answer `automations.enable`; config rules answer `Conflict`, unknown names
/// `NotFound`.
pub(crate) async fn enable(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let LeadRuleEnable { name, enabled } = decode(req)?;
    let id = lead_target(svc, &name)?;
    let target = id.clone();
    let found = svc
        .with_db(move |db| db.set_lead_rule_enabled(&target, enabled))
        .await?;
    if !found {
        return Err(ControlError::NotFound(format!("automation rule {name}")));
    }
    crate::automation_schedule::rearm();
    encode(&LeadRuleEnableReply { id, enabled })
}

/// Answer `automations.runs`: newest first, at most 200 rows.
pub(crate) async fn runs(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let query: AutomationRunsQuery = decode(req)?;
    let rule = query.rule_id(&config_names(svc));
    let limit = query.bounded_limit();
    let runs = svc
        .with_db(move |db| db.automation_runs(rule.as_deref(), limit))
        .await?;
    encode(&AutomationRunsReply { runs })
}

/// The Lead-authored rules, appended after the config rows by
/// `automations.list`. A stored row that no longer validates is listed with
/// the reason it is inert rather than hidden.
pub(crate) async fn list_lead_rules(svc: &DaemonService) -> ControlResult<Vec<AutomationRuleInfo>> {
    let names = config_names(svc);
    let (automations_enabled, armed) = runtime_state(svc);
    svc.with_db(move |db| {
        let rows = db.lead_rules()?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let recent_outcome = db
                    .automation_runs(Some(&row.id), 1)
                    .ok()
                    .and_then(|runs| runs.into_iter().next())
                    .map(|run| run.outcome);
                match authoring::compile_lead_row(&row, &names) {
                    Ok(rule) => AutomationRuleInfo {
                        inert_reason: authoring::lead_inert_reason(
                            automations_enabled,
                            armed,
                            rule.enabled,
                            Some(rule.event),
                        ),
                        name: rule.id,
                        enabled: rule.enabled,
                        trusted_layer: "lead".into(),
                        event: rule.event.as_str().into(),
                        action: rule.action.cap,
                        recent_outcome,
                    },
                    Err(errors) => {
                        let (event, action) = authoring::stored_shape(&row.body_json);
                        AutomationRuleInfo {
                            name: row.id,
                            enabled: row.enabled,
                            trusted_layer: "lead".into(),
                            event,
                            action,
                            inert_reason: Some(format!("invalid: {}", errors.join("; "))),
                            recent_outcome,
                        }
                    }
                }
            })
            .collect())
    })
    .await
}

/// A Lead-authored rule by name (`ping` or `lead:ping`), the `automations.test`
/// fallback when no config rule matches. Only a rule that still validates —
/// and so still passes the action allowlist — is returned.
pub(crate) async fn find_rule(
    svc: &DaemonService,
    name: &str,
) -> ControlResult<Option<thegn_core::automation::AutomationRule>> {
    let Ok(id) = authoring::lead_rule_id(name) else {
        return Ok(None);
    };
    let names = config_names(svc);
    svc.with_db(move |db| {
        Ok(db
            .lead_rules()?
            .into_iter()
            .find(|row| row.id == id)
            .and_then(|row| authoring::compile_lead_row(&row, &names).ok()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::session::IdleTransition;
    use crate::daemon::tombstone::{MAX_TOMBSTONES, TOMBSTONE_TTL_MS};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use thegn_core::config_automations::{
        AutomationActionConfig, AutomationRuleConfig, AutomationsConfig,
    };
    use thegn_core::store::NewAutomationRun;
    use thegn_core::{db::Db, graveyard::Graveyard};
    use tokio::sync::{broadcast, mpsc};

    fn service(enabled: bool) -> (DaemonService, tempfile::TempDir) {
        let profile = tempfile::Builder::new()
            .prefix("lead-automations-")
            .tempdir()
            .expect("private profile tempdir");
        let config = thegn_core::config::Config {
            automations: AutomationsConfig {
                enabled,
                rules: vec![AutomationRuleConfig {
                    name: "cfg-rule".into(),
                    when: "agent_finished".into(),
                    then: AutomationActionConfig {
                        cap: "tools.run".into(),
                        name: Some("deploy".into()),
                        ..AutomationActionConfig::default()
                    },
                    ..AutomationRuleConfig::default()
                }],
                ..AutomationsConfig::default()
            },
            ..Default::default()
        };
        let (events, _rx) = broadcast::channel(16);
        let (idle_tx, _idle_rx) = mpsc::unbounded_channel::<IdleTransition>();
        let svc = DaemonService {
            daemon_id: "test".into(),
            sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            actor_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            accepting: AtomicBool::new(true),
            tombs: Arc::new(tokio::sync::Mutex::new(Graveyard::new(
                MAX_TOMBSTONES,
                TOMBSTONE_TTL_MS,
            ))),
            native_ids: Mutex::new(HashMap::new()),
            spawning_relaunches: Arc::new(Mutex::new(HashSet::new())),
            launches: Mutex::new(crate::daemon::stage_launch::LaunchRegistry::default()),
            events,
            db: Arc::new(Mutex::new(Db::open_memory().expect("db"))),
            grace_ms: 0,
            idle_tx,
            shutdown: tokio_util::sync::CancellationToken::new(),
            config: Arc::new(config),
            profile_root: profile.path().to_path_buf(),
            endpoint: "/tmp/test.sock".into(),
        };
        (svc, profile)
    }

    fn ping(cap: &str) -> serde_json::Value {
        json!({"rule": {
            "name": "ping",
            "when": "agent_finished",
            "then": {"cap": cap, "params": {"body": "done"}}
        }})
    }

    #[tokio::test]
    async fn put_list_test_enable_rm_roundtrip() {
        let (svc, _profile) = service(true);
        let reply = put(&svc, ping("notify.push")).await.unwrap();
        assert_eq!(reply, json!({"id": "lead:ping", "created": true}));
        let reply = put(&svc, ping("notify.push")).await.unwrap();
        assert_eq!(reply["created"], json!(false));

        let listed = list_lead_rules(&svc).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "lead:ping");
        assert_eq!(listed[0].trusted_layer, "lead");
        assert_eq!(listed[0].action, "notify.push");
        assert_eq!(listed[0].inert_reason, None);

        let found = find_rule(&svc, "ping").await.unwrap().unwrap();
        assert_eq!(found.id, "lead:ping");
        assert!(find_rule(&svc, "lead:ping").await.unwrap().is_some());
        assert!(find_rule(&svc, "nope").await.unwrap().is_none());
        assert!(find_rule(&svc, "Bad Name").await.unwrap().is_none());

        let reply = enable(&svc, json!({"name": "ping", "enabled": false}))
            .await
            .unwrap();
        assert_eq!(reply, json!({"id": "lead:ping", "enabled": false}));
        let listed = list_lead_rules(&svc).await.unwrap();
        assert!(!listed[0].enabled);
        assert_eq!(listed[0].inert_reason.as_deref(), Some("rule disabled"));

        let reply = rm(&svc, json!({"name": "lead:ping"})).await.unwrap();
        assert_eq!(reply, json!({"id": "lead:ping", "removed": true}));
        assert!(list_lead_rules(&svc).await.unwrap().is_empty());
        assert!(matches!(
            rm(&svc, json!({"name": "ping"})).await,
            Err(ControlError::NotFound(_))
        ));
        assert!(matches!(
            enable(&svc, json!({"name": "ping", "enabled": true})).await,
            Err(ControlError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn escalating_and_colliding_puts_are_refused() {
        let (svc, _profile) = service(true);
        for cap in ["tools.run", "merge.add"] {
            let mut req = ping(cap);
            req["rule"]["then"] = json!({"cap": cap, "name": "deploy"});
            match put(&svc, req).await {
                Err(ControlError::InvalidArgument(message)) => {
                    assert!(
                        message.contains("may only notify or open a session"),
                        "{message}"
                    )
                }
                other => panic!("{cap}: {other:?}"),
            }
        }
        let mut colliding = ping("notify.push");
        colliding["rule"]["name"] = json!("cfg-rule");
        assert!(matches!(
            put(&svc, colliding).await,
            Err(ControlError::InvalidArgument(_))
        ));
        assert!(list_lead_rules(&svc).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn config_rules_are_read_only() {
        let (svc, _profile) = service(true);
        for result in [
            rm(&svc, json!({"name": "cfg-rule"})).await,
            enable(&svc, json!({"name": "cfg-rule", "enabled": false})).await,
        ] {
            match result {
                Err(ControlError::Conflict(message)) => {
                    assert_eq!(message, "config rules are read-only")
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_smuggled_row_is_listed_invalid_and_never_found() {
        let (svc, _profile) = service(true);
        let body = json!({
            "name": "lead:sneaky",
            "when": "agent_finished",
            "then": {"cap": "tools.run", "name": "deploy"}
        })
        .to_string();
        svc.with_db(move |db| db.put_lead_rule("lead:sneaky", &body, "lead", 1))
            .await
            .unwrap();
        let listed = list_lead_rules(&svc).await.unwrap();
        assert_eq!(listed[0].action, "tools.run");
        assert!(
            listed[0]
                .inert_reason
                .as_deref()
                .unwrap()
                .starts_with("invalid:")
        );
        assert!(find_rule(&svc, "sneaky").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_reports_why_a_rule_is_inert() {
        let (svc, _profile) = service(false);
        let reply = put(&svc, ping("notify.push")).await.unwrap();
        assert_eq!(reply["inert_reason"], json!("automations disabled"));

        let (svc, _profile) = service(true);
        let idle = json!({"rule": {
            "name": "idle",
            "when": "worktree_idle",
            "idle_secs": 600,
            "then": {"cap": "notify.push", "body": "idle"}
        }});
        let reply = put(&svc, idle).await.unwrap();
        assert_eq!(reply["inert_reason"], json!(authoring::LEAD_IDLE_INERT));
    }

    #[tokio::test]
    async fn runs_are_bounded_and_resolve_lead_names() {
        let (svc, _profile) = service(true);
        svc.with_db(|db| {
            for index in 0..250 {
                let rule = if index % 2 == 0 {
                    "lead:ping"
                } else {
                    "cfg-rule"
                };
                db.start_automation_run(&NewAutomationRun {
                    rule_id: rule.into(),
                    event_id: format!("e{index}"),
                    started_at: index,
                    ..NewAutomationRun::default()
                })?;
            }
            Ok(())
        })
        .await
        .unwrap();
        let all = runs(&svc, json!({"limit": 10_000})).await.unwrap();
        assert_eq!(all["runs"].as_array().unwrap().len(), 200);
        let lead = runs(&svc, json!({"rule": "ping", "limit": 5}))
            .await
            .unwrap();
        let lead = lead["runs"].as_array().unwrap();
        assert_eq!(lead.len(), 5);
        assert!(lead.iter().all(|run| run["rule_id"] == "lead:ping"));
        let config = runs(&svc, json!({"rule": "cfg-rule"})).await.unwrap();
        assert!(
            config["runs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|run| run["rule_id"] == "cfg-rule")
        );
        assert!(matches!(
            runs(&svc, json!({"limit": "x"})).await,
            Err(ControlError::InvalidArgument(_))
        ));
    }
}
