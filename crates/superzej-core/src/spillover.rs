//! **Spillover** — the placement engine's last paid lane: when the owned pool
//! (packed / dedicated / autoscale) is exhausted, hand the worktree to an
//! external sandbox vendor via an ordered list of provider-placement envs
//! (`[placement] spillover_envs`), each riding the EXISTING provider pipeline
//! (adapters, pool, checkpoints, exec) untouched. This module is the pure
//! choice-and-health half, mirroring the proxy router's exhaustion shape
//! (reusing [`crate::proxy::backoff`]): a payment failure marks a provider
//! budget-dead until the compute ledger clears it (surviving restarts via
//! `placement_health` under `provider:` keys), a quota rejection cools it
//! down honoring Retry-After, a create failure cools down with capped
//! escalation — and every marker fails back implicitly on expiry.

use crate::proxy::backoff::{ExhaustionKind, calculate_backoff};

/// Why a spillover provider is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillKind {
    /// Payment/billing failure (HTTP 402 or explicit budget verdict). Not
    /// time-cooled: the compute ledger is the authority for when it clears.
    Budget,
    /// Rate/quota rejection (429) — Retry-After honored when present.
    Quota,
    /// Create failed (timeout / 5xx / transport) — escalating cooldown.
    CreateFailure,
}

impl SpillKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SpillKind::Budget => "budget",
            SpillKind::Quota => "quota",
            SpillKind::CreateFailure => "create_failure",
        }
    }
    pub fn parse(s: &str) -> Option<SpillKind> {
        match s {
            "budget" => Some(SpillKind::Budget),
            "quota" => Some(SpillKind::Quota),
            "create_failure" => Some(SpillKind::CreateFailure),
            _ => None,
        }
    }
}

/// Classify a provider create failure. `status` when the caller has a real
/// HTTP code; otherwise the error text is scanned for the standard
/// `status <code>` shapes the provider layer embeds.
pub fn classify_spill(status: Option<u16>, error: &str) -> SpillKind {
    let code = status.or_else(|| sniff_status(error));
    match code {
        Some(402) => SpillKind::Budget,
        Some(429) => SpillKind::Quota,
        _ => {
            if error.to_ascii_lowercase().contains("payment") {
                SpillKind::Budget
            } else {
                SpillKind::CreateFailure
            }
        }
    }
}

/// Best-effort HTTP status extraction from a provider error chain
/// (`"... status 429 ..."` / `"... HTTP 402"` shapes).
fn sniff_status(error: &str) -> Option<u16> {
    let lower = error.to_ascii_lowercase();
    for marker in ["status ", "status: ", "http "] {
        if let Some(i) = lower.find(marker) {
            let rest = &lower[i + marker.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.len() == 3
                && let Ok(v) = digits.parse::<u16>()
            {
                return Some(v);
            }
        }
    }
    None
}

/// Cooldown for a spill kind at the Nth consecutive failure. Budget returns
/// a long parking interval only as a UI hint — eligibility is the live
/// ledger predicate, never this timer. Quota/CreateFailure reuse the proxy's
/// backoff tables (RateLimit / ServerError class).
pub fn spill_cooldown_ms(kind: SpillKind, consecutive: u32, retry_after_secs: Option<u64>) -> i64 {
    if let Some(ra) = retry_after_secs {
        return (ra.min(60 * 60) * 1000) as i64;
    }
    let k = match kind {
        SpillKind::Budget => return 24 * 60 * 60 * 1000, // display-only parking
        SpillKind::Quota => ExhaustionKind::RateLimit,
        SpillKind::CreateFailure => ExhaustionKind::ServerError,
    };
    calculate_backoff(k, consecutive).as_millis() as i64
}

/// A provider's current marker as the picker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillState {
    pub kind: SpillKind,
    pub retry_at_ms: i64,
}

/// Choose the next spillover env: walk `order` (env names, most preferred
/// first), skipping entries whose provider is budget-dead (live ledger
/// predicate) or still cooling. Fail-back is implicit — an expired marker
/// simply stops matching.
pub fn pick_spillover<'a>(
    order: &'a [String],
    marker_for: &dyn Fn(&str) -> Option<SpillState>,
    budget_ok: &dyn Fn(&str) -> bool,
    now_ms: i64,
) -> Option<&'a str> {
    for env in order {
        if !budget_ok(env) {
            continue;
        }
        match marker_for(env) {
            Some(m) if m.kind == SpillKind::Budget => continue, // ledger says no
            Some(m) if m.retry_at_ms > now_ms => continue,      // cooling
            _ => return Some(env.as_str()),
        }
    }
    None
}

