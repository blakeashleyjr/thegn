//! Lead rule arming (b155 6c, THE-2204): a Lead rule asking above its cap is
//! stored disarmed and raises an arming approval; it compiles as
//! `RuleOrigin::LeadApproved` only while a live arming approval matches the
//! current body digest (30-day expiry).
//!
//! Nothing here writes an approval: the owner grants it through
//! `approvals.resolve` (Admin, HTTP + CLI, never MCP) or a phone's
//! `approvals.redeem` (refused for Lead-confined callers). Arming is read at
//! every rule load ([`armed_set`]), so approving, expiring, revoking or
//! editing the rule takes effect on the next evaluation.

use std::collections::BTreeSet;
use std::sync::Arc;

use thegn_core::approval_digest::rule_arm_ids;
use thegn_core::automation_authoring::{ARMING_MAX_AGE_SECS, needs_arming};
use thegn_core::config_automations::AutomationRuleConfig;
use thegn_core::db::Db;
use thegn_core::pipeline_approval::state_for;
use thegn_core::store::{AutomationRuleStore, LeadRuleRow};

use crate::daemon::service::DaemonService;

/// The stored body of `row` and the autonomy it asks to be armed at, when it
/// asks for one.
fn requested(
    row: &LeadRuleRow,
) -> Option<(
    AutomationRuleConfig,
    thegn_core::automation_autonomy::Autonomy,
)> {
    let body: AutomationRuleConfig = serde_json::from_str(&row.body_json).ok()?;
    let level = needs_arming(&body)?;
    Some((body, level))
}

/// Whether `row` has a live arming approval for its current body at `now_ms`;
/// `Some(digest)` when it does.
fn live_digest(db: &Db, row: &LeadRuleRow, now_ms: i64) -> Option<String> {
    let (_, level) = requested(row)?;
    let (issue, stage, sha) = rule_arm_ids(&row.id, &row.body_json, level);
    let latest = match db.latest_approval(&issue, &stage) {
        Ok(latest) => latest,
        Err(error) => {
            tracing::warn!(
                target: "thegn::automation",
                rule = %row.id,
                %error,
                "failed to read a rule arming approval; the rule stays disarmed"
            );
            return None;
        }
    };
    if !latest
        .as_ref()
        .is_some_and(|a| a.commit_sha.trim().eq_ignore_ascii_case(&sha))
    {
        return None;
    }
    state_for(latest.as_ref(), &sha, now_ms, ARMING_MAX_AGE_SECS)
        .is_live()
        .then_some(sha)
}

/// Re-check the rule against the approved digest and arm it. Arming is read
/// at load, so this only logs whether the approval matches the stored body
/// and nudges the schedule producer to re-read the rule set.
pub(crate) async fn on_approved(
    svc: &Arc<DaemonService>,
    rule_id: &str,
    sha: &str,
) -> anyhow::Result<()> {
    let id = rule_id.to_string();
    let approved = sha.to_string();
    let current = svc
        .with_db(move |db| {
            Ok(db
                .lead_rules()?
                .into_iter()
                .find(|row| row.id == id)
                .and_then(|row| requested(&row).map(|(_, level)| (row, level)))
                .map(|(row, level)| rule_arm_ids(&row.id, &row.body_json, level).2))
        })
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    match current {
        Some(digest) if digest == approved => tracing::info!(
            target: "thegn::automation",
            rule = %rule_id,
            outcome = "armed",
            "Lead rule armed by the owner"
        ),
        Some(_) => tracing::warn!(
            target: "thegn::automation",
            rule = %rule_id,
            outcome = "stale",
            "arming approval does not match the stored rule (it was edited); the rule stays disarmed"
        ),
        None => tracing::warn!(
            target: "thegn::automation",
            rule = %rule_id,
            outcome = "unknown",
            "arming approval names no stored rule asking to be armed; nothing armed"
        ),
    }
    crate::automation_schedule::rearm();
    Ok(())
}

/// `(rule id, body digest)` pairs with a live arming approval: for each
/// stored Lead rule that asks to be armed, the latest `(rule:<id>, arm)`
/// approval is Live for the digest of its CURRENT body (granted at most
/// [`ARMING_MAX_AGE_SECS`] ago, not revoked). An edited body has a new digest
/// and so is not in the set. A DB error arms nothing.
pub(crate) fn armed_set(db: &Db, now_ms: i64) -> BTreeSet<(String, String)> {
    let rows = match db.lead_rules() {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "thegn::automation",
                %error,
                "failed to read Lead rules for arming; nothing is armed"
            );
            return BTreeSet::new();
        }
    };
    rows.iter()
        .filter_map(|row| live_digest(db, row, now_ms).map(|sha| (row.id.clone(), sha)))
        .collect()
}

/// The trusted description of an arming approval, from the stored rule:
/// `Arm Lead rule <id>: when <kind> if <structural summary> then
/// pipeline.start <stage> at <autonomy>`. `None` when no stored rule asking
/// to be armed has that digest (a forged or stale triple).
pub(crate) fn describe(db: &Db, rule_id: &str, sha: &str) -> Option<String> {
    let row = db
        .lead_rules()
        .ok()?
        .into_iter()
        .find(|row| row.id == rule_id)?;
    let (body, level) = requested(&row)?;
    (rule_arm_ids(&row.id, &row.body_json, level).2 == sha)
        .then(|| describe_body(&row.id, &body, level))
}

