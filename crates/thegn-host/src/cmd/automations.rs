//! `thegn automations` — inspect trusted rules and dry-run an event fixture.
//!
//! `test` is intentionally pure: it loads effective global/profile config,
//! deserializes a caller-supplied event, and invokes `thegn_core::automation::evaluate`
//! with empty ledger state. It never opens SQLite and never dispatches work.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Serialize;
use thegn_core::automation::{AutomationEvent, EvaluationDecision, EvaluationState, evaluate};
use thegn_core::automation_autonomy::Autonomy;
use thegn_core::automation_schedule::Schedule;
use thegn_core::config::Config;
use thegn_core::store::AutomationStore;

/// Largest rule file `add --file` reads.
const MAX_RULE_BYTES: u64 = 64 * 1024;

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
    /// Store one rule through the daemon. CLI-added rules are Lead-class:
    /// restricted actions, never `merge`, `pipeline.start` only at `propose`.
    /// Trusted rules belong in `[[automations.rules]]` in config.toml.
    Add {
        /// TOML (or `.json`) file shaped like one `[[automations.rules]]` entry.
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "json_rule",
            required_unless_present = "json_rule"
        )]
        file: Option<PathBuf>,
        /// The rule as an inline JSON object (exclusive with `--file`).
        #[arg(
            long = "json",
            value_name = "RULE",
            conflicts_with = "file",
            required_unless_present = "file"
        )]
        json_rule: Option<String>,
    },
    /// Remove a stored (Lead-class) rule; config rules are read-only.
    Rm {
        /// Rule name.
        rule: String,
    },
    /// Show recent audited automation runs, newest first.
    Runs {
        /// Only this rule.
        rule: Option<String>,
        /// Maximum rows.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
        /// Emit one JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Pause every automation rule (events are audited as skipped).
    Pause,
    /// Resume automation rules after a pause (operator only).
    Resume,
}

/// The `automations.runs` row fields the table and `--json` show.
#[derive(Debug, Default, Serialize, serde::Deserialize)]
#[serde(default)]
struct RunView {
    rule_id: String,
    trigger_kind: String,
    action_cap: String,
    outcome: String,
    skip_reason: Option<String>,
    error: Option<String>,
    started_at: i64,
    autonomy: Option<String>,
}

#[derive(Debug, Serialize)]
struct RuleRow {
    name: String,
    enabled: bool,
    trusted_layer: &'static str,
    event: String,
    action: String,
    schedule: Option<String>,
    autonomy: String,
    inert_reason: Option<String>,
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
        Action::Add { file, json_rule } => add(file, json_rule),
        Action::Rm { rule } => rm(&rule),
        Action::Runs { rule, limit, json } => runs(rule, limit, json),
        Action::Pause => set_pause(true),
        Action::Resume => set_pause(false),
    }
}

/// Decode a rule file (`.json` by extension, otherwise TOML) or inline JSON
/// into the JSON object `automations.put` takes as `rule`.
fn decode_rule(raw: &str, json: bool) -> Result<serde_json::Value> {
    let value: serde_json::Value = if json {
        serde_json::from_str(raw).context("parse rule JSON")?
    } else {
        let table: toml::Value = toml::from_str(raw).context("parse rule TOML")?;
        serde_json::to_value(table).context("convert rule TOML")?
    };
    anyhow::ensure!(value.is_object(), "a rule must be an object");
    Ok(value)
}

/// Read at most [`MAX_RULE_BYTES`]; a larger file is refused, not truncated.
fn read_rule_file(path: &Path) -> Result<String> {
    let file =
        std::fs::File::open(path).with_context(|| format!("read rule file {}", path.display()))?;
    let mut buf = Vec::new();
    file.take(MAX_RULE_BYTES + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("read rule file {}", path.display()))?;
    anyhow::ensure!(
        buf.len() as u64 <= MAX_RULE_BYTES,
        "rule file {} exceeds {} bytes",
        path.display(),
        MAX_RULE_BYTES
    );
    String::from_utf8(buf).context("rule file is not UTF-8")
}

