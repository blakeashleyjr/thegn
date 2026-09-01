//! Trusted `[automations]` configuration and semantic validation.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agent_task::validate_template;
use crate::automation::{
    ActionTemplate, AutomationEventKind, AutomationPredicate, AutomationRule, EVENT_TEMPLATE_VARS,
    SUPPORTED_ACTION_CAPS,
};
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
    pub debounce_secs: u64,
    /// Required only for `when = "worktree_idle"`; independent of debounce.
    pub idle_secs: Option<u64>,
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
        if matches!(
            (cap, name.as_str()),
            ("sessions.open", "agent") | ("tools.run", "name")
        ) {
            if !valid_configured_name(value) {
                errors.push(format!(
                    "{key}.then.{name}: must be one configured name (letters, digits, `.`, `_`, `-`)"
                ));
            }
        } else if let Err(error) = validate_template(value, EVENT_TEMPLATE_VARS, false) {
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
    AutomationRule {
        id: rule.name.trim().to_string(),
        enabled: rule.enabled,
        event: AutomationEventKind::parse(rule.when.trim())
            .expect("compiled_rules validates event kinds"),
        predicate: AutomationPredicate {
            workspace: rule.predicate.workspace.clone(),
            repo: rule.predicate.repo.clone(),
            worktree: rule.predicate.worktree.clone(),
            branch: rule.predicate.branch.clone(),
            agent_role: rule.predicate.agent_role.clone(),
            notification_kind: rule
                .predicate
                .notification_kind
                .as_deref()
                .and_then(|value| {
                    NotificationKind::ALL
                        .into_iter()
                        .find(|kind| kind.as_str() == value)
                }),
            min_priority: rule.predicate.priority.as_deref().and_then(Priority::parse),
            source_prefix: rule.predicate.source_prefix.clone(),
            message_regex: rule.predicate.message_regex.clone(),
            session_id: rule.predicate.session_id.clone(),
            pr_checks_passed: rule.predicate.pr_checks_passed,
            pr_review_requested: rule.predicate.pr_review_requested,
            pr_merged: rule.predicate.pr_merged,
        },
        action: ActionTemplate {
            cap: rule.then.cap.trim().to_string(),
            params: rule.then.params(),
        },
        debounce_secs: rule.debounce_secs,
        idle_secs: rule.idle_secs,
        once_per_key: rule.once_per_key,
        max_per_hour: rule.max_per_hour,
        max_action_per_hour: rule.max_action_per_hour,
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
        let mut cfg = crate::config::Config::default();
        cfg.automations = valid();
        cfg.profile = "work".into();
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
}
