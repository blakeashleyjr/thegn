//! Pure model-proxy budget-cap classification shared by enforcement and alerts.
//!
//! The proxy owns spend accounting; this module only compares persisted state
//! with configured caps. It deliberately ignores the manual kill-switch, which
//! is an enforcement control rather than a configured cap breach.

use crate::config_model_proxy::BudgetConfig;
use crate::store::ModelProxyBudgetStateRow;

/// The configured cap dimension reached by a spend accumulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetDimension {
    Tokens,
    Cost,
}

impl BudgetDimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::Cost => "cost",
        }
    }
}

/// The dimensions whose configured caps have been reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapBreaches {
    pub tokens: bool,
    pub cost: bool,
}

impl CapBreaches {
    pub fn any(self) -> bool {
        self.tokens || self.cost
    }
}

/// Compare spend with optional caps. Equality is a breach, matching the proxy's
/// pre-routing refusal boundary.
pub fn cap_breaches(
    spent_tokens: i64,
    spent_cost: f64,
    token_cap: Option<i64>,
    cost_cap: Option<f64>,
) -> CapBreaches {
    CapBreaches {
        tokens: token_cap.is_some_and(|cap| spent_tokens >= cap),
        cost: cost_cap.is_some_and(|cap| spent_cost >= cap),
    }
}

/// Whether an accumulator belongs to a completed half-open rolling window.
/// A zero-length window is cumulative and therefore never lapses.
pub fn window_lapsed(window_len_ms: i64, window_start_ms: i64, now_ms: i64) -> bool {
    window_len_ms > 0 && now_ms.saturating_sub(window_start_ms) >= window_len_ms
}

/// One stable alertable cap fact. A row may produce one fact per dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetBreachFact {
    pub scope: String,
    pub window_start_ms: i64,
    pub spent_tokens: i64,
    pub spent_cost: f64,
    pub dimension: BudgetDimension,
}

/// Classify current accumulator rows against an enabled budget configuration.
///
/// Results are sorted by scope and then dimension, independent of DB row order.
/// Unknown scopes, disabled caps, killed-only rows, and lapsed windows produce
/// no facts.
pub fn classify_breaches(
    config: &BudgetConfig,
    rows: &[ModelProxyBudgetStateRow],
    now_ms: i64,
) -> Vec<BudgetBreachFact> {
    if !config.enabled {
        return Vec::new();
    }

    let window_len_ms = config.window_len_ms();
    let mut facts = Vec::new();
    for row in rows {
        let Some(limit) = config.scopes.get(&row.scope) else {
            continue;
        };
        if window_lapsed(window_len_ms, row.window_start_ms, now_ms) {
            continue;
        }
        let crossed = cap_breaches(
            row.spent_tokens,
            row.spent_cost,
            limit.tokens,
            limit.cost_usd,
        );
        for dimension in [BudgetDimension::Tokens, BudgetDimension::Cost] {
            let reached = match dimension {
                BudgetDimension::Tokens => crossed.tokens,
                BudgetDimension::Cost => crossed.cost,
            };
            if reached {
                facts.push(BudgetBreachFact {
                    scope: row.scope.clone(),
                    window_start_ms: row.window_start_ms,
                    spent_tokens: row.spent_tokens,
                    spent_cost: row.spent_cost,
                    dimension,
                });
            }
        }
    }
    facts.sort_by(|a, b| {
        (&a.scope, a.window_start_ms, a.dimension).cmp(&(&b.scope, b.window_start_ms, b.dimension))
    });
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_model_proxy::BudgetLimit;

    fn config(scopes: &[(&str, Option<i64>, Option<f64>)]) -> BudgetConfig {
        BudgetConfig {
            enabled: true,
            scopes: scopes
                .iter()
                .map(|(scope, tokens, cost_usd)| {
                    (
                        (*scope).to_string(),
                        BudgetLimit {
                            tokens: *tokens,
                            cost_usd: *cost_usd,
                        },
                    )
                })
                .collect(),
            ..BudgetConfig::default()
        }
    }

    fn row(scope: &str, tokens: i64, cost: f64) -> ModelProxyBudgetStateRow {
        ModelProxyBudgetStateRow {
            scope: scope.into(),
            window_start_ms: 100,
            spent_tokens: tokens,
            spent_cost: cost,
            killed: false,
        }
    }

    #[test]
    fn disabled_budget_and_disabled_caps_are_ignored() {
        let r = row("global", 100, 10.0);
        assert!(classify_breaches(&BudgetConfig::default(), &[r.clone()], 100).is_empty());
        assert!(classify_breaches(&config(&[("global", None, None)]), &[r], 100).is_empty());
    }

    #[test]
    fn token_and_cost_caps_classify_independently() {
        let r = row("global", 10, 2.0);
        let tokens = classify_breaches(&config(&[("global", Some(10), None)]), &[r.clone()], 100);
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].dimension, BudgetDimension::Tokens);

        let cost = classify_breaches(&config(&[("global", None, Some(2.0))]), &[r.clone()], 100);
        assert_eq!(cost.len(), 1);
        assert_eq!(cost[0].dimension, BudgetDimension::Cost);

        let both = classify_breaches(&config(&[("global", Some(10), Some(2.0))]), &[r], 100);
        assert_eq!(
            both.iter().map(|f| f.dimension).collect::<Vec<_>>(),
            vec![BudgetDimension::Tokens, BudgetDimension::Cost]
        );
    }

    #[test]
    fn equality_is_a_breach_and_kill_switch_does_not_create_one() {
        assert_eq!(
            cap_breaches(10, 1.5, Some(10), Some(1.5)),
            CapBreaches {
                tokens: true,
                cost: true
            }
        );
        let mut killed = row("global", 0, 0.0);
        killed.killed = true;
        assert!(classify_breaches(&config(&[("global", None, None)]), &[killed], 100).is_empty());
    }

    #[test]
    fn rolling_window_is_half_open_and_cumulative_never_lapses() {
        let mut rolling = config(&[("global", Some(1), None)]);
        rolling.window_secs = 1;
        let r = row("global", 1, 0.0);
        assert_eq!(classify_breaches(&rolling, &[r.clone()], 1_099).len(), 1);
        assert!(classify_breaches(&rolling, &[r.clone()], 1_100).is_empty());

        let cumulative = config(&[("global", Some(1), None)]);
        assert_eq!(classify_breaches(&cumulative, &[r], i64::MAX).len(), 1);
    }

    #[test]
    fn unrelated_scopes_are_ignored_and_facts_are_deterministic() {
        let cfg = config(&[
            ("agent:a", Some(1), Some(1.0)),
            ("worktree:/z", Some(1), None),
        ]);
        let rows = [
            row("worktree:/z", 2, 0.0),
            row("unknown", 100, 100.0),
            row("agent:a", 2, 2.0),
        ];
        let facts = classify_breaches(&cfg, &rows, 100);
        assert_eq!(
            facts
                .iter()
                .map(|f| (f.scope.as_str(), f.window_start_ms, f.dimension))
                .collect::<Vec<_>>(),
            vec![
                ("agent:a", 100, BudgetDimension::Tokens),
                ("agent:a", 100, BudgetDimension::Cost),
                ("worktree:/z", 100, BudgetDimension::Tokens),
            ]
        );
    }
}
