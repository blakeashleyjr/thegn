//! Per-token cost estimation.
//!
//! Port of the pricing/estimate half of `metrics.go` (`costForBackend`,
//! `estimatedCost`, `pricingFor`, `fallbackPricing`). The Go original mixes in
//! HTTP-header cost extraction (`costFromHeaders`) and env-driven pricing
//! overrides; those stay in the `superzej-proxy` I/O layer, which constructs a
//! [`PriceTable`] from config and calls the pure logic here.
//!
//! Only "cost-bearing" backends (paid per-token lanes) incur a charge; every
//! other backend is covered by a flat subscription/OAuth login and logs as $0
//! with source `"subscription"`, so the cost graph reflects only marginal spend.

use std::collections::HashMap;

use crate::proxy::creds::provider_base;

/// USD per 1M tokens for a given (backend, model).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PricePoint {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
}

/// Token usage observed for a response.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
    pub fn is_empty(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0
    }
}

/// Where a logged cost figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    /// Covered by a flat subscription/OAuth login — no marginal cost.
    Subscription,
    /// Reported by an upstream cost header.
    Header,
    /// Computed from the pricing table.
    Estimate,
    /// Cost-bearing but no usage and no pricing entry.
    Unknown,
}

impl CostSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CostSource::Subscription => "subscription",
            CostSource::Header => "header",
            CostSource::Estimate => "estimate",
            CostSource::Unknown => "unknown",
        }
    }
}

/// Pricing lookup: a set of cost-bearing provider names plus a per-key price
/// table, with optional config overrides layered on by the caller.
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    cost_bearing: std::collections::HashSet<String>,
    prices: HashMap<String, PricePoint>,
}

impl PriceTable {
    /// Builds the default table matching the Go `costBearingBackends` +
    /// `fallbackPricing`. Only `openrouter` and `kilo` are cost-bearing.
    pub fn with_defaults() -> Self {
        let mut t = PriceTable::default();
        t.cost_bearing.insert("openrouter".into());
        t.cost_bearing.insert("kilo".into());
        // The only entries that actually matter are the cost-bearing lanes; the
        // $0 subscription rows in Go are informational. We keep the paid rows.
        t.set_price(
            "kilo:deepseek/deepseek-v4-pro",
            PricePoint {
                input_usd_per_mtok: 0.27,
                output_usd_per_mtok: 1.1,
            },
        );
        t.set_price(
            "openrouter:deepseek/deepseek-v4-pro",
            PricePoint {
                input_usd_per_mtok: 0.27,
                output_usd_per_mtok: 1.1,
            },
        );
        t.set_price(
            "openrouter:deepseek/deepseek-v4-flash",
            PricePoint {
                input_usd_per_mtok: 0.05,
                output_usd_per_mtok: 0.2,
            },
        );
        t
    }

    /// Marks a provider base (e.g. `"openrouter"`) as cost-bearing.
    pub fn add_cost_bearing(&mut self, provider: impl Into<String>) {
        self.cost_bearing.insert(provider.into());
    }

    /// Sets the price for a `"backend:model"` key (or a bare `"model"` key).
    pub fn set_price(&mut self, key: impl Into<String>, point: PricePoint) {
        self.prices.insert(key.into(), point);
    }

    fn is_cost_bearing(&self, backend: &str) -> bool {
        self.cost_bearing.contains(provider_base(backend))
    }

    /// Looks up pricing for (backend, model), trying `"backend:model"` then the
    /// bare `"model"` key. The backend half is reduced to its provider base so a
    /// single pricing entry applies across all of a provider's keyed lanes
    /// (e.g. `"openrouter#1"` prices off the `"openrouter:…"` entry). Mirrors
    /// `pricingFor`'s override fallthrough plus the keyed-lane intent.
    fn pricing_for(&self, backend: &str, model: &str) -> Option<PricePoint> {
        let key = format!("{}:{}", provider_base(backend), model);
        self.prices
            .get(&key)
            .or_else(|| self.prices.get(model))
            .copied()
    }
}

