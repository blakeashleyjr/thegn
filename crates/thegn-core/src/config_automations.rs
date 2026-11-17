//! Trusted `[automations]` configuration and semantic validation.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent_task::validate_template;
use crate::automation::{
    ActionTemplate, AutomationEventKind, AutomationPredicate, AutomationRule, EVENT_TEMPLATE_VARS,
    LEAD_WAKE_TEMPLATE_VARS, SUPPORTED_ACTION_CAPS,
};
use crate::automation_autonomy::{Autonomy, RuleOrigin, effective_autonomy, validate_autonomy};
use crate::automation_schedule::parse_schedule;
use crate::notification::{NotificationKind, Priority};

const MAX_RULES: usize = 256;
const MAX_NAME_LEN: usize = 128;
const MAX_TEMPLATE_LEN: usize = 16 * 1024;
const MAX_REGEX_LEN: usize = 4 * 1024;
const MAX_PARAMS: usize = 8;

fn default_rule_enabled() -> bool {
    true
}

fn default_max_concurrent() -> usize {
    2
}

fn default_queue_capacity() -> usize {
    64
}

fn default_action_timeout_secs() -> u64 {
    300
}

fn default_audit_retention_per_rule() -> usize {
    200
}

fn default_debounce_secs() -> u64 {
    30
}

fn default_max_per_hour() -> u16 {
    30
}

/// Global/profile automation settings and ordered rules. Defaults are inert.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutomationsConfig {
    pub enabled: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default = "default_action_timeout_secs")]
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub action_timeout_secs: u64,
    #[serde(default = "default_audit_retention_per_rule")]
    pub audit_retention_per_rule: usize,
    pub rules: Vec<AutomationRuleConfig>,
}

impl Default for AutomationsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_concurrent: default_max_concurrent(),
            queue_capacity: default_queue_capacity(),
            action_timeout_secs: default_action_timeout_secs(),
            audit_retention_per_rule: default_audit_retention_per_rule(),
            rules: Vec::new(),
        }
    }
}

/// Optional named-profile refinements. A present `rules` list replaces the
/// global list; absent fields inherit it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutomationsOverlay {
    pub enabled: Option<bool>,
    pub max_concurrent: Option<usize>,
    pub queue_capacity: Option<usize>,
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub action_timeout_secs: Option<u64>,
    pub audit_retention_per_rule: Option<usize>,
    pub rules: Option<Vec<AutomationRuleConfig>>,
}

