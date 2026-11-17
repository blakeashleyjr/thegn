//! Lead-authored automation rules: `automations.put` / `rm` / `enable` /
//! `runs` (THE-2181), plus the two hooks `automations.list` and
//! `automations.test` call to see Lead rules beside config rules.
//!
//! The escalation boundary: these verbs are Write-scoped (`runs` is Read), so a
//! rule stored here may only target the Lead allowlist
//! (`thegn_core::automation_authoring`). Config rules stay read-only: `rm` and
//! `enable` on one answer `Conflict`.
//!
//! A `pipeline.start` asking for `review` or `merge` is stored disarmed and
//! raises an arming approval (b155 6c, THE-2204); the Lead can ask but never
//! answer (`approvals.resolve` is Admin and never MCP). Editing or removing
//! the rule revokes any earlier arming approval.

use std::collections::BTreeSet;

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use thegn_core::approval_digest::{ARM_STAGE, RULE_PREFIX, rule_arm_ids};
use thegn_core::approval_request::ApprovalRequest;
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
    // b155 (6c): the triple an owner approves to arm this exact body.
    let arming = authoring::needs_arming(&rule)
        .map(|level| (level, rule_arm_ids(&compiled.id, &body, level)));
    let arm_sha = arming.as_ref().map(|(_, (_, _, sha))| sha.clone());
    let outcome = svc
        .with_db(move |db| {
            let previous = db
                .lead_rules()?
                .into_iter()
                .find(|row| row.id == id)
                .map(|row| row.body_json);
            let created = match db.put_lead_rule(&id, &body, LEAD_AUTHOR, thegn_core::util::now()) {
                Ok(created) => created,
                Err(error) if error.downcast_ref::<LeadRuleLimitReached>().is_some() => {
                    return Ok(Err(LeadRuleLimitReached));
                }
                Err(error) => return Err(error),
            };
            db.set_lead_rule_enabled(&id, enabled)?;
            if previous.is_some_and(|previous| previous != body) {
                // An edit disarms: an approval of an earlier body never
                // comes back if the body is changed back.
                revoke_arming(db, &id)?;
            }
            let armed = arm_sha.is_some_and(|sha| {
                crate::automation_arming::armed_set(db, thegn_core::util::now_ms())
                    .contains(&(id.clone(), sha))
            });
            Ok(Ok((created, armed)))
        })
        .await?;
    let (created, already_armed) =
        outcome.map_err(|limit| ControlError::ResourceExhausted(limit.to_string()))?;
    // A stored rule may add or change a schedule (THE-2193).
    crate::automation_schedule::rearm();
    let (automations_enabled, armed) = runtime_state(svc);
    let awaiting = arming.is_some() && !already_armed;
    let inert_reason = if awaiting {
        Some(authoring::awaiting_arming_reason(&compiled.id))
    } else {
        authoring::lead_inert_reason(automations_enabled, armed, enabled, Some(compiled.event))
    };
    if let Some((level, (issue_id, stage, commit_sha))) = arming.filter(|_| awaiting) {
        let request = ApprovalRequest {
            issue_id,
            stage,
            commit_sha,
            worktree: None,
            reason: Some(format!(
                "Lead rule {} asks for autonomy {}",
                compiled.id,
                level.as_str()
            )),
        };
        if let Err(error) = super::approvals::raise(svc, request).await {
            tracing::warn!(
                target: "thegn::automation",
                rule = %compiled.id,
                %error,
                "failed to raise the arming approval; the rule stays disarmed (re-put to retry)"
            );
        }
    }
    let mut reply = encode(&LeadRulePutReply {
        id: compiled.id,
        created,
        inert_reason,
    })?;
    if awaiting && let Some(object) = reply.as_object_mut() {
        object.insert("awaiting_approval".into(), serde_json::Value::Bool(true));
    }
    Ok(reply)
}

/// Withdraw every arming approval for the Lead rule `id`.
fn revoke_arming(db: &thegn_core::db::Db, id: &str) -> anyhow::Result<()> {
    db.revoke_approvals(
        &format!("{RULE_PREFIX}{id}"),
        ARM_STAGE,
        thegn_core::util::now_ms(),
    )?;
    Ok(())
}

/// The `(rule id, digest)` pairs armed right now.
fn armed_now(db: &thegn_core::db::Db) -> BTreeSet<(String, String)> {
    crate::automation_arming::armed_set(db, thegn_core::util::now_ms())
}

