//! Host-edge formatting and routing facts for model-proxy budget-cap alerts.

use thegn_core::budget_alert::BudgetBreachFact;
use thegn_core::notification::NotificationKind;

/// A bounded notification projection: exactly one stable fact per breached
/// scope/window/dimension, with no changing spend value in its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BudgetNotification {
    pub kind: &'static str,
    pub source_ref: String,
    pub message: String,
    pub worktree: String,
}

pub(crate) fn notification(fact: &BudgetBreachFact) -> BudgetNotification {
    let dimension = fact.dimension.as_str();
    let scope = if fact.scope.is_empty() {
        "unknown scope"
    } else {
        fact.scope.as_str()
    };
    BudgetNotification {
        kind: NotificationKind::UsageLimit.as_str(),
        source_ref: format!(
            "model-proxy-budget:{}:{}:{dimension}",
            fact.scope, fact.window_start_ms
        ),
        message: format!("Model-proxy {dimension} budget reached for {scope}"),
        worktree: fact
            .scope
            .strip_prefix("worktree:")
            .filter(|path| !path.is_empty())
            .unwrap_or("")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::budget_alert::BudgetDimension;

    fn fact(scope: &str, dimension: BudgetDimension) -> BudgetBreachFact {
        BudgetBreachFact {
            scope: scope.into(),
            window_start_ms: 1234,
            spent_tokens: 99,
            spent_cost: 1.25,
            dimension,
        }
    }

    #[test]
    fn source_key_is_stable_and_dimension_specific() {
        let token = notification(&fact("agent:alice", BudgetDimension::Tokens));
        let cost = notification(&fact("agent:alice", BudgetDimension::Cost));
        assert_eq!(token.kind, "usage_limit");
        assert_eq!(
            token.source_ref,
            "model-proxy-budget:agent:alice:1234:tokens"
        );
        assert_eq!(cost.source_ref, "model-proxy-budget:agent:alice:1234:cost");
        assert_eq!(
            notification(&fact("agent:alice", BudgetDimension::Tokens)),
            token
        );
    }

    #[test]
    fn message_distinguishes_token_and_cost_caps() {
        assert_eq!(
            notification(&fact("global", BudgetDimension::Tokens)).message,
            "Model-proxy tokens budget reached for global"
        );
        assert_eq!(
            notification(&fact("global", BudgetDimension::Cost)).message,
            "Model-proxy cost budget reached for global"
        );
    }

    #[test]
    fn only_nonempty_worktree_scopes_route_to_a_worktree() {
        assert_eq!(
            notification(&fact("worktree:/repo/wt", BudgetDimension::Tokens)).worktree,
            "/repo/wt"
        );
        assert!(
            notification(&fact("worktree:", BudgetDimension::Tokens))
                .worktree
                .is_empty()
        );
        assert!(
            notification(&fact("workspace:/repo", BudgetDimension::Tokens))
                .worktree
                .is_empty()
        );
    }

    #[test]
    fn empty_and_unknown_scopes_degrade_safely() {
        let empty = notification(&fact("", BudgetDimension::Cost));
        assert_eq!(
            empty.message,
            "Model-proxy cost budget reached for unknown scope"
        );
        assert!(empty.worktree.is_empty());

        let unknown = notification(&fact("custom", BudgetDimension::Tokens));
        assert_eq!(
            unknown.message,
            "Model-proxy tokens budget reached for custom"
        );
        assert!(unknown.worktree.is_empty());
    }
}
