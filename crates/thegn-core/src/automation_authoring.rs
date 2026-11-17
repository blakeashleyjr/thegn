//! Lead-authored automation rules (`automations.put` / `rm` / `enable` /
//! `runs`, THE-2181): the put-time validation, the `origin = lead` action
//! allowlist (`notify.push` and `sessions.open` only), and the merge that puts
//! Lead rules beside the trusted config rules at evaluation time.
//!
//! This is the escalation boundary. `automations.put` is a Write-scoped verb,
//! so a rule it stores must never reach an Exec-scoped (`tools.run`) or
//! MergeAdd-scoped (`merge.add`) action. The allowlist is enforced twice:
//! [`validate_lead_rule`] refuses at put time, and every load re-validates the
//! stored body ([`compile_lead_rows`]) before [`merge_rules`] filters again.
//! A row written straight into SQLite therefore still cannot escalate.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::automation::{AutomationEventKind, AutomationRule};
use crate::config_automations::{AutomationRuleConfig, compile_one};
use crate::store::{AutomationRunRow, LeadRuleRow};

/// Every Lead-authored rule id starts with this prefix, so a Lead rule can
/// never share a throttle/audit key with a config rule.
pub const LEAD_RULE_PREFIX: &str = "lead:";

/// The only catalog actions a Lead-authored rule may target.
pub const LEAD_ALLOWED_ACTIONS: &[&str] = &["notify.push", "sessions.open"];

/// The refusal for any other action.
pub const LEAD_ESCALATION_REFUSAL: &str = "Lead-authored rules may only notify or open a session; merge.add and tools.run need an approval-gated rule (phase 6)";

/// The `author` recorded on rows written through the control API. The caller
/// id is not available at the control layer; it stays in the MCP audit line.
pub const LEAD_AUTHOR: &str = "lead";

/// Stored Lead rules are read (and may be created) up to this bound.
pub const LEAD_RULES_MAX: usize = 500;

/// `automations.runs` never returns more than this many rows.
pub const LEAD_RUNS_MAX: usize = 200;

/// `automations.runs` default page.
pub const LEAD_RUNS_DEFAULT: usize = 50;

const MAX_SLUG_LEN: usize = 48;

/// Why a `worktree_idle` Lead rule is stored but never fires in this release.
pub const LEAD_IDLE_INERT: &str = "worktree_idle Lead rules arm in a later release";

/// Normalize a Lead rule name to its stored id: `lead:<slug>`, with the slug
/// matching `[a-z0-9_-]{1,48}`. The prefix is added when missing and is never
/// doubled (`lead:lead:x` is refused, not collapsed).
pub fn lead_rule_id(name: &str) -> Result<String, String> {
    let name = name.trim();
    let slug = name.strip_prefix(LEAD_RULE_PREFIX).unwrap_or(name);
    if slug.is_empty() || slug.len() > MAX_SLUG_LEN {
        return Err(format!(
            "Lead rule name must be 1..={MAX_SLUG_LEN} characters of [a-z0-9_-] (got {name:?})"
        ));
    }
    if !slug.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    }) {
        return Err(format!(
            "Lead rule name must match [a-z0-9_-]{{1,{MAX_SLUG_LEN}}} after the optional `{LEAD_RULE_PREFIX}` prefix (got {name:?})"
        ));
    }
    Ok(format!("{LEAD_RULE_PREFIX}{slug}"))
}

/// Whether a Lead rule may target `cap`.
pub fn lead_action_allowed(cap: &str) -> bool {
    LEAD_ALLOWED_ACTIONS.contains(&cap.trim())
}

/// The config defaults a Lead rule's rate limits are capped at.
fn rate_caps() -> (u16, u16) {
    let defaults = AutomationRuleConfig::default();
    (defaults.max_per_hour, defaults.max_action_per_hour)
}

