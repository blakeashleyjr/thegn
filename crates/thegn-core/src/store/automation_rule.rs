//! Lead-authored automation rule persistence seam (schema v111, THE-2181).
//!
//! Config rules are never stored here: they live in trusted config and are
//! read-only to the control API. Every row is `origin = 'lead'`.

use anyhow::Result;

/// One stored Lead-authored rule. `body_json` is the normalized
/// `AutomationRuleConfig`; `enabled` is the effective switch.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct LeadRuleRow {
    pub id: String,
    pub body_json: String,
    pub enabled: bool,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `put_lead_rule` refused a new row because [`crate::automation_authoring::LEAD_RULES_MAX`]
/// rules already exist. Replacing an existing rule is always allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeadRuleLimitReached;

impl std::fmt::Display for LeadRuleLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "at most {} Lead-authored automation rules may exist; remove one first",
            crate::automation_authoring::LEAD_RULES_MAX
        )
    }
}

impl std::error::Error for LeadRuleLimitReached {}

/// Object-safe synchronous store. Callers keep SQLite work off the render loop.
pub trait AutomationRuleStore {
    /// Insert or replace a rule body. Returns whether the row was created.
    /// Replacing keeps `enabled` and `created_at`; creating past the bound
    /// fails with [`LeadRuleLimitReached`].
    fn put_lead_rule(&self, id: &str, body_json: &str, author: &str, now: i64) -> Result<bool>;

    /// Delete a rule. Returns whether a row existed. Throttle state and audit
    /// rows are kept, so delete-and-recreate cannot reset a rate limit.
    fn delete_lead_rule(&self, id: &str) -> Result<bool>;

    /// Set the effective switch. Returns whether a row existed.
    fn set_lead_rule_enabled(&self, id: &str, enabled: bool) -> Result<bool>;

    /// Every stored rule, oldest first, bounded at
    /// [`crate::automation_authoring::LEAD_RULES_MAX`].
    fn lead_rules(&self) -> Result<Vec<LeadRuleRow>>;
}