impl AutomationsOverlay {
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.max_concurrent.is_none()
            && self.queue_capacity.is_none()
            && self.action_timeout_secs.is_none()
            && self.audit_retention_per_rule.is_none()
            && self.rules.is_none()
    }

    pub fn apply(self, base: &mut AutomationsConfig) {
        if let Some(value) = self.enabled {
            base.enabled = value;
        }
        if let Some(value) = self.max_concurrent {
            base.max_concurrent = value;
        }
        if let Some(value) = self.queue_capacity {
            base.queue_capacity = value;
        }
        if let Some(value) = self.action_timeout_secs {
            base.action_timeout_secs = value;
        }
        if let Some(value) = self.audit_retention_per_rule {
            base.audit_retention_per_rule = value;
        }
        if let Some(value) = self.rules {
            base.rules = value;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutomationRuleConfig {
    pub name: String,
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,
    pub when: String,
    #[serde(rename = "if")]
    pub predicate: AutomationPredicateConfig,
    pub then: AutomationActionConfig,
    #[serde(default = "default_debounce_secs")]
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub debounce_secs: u64,
    /// Required only for `when = "worktree_idle"`; independent of debounce.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub idle_secs: Option<u64>,
    /// 5-field cron expression in the host's local time zone. Exactly one of
    /// `schedule`/`every` is required for `when = "schedule_due"`, and neither
    /// is valid for any other kind.
    pub schedule: Option<String>,
    /// Fixed interval `<n>s|m|h|d` (60 s to 7 d), counted from daemon start.
    pub every: Option<String>,
    /// `propose` | `review` | `merge` (default `review`). Policy may lower it:
    /// a rule matching free text (`if.message_regex`) is capped at `review`.
    pub autonomy: Option<String>,
    /// Forced on for `when = "issue_matched"`: one issue fires a rule once.
    pub once_per_key: bool,
    #[serde(default = "default_max_per_hour")]
    pub max_per_hour: u16,
    #[serde(default = "default_max_per_hour")]
    pub max_action_per_hour: u16,
}

impl Default for AutomationRuleConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            enabled: true,
            when: String::new(),
            predicate: AutomationPredicateConfig::default(),
            then: AutomationActionConfig::default(),
            debounce_secs: default_debounce_secs(),
            idle_secs: None,
            schedule: None,
            every: None,
            autonomy: None,
            once_per_key: false,
            max_per_hour: default_max_per_hour(),
            max_action_per_hour: default_max_per_hour(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutomationPredicateConfig {
    pub workspace: Option<String>,
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub agent_role: Option<String>,
    pub notification_kind: Option<String>,
    pub priority: Option<String>,
    pub source_prefix: Option<String>,
    pub message_regex: Option<String>,
    pub session_id: Option<String>,
    pub pr_checks_passed: Option<bool>,
    pub pr_review_requested: Option<bool>,
    pub pr_merged: Option<bool>,
    /// Issue selectors: valid on any rule, but they only ever match
    /// `issue_matched` events (any other event carries no issue).
    pub account: Option<String>,
    pub project: Option<String>,
    pub label: Option<String>,
    /// `"me"` = assigned to the tracker account's own user (provenance).
    pub assignee: Option<String>,
    pub status: Option<String>,
}

/// `cap` is the only action discriminator. Explicit optional fields keep the
/// strict config schema closed: an unknown parameter is rejected rather than
/// disappearing into an unvalidated free-form map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub struct AutomationActionConfig {
    pub cap: String,
    pub agent: Option<String>,
    pub prompt: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub urgency: Option<String>,
    pub name: Option<String>,
    /// `pipeline.start`: the configured `[[pipeline.stages]]` name to start at.
    pub stage: Option<String>,
}

impl AutomationActionConfig {
    fn params(&self) -> BTreeMap<String, String> {
        [
            ("agent", &self.agent),
            ("prompt", &self.prompt),
            ("title", &self.title),
            ("body", &self.body),
            ("urgency", &self.urgency),
            ("name", &self.name),
            ("stage", &self.stage),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.clone().map(|value| (name.to_string(), value)))
        .collect()
    }
}

impl AutomationsConfig {
    /// Semantic errors the TOML/schema shape cannot express.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        bounded(
            &mut errors,
            "automations.max_concurrent",
            self.max_concurrent,
            1,
            16,
        );
        bounded(
            &mut errors,
            "automations.queue_capacity",
            self.queue_capacity,
            1,
            1_024,
        );
        bounded(
            &mut errors,
            "automations.action_timeout_secs",
            self.action_timeout_secs,
            1,
            3_600,
        );
        bounded(
            &mut errors,
            "automations.audit_retention_per_rule",
            self.audit_retention_per_rule,
            1,
            10_000,
        );
        if self.rules.len() > MAX_RULES {
            errors.push(format!(
                "automations.rules: at most {MAX_RULES} rules are allowed"
            ));
        }

        let mut names = BTreeSet::new();
        for (index, rule) in self.rules.iter().enumerate() {
            let key = if rule.name.trim().is_empty() {
                format!("automations.rules[{index}]")
            } else {
                format!("automations.rules.{}", rule.name)
            };
            validate_rule(rule, &key, &mut errors);
            let normalized = rule.name.trim();
            if !normalized.is_empty() && !names.insert(normalized.to_string()) {
                errors.push(format!("{key}.name: duplicate automation rule name"));
            }
        }
        errors
    }

    /// Build pure engine rules after validation.
    pub fn compiled_rules(&self) -> Result<Vec<AutomationRule>, Vec<String>> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(self.rules.iter().map(compile_rule).collect())
    }
}

/// Validate and compile one rule outside a config document, through the same
/// validator `compiled_rules` uses. Lead-authored rules (THE-2181) go through
/// here; cross-rule checks (duplicate names, the rule count) are the caller's.
pub fn compile_one(rule: &AutomationRuleConfig) -> Result<AutomationRule, Vec<String>> {
    let key = format!("automations.rules.{}", rule.name.trim());
    let mut errors = Vec::new();
    validate_rule(rule, &key, &mut errors);
    if errors.is_empty() {
        Ok(compile_rule(rule))
    } else {
        Err(errors)
    }
}