/// Validate a Lead-authored rule and compile it through the same validator as
/// config rules. On success the rule's id is its `lead:` id and its rate
/// limits are capped at the config defaults.
///
/// Refuses: an invalid name, a name colliding with a config rule (either as
/// written or as its `lead:` id), any action outside [`LEAD_ALLOWED_ACTIONS`],
/// and anything the config validator refuses.
pub fn validate_lead_rule(
    rule: &AutomationRuleConfig,
    config_names: &[String],
) -> Result<AutomationRule, Vec<String>> {
    let mut errors = Vec::new();
    let id = match lead_rule_id(&rule.name) {
        Ok(id) => Some(id),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    let written = rule.name.trim();
    if config_names
        .iter()
        .map(|name| name.trim())
        .any(|name| name == written || id.as_deref() == Some(name))
    {
        errors.push(format!(
            "Lead rule name {written:?} collides with a config rule; config rules are read-only"
        ));
    }
    if !lead_action_allowed(&rule.then.cap) {
        errors.push(LEAD_ESCALATION_REFUSAL.to_string());
    }
    let Some(id) = id else {
        return Err(errors);
    };
    let normalized = normalize(rule, &id);
    match compile_one(&normalized) {
        Ok(compiled) if errors.is_empty() => Ok(compiled),
        Ok(_) => Err(errors),
        Err(more) => {
            errors.extend(more);
            Err(errors)
        }
    }
}

/// The rule as it is stored: `lead:` id as its name, rates capped.
pub fn normalize(rule: &AutomationRuleConfig, id: &str) -> AutomationRuleConfig {
    let (max_rule, max_action) = rate_caps();
    let mut normalized = rule.clone();
    normalized.name = id.to_string();
    normalized.max_per_hour = normalized.max_per_hour.min(max_rule);
    normalized.max_action_per_hour = normalized.max_action_per_hour.min(max_action);
    normalized
}

/// Config rules first; then the enabled Lead rules that still pass the
/// allowlist, carry the `lead:` prefix and do not shadow a config id, with
/// their rates re-capped. Defense in depth: a Lead rule that slipped past put
/// time (a hand-written row, an older build) still cannot escalate here.
pub fn merge_rules(config: Vec<AutomationRule>, lead: Vec<AutomationRule>) -> Vec<AutomationRule> {
    let (max_rule, max_action) = rate_caps();
    let config_ids: BTreeSet<String> = config.iter().map(|rule| rule.id.clone()).collect();
    let mut seen = BTreeSet::new();
    let mut merged = config;
    for mut rule in lead {
        if !rule.enabled
            || !rule.id.starts_with(LEAD_RULE_PREFIX)
            || !lead_action_allowed(&rule.action.cap)
            || config_ids.contains(&rule.id)
            || !seen.insert(rule.id.clone())
        {
            continue;
        }
        rule.max_per_hour = rule.max_per_hour.min(max_rule);
        rule.max_action_per_hour = rule.max_action_per_hour.min(max_action);
        merged.push(rule);
    }
    merged
}

/// Decode and re-validate stored rows. The row's `enabled` column is the
/// effective switch. Rows that no longer validate are returned as
/// `(id, errors)` so callers can log or surface them; they never run.
pub fn compile_lead_rows(
    rows: &[LeadRuleRow],
    config_names: &[String],
) -> (Vec<AutomationRule>, Vec<(String, Vec<String>)>) {
    let mut rules = Vec::with_capacity(rows.len());
    let mut invalid = Vec::new();
    for row in rows {
        match compile_lead_row(row, config_names) {
            Ok(rule) => rules.push(rule),
            Err(errors) => invalid.push((row.id.clone(), errors)),
        }
    }
    (rules, invalid)
}

/// Decode and re-validate one stored row.
pub fn compile_lead_row(
    row: &LeadRuleRow,
    config_names: &[String],
) -> Result<AutomationRule, Vec<String>> {
    let body: AutomationRuleConfig = serde_json::from_str(&row.body_json)
        .map_err(|error| vec![format!("stored rule body is not a rule: {error}")])?;
    let mut rule = validate_lead_rule(&body, config_names)?;
    if rule.id != row.id {
        return Err(vec![format!(
            "stored rule body names {:?}, not its row id {:?}",
            rule.id, row.id
        )]);
    }
    rule.enabled = row.enabled;
    Ok(rule)
}

/// The automation runtime arms when automations are enabled and there is
/// something to evaluate: config rules, or a configured Lead that may author
/// rules later. Config-only, so the gate never reads the DB.
pub fn runtime_armed(
    automations_enabled: bool,
    config_rules: usize,
    lead_configured: bool,
) -> bool {
    automations_enabled && (config_rules > 0 || lead_configured)
}

/// Why a Lead rule would not fire right now, most fundamental first.
pub fn lead_inert_reason(
    automations_enabled: bool,
    runtime_armed: bool,
    rule_enabled: bool,
    event: Option<AutomationEventKind>,
) -> Option<String> {
    if !automations_enabled {
        Some("automations disabled".into())
    } else if !runtime_armed {
        Some("the automation runtime is not armed: no config rules and no [lead] section".into())
    } else if event == Some(AutomationEventKind::WorktreeIdle) {
        Some(LEAD_IDLE_INERT.into())
    } else if !rule_enabled {
        Some("rule disabled".into())
    } else {
        None
    }
}

/// Strictly decode an `automations.put` rule body.
///
/// Unknown fields are refused rather than ignored: a typo'd `if` key must not
/// silently become a rule that matches everything. As a convenience the
/// action's parameters may also be given nested as `then.params`
/// (`{"cap":"notify.push","params":{"body":"done"}}`); they are flattened
/// into `then` and a key given both ways is refused.
pub fn decode_lead_rule(value: serde_json::Value) -> Result<AutomationRuleConfig, Vec<String>> {
    let serde_json::Value::Object(mut rule) = value else {
        return Err(vec!["rule must be a JSON object".into()]);
    };
    let mut errors = Vec::new();
    if let Some(serde_json::Value::Object(then)) = rule.get_mut("then")
        && let Some(params) = then.remove("params")
    {
        match params {
            serde_json::Value::Object(params) => {
                for (key, param) in params {
                    if then.contains_key(&key) {
                        errors.push(format!(
                            "rule.then.{key}: given both directly and in params"
                        ));
                    } else {
                        then.insert(key, param);
                    }
                }
            }
            _ => errors.push("rule.then.params must be an object".into()),
        }
    }
    let template =
        serde_json::to_value(AutomationRuleConfig::default()).expect("the rule config serializes");
    let rule = serde_json::Value::Object(rule);
    unknown_keys(&rule, &template, "rule", &mut errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    serde_json::from_value(rule).map_err(|error| vec![format!("rule: {error}")])
}

fn unknown_keys(
    value: &serde_json::Value,
    template: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    let (Some(value), Some(template)) = (value.as_object(), template.as_object()) else {
        return;
    };
    for (key, child) in value {
        match template.get(key) {
            None => errors.push(format!("{path}.{key}: unknown field")),
            Some(inner) if inner.is_object() => {
                unknown_keys(child, inner, &format!("{path}.{key}"), errors)
            }
            Some(_) => {}
        }
    }
}

/// `automations.put` request.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LeadRulePut {
    pub rule: serde_json::Value,
}

/// `automations.put` reply.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeadRulePutReply {
    pub id: String,
    pub created: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inert_reason: Option<String>,
}