/// May a READY pool spare on a marked provider still be claimed? Quota and
/// create-failure markers gate CREATES, not starts — a parked spare is
/// already-paid provisioning. Budget death gates everything: waking a
/// scale-to-zero spare resumes spend.
pub fn spare_claimable(marker: Option<&SpillState>, budget_ok: bool, now_ms: i64) -> bool {
    if !budget_ok {
        return false;
    }
    match marker {
        Some(m) if m.kind == SpillKind::Budget => false,
        Some(m) if m.retry_at_ms > now_ms => true, // cooled ≠ unclaimable
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_table() {
        assert_eq!(classify_spill(Some(402), "x"), SpillKind::Budget);
        assert_eq!(classify_spill(Some(429), "x"), SpillKind::Quota);
        assert_eq!(classify_spill(Some(500), "x"), SpillKind::CreateFailure);
        assert_eq!(
            classify_spill(None, "request failed: status 402 Payment Required"),
            SpillKind::Budget
        );
        assert_eq!(
            classify_spill(None, "control call: HTTP 429 too many"),
            SpillKind::Quota
        );
        assert_eq!(
            classify_spill(None, "payment method missing"),
            SpillKind::Budget
        );
        assert_eq!(
            classify_spill(None, "connect timeout"),
            SpillKind::CreateFailure
        );
        assert_eq!(
            classify_spill(None, "status 40"),
            SpillKind::CreateFailure,
            "2-digit junk"
        );
    }

    #[test]
    fn cooldowns_escalate_and_honor_retry_after() {
        let a = spill_cooldown_ms(SpillKind::CreateFailure, 1, None);
        let b = spill_cooldown_ms(SpillKind::CreateFailure, 3, None);
        assert!(b > a, "escalates ({a} → {b})");
        assert_eq!(
            spill_cooldown_ms(SpillKind::Quota, 1, Some(90)),
            90_000,
            "Retry-After wins"
        );
        assert_eq!(
            spill_cooldown_ms(SpillKind::Quota, 1, Some(999_999)),
            3_600_000,
            "Retry-After clamped to an hour"
        );
        assert_eq!(spill_cooldown_ms(SpillKind::Budget, 5, None), 86_400_000);
    }

    #[test]
    fn picker_walks_order_skipping_dead_and_cooling() {
        let order = vec!["sprites".to_string(), "daytona".to_string()];
        let none = |_: &str| None;
        let all_ok = |_: &str| true;
        assert_eq!(pick_spillover(&order, &none, &all_ok, 0), Some("sprites"));

        // First cooling ⇒ second.
        let cooling = |e: &str| {
            (e == "sprites").then_some(SpillState {
                kind: SpillKind::Quota,
                retry_at_ms: 10_000,
            })
        };
        assert_eq!(
            pick_spillover(&order, &cooling, &all_ok, 5_000),
            Some("daytona")
        );
        // Fail-back at expiry: same marker, later clock.
        assert_eq!(
            pick_spillover(&order, &cooling, &all_ok, 10_000),
            Some("sprites")
        );

        // Budget-dead skips regardless of clock (ledger predicate).
        let no_budget = |e: &str| e != "sprites";
        assert_eq!(
            pick_spillover(&order, &none, &no_budget, 0),
            Some("daytona")
        );
        // Budget MARKER also skips even when the timer looks expired.
        let dead_marker = |e: &str| {
            (e == "sprites").then_some(SpillState {
                kind: SpillKind::Budget,
                retry_at_ms: 0,
            })
        };
        assert_eq!(
            pick_spillover(&order, &dead_marker, &all_ok, 99_999),
            Some("daytona")
        );
        // Everything out ⇒ None.
        let none_ok = |_: &str| false;
        assert_eq!(pick_spillover(&order, &none, &none_ok, 0), None);
    }

    #[test]
    fn spare_claim_rules() {
        let quota = SpillState {
            kind: SpillKind::Quota,
            retry_at_ms: 10_000,
        };
        let budget = SpillState {
            kind: SpillKind::Budget,
            retry_at_ms: 0,
        };
        assert!(spare_claimable(None, true, 0));
        assert!(
            spare_claimable(Some(&quota), true, 0),
            "quota gates creates, not starts"
        );
        assert!(
            !spare_claimable(Some(&budget), true, 0),
            "waking resumes spend"
        );
        assert!(!spare_claimable(None, false, 0), "ledger says no");
    }

    #[test]
    fn kind_round_trips() {
        for k in [
            SpillKind::Budget,
            SpillKind::Quota,
            SpillKind::CreateFailure,
        ] {
            assert_eq!(SpillKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(SpillKind::parse("nope"), None);
    }
}