/// The description text for a stored rule body.
fn describe_body(
    rule_id: &str,
    body: &AutomationRuleConfig,
    level: thegn_core::automation_autonomy::Autonomy,
) -> String {
    let summary = predicate_summary(&body.predicate);
    let stage = body.then.stage.as_deref().unwrap_or("?");
    format!(
        "Arm Lead rule {rule_id}: when {} if {summary} then pipeline.start {} at {}",
        clean(&body.when, 64),
        clean(stage, 64),
        level.as_str()
    )
}

/// The set predicate fields as `key=value`, joined by ` and `; `always` when
/// none is set. Free text is flagged rather than shown.
fn predicate_summary(p: &thegn_core::automation::AutomationPredicate) -> String {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(p) else {
        return "?".into();
    };
    let mut parts: Vec<String> = fields
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| {
            if thegn_core::automation_autonomy::FREE_TEXT_PREDICATES.contains(&key.as_str()) {
                format!("{key}=<free text>")
            } else {
                let shown = match value {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                format!("{key}={}", clean(&shown, 64))
            }
        })
        .collect();
    parts.sort();
    if parts.is_empty() {
        "always".into()
    } else {
        parts.join(" and ")
    }
}

/// At most `max` characters of `s`, control characters replaced by spaces.
fn clean(s: &str, max: usize) -> String {
    s.chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::automation_autonomy::Autonomy;
    use thegn_core::pipeline_approval::Approval;

    const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

    fn body(level: &str, stage: &str) -> String {
        serde_json::json!({
            "name": "lead:start",
            "when": "issue_matched",
            "autonomy": level,
            "if": {"label": "auto", "project": "core"},
            "then": {"cap": "pipeline.start", "stage": stage, "agent": "codex"}
        })
        .to_string()
    }

    fn store(db: &Db, id: &str, body: &str) {
        db.put_lead_rule(id, body, "lead", 1).unwrap();
    }

    fn approve(db: &Db, id: &str, body: &str, level: Autonomy, at_ms: i64) -> String {
        let (issue, stage, sha) = rule_arm_ids(id, body, level);
        db.grant_approval(&Approval {
            issue_id: issue,
            stage,
            commit_sha: sha.clone(),
            approver: "owner".into(),
            note: None,
            granted_at_ms: at_ms,
            expires_at_ms: None,
            revoked_at_ms: None,
        })
        .unwrap();
        sha
    }

    #[test]
    fn armed_set_is_live_approvals_for_the_current_body() {
        let db = Db::open_memory().unwrap();
        let now = 100 * DAY_MS;
        let review = body("review", "code");
        store(&db, "lead:start", &review);
        // Nothing approved yet.
        assert!(armed_set(&db, now).is_empty());
        let sha = approve(&db, "lead:start", &review, Autonomy::Review, now - DAY_MS);
        assert_eq!(
            armed_set(&db, now),
            BTreeSet::from([("lead:start".to_string(), sha)])
        );
        // Editing the body disarms it.
        store(&db, "lead:start", &body("review", "deploy"));
        assert!(armed_set(&db, now).is_empty());
    }

    #[test]
    fn armed_set_ignores_expired_revoked_and_other_levels() {
        let db = Db::open_memory().unwrap();
        let now = 100 * DAY_MS;
        let merge = body("merge", "code");
        store(&db, "lead:start", &merge);
        // Expired: granted 31 days ago.
        approve(
            &db,
            "lead:start",
            &merge,
            Autonomy::Merge,
            now - 31 * DAY_MS,
        );
        assert!(armed_set(&db, now).is_empty());
        // Approved for another level: a different digest.
        approve(&db, "lead:start", &merge, Autonomy::Review, now - DAY_MS);
        assert!(armed_set(&db, now).is_empty());
        // Live, then revoked.
        approve(&db, "lead:start", &merge, Autonomy::Merge, now);
        assert_eq!(armed_set(&db, now).len(), 1);
        db.revoke_approvals("rule:lead:start", "arm", now).unwrap();
        assert!(armed_set(&db, now + 1).is_empty());
        // A propose rule never asks, so even an approval does nothing.
        let propose = body("propose", "code");
        store(&db, "lead:quiet", &propose);
        approve(&db, "lead:quiet", &propose, Autonomy::Review, now);
        assert!(armed_set(&db, now).is_empty());
    }

    #[test]
    fn describe_is_built_from_the_stored_rule() {
        let db = Db::open_memory().unwrap();
        let merge = body("merge", "code");
        store(&db, "lead:start", &merge);
        let (_, _, sha) = rule_arm_ids("lead:start", &merge, Autonomy::Merge);
        assert_eq!(
            describe(&db, "lead:start", &sha).as_deref(),
            Some(
                "Arm Lead rule lead:start: when issue_matched if label=auto and project=core \
                 then pipeline.start code at merge"
            )
        );
        // A forged or stale digest, or an unknown rule, has no description.
        assert_eq!(describe(&db, "lead:start", &"0".repeat(40)), None);
        assert_eq!(describe(&db, "lead:nope", &sha), None);
    }

    #[test]
    fn predicate_summary_flags_free_text_and_strips_control_characters() {
        let p = thegn_core::automation::AutomationPredicate {
            message_regex: Some("ship it".into()),
            branch: Some("tg/\nx".into()),
            ..Default::default()
        };
        assert_eq!(
            predicate_summary(&p),
            "branch=tg/ x and message_regex=<free text>"
        );
        assert_eq!(predicate_summary(&Default::default()), "always");
    }
}
