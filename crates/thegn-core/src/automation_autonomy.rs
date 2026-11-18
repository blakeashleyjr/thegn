//! Autonomy ceilings for automation rules (THE-2195).
//!
//! A rule declares how far an unattended run may go: `propose` (raise an
//! approval, start nothing), `review` (start the pipeline; a human lands) or
//! `merge` (the pipeline may land). The declared level is capped by policy
//! before it is stored on the compiled rule, so every consumer reads the
//! effective ceiling, never the request.
//!
//! The policy (filled by b153 lane 4):
//! - `merge` requires a structural-only `if`. The only free-text predicate is
//!   `message_regex`; a config rule using it is capped at `review` with a
//!   warning, and a Lead rule asking for `merge` is refused.
//! - An `origin = lead` rule is never `merge`, and `pipeline.start` from a
//!   Lead rule runs at `propose` only.

use serde::{Deserialize, Serialize};

use crate::automation::AutomationPredicate;

/// How far an automation run may go without a human. Ordered: `Propose` <
/// `Review` < `Merge`, so `min` is "the stricter of".
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Autonomy {
    /// No worktree, no spawn: raise an approval request only.
    Propose,
    /// Start the work; a human reviews and lands it.
    #[default]
    Review,
    /// Start the work and let the pipeline land it.
    Merge,
}

impl Autonomy {
    pub const ALL: [Self; 3] = [Self::Propose, Self::Review, Self::Merge];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Propose => "propose",
            Self::Review => "review",
            Self::Merge => "merge",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|level| level.as_str() == s)
    }
}

/// Who authored a rule. Lead-authored rules are untrusted relative to config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleOrigin {
    Config,
    Lead,
}

/// Whether the predicate reads attacker-influenced free text.
#[allow(dead_code, reason = "filled by b153 lane 4")]
pub fn uses_free_text(p: &AutomationPredicate) -> bool {
    p.message_regex.is_some()
}

/// The ceiling a rule actually runs at, plus a warning when policy lowered
/// the declared level.
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 4")]
pub fn effective_autonomy(
    declared: Autonomy,
    p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> (Autonomy, Option<&'static str>) {
    (declared.min(Autonomy::Review), None)
}

/// Refuse a declaration policy forbids outright (checked at put/validate).
#[allow(dead_code, unused_variables, reason = "filled by b153 lane 4")]
pub fn validate_autonomy(
    declared: Autonomy,
    p: &AutomationPredicate,
    cap: &str,
    origin: RuleOrigin,
) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_names_round_trip_and_default_is_review() {
        for level in Autonomy::ALL {
            assert_eq!(Autonomy::parse(level.as_str()), Some(level));
            assert_eq!(
                serde_json::to_string(&level).unwrap(),
                format!("\"{}\"", level.as_str())
            );
        }
        assert_eq!(Autonomy::parse("Merge"), None);
        assert_eq!(Autonomy::parse("yolo"), None);
        assert_eq!(Autonomy::default(), Autonomy::Review);
        assert!(Autonomy::Propose < Autonomy::Review && Autonomy::Review < Autonomy::Merge);
    }
}