fn call_daemon(path: &'static str, body: serde_json::Value) -> Result<serde_json::Value> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        crate::automation_executor::client()
            .await?
            .call_raw("POST", path, Some(body))
            .await
    })
}

fn add(file: Option<PathBuf>, json_rule: Option<String>) -> Result<()> {
    let rule = match (file, json_rule) {
        (Some(path), None) => {
            let is_json = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
            decode_rule(&read_rule_file(&path)?, is_json)?
        }
        (None, Some(raw)) => decode_rule(&raw, true)?,
        _ => unreachable!("clap enforces exactly one rule source"),
    };
    let reply = call_daemon("/v1/automations/put", serde_json::json!({ "rule": rule }))?;
    let id = reply.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    thegn_core::outln!("stored rule {id}");
    if let Some(reason) = reply.get("inert_reason").and_then(|v| v.as_str()) {
        thegn_core::outln!("inert: {reason}");
    }
    thegn_core::outln!("stored as a Lead-class rule; trusted rules belong in config.toml");
    Ok(())
}

fn rm(rule: &str) -> Result<()> {
    let reply = call_daemon("/v1/automations/rm", serde_json::json!({ "name": rule }))?;
    let id = reply.get("id").and_then(|v| v.as_str()).unwrap_or(rule);
    thegn_core::outln!("removed rule {id}");
    Ok(())
}

fn set_pause(pause: bool) -> Result<()> {
    let path = if pause {
        "/v1/automations/pause"
    } else {
        "/v1/automations/resume"
    };
    call_daemon(path, serde_json::json!({}))?;
    thegn_core::outln!("automations {}", if pause { "paused" } else { "resumed" });
    Ok(())
}

fn runs(rule: Option<String>, limit: Option<usize>, json: bool) -> Result<()> {
    let reply = call_daemon(
        "/v1/automations/runs",
        serde_json::json!({ "rule": rule, "limit": limit }),
    )?;
    let rows: Vec<RunView> = serde_json::from_value(
        reply
            .get("runs")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    )
    .context("decode automation runs")?;
    if json {
        return super::emit_json(&rows);
    }
    for line in format_runs(&rows) {
        thegn_core::outln!("{line}");
    }
    Ok(())
}

/// The runs table: time, rule, trigger, cap, outcome, autonomy, error.
fn format_runs(rows: &[RunView]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["No automation runs recorded.".into()];
    }
    let mut out = vec![format!(
        "{:<19}  {:<24} {:<18} {:<16} {:<10} {:<8} {}",
        "TIME", "RULE", "TRIGGER", "CAP", "OUTCOME", "AUTONOMY", "ERROR"
    )];
    for row in rows {
        let time = chrono::DateTime::from_timestamp(row.started_at, 0)
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| row.started_at.to_string());
        let error = row
            .error
            .as_deref()
            .or(row.skip_reason.as_deref())
            .unwrap_or("-")
            .replace(['\n', '\r'], " ");
        out.push(format!(
            "{:<19}  {:<24} {:<18} {:<16} {:<10} {:<8} {}",
            time,
            row.rule_id,
            row.trigger_kind,
            row.action_cap,
            row.outcome,
            row.autonomy.as_deref().unwrap_or("-"),
            error
        ));
    }
    out
}

/// `review`, or `review (capped from merge)` when policy lowered it.
fn autonomy_cell(declared: Option<&str>, effective: Autonomy) -> String {
    match declared.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) if d != effective.as_str() => {
            format!("{} (capped from {d})", effective.as_str())
        }
        _ => effective.as_str().to_string(),
    }
}