/// `automations.rm` request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeadRuleRm {
    pub name: String,
}

/// `automations.rm` reply.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeadRuleRmReply {
    pub id: String,
    pub removed: bool,
}

/// `automations.enable` request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeadRuleEnable {
    pub name: String,
    pub enabled: bool,
}

/// `automations.enable` reply.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LeadRuleEnableReply {
    pub id: String,
    pub enabled: bool,
}

/// `automations.runs` request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct AutomationRunsQuery {
    pub rule: Option<String>,
    pub limit: Option<usize>,
}

impl AutomationRunsQuery {
    /// The page size, defaulted and capped at [`LEAD_RUNS_MAX`].
    pub fn bounded_limit(&self) -> usize {
        self.limit
            .unwrap_or(LEAD_RUNS_DEFAULT)
            .clamp(1, LEAD_RUNS_MAX)
    }

    /// The audit rule id to filter on: a config rule name as written,
    /// otherwise its `lead:` id when the name is a valid Lead name.
    pub fn rule_id(&self, config_names: &[String]) -> Option<String> {
        let rule = self.rule.as_deref()?.trim();
        if rule.is_empty() {
            return None;
        }
        if config_names.iter().any(|name| name.trim() == rule) {
            return Some(rule.to_string());
        }
        Some(lead_rule_id(rule).unwrap_or_else(|_| rule.to_string()))
    }
}