/// Answer `automations.rm`; config rules answer `Conflict`, unknown names
/// `NotFound`.
pub(crate) async fn rm(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let LeadRuleRm { name } = decode(req)?;
    let id = lead_target(svc, &name)?;
    let target = id.clone();
    let removed = svc
        .with_db(move |db| {
            let removed = db.delete_lead_rule(&target)?;
            if removed {
                // A rule re-created under the same id asks again.
                revoke_arming(db, &target)?;
            }
            Ok(removed)
        })
        .await?;
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
        let armed_rules = armed_now(db);
        Ok(rows
            .into_iter()
            .map(|row| {
                let recent_outcome = db
                    .automation_runs(Some(&row.id), 1)
                    .ok()
                    .and_then(|runs| runs.into_iter().next())
                    .map(|run| run.outcome);
                match authoring::compile_lead_row_armed(&row, &names, &armed_rules) {
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
                        let inert_reason = if authoring::is_awaiting_arming(&errors) {
                            errors.join("; ")
                        } else {
                            format!("invalid: {}", errors.join("; "))
                        };
                        AutomationRuleInfo {
                            name: row.id,
                            enabled: row.enabled,
                            trusted_layer: "lead".into(),
                            event,
                            action,
                            inert_reason: Some(inert_reason),
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
/// and so still passes the action allowlist, and is armed when it asks to
/// be — is returned.
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
            .and_then(|row| authoring::compile_lead_row_armed(&row, &names, &armed_now(db)).ok()))
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
        for (cap, then) in [
            ("tools.run", json!({"cap": "tools.run", "name": "deploy"})),
            ("merge.add", json!({"cap": "merge.add", "name": "deploy"})),
            ("merge.drain", json!({"cap": "merge.drain"})),
        ] {
            let mut req = ping(cap);
            req["rule"]["then"] = then;
            match put(&svc, req).await {
                Err(ControlError::InvalidArgument(message)) => {
                    assert!(
                        message.contains(authoring::LEAD_ESCALATION_REFUSAL),
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

    fn start(autonomy: Option<&str>) -> serde_json::Value {
        let mut req = json!({"rule": {
            "name": "start",
            "when": "issue_matched",
            "if": {"label": "auto"},
            "then": {"cap": "pipeline.start", "stage": "code", "agent": "codex"}
        }});
        if let Some(level) = autonomy {
            req["rule"]["autonomy"] = json!(level);
        }
        req
    }

    #[tokio::test]
    async fn lead_autonomy_ceiling_is_enforced_at_put() {
        let (svc, _profile) = service(true);
        let mut merge = ping("notify.push");
        merge["rule"]["autonomy"] = json!("merge");
        match put(&svc, merge).await {
            Err(ControlError::InvalidArgument(message)) => assert!(
                message.contains(thegn_core::automation_autonomy::LEAD_MERGE_REFUSAL),
                "{message}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(list_lead_rules(&svc).await.unwrap().is_empty());

        let reply = put(&svc, start(Some("propose"))).await.unwrap();
        assert_eq!(reply["id"], json!("lead:start"));
        assert_eq!(reply.get("awaiting_approval"), None);
        let found = find_rule(&svc, "start").await.unwrap().unwrap();
        assert_eq!(
            found.autonomy,
            thegn_core::automation_autonomy::Autonomy::Propose
        );
        // A propose rule raises no arming request.
        let pending = svc
            .with_db(|db| {
                crate::daemon::lead_caps::approvals::pending_with_ttl(
                    db,
                    thegn_core::util::now_ms(),
                    0,
                    50,
                )
            })
            .await
            .unwrap();
        assert!(pending.is_empty());
    }

    fn grant(svc: &DaemonService, issue: &str, stage: &str, sha: &str) {
        let db = svc.db.lock().unwrap();
        db.grant_approval(&thegn_core::pipeline_approval::Approval {
            issue_id: issue.into(),
            stage: stage.into(),
            commit_sha: sha.into(),
            approver: "owner".into(),
            note: None,
            granted_at_ms: thegn_core::util::now_ms(),
            expires_at_ms: None,
            revoked_at_ms: None,
        })
        .unwrap();
    }

    async fn pending_arms(svc: &DaemonService) -> Vec<(String, String, String)> {
        svc.with_db(|db| {
            crate::daemon::lead_caps::approvals::pending_with_ttl(
                db,
                thegn_core::util::now_ms(),
                0,
                50,
            )
        })
        .await
        .unwrap()
        .into_iter()
        .filter(|p| p.stage == "arm")
        .map(|p| (p.issue_id, p.stage, p.commit_sha))
        .collect()
    }

    #[tokio::test]
    async fn an_escalating_start_is_stored_disarmed_and_armed_by_the_owner() {
        use thegn_core::automation_autonomy::Autonomy;
        let (svc, _profile) = service(true);
        for (level, expected) in [
            (Some("review"), Autonomy::Review),
            (None, Autonomy::Review),
            (Some("merge"), Autonomy::Merge),
        ] {
            let reply = put(&svc, start(level)).await.unwrap();
            assert_eq!(reply["awaiting_approval"], json!(true), "{level:?}");
            assert_eq!(
                reply["inert_reason"],
                json!("awaiting owner approval (rule:lead:start arm)")
            );
            // Stored, inert, and listed as awaiting rather than invalid.
            assert!(find_rule(&svc, "start").await.unwrap().is_none());
            let listed = list_lead_rules(&svc).await.unwrap();
            assert_eq!(
                listed[0].inert_reason.as_deref(),
                Some("awaiting owner approval (rule:lead:start arm)")
            );
            // A pending arming request for exactly the stored body (earlier
            // rounds' requests may also still be unread).
            let body = svc
                .with_db(|db| Ok(db.lead_rules()?.remove(0).body_json))
                .await
                .unwrap();
            let (issue, stage, sha) = rule_arm_ids("lead:start", &body, expected);
            assert_eq!((issue.as_str(), stage.as_str()), ("rule:lead:start", "arm"));
            let pending = pending_arms(&svc).await;
            assert!(
                pending.contains(&(issue.clone(), stage.clone(), sha.clone())),
                "{level:?}: {pending:?}"
            );

            // The owner approves: armed at the requested level.
            grant(&svc, &issue, &stage, &sha);
            let found = find_rule(&svc, "start").await.unwrap().unwrap();
            assert_eq!(found.autonomy, expected, "{level:?}");
            assert_eq!(list_lead_rules(&svc).await.unwrap()[0].inert_reason, None);
            // Re-putting the identical body keeps it armed and asks nothing.
            let reply = put(&svc, start(level)).await.unwrap();
            assert_eq!(reply.get("awaiting_approval"), None, "{level:?}");
            assert!(find_rule(&svc, "start").await.unwrap().is_some());
            // Removing it revokes the arming, so the next put asks again.
            rm(&svc, json!({"name": "start"})).await.unwrap();
        }
    }

    #[tokio::test]
    async fn editing_an_armed_rule_disarms_it_for_good() {
        let (svc, _profile) = service(true);
        put(&svc, start(Some("review"))).await.unwrap();
        let (issue, stage, sha) = pending_arms(&svc).await.remove(0);
        grant(&svc, &issue, &stage, &sha);
        assert!(find_rule(&svc, "start").await.unwrap().is_some());

        let mut edited = start(Some("review"));
        edited["rule"]["then"]["stage"] = json!("deploy");
        let reply = put(&svc, edited).await.unwrap();
        assert_eq!(reply["awaiting_approval"], json!(true));
        assert!(find_rule(&svc, "start").await.unwrap().is_none());
        // Changing it back does not resurrect the old approval: it was revoked.
        let reply = put(&svc, start(Some("review"))).await.unwrap();
        assert_eq!(reply["awaiting_approval"], json!(true));
        assert!(find_rule(&svc, "start").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_smuggled_merge_row_loads_capped() {
        let (svc, _profile) = service(true);
        for (id, body) in [
            (
                "lead:loud",
                json!({
                    "name": "lead:loud",
                    "when": "agent_finished",
                    "autonomy": "merge",
                    "then": {"cap": "notify.push", "body": "x"}
                }),
            ),
            (
                "lead:start",
                json!({
                    "name": "lead:start",
                    "when": "issue_matched",
                    "autonomy": "merge",
                    "then": {"cap": "pipeline.start", "stage": "code", "agent": "codex"}
                }),
            ),
        ] {
            let body = body.to_string();
            svc.with_db(move |db| db.put_lead_rule(id, &body, "lead", 1))
                .await
                .unwrap();
        }
        let loud = find_rule(&svc, "loud").await.unwrap().unwrap();
        assert_eq!(
            loud.autonomy,
            thegn_core::automation_autonomy::Autonomy::Review
        );
        // A hand-written escalation has no arming approval: inert.
        assert!(find_rule(&svc, "start").await.unwrap().is_none());
    }
}