fn schedule_cell(schedule: Option<&Schedule>) -> Option<String> {
    match schedule? {
        Schedule::Cron(spec) => Some(format!("cron {}", spec.source)),
        Schedule::Every { secs } => Some(format!("every {secs}s")),
    }
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    use thegn_core::store::AutomationRuleStore;
    let effective = cfg.effective_automations();
    let compiled = effective
        .compiled_rules()
        .map_err(|errors| anyhow::anyhow!(errors.join("\n")))?;
    let db = thegn_core::db::Db::open().ok();
    let names: Vec<String> = compiled.iter().map(|r| r.id.clone()).collect();
    let lead_rules = db
        .as_ref()
        .map(|db| {
            thegn_core::automation_authoring::compile_lead_rows(
                &db.lead_rules().unwrap_or_default(),
                &names,
            )
            .0
        })
        .unwrap_or_default();
    let daemon_available = db.as_ref().is_some_and(|db| {
        thegn_svc::control::client::discover(
            db,
            &crate::daemon::scope_key(),
            thegn_core::util::now().saturating_mul(1_000),
        )
        .is_some()
    });
    let declared: std::collections::HashMap<String, String> = effective
        .rules
        .iter()
        .filter_map(|r| Some((r.name.trim().to_string(), r.autonomy.clone()?)))
        .collect();
    let paused = db
        .as_ref()
        .is_some_and(|db| thegn_core::automation_control::paused(db));
    let rows: Vec<RuleRow> = compiled
        .into_iter()
        .map(|rule| (rule, false))
        .chain(lead_rules.into_iter().map(|rule| (rule, true)))
        .map(|(rule, is_lead)| {
            let recent_outcome = db
                .as_ref()
                .and_then(|db| db.automation_runs(Some(&rule.id), 1).ok())
                .and_then(|rows| rows.into_iter().next())
                .map(|row| row.outcome);
            let name = rule.id.clone();
            RuleRow {
                name: rule.id,
                enabled: rule.enabled,
                trusted_layer: if is_lead {
                    "lead"
                } else if cfg.active_profile().is_some() {
                    "global+profile"
                } else {
                    "global"
                },
                event: rule.event.as_str().to_string(),
                action: rule.action.cap.clone(),
                schedule: schedule_cell(rule.schedule.as_ref()),
                autonomy: autonomy_cell(
                    if is_lead {
                        None
                    } else {
                        declared.get(&name).map(String::as_str)
                    },
                    rule.autonomy,
                ),
                inert_reason: inert_reason(
                    effective.enabled,
                    rule.enabled,
                    rule.event,
                    &rule.action.cap,
                    cfg.daemon.enabled,
                    daemon_available,
                ),
                recent_outcome,
            }
        })
        .collect();
    if json {
        return super::emit_json(&rows);
    }
    if paused {
        thegn_core::outln!("PAUSED: automations are paused (`thegn automations resume`)");
    }
    if rows.is_empty() {
        thegn_core::outln!("No trusted automation rules configured.");
        return Ok(());
    }
    for row in rows {
        let status = row.inert_reason.as_deref().unwrap_or("active");
        let recent = row.recent_outcome.as_deref().unwrap_or("never");
        thegn_core::outln!(
            "{:<24} {:<20} -> {:<16}  autonomy: {} · schedule: {} · {} · recent: {}",
            row.name,
            row.event,
            row.action,
            row.autonomy,
            row.schedule.as_deref().unwrap_or("-"),
            status,
            recent
        );
    }
    Ok(())
}

