//! The `auto` tier classifier (U 282, auto-downgrade).
//!
//! When a client requests the reserved `auto` model id, the proxy must pick a
//! concrete tier from the *request's features alone* — no model call, no learned
//! heuristic, fully deterministic and reproducible. This module is that pure
//! decision; the daemon feeds it the tiers declared in `[[model_proxy.routes]]`
//! (each with an optional `auto_max_tokens` bound) plus the request's estimated
//! prompt size, tool presence, streaming flag, and an optional client mode hint
//! (`x-thegn-mode`), and gets back a tier name.
//!
//! Precedence is enforced by the caller: an explicit route name or alias match
//! is resolved *before* `auto` is consulted, so this classifier only ever runs
//! for the `auto` target.

/// A tier the `auto` classifier may pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoTier {
    /// The route/tier name (e.g. `fast`, `standard`, `heavy`).
    pub name: String,
    /// Upper bound of estimated prompt tokens this tier will accept. `None`
    /// means unbounded (the catch-all heavy tier).
    pub max_tokens: Option<usize>,
}

/// The request features the classifier decides from. Deterministic inputs only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestFeatures {
    /// Estimated prompt tokens (`transform::estimated_request_tokens`).
    pub est_tokens: usize,
    /// Whether the request carries a non-empty `tools` array.
    pub has_tools: bool,
    /// Whether the request asked for a streamed response.
    pub streaming: bool,
    /// Optional `x-thegn-mode` client hint naming a tier directly.
    pub mode_hint: Option<String>,
}

/// A tool-bearing request is treated as needing this much extra headroom, so a
/// small-but-tool-using turn escalates one tier (tool loops are heavier than
/// their prompt size suggests).
const TOOL_TOKEN_BUMP: usize = 8192;

/// Picks a tier for the `auto` target from `tiers` and request `features`.
///
/// 1. An `x-thegn-mode` hint that names a tier (case-insensitive) wins outright.
/// 2. Otherwise the smallest-capacity tier whose `max_tokens` bound admits the
///    effective token count (estimated tokens, bumped for tool use) is chosen;
///    ties on capacity break by name so the result is reproducible.
/// 3. If every bounded tier is too small, the largest-capacity tier is used
///    (unbounded tiers count as largest), so a giant prompt still routes.
///
/// Returns `None` only when `tiers` is empty.
pub fn route_select(tiers: &[AutoTier], features: &RequestFeatures) -> Option<String> {
    if tiers.is_empty() {
        return None;
    }
    // 1. Explicit mode hint that names a real tier.
    if let Some(hint) = features.mode_hint.as_deref() {
        let hint = hint.trim();
        if !hint.is_empty()
            && let Some(t) = tiers.iter().find(|t| t.name.eq_ignore_ascii_case(hint))
        {
            return Some(t.name.clone());
        }
    }

    let effective = features.est_tokens.saturating_add(if features.has_tools {
        TOOL_TOKEN_BUMP
    } else {
        0
    });

    // 2. Smallest tier that admits `effective`. `None` bound == unbounded ==
    //    admits everything (treated as maximal capacity). Ties on capacity break
    //    by name for determinism.
    let capacity = |t: &AutoTier| t.max_tokens.unwrap_or(usize::MAX);
    let admits: Vec<&AutoTier> = tiers.iter().filter(|t| capacity(t) >= effective).collect();
    if let Some(best) = admits
        .iter()
        .min_by(|a, b| capacity(a).cmp(&capacity(b)).then(a.name.cmp(&b.name)))
    {
        return Some(best.name.clone());
    }

    // 3. Nothing admits it — fall to the largest-capacity tier.
    tiers
        .iter()
        .max_by(|a, b| {
            capacity(a)
                .cmp(&capacity(b))
                .then_with(|| b.name.cmp(&a.name))
        })
        .map(|t| t.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers() -> Vec<AutoTier> {
        vec![
            AutoTier {
                name: "fast".into(),
                max_tokens: Some(8_000),
            },
            AutoTier {
                name: "standard".into(),
                max_tokens: Some(64_000),
            },
            AutoTier {
                name: "heavy".into(),
                max_tokens: None, // unbounded catch-all
            },
        ]
    }

    #[test]
    fn empty_tiers_yields_none() {
        assert_eq!(route_select(&[], &RequestFeatures::default()), None);
    }

    #[test]
    fn small_prompt_picks_fast() {
        let f = RequestFeatures {
            est_tokens: 500,
            ..Default::default()
        };
        assert_eq!(route_select(&tiers(), &f).as_deref(), Some("fast"));
    }

    #[test]
    fn medium_prompt_picks_standard() {
        let f = RequestFeatures {
            est_tokens: 20_000,
            ..Default::default()
        };
        assert_eq!(route_select(&tiers(), &f).as_deref(), Some("standard"));
    }

    #[test]
    fn huge_prompt_falls_to_heavy() {
        let f = RequestFeatures {
            est_tokens: 500_000,
            ..Default::default()
        };
        assert_eq!(route_select(&tiers(), &f).as_deref(), Some("heavy"));
    }

    #[test]
    fn tools_escalate_a_small_prompt() {
        // 4000 tokens alone → fast; with tools (+8192) → 12192 → standard.
        let f = RequestFeatures {
            est_tokens: 4_000,
            has_tools: true,
            ..Default::default()
        };
        assert_eq!(route_select(&tiers(), &f).as_deref(), Some("standard"));
    }

    #[test]
    fn mode_hint_wins_over_size() {
        let f = RequestFeatures {
            est_tokens: 500, // would be fast
            mode_hint: Some("HEAVY".into()),
            ..Default::default()
        };
        assert_eq!(route_select(&tiers(), &f).as_deref(), Some("heavy"));
    }

    #[test]
    fn unknown_mode_hint_is_ignored() {
        let f = RequestFeatures {
            est_tokens: 500,
            mode_hint: Some("does-not-exist".into()),
            ..Default::default()
        };
        // Falls back to size-based selection.
        assert_eq!(route_select(&tiers(), &f).as_deref(), Some("fast"));
    }

    #[test]
    fn is_deterministic() {
        let f = RequestFeatures {
            est_tokens: 30_000,
            has_tools: true,
            streaming: true,
            mode_hint: None,
        };
        let a = route_select(&tiers(), &f);
        let b = route_select(&tiers(), &f);
        assert_eq!(a, b);
        assert_eq!(a.as_deref(), Some("standard"));
    }

    #[test]
    fn all_bounded_too_small_picks_largest_bound() {
        let tiers = vec![
            AutoTier {
                name: "a".into(),
                max_tokens: Some(100),
            },
            AutoTier {
                name: "b".into(),
                max_tokens: Some(200),
            },
        ];
        let f = RequestFeatures {
            est_tokens: 10_000,
            ..Default::default()
        };
        assert_eq!(route_select(&tiers, &f).as_deref(), Some("b"));
    }

    #[test]
    fn equal_capacity_breaks_by_name() {
        let tiers = vec![
            AutoTier {
                name: "zeta".into(),
                max_tokens: Some(10_000),
            },
            AutoTier {
                name: "alpha".into(),
                max_tokens: Some(10_000),
            },
        ];
        let f = RequestFeatures {
            est_tokens: 100,
            ..Default::default()
        };
        assert_eq!(route_select(&tiers, &f).as_deref(), Some("alpha"));
    }
}