fn bounded<T>(errors: &mut Vec<String>, key: &str, value: T, min: T, max: T)
where
    T: Ord + std::fmt::Display + Copy,
{
    if value < min || value > max {
        errors.push(format!(
            "{key}: must be between {min} and {max} (got {value})"
        ));
    }
}

fn validate_rule(rule: &AutomationRuleConfig, key: &str, errors: &mut Vec<String>) {
    let name = rule.name.trim();
    if name.is_empty() {
        errors.push(format!("{key}.name: must not be empty"));
    } else if name.len() > MAX_NAME_LEN {
        errors.push(format!("{key}.name: must be at most {MAX_NAME_LEN} bytes"));
    }
    if AutomationEventKind::parse(rule.when.trim()).is_none() {
        errors.push(format!(
            "{key}.when: unknown event kind {:?}; expected one of {}",
            rule.when,
            AutomationEventKind::ALL
                .iter()
                .map(|kind| kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    bounded(
        errors,
        &format!("{key}.debounce_secs"),
        rule.debounce_secs,
        0,
        86_400,
    );
    let event = AutomationEventKind::parse(rule.when.trim());
    match (event, rule.idle_secs) {
        (Some(AutomationEventKind::WorktreeIdle), Some(value)) => {
            bounded(errors, &format!("{key}.idle_secs"), value, 60, 86_400)
        }
        (Some(AutomationEventKind::WorktreeIdle), None) => errors.push(format!(
            "{key}.idle_secs: required for worktree_idle (60..86400)"
        )),
        (_, Some(_)) => errors.push(format!(
            "{key}.idle_secs: valid only when when = \"worktree_idle\""
        )),
        _ => {}
    }
    match (event, rule.schedule.as_deref(), rule.every.as_deref()) {
        (_, Some(_), Some(_)) => errors.push(format!(
            "{key}.schedule: schedule and every are mutually exclusive"
        )),
        (Some(AutomationEventKind::ScheduleDue), None, None) => errors.push(format!(
            "{key}.schedule: schedule_due requires schedule (cron) or every (interval)"
        )),
        (Some(AutomationEventKind::ScheduleDue), cron, every) => {
            if let Err(reason) = parse_schedule(cron, every) {
                let field = if cron.is_some() { "schedule" } else { "every" };
                errors.push(format!("{key}.{field}: {reason}"));
            }
        }
        (_, Some(_), None) => errors.push(format!(
            "{key}.schedule: valid only when when = \"schedule_due\""
        )),
        (_, None, Some(_)) => errors.push(format!(
            "{key}.every: valid only when when = \"schedule_due\""
        )),
        (_, None, None) => {}
    }
    if rule.then.cap.trim() == "pipeline.start"
        && event.is_some_and(|kind| kind != AutomationEventKind::IssueMatched)
    {
        errors.push(format!(
            "{key}.then.cap: pipeline.start needs an issue; use when = \"issue_matched\""
        ));
    }
    if let Some(declared) = rule.autonomy.as_deref() {
        match Autonomy::parse(declared.trim()) {
            None => errors.push(format!(
                "{key}.autonomy: unknown autonomy {declared:?}; expected propose, review, or merge"
            )),
            Some(level) => {
                if let Err(reason) = validate_autonomy(
                    level,
                    &compile_predicate(&rule.predicate),
                    rule.then.cap.trim(),
                    RuleOrigin::Config,
                ) {
                    errors.push(format!("{key}.autonomy: {reason}"));
                }
            }
        }
    }
    bounded(
        errors,
        &format!("{key}.max_per_hour"),
        rule.max_per_hour,
        1,
        1_000,
    );
    bounded(
        errors,
        &format!("{key}.max_action_per_hour"),
        rule.max_action_per_hour,
        1,
        1_000,
    );
    validate_predicate(&rule.predicate, key, errors);
    validate_action(&rule.then, key, errors);
}

fn validate_predicate(predicate: &AutomationPredicateConfig, key: &str, errors: &mut Vec<String>) {
    for (field, pattern) in [
        ("workspace", &predicate.workspace),
        ("repo", &predicate.repo),
        ("worktree", &predicate.worktree),
        ("branch", &predicate.branch),
    ] {
        if let Some(pattern) = pattern
            && let Err(reason) = validate_glob(pattern)
        {
            errors.push(format!("{key}.if.{field}: invalid glob: {reason}"));
        }
    }
    if let Some(priority) = &predicate.priority
        && Priority::parse(priority).is_none()
    {
        errors.push(format!(
            "{key}.if.priority: expected info, notice, or alert"
        ));
    }
    if let Some(kind) = &predicate.notification_kind
        && !NotificationKind::ALL
            .into_iter()
            .any(|candidate| candidate.as_str() == kind)
    {
        errors.push(format!(
            "{key}.if.notification_kind: unknown notification kind {kind:?}"
        ));
    }
    if let Some(pattern) = &predicate.message_regex {
        if pattern.len() > MAX_REGEX_LEN {
            errors.push(format!(
                "{key}.if.message_regex: must be at most {MAX_REGEX_LEN} bytes"
            ));
        } else if let Err(error) = regex::Regex::new(pattern) {
            errors.push(format!("{key}.if.message_regex: invalid regex: {error}"));
        }
    }
    for (field, value) in [
        ("agent_role", &predicate.agent_role),
        ("source_prefix", &predicate.source_prefix),
        ("session_id", &predicate.session_id),
        ("account", &predicate.account),
        ("project", &predicate.project),
        ("label", &predicate.label),
        ("assignee", &predicate.assignee),
        ("status", &predicate.status),
    ] {
        if value
            .as_ref()
            .is_some_and(|value| value.len() > MAX_NAME_LEN)
        {
            errors.push(format!(
                "{key}.if.{field}: must be at most {MAX_NAME_LEN} bytes"
            ));
        }
    }
}

/// The repository's glob vocabulary is deliberately small (`*` and `?`).
/// Reject shell/character-class syntax instead of accepting a pattern users
/// would reasonably expect to mean something else.
fn validate_glob(pattern: &str) -> Result<(), &'static str> {
    if pattern.is_empty() {
        return Err("must not be empty");
    }
    if pattern.len() > 1_024 {
        return Err("must be at most 1024 bytes");
    }
    if pattern.contains(['\n', '\r', '\0']) {
        return Err("must be one line");
    }
    if pattern.contains(['[', ']', '{', '}', '\\']) {
        return Err("only `*` and `?` wildcard syntax is supported");
    }
    Ok(())
}

fn validate_action(action: &AutomationActionConfig, key: &str, errors: &mut Vec<String>) {
    let cap = action.cap.trim();
    if !SUPPORTED_ACTION_CAPS.contains(&cap) {
        errors.push(format!(
            "{key}.then.cap: unsupported catalog action {:?}; expected one of {}",
            action.cap,
            SUPPORTED_ACTION_CAPS.join(", ")
        ));
        return;
    }
    let params = action.params();
    if params.len() > MAX_PARAMS {
        errors.push(format!(
            "{key}.then: at most {MAX_PARAMS} parameters are allowed"
        ));
    }
    let (allowed, required): (&[&str], &[&str]) = match cap {
        "sessions.open" => (&["agent", "prompt"], &["agent"]),
        "merge.add" => (&[], &[]),
        "notify.push" => (&["title", "body", "urgency"], &["body"]),
        "tools.run" => (&["name"], &["name"]),
        "merge.drain" => (&[], &[]),
        "pipeline.start" => (&["stage", "agent"], &[]),
        "lead.wake" => (&["prompt"], &["prompt"]),
        _ => unreachable!("cap checked above"),
    };
    for name in params.keys() {
        if !allowed.contains(&name.as_str()) {
            errors.push(format!("{key}.then.{name}: not valid for {cap}"));
        }
    }
    for name in required {
        if params
            .get(*name)
            .is_none_or(|value| value.trim().is_empty())
        {
            errors.push(format!("{key}.then.{name}: required for {cap}"));
        }
    }
    for (name, value) in &params {
        if value.len() > MAX_TEMPLATE_LEN {
            errors.push(format!(
                "{key}.then.{name}: must be at most {MAX_TEMPLATE_LEN} bytes"
            ));
            continue;
        }
        let checked = match (cap, name.as_str()) {
            ("sessions.open", "agent")
            | ("tools.run", "name")
            | ("pipeline.start", "stage" | "agent") => {
                if valid_configured_name(value) {
                    Ok(())
                } else {
                    Err("must be one configured name (letters, digits, `.`, `_`, `-`)".to_string())
                }
            }
            // The Lead's prompt may not splice untrusted event text.
            ("lead.wake", _) => validate_template(value, LEAD_WAKE_TEMPLATE_VARS, false)
                .map_err(|error| error.to_string()),
            _ => validate_template(value, EVENT_TEMPLATE_VARS, false)
                .map_err(|error| error.to_string()),
        };
        if let Err(error) = checked {
            errors.push(format!("{key}.then.{name}: {error}"));
        }
    }
    if cap == "notify.push"
        && let Some(urgency) = params.get("urgency")
        && Priority::parse(urgency).is_none()
    {
        errors.push(format!(
            "{key}.then.urgency: expected info, notice, or alert"
        ));
    }
}

fn valid_configured_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NAME_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn compile_rule(rule: &AutomationRuleConfig) -> AutomationRule {
    let event =
        AutomationEventKind::parse(rule.when.trim()).expect("compiled_rules validates event kinds");
    let predicate = compile_predicate(&rule.predicate);
    let cap = rule.then.cap.trim().to_string();
    let declared = rule
        .autonomy
        .as_deref()
        .and_then(|value| Autonomy::parse(value.trim()))
        .unwrap_or_default();
    let (autonomy, warning) = effective_autonomy(declared, &predicate, &cap, RuleOrigin::Config);
    if let Some(warning) = warning {
        tracing::warn!(
            target: "thegn::automation",
            rule = rule.name.trim(),
            declared = declared.as_str(),
            effective = autonomy.as_str(),
            "{warning}"
        );
    }
    AutomationRule {
        id: rule.name.trim().to_string(),
        enabled: rule.enabled,
        event,
        predicate,
        action: ActionTemplate {
            cap,
            params: rule.then.params(),
        },
        debounce_secs: rule.debounce_secs,
        idle_secs: rule.idle_secs,
        // One issue fires a rule once: the event key is the issue identity.
        once_per_key: rule.once_per_key || event == AutomationEventKind::IssueMatched,
        max_per_hour: rule.max_per_hour,
        max_action_per_hour: rule.max_action_per_hour,
        schedule: parse_schedule(rule.schedule.as_deref(), rule.every.as_deref())
            .ok()
            .flatten(),
        autonomy,
    }
}

fn compile_predicate(predicate: &AutomationPredicateConfig) -> AutomationPredicate {
    AutomationPredicate {
        workspace: predicate.workspace.clone(),
        repo: predicate.repo.clone(),
        worktree: predicate.worktree.clone(),
        branch: predicate.branch.clone(),
        agent_role: predicate.agent_role.clone(),
        notification_kind: predicate.notification_kind.as_deref().and_then(|value| {
            NotificationKind::ALL
                .into_iter()
                .find(|kind| kind.as_str() == value)
        }),
        min_priority: predicate.priority.as_deref().and_then(Priority::parse),
        source_prefix: predicate.source_prefix.clone(),
        message_regex: predicate.message_regex.clone(),
        session_id: predicate.session_id.clone(),
        pr_checks_passed: predicate.pr_checks_passed,
        pr_review_requested: predicate.pr_review_requested,
        pr_merged: predicate.pr_merged,
        account: predicate.account.clone(),
        project: predicate.project.clone(),
        label: predicate.label.clone(),
        assignee: predicate.assignee.clone(),
        status: predicate.status.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> AutomationsConfig {
        AutomationsConfig {
            enabled: true,
            rules: vec![AutomationRuleConfig {
                name: "blocked-coder".into(),
                when: "agent_needs_you".into(),
                predicate: AutomationPredicateConfig {
                    branch: Some("tg/*".into()),
                    priority: Some("alert".into()),
                    ..AutomationPredicateConfig::default()
                },
                then: AutomationActionConfig {
                    cap: "notify.push".into(),
                    title: Some("Coder needs attention".into()),
                    body: Some("{message}".into()),
                    urgency: Some("alert".into()),
                    ..AutomationActionConfig::default()
                },
                ..AutomationRuleConfig::default()
            }],
            ..AutomationsConfig::default()
        }
    }

    #[test]
    fn valid_config_compiles() {
        let rules = valid().compiled_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].event, AutomationEventKind::AgentNeedsYou);
        assert_eq!(rules[0].predicate.min_priority, Some(Priority::Alert));
    }

    #[test]
    fn requested_nested_toml_shape_parses_and_strict_validates() {
        let body = r#"
[automations]
enabled = true
max_concurrent = 2
queue_capacity = 64
action_timeout_secs = 30
audit_retention_per_rule = 50

[[automations.rules]]
name = "blocked-coder"
when = "agent_needs_you"
debounce_secs = 60
once_per_key = true
max_per_hour = 20
max_action_per_hour = 10

[automations.rules.if]
branch = "tg/*"
agent_role = "coder"
priority = "alert"

[automations.rules.then]
cap = "notify.push"
title = "Coder needs attention"
body = "{message}"
urgency = "alert"
"#;
        let cfg: crate::config::Config = toml::from_str(body).unwrap();
        assert_eq!(cfg.automations.rules.len(), 1);
        assert_eq!(cfg.automations.rules[0].then.cap, "notify.push");
        let errors = crate::config_validate::validate_str(body);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn rejects_duplicate_empty_unknown_and_out_of_bounds() {
        let mut cfg = valid();
        cfg.max_concurrent = 0;
        cfg.rules.push(cfg.rules[0].clone());
        cfg.rules.push(AutomationRuleConfig::default());
        let errors = cfg.validate().join("\n");
        assert!(errors.contains("max_concurrent"));
        assert!(errors.contains("duplicate"));
        assert!(errors.contains("must not be empty"));
        assert!(errors.contains("unknown event kind"));
    }

    #[test]
    fn rejects_globs_regex_templates_limits_and_ambiguous_params() {
        let mut cfg = valid();
        let rule = &mut cfg.rules[0];
        rule.predicate.branch = Some("[ab]*".into());
        rule.predicate.message_regex = Some("(".into());
        rule.max_per_hour = 0;
        rule.then.body = Some("{typo}".into());
        rule.then.agent = Some("unexpected".into());
        let errors = cfg.validate().join("\n");
        assert!(errors.contains("invalid glob"));
        assert!(errors.contains("invalid regex"));
        assert!(errors.contains("max_per_hour"));
        assert!(errors.contains("unknown placeholder"));
        assert!(errors.contains("agent: not valid for notify.push"));
    }

    #[test]
    fn rejects_oversized_message_regex() {
        let mut cfg = valid();
        cfg.rules[0].predicate.message_regex = Some("x".repeat(MAX_REGEX_LEN + 1));
        assert!(
            cfg.validate()
                .join("\n")
                .contains("message_regex: must be at most")
        );
    }

    #[test]
    fn named_actions_reject_templates_and_generic_escape_hatches() {
        let mut cfg = valid();
        cfg.rules[0].then = AutomationActionConfig {
            cap: "tools.run".into(),
            name: Some("{message}".into()),
            ..AutomationActionConfig::default()
        };
        assert!(cfg.validate().join("\n").contains("configured name"));
        cfg.rules[0].then.cap = "invoke".into();
        assert!(
            cfg.validate()
                .join("\n")
                .contains("unsupported catalog action")
        );
    }

    #[test]
    fn profile_overlay_inherits_settings_and_replaces_rules_only_when_present() {
        let mut cfg = valid();
        let original = cfg.rules.clone();
        AutomationsOverlay {
            max_concurrent: Some(4),
            ..AutomationsOverlay::default()
        }
        .apply(&mut cfg);
        assert_eq!(cfg.max_concurrent, 4);
        assert_eq!(cfg.rules, original);
        AutomationsOverlay {
            rules: Some(Vec::new()),
            ..AutomationsOverlay::default()
        }
        .apply(&mut cfg);
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn active_profile_is_trusted_but_repo_automation_is_only_warned() {
        let mut cfg = crate::config::Config {
            automations: valid(),
            profile: "work".into(),
            ..Default::default()
        };
        cfg.profiles.insert(
            "work".into(),
            crate::config::ProfileConfig {
                automations: AutomationsOverlay {
                    max_concurrent: Some(5),
                    ..AutomationsOverlay::default()
                },
                ..crate::config::ProfileConfig::default()
            },
        );
        assert_eq!(cfg.effective_automations().max_concurrent, 5);

        for (filename, body) in [
            (
                ".thegn.toml",
                "[automations]\nenabled = true\n[[automations.rules]]\nname = 'hostile'\n",
            ),
            (".thegn.yaml", "automations:\n  enabled: true\n"),
            (".thegn.json", r#"{"automations":{"enabled":true}}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(filename), body).unwrap();
            let warnings = cfg.repo_automation_warnings(dir.path());
            assert_eq!(warnings.len(), 1, "{filename}");
            assert!(warnings[0].contains(filename), "{filename}");
            assert!(warnings[0].contains("global/profile config only"));
        }
        // Detection never changes effective trusted rules.
        assert_eq!(cfg.effective_automations().rules[0].name, "blocked-coder");
    }

    fn rule_with(when: &str, then: AutomationActionConfig) -> AutomationsConfig {
        let mut cfg = valid();
        cfg.rules[0].when = when.into();
        cfg.rules[0].predicate = AutomationPredicateConfig::default();
        cfg.rules[0].then = then;
        cfg
    }

    fn notify() -> AutomationActionConfig {
        AutomationActionConfig {
            cap: "notify.push".into(),
            body: Some("{event_kind}".into()),
            ..AutomationActionConfig::default()
        }
    }

    #[test]
    fn schedule_and_every_are_exclusive_and_required_only_for_schedule_due() {
        let mut both = rule_with("schedule_due", notify());
        both.rules[0].schedule = Some("0 9 * * *".into());
        both.rules[0].every = Some("1h".into());
        assert!(both.validate().join("\n").contains("mutually exclusive"));

        let missing = rule_with("schedule_due", notify());
        assert!(
            missing
                .validate()
                .join("\n")
                .contains("schedule_due requires schedule")
        );

        let mut stray = rule_with("agent_finished", notify());
        stray.rules[0].schedule = Some("0 9 * * *".into());
        assert!(
            stray
                .validate()
                .join("\n")
                .contains(".schedule: valid only when")
        );
        stray.rules[0].schedule = None;
        stray.rules[0].every = Some("1h".into());
        assert!(
            stray
                .validate()
                .join("\n")
                .contains(".every: valid only when")
        );
    }

    #[test]
    fn pipeline_start_requires_issue_matched_and_compiles_once_per_key() {
        let start = AutomationActionConfig {
            cap: "pipeline.start".into(),
            stage: Some("code".into()),
            agent: Some("codex".into()),
            ..AutomationActionConfig::default()
        };
        let refused = rule_with("agent_finished", start.clone());
        assert!(
            refused
                .validate()
                .join("\n")
                .contains("pipeline.start needs an issue; use when = \"issue_matched\"")
        );
        let ok = rule_with("issue_matched", start.clone());
        let rules = ok.compiled_rules().unwrap();
        assert_eq!(rules[0].event, AutomationEventKind::IssueMatched);
        assert!(rules[0].once_per_key, "one issue fires a rule once");
        assert_eq!(rules[0].action.params["stage"], "code");

        let mut templated = start;
        templated.stage = Some("{message}".into());
        assert!(
            rule_with("issue_matched", templated)
                .validate()
                .join("\n")
                .contains("stage: must be one configured name")
        );
    }

    #[test]
    fn lead_wake_prompt_may_not_reference_message() {
        let wake = |prompt: &str| AutomationActionConfig {
            cap: "lead.wake".into(),
            prompt: Some(prompt.into()),
            ..AutomationActionConfig::default()
        };
        let refused = rule_with("issue_matched", wake("triage {message}"));
        assert!(refused.validate().join("\n").contains("then.prompt"));
        assert!(
            rule_with("issue_matched", wake("triage {issue_id} in {account}"))
                .validate()
                .is_empty()
        );
        let empty = rule_with(
            "issue_matched",
            AutomationActionConfig {
                cap: "lead.wake".into(),
                ..AutomationActionConfig::default()
            },
        );
        assert!(
            empty
                .validate()
                .join("\n")
                .contains("prompt: required for lead.wake")
        );
    }

    #[test]
    fn merge_drain_takes_no_params() {
        let drain = AutomationActionConfig {
            cap: "merge.drain".into(),
            ..AutomationActionConfig::default()
        };
        assert!(
            rule_with("merge_landed", drain.clone())
                .validate()
                .is_empty()
        );
        let mut extra = drain;
        extra.name = Some("x".into());
        assert!(
            rule_with("merge_landed", extra)
                .validate()
                .join("\n")
                .contains("name: not valid for merge.drain")
        );
    }

    #[test]
    fn autonomy_must_parse_and_defaults_to_review() {
        let mut cfg = valid();
        cfg.rules[0].autonomy = Some("yolo".into());
        assert!(
            cfg.validate()
                .join("\n")
                .contains("unknown autonomy \"yolo\"")
        );
        cfg.rules[0].autonomy = Some("propose".into());
        assert_eq!(cfg.compiled_rules().unwrap()[0].autonomy, Autonomy::Propose);
        cfg.rules[0].autonomy = None;
        assert_eq!(cfg.compiled_rules().unwrap()[0].autonomy, Autonomy::Review);
    }

    #[test]
    fn issue_selectors_compile_onto_the_predicate() {
        let mut cfg = valid();
        cfg.rules[0].predicate.account = Some("work".into());
        cfg.rules[0].predicate.project = Some("p1".into());
        cfg.rules[0].predicate.label = Some("bug".into());
        cfg.rules[0].predicate.assignee = Some("me".into());
        cfg.rules[0].predicate.status = Some("todo".into());
        let rule = &cfg.compiled_rules().unwrap()[0];
        assert_eq!(rule.predicate.account.as_deref(), Some("work"));
        assert_eq!(rule.predicate.project.as_deref(), Some("p1"));
        assert_eq!(rule.predicate.label.as_deref(), Some("bug"));
        assert_eq!(rule.predicate.assignee.as_deref(), Some("me"));
        assert_eq!(rule.predicate.status.as_deref(), Some("todo"));
        cfg.rules[0].predicate.label = Some("x".repeat(MAX_NAME_LEN + 1));
        assert!(
            cfg.validate()
                .join("\n")
                .contains("if.label: must be at most")
        );
    }

    #[test]
    fn new_rule_keys_parse_from_toml_and_strict_validate() {
        let body = r#"
[automations]
enabled = true

[[automations.rules]]
name = "triage"
when = "issue_matched"
autonomy = "propose"

[automations.rules.if]
account = "work"
project = "p1"
label = "bug"
assignee = "me"
status = "todo"

[automations.rules.then]
cap = "pipeline.start"
stage = "code"
"#;
        let cfg: crate::config::Config = toml::from_str(body).unwrap();
        let rule = &cfg.automations.rules[0];
        assert_eq!(rule.autonomy.as_deref(), Some("propose"));
        assert_eq!(rule.then.stage.as_deref(), Some("code"));
        assert_eq!(rule.predicate.assignee.as_deref(), Some("me"));
        let errors = crate::config_validate::validate_str(body);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn committed_fixtures_parse_and_session_exited_matches_a_notify_rule() {
        use crate::automation::{AutomationEvent, EvaluationDecision, EvaluationState, evaluate};
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/fixtures/automation");
        for name in ["session_exited.json", "inbox_item.json"] {
            let raw = std::fs::read_to_string(root.join(name)).unwrap();
            let event: AutomationEvent = serde_json::from_str(&raw).unwrap();
            event.validate_required_facts().unwrap();
        }
        let raw = std::fs::read_to_string(root.join("session_exited.json")).unwrap();
        let event: AutomationEvent = serde_json::from_str(&raw).unwrap();
        assert_eq!(event.kind, AutomationEventKind::SessionExited);
        let cfg = rule_with(
            "session_exited",
            AutomationActionConfig {
                cap: "notify.push".into(),
                body: Some("{session_id} exited {exit_code}".into()),
                ..AutomationActionConfig::default()
            },
        );
        let rules = cfg.compiled_rules().unwrap();
        let decisions = evaluate(&rules, &event, &EvaluationState::new(), event.occurred_at);
        match decisions.as_slice() {
            [EvaluationDecision::Planned(action)] => {
                assert_eq!(action.params["body"], "daemon-session-1 exited 0");
            }
            other => panic!("expected one plan, got {other:?}"),
        }
    }
}