fn inert_reason(
    automations_enabled: bool,
    rule_enabled: bool,
    event: thegn_core::automation::AutomationEventKind,
    action: &str,
    daemon_enabled: bool,
    daemon_available: bool,
) -> Option<String> {
    if !automations_enabled {
        return Some("automations disabled".into());
    }
    if !rule_enabled {
        return Some("rule disabled".into());
    }
    if !daemon_enabled && event == thegn_core::automation::AutomationEventKind::ScheduleDue {
        return Some("schedule_due needs the daemon".into());
    }
    let daemon_trigger = matches!(
        event,
        thegn_core::automation::AutomationEventKind::AgentNeedsYou
            | thegn_core::automation::AutomationEventKind::AgentFinished
            | thegn_core::automation::AutomationEventKind::AgentFailed
            | thegn_core::automation::AutomationEventKind::WorktreeIdle
    );
    if !daemon_enabled {
        return Some(if daemon_trigger {
            format!(
                "daemon disabled: {} trigger and {action} action are inert",
                event.as_str()
            )
        } else {
            format!("daemon disabled: {action} action is inert")
        });
    }
    if !daemon_available {
        return Some(if daemon_trigger {
            format!(
                "daemon unavailable: {} trigger and {action} action are inert",
                event.as_str()
            )
        } else {
            format!("daemon unavailable: {action} action is inert")
        });
    }
    None
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
    event
        .validate_required_facts()
        .map_err(anyhow::Error::msg)?;
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
    fn rule_decodes_from_toml_and_json() {
        let t = decode_rule(
            "name = \"r\"\nwhen = \"agent_finished\"\n[then]\ncap = \"notify.push\"\n",
            false,
        )
        .unwrap();
        assert_eq!(t["then"]["cap"], "notify.push");
        let j = decode_rule(r#"{"name":"r","when":"agent_finished"}"#, true).unwrap();
        assert_eq!(j["name"], "r");
        assert!(decode_rule("[1]", true).is_err());
        assert!(decode_rule("not = [", false).is_err());
    }

    #[test]
    fn rule_file_over_64k_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.toml");
        std::fs::write(&big, vec![b'#'; 64 * 1024 + 1]).unwrap();
        assert!(read_rule_file(&big).is_err());
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, "a = 1").unwrap();
        assert_eq!(read_rule_file(&ok).unwrap(), "a = 1");
    }

    #[test]
    fn add_arg_parsing_requires_exactly_one_source() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(subcommand)]
            action: Action,
        }
        use clap::Parser;
        assert!(Cli::try_parse_from(["x", "add"]).is_err());
        assert!(Cli::try_parse_from(["x", "add", "--file", "a", "--json", "{}"]).is_err());
        assert!(Cli::try_parse_from(["x", "add", "--file", "a.toml"]).is_ok());
        assert!(Cli::try_parse_from(["x", "add", "--json", "{}"]).is_ok());
        assert!(Cli::try_parse_from(["x", "runs", "r", "--limit", "5", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["x", "rm"]).is_err());
        assert!(Cli::try_parse_from(["x", "pause"]).is_ok());
        assert!(Cli::try_parse_from(["x", "resume"]).is_ok());
    }

    #[test]
    fn runs_table_has_header_rows_and_empty_note() {
        assert_eq!(format_runs(&[])[0], "No automation runs recorded.");
        let row = RunView {
            rule_id: "r1".into(),
            trigger_kind: "schedule_due".into(),
            action_cap: "notify.push".into(),
            outcome: "failed".into(),
            error: Some("a\nb".into()),
            autonomy: Some("review".into()),
            ..Default::default()
        };
        let lines = format_runs(&[row]);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("AUTONOMY"));
        assert!(lines[1].contains("r1") && lines[1].contains("review"));
        assert!(lines[1].ends_with("a b"));
    }

    #[test]
    fn autonomy_cell_notes_a_cap_and_schedule_inert_reason() {
        assert_eq!(autonomy_cell(None, Autonomy::Review), "review");
        assert_eq!(
            autonomy_cell(Some("merge"), Autonomy::Review),
            "review (capped from merge)"
        );
        let reason = inert_reason(
            true,
            true,
            thegn_core::automation::AutomationEventKind::ScheduleDue,
            "notify.push",
            false,
            false,
        );
        assert_eq!(reason.as_deref(), Some("schedule_due needs the daemon"));
    }

    #[test]
    fn dry_run_evaluates_without_opening_a_store() {
        let cfg = Config {
            automations: AutomationsConfig {
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
            },
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