/// Computes the cost to log for a served response. A non-cost-bearing lane is
/// always `(0.0, Subscription)`. A cost-bearing lane prefers `header_cost` (when
/// the I/O layer extracted one), else estimates from the table. Mirrors
/// `costForBackend` + `estimatedCost`.
pub fn cost_usd(
    table: &PriceTable,
    backend: &str,
    model: &str,
    usage: Usage,
    header_cost: Option<f64>,
) -> (f64, CostSource) {
    if !table.is_cost_bearing(backend) {
        return (0.0, CostSource::Subscription);
    }
    if let Some(c) = header_cost {
        return (c, CostSource::Header);
    }
    if usage.is_empty() {
        return (0.0, CostSource::Unknown);
    }
    if let Some(pp) = table.pricing_for(backend, model) {
        let cost = (usage.prompt_tokens as f64 * pp.input_usd_per_mtok
            + usage.completion_tokens as f64 * pp.output_usd_per_mtok)
            / 1_000_000.0;
        return (cost, CostSource::Estimate);
    }
    (0.0, CostSource::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_cost_bearing_is_subscription() {
        let t = PriceTable::with_defaults();
        let (c, src) = cost_usd(
            &t,
            "codex",
            "gpt-5.5",
            Usage {
                prompt_tokens: 1000,
                completion_tokens: 1000,
            },
            None,
        );
        assert_eq!(c, 0.0);
        assert_eq!(src, CostSource::Subscription);
    }

    #[test]
    fn header_cost_preferred_for_paid_lane() {
        let t = PriceTable::with_defaults();
        let (c, src) = cost_usd(
            &t,
            "openrouter",
            "deepseek/deepseek-v4-pro",
            Usage::default(),
            Some(0.42),
        );
        assert_eq!(c, 0.42);
        assert_eq!(src, CostSource::Header);
    }

    #[test]
    fn estimate_from_table() {
        let t = PriceTable::with_defaults();
        // 1M input @ 0.27, 1M output @ 1.1 → 1.37 USD.
        let (c, src) = cost_usd(
            &t,
            "openrouter",
            "deepseek/deepseek-v4-pro",
            Usage {
                prompt_tokens: 1_000_000,
                completion_tokens: 1_000_000,
            },
            None,
        );
        assert!((c - 1.37).abs() < 1e-9);
        assert_eq!(src, CostSource::Estimate);
    }

    #[test]
    fn paid_lane_with_no_usage_is_unknown() {
        let t = PriceTable::with_defaults();
        let (c, src) = cost_usd(
            &t,
            "kilo",
            "deepseek/deepseek-v4-pro",
            Usage::default(),
            None,
        );
        assert_eq!(c, 0.0);
        assert_eq!(src, CostSource::Unknown);
    }

    #[test]
    fn keyed_lane_uses_provider_base() {
        let t = PriceTable::with_defaults();
        // "openrouter#1" is a multi-key lane; provider_base strips the suffix.
        let (_, src) = cost_usd(
            &t,
            "openrouter#1",
            "deepseek/deepseek-v4-flash",
            Usage {
                prompt_tokens: 100,
                completion_tokens: 100,
            },
            None,
        );
        assert_eq!(src, CostSource::Estimate);
    }

    #[test]
    fn usage_helpers_and_source_strings() {
        let u = Usage {
            prompt_tokens: 3,
            completion_tokens: 4,
        };
        assert_eq!(u.total(), 7);
        assert!(!u.is_empty());
        assert!(Usage::default().is_empty());
        assert_eq!(CostSource::Subscription.as_str(), "subscription");
        assert_eq!(CostSource::Header.as_str(), "header");
        assert_eq!(CostSource::Estimate.as_str(), "estimate");
        assert_eq!(CostSource::Unknown.as_str(), "unknown");
    }

    #[test]
    fn config_override_via_bare_model_key() {
        let mut t = PriceTable::default();
        t.add_cost_bearing("custom");
        t.set_price(
            "super-model",
            PricePoint {
                input_usd_per_mtok: 2.0,
                output_usd_per_mtok: 4.0,
            },
        );
        let (c, src) = cost_usd(
            &t,
            "custom",
            "super-model",
            Usage {
                prompt_tokens: 1_000_000,
                completion_tokens: 0,
            },
            None,
        );
        assert!((c - 2.0).abs() < 1e-9);
        assert_eq!(src, CostSource::Estimate);
    }
}