/// `automations.runs` reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AutomationRunsReply {
    pub runs: Vec<AutomationRunRow>,
}

/// The `when` and `then.cap` of a stored body, best-effort, for listing a row
/// that no longer validates.
pub fn stored_shape(body_json: &str) -> (String, String) {
    let value: serde_json::Value = serde_json::from_str(body_json).unwrap_or_default();
    let field = |pointer: &str| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    (field("/when"), field("/then/cap"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_automations::AutomationActionConfig;
    use serde_json::json;

    fn rule(name: &str, cap: &str) -> AutomationRuleConfig {
        let mut then = AutomationActionConfig {
            cap: cap.into(),
            ..AutomationActionConfig::default()
        };
        match cap {
            "notify.push" => then.body = Some("done: {message}".into()),
            "sessions.open" => then.agent = Some("claude".into()),
            "tools.run" => then.name = Some("deploy".into()),
            _ => {}
        }
        AutomationRuleConfig {
            name: name.into(),
            when: "agent_finished".into(),
            then,
            ..AutomationRuleConfig::default()
        }
    }

    fn compiled(name: &str, cap: &str) -> AutomationRule {
        compile_one(&rule(name, cap)).unwrap()
    }

    #[test]
    fn lead_rule_id_slugs_and_never_doubles_the_prefix() {
        assert_eq!(lead_rule_id("ping").unwrap(), "lead:ping");
        assert_eq!(lead_rule_id(" lead:ping ").unwrap(), "lead:ping");
        assert_eq!(lead_rule_id("a_b-9").unwrap(), "lead:a_b-9");
        assert_eq!(
            lead_rule_id(&"x".repeat(MAX_SLUG_LEN)).unwrap().len(),
            LEAD_RULE_PREFIX.len() + MAX_SLUG_LEN
        );
        let too_long = "x".repeat(MAX_SLUG_LEN + 1);
        for bad in [
            "",
            "lead:",
            "lead:lead:x",
            "Ping",
            "a b",
            "a.b",
            "a/b",
            "ä",
            too_long.as_str(),
        ] {
            assert!(lead_rule_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn allowed_actions_validate_and_get_the_lead_id() {
        for cap in LEAD_ALLOWED_ACTIONS {
            let compiled = validate_lead_rule(&rule("ping", cap), &[]).unwrap();
            assert_eq!(compiled.id, "lead:ping");
            assert_eq!(compiled.action.cap, *cap);
        }
    }

    #[test]
    fn escalating_actions_are_refused_at_put() {
        for cap in ["merge.add", "tools.run", " tools.run ", "invoke", ""] {
            let errors = validate_lead_rule(&rule("ping", cap), &[]).unwrap_err();
            assert!(
                errors.iter().any(|e| e == LEAD_ESCALATION_REFUSAL),
                "{cap:?}: {errors:?}"
            );
        }
    }

    #[test]
    fn config_name_collisions_are_refused() {
        let config = vec!["ping".to_string(), "lead:pong".to_string()];
        let errors = validate_lead_rule(&rule("ping", "notify.push"), &config).unwrap_err();
        assert!(errors.join("\n").contains("collides with a config rule"));
        let errors = validate_lead_rule(&rule("pong", "notify.push"), &config).unwrap_err();
        assert!(errors.join("\n").contains("collides with a config rule"));
        assert!(validate_lead_rule(&rule("other", "notify.push"), &config).is_ok());
    }

    #[test]
    fn config_validator_errors_surface() {
        let mut bad = rule("ping", "notify.push");
        bad.when = "never".into();
        bad.then.body = Some("{typo}".into());
        let errors = validate_lead_rule(&bad, &[]).unwrap_err().join("\n");
        assert!(errors.contains("unknown event kind"), "{errors}");
        assert!(errors.contains("unknown placeholder"), "{errors}");
    }

    #[test]
    fn rate_limits_are_capped_at_the_config_defaults() {
        let defaults = AutomationRuleConfig::default();
        let mut greedy = rule("ping", "notify.push");
        greedy.max_per_hour = 1_000;
        greedy.max_action_per_hour = 999;
        let compiled = validate_lead_rule(&greedy, &[]).unwrap();
        assert_eq!(compiled.max_per_hour, defaults.max_per_hour);
        assert_eq!(compiled.max_action_per_hour, defaults.max_action_per_hour);
        let mut modest = rule("ping", "notify.push");
        modest.max_per_hour = 2;
        modest.max_action_per_hour = 1;
        let compiled = validate_lead_rule(&modest, &[]).unwrap();
        assert_eq!(
            (compiled.max_per_hour, compiled.max_action_per_hour),
            (2, 1)
        );
    }

    #[test]
    fn merge_rules_keeps_config_first_and_filters_lead_again() {
        let config = vec![
            compiled("cfg", "tools.run"),
            compiled("lead:shadow", "merge.add"),
        ];
        let mut disabled = compiled("lead:off", "notify.push");
        disabled.enabled = false;
        let mut greedy = compiled("lead:greedy", "notify.push");
        greedy.max_per_hour = 1_000;
        let lead = vec![
            compiled("lead:ok", "notify.push"),
            compiled("lead:open", "sessions.open"),
            compiled("lead:merge", "merge.add"),
            compiled("lead:tool", "tools.run"),
            compiled("unprefixed", "notify.push"),
            compiled("lead:shadow", "notify.push"),
            compiled("lead:ok", "notify.push"),
            disabled,
            greedy,
        ];
        let merged = merge_rules(config, lead);
        let ids: Vec<&str> = merged.iter().map(|rule| rule.id.as_str()).collect();
        assert_eq!(
            ids,
            ["cfg", "lead:shadow", "lead:ok", "lead:open", "lead:greedy"]
        );
        // Config rules are untouched; the shadowing Lead rule was dropped.
        assert_eq!(merged[0].action.cap, "tools.run");
        assert_eq!(merged[1].action.cap, "merge.add");
        assert_eq!(
            merged[4].max_per_hour,
            AutomationRuleConfig::default().max_per_hour
        );
    }

    fn row(id: &str, body: &AutomationRuleConfig, enabled: bool) -> LeadRuleRow {
        LeadRuleRow {
            id: id.into(),
            body_json: serde_json::to_string(body).unwrap(),
            enabled,
            author: LEAD_AUTHOR.into(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn stored_rows_are_revalidated_on_load() {
        let ok = normalize(&rule("ok", "notify.push"), "lead:ok");
        let tool = normalize(&rule("tool", "tools.run"), "lead:tool");
        let renamed = normalize(&rule("x", "notify.push"), "lead:x");
        let rows = vec![
            row("lead:ok", &ok, false),
            row("lead:tool", &tool, true),
            row("lead:other", &renamed, true),
            LeadRuleRow {
                id: "lead:junk".into(),
                body_json: "not json".into(),
                ..LeadRuleRow::default()
            },
        ];
        let (rules, invalid) = compile_lead_rows(&rows, &[]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "lead:ok");
        assert!(!rules[0].enabled, "the row's enabled column wins");
        let invalid_ids: Vec<&str> = invalid.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(invalid_ids, ["lead:tool", "lead:other", "lead:junk"]);
        assert!(invalid[0].1.iter().any(|e| e == LEAD_ESCALATION_REFUSAL));
        // A config rule added later with the same id shadows the stored rule.
        let (rules, invalid) = compile_lead_rows(&rows[..1], &["lead:ok".to_string()]);
        assert!(rules.is_empty());
        assert_eq!(invalid.len(), 1);
    }

    #[test]
    fn runtime_gate_is_config_only() {
        assert!(!runtime_armed(false, 3, true));
        assert!(!runtime_armed(true, 0, false));
        assert!(runtime_armed(true, 1, false));
        assert!(runtime_armed(true, 0, true));
    }

    #[test]
    fn inert_reasons_are_ordered() {
        let idle = Some(AutomationEventKind::WorktreeIdle);
        let done = Some(AutomationEventKind::AgentFinished);
        assert_eq!(
            lead_inert_reason(false, false, false, idle).as_deref(),
            Some("automations disabled")
        );
        assert!(
            lead_inert_reason(true, false, true, done)
                .unwrap()
                .contains("not armed")
        );
        assert_eq!(
            lead_inert_reason(true, true, false, idle).as_deref(),
            Some(LEAD_IDLE_INERT)
        );
        assert_eq!(
            lead_inert_reason(true, true, false, done).as_deref(),
            Some("rule disabled")
        );
        assert_eq!(lead_inert_reason(true, true, true, done), None);
    }

    #[test]
    fn decode_flattens_params_and_refuses_unknown_fields() {
        let decoded = decode_lead_rule(json!({
            "name": "ping",
            "when": "agent_finished",
            "then": {"cap": "notify.push", "params": {"body": "done"}}
        }))
        .unwrap();
        assert_eq!(decoded.then.body.as_deref(), Some("done"));
        assert!(validate_lead_rule(&decoded, &[]).is_ok());

        let decoded = decode_lead_rule(json!({
            "name": "ping",
            "when": "agent_finished",
            "if": {"branch": "tg/*"},
            "then": {"cap": "notify.push", "body": "done"}
        }))
        .unwrap();
        assert_eq!(decoded.predicate.branch.as_deref(), Some("tg/*"));

        for (value, needle) in [
            (json!("x"), "JSON object"),
            (json!({"name": "a", "iff": {}}), "rule.iff: unknown field"),
            (
                json!({"name": "a", "if": {"brnch": "x"}}),
                "rule.if.brnch: unknown field",
            ),
            (
                json!({"name": "a", "then": {"cap": "notify.push", "argv": ["sh"]}}),
                "rule.then.argv: unknown field",
            ),
            (
                json!({"name": "a", "then": {"cap": "notify.push", "body": "a", "params": {"body": "b"}}}),
                "given both",
            ),
            (
                json!({"name": "a", "then": {"cap": "notify.push", "params": "x"}}),
                "params must be an object",
            ),
            (json!({"name": 3}), "rule:"),
        ] {
            let errors = decode_lead_rule(value.clone()).unwrap_err().join("\n");
            assert!(errors.contains(needle), "{value}: {errors}");
        }
    }

    #[test]
    fn runs_query_bounds_and_resolves_rule_ids() {
        let config = vec!["cfg".to_string()];
        let query = |rule: Option<&str>, limit: Option<usize>| AutomationRunsQuery {
            rule: rule.map(str::to_string),
            limit,
        };
        assert_eq!(query(None, None).bounded_limit(), LEAD_RUNS_DEFAULT);
        assert_eq!(query(None, Some(0)).bounded_limit(), 1);
        assert_eq!(query(None, Some(10_000)).bounded_limit(), LEAD_RUNS_MAX);
        assert_eq!(query(None, None).rule_id(&config), None);
        assert_eq!(query(Some(" "), None).rule_id(&config), None);
        assert_eq!(
            query(Some("cfg"), None).rule_id(&config).as_deref(),
            Some("cfg")
        );
        assert_eq!(
            query(Some("ping"), None).rule_id(&config).as_deref(),
            Some("lead:ping")
        );
        assert_eq!(
            query(Some("Weird Name"), None).rule_id(&config).as_deref(),
            Some("Weird Name")
        );
    }

    #[test]
    fn stored_shape_is_best_effort() {
        let body = serde_json::to_string(&rule("x", "tools.run")).unwrap();
        assert_eq!(
            stored_shape(&body),
            ("agent_finished".to_string(), "tools.run".to_string())
        );
        assert_eq!(stored_shape("garbage"), (String::new(), String::new()));
    }

    #[test]
    fn dto_shapes_roundtrip() {
        let reply = LeadRulePutReply {
            id: "lead:x".into(),
            created: true,
            inert_reason: None,
        };
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({"id": "lead:x", "created": true})
        );
        let enable: LeadRuleEnable =
            serde_json::from_value(json!({"name": "x", "enabled": false})).unwrap();
        assert!(!enable.enabled);
        let rm: LeadRuleRm = serde_json::from_value(json!({"name": "x"})).unwrap();
        assert_eq!(rm.name, "x");
        let put: LeadRulePut = serde_json::from_value(json!({"rule": {"name": "x"}})).unwrap();
        assert_eq!(put.rule["name"], "x");
        serde_json::to_value(LeadRuleRmReply {
            id: "lead:x".into(),
            removed: true,
        })
        .unwrap();
        serde_json::to_value(LeadRuleEnableReply {
            id: "lead:x".into(),
            enabled: true,
        })
        .unwrap();
        serde_json::to_value(AutomationRunsReply::default()).unwrap();
    }
}
