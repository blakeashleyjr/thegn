//! `thegn automations` — inspect trusted rules and dry-run an event fixture.
//!
//! `test` is intentionally pure: it loads effective global/profile config,
//! deserializes a caller-supplied event, and invokes `thegn_core::automation::evaluate`
//! with empty ledger state. It never opens SQLite and never dispatches work.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;
use thegn_core::automation::{AutomationEvent, EvaluationDecision, EvaluationState, evaluate};
use thegn_core::config::Config;
use thegn_core::store::AutomationStore;

#[derive(Subcommand, Clone)]
pub enum Action {
    /// List trusted global/profile automation rules and recent outcomes.
    List {
        /// Emit one JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Purely evaluate one named rule against a JSON event fixture.
    Test {
        /// Stable rule name from `[[automations.rules]]`.
        rule: String,
        /// JSON event supplied inline (exclusive with `--fixture`).
        #[arg(long, conflicts_with = "fixture", required_unless_present = "fixture")]
        event: Option<String>,
        /// Path to a JSON event fixture (exclusive with `--event`).
        #[arg(long, conflicts_with = "event", required_unless_present = "event")]
        fixture: Option<PathBuf>,
        /// Evaluation clock in Unix seconds; defaults to the fixture timestamp.
        #[arg(long)]
        at: Option<i64>,
        /// Emit one JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
struct RuleRow {
    name: String,
    enabled: bool,
    trusted_layer: &'static str,
    event: String,
    action: String,
    inert_reason: Option<&'static str>,
    recent_outcome: Option<String>,
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(cfg, json),
        Action::Test {
            rule,
            event,
            fixture,
            at,
            json,
        } => test(cfg, &rule, event, fixture, at, json),
    }
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    let effective = cfg.effective_automations();
    let compiled = effective
        .compiled_rules()
        .map_err(|errors| anyhow::anyhow!(errors.join("\n")))?;
    let db = thegn_core::db::Db::open().ok();
    let rows: Vec<RuleRow> = compiled
        .into_iter()
        .map(|rule| {
            let recent_outcome = db
                .as_ref()
                .and_then(|db| db.automation_runs(Some(&rule.id), 1).ok())
                .and_then(|rows| rows.into_iter().next())
                .map(|row| row.outcome);
            RuleRow {
                name: rule.id,
                enabled: rule.enabled,
                trusted_layer: if cfg.active_profile().is_some() {
                    "global+profile"
                } else {
                    "global"
                },
                event: rule.event.as_str().to_string(),
                action: rule.action.cap,
                inert_reason: if !effective.enabled {
                    Some("automations disabled")
                } else if !rule.enabled {
                    Some("rule disabled")
                } else {
                    None
                },
                recent_outcome,
            }
        })
        .collect();
    if json {
        return super::emit_json(&rows);
    }
    if rows.is_empty() {
        thegn_core::outln!("No trusted automation rules configured.");
        return Ok(());
    }
    for row in rows {
        let status = row.inert_reason.unwrap_or("active");
        let recent = row.recent_outcome.as_deref().unwrap_or("never");
        thegn_core::outln!(
            "{:<24} {:<20} -> {:<16}  {} · recent: {}",
            row.name,
            row.event,
            row.action,
            status,
            recent
        );
    }
    Ok(())
}

fn test(
    cfg: &Config,
    rule_name: &str,
    inline: Option<String>,
    fixture: Option<PathBuf>,
    at: Option<i64>,
    json: bool,
) -> Result<()> {
    let raw = match (inline, fixture) {
        (Some(raw), None) => raw,
        (None, Some(path)) => std::fs::read_to_string(&path)
            .with_context(|| format!("read automation fixture {}", path.display()))?,
        _ => unreachable!("clap enforces exactly one fixture source"),
    };
    let event: AutomationEvent =
        serde_json::from_str(&raw).context("parse automation event fixture")?;
    let effective = cfg.effective_automations();
    let rules = effective
        .compiled_rules()
        .map_err(|errors| anyhow::anyhow!(errors.join("\n")))?;
    let rule = rules
        .into_iter()
        .find(|rule| rule.id == rule_name)
        .with_context(|| format!("automation rule {rule_name:?} not found"))?;
    let decisions = evaluate(
        std::slice::from_ref(&rule),
        &event,
        &EvaluationState::new(),
        at.unwrap_or(event.occurred_at),
    );
    if json {
        return super::emit_json(&serde_json::json!({
            "rule": rule_name,
            "event": event,
            "decisions": decisions,
            "executed": false,
        }));
    }
    if decisions.is_empty() {
        thegn_core::outln!("{rule_name}: no match (dry run; nothing executed)");
    } else {
        for decision in decisions {
            match decision {
                EvaluationDecision::Planned(action) => thegn_core::outln!(
                    "{}: planned {} {:?} (dry run; nothing executed)",
                    action.rule_id,
                    action.cap,
                    action.params
                ),
                EvaluationDecision::Skipped {
                    rule_id, reason, ..
                } => {
                    thegn_core::outln!("{rule_id}: skipped {reason:?} (dry run)")
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::config_automations::{
        AutomationActionConfig, AutomationRuleConfig, AutomationsConfig,
    };

    #[test]
    fn dry_run_evaluates_without_opening_a_store() {
        let mut cfg = Config::default();
        cfg.automations = AutomationsConfig {
            enabled: true,
            rules: vec![AutomationRuleConfig {
                name: "say-done".into(),
                when: "agent_finished".into(),
                then: AutomationActionConfig {
                    cap: "notify.push".into(),
                    body: Some("{message}".into()),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let fixture = serde_json::json!({
            "id": "evt-1",
            "occurred_at": 10,
            "key": "session:s1:done",
            "kind": "agent_finished",
            "workspace": null,
            "repo": null,
            "worktree": "/tmp/wt",
            "branch": null,
            "agent_role": null,
            "priority": "Notice",
            "source_ref": null,
            "message": "finished",
            "session_id": "s1",
            "pr_checks_passed": null,
            "pr_review_requested": null,
            "pr_merged": null,
            "origin": null
        });
        test(
            &cfg,
            "say-done",
            Some(fixture.to_string()),
            None,
            None,
            true,
        )
        .unwrap();
    }
}
