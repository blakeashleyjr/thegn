//! Multi-key credential pools and lane ordering.
//!
//! Port of `credentials.go`. A provider may carry several API keys; each becomes
//! its own backend "lane" sharing one [`CredPool`]. The pool's [`CredPool::order`]
//! decides, per request, which lane to try first and the fall-through order:
//! round-robin spreads first-attempts evenly, failover drains lane 0 first,
//! random picks a random start, weighted biases by per-lane weight via smooth
//! weighted round-robin (the nginx algorithm).
//!
//! Randomness for the [`KeyStrategy::Random`] strategy is injected by the caller
//! (`rand_start`) so this logic stays pure and deterministic under test.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Decides the order a provider's keys are tried in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyStrategy {
    /// Spread load: advance the first-choice lane each request.
    #[default]
    RoundRobin,
    /// Drain lane 0 until exhausted, then 1, …
    Failover,
    /// Random first-choice lane each request.
    Random,
    /// First-choice lane chosen proportionally to weights.
    Weighted,
}

impl KeyStrategy {
    /// Renders the strategy for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            KeyStrategy::RoundRobin => "roundrobin",
            KeyStrategy::Failover => "failover",
            KeyStrategy::Random => "random",
            KeyStrategy::Weighted => "weighted",
        }
    }

    /// Maps a config string to a strategy, defaulting to round-robin for empty
    /// or unrecognized input. Port of `parseStrategy`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "failover" | "sequential" => KeyStrategy::Failover,
            "random" => KeyStrategy::Random,
            "weighted" | "weight" => KeyStrategy::Weighted,
            _ => KeyStrategy::RoundRobin,
        }
    }
}

/// Rotation state shared by every lane of one (provider, model).
pub struct CredPool {
    strategy: KeyStrategy,
    /// Per-lane weights (weighted strategy only).
    weights: Vec<u32>,
    /// Round-robin position.
    cursor: AtomicU64,
    /// Smooth-weighted-round-robin running counters (weighted strategy only).
    swrr: Mutex<Vec<i64>>,
}

impl CredPool {
    pub fn new(strategy: KeyStrategy, weights: Vec<u32>) -> Self {
        Self {
            strategy,
            weights,
            cursor: AtomicU64::new(0),
            swrr: Mutex::new(Vec::new()),
        }
    }

    /// Returns a permutation of `0..n` giving the order to try this pool's `n`
    /// lanes. The first element is the strategy's first-choice lane; the rest
    /// are fall-through targets. `rand_start` is consulted only by
    /// [`KeyStrategy::Random`] (supply any value otherwise).
    pub fn order(&self, n: usize, rand_start: usize) -> Vec<usize> {
        if n <= 1 {
            return (0..n).collect();
        }
        match self.strategy {
            KeyStrategy::Failover => natural_order(n),
            KeyStrategy::Random => rotated(n, rand_start % n),
            KeyStrategy::Weighted => rotated(n, self.weighted_start(n)),
            KeyStrategy::RoundRobin => {
                let start = (self.cursor.fetch_add(1, Ordering::Relaxed) as usize) % n;
                rotated(n, start)
            }
        }
    }

    /// Picks the next first-choice lane via smooth weighted round-robin: add each
    /// lane's weight to a running counter, select the highest, subtract the total
    /// from the winner. Port of `weightedStart`.
    fn weighted_start(&self, n: usize) -> usize {
        let mut swrr = self.swrr.lock().unwrap();
        if swrr.len() != n {
            *swrr = vec![0; n];
        }
        let mut total = 0i64;
        let mut best = 0usize;
        for i in 0..n {
            let w = self
                .weights
                .get(i)
                .filter(|w| **w > 0)
                .map(|w| *w as i64)
                .unwrap_or(1);
            swrr[i] += w;
            total += w;
            if swrr[i] > swrr[best] {
                best = i;
            }
        }
        swrr[best] -= total;
        best
    }
}

/// Returns `[0, 1, …, n-1]`.
fn natural_order(n: usize) -> Vec<usize> {
    (0..n).collect()
}

/// Returns `[start, start+1, …]` wrapping modulo `n`.
fn rotated(n: usize, start: usize) -> Vec<usize> {
    (0..n).map(|i| (start + i) % n).collect()
}

/// Strips a `#<idx>` key suffix from a name/identity, recovering the base
/// provider (e.g. `"minimax#1"` → `"minimax"`). Port of `providerBase`.
pub fn provider_base(name: &str) -> &str {
    match name.find('#') {
        Some(i) => &name[..i],
        None => name,
    }
}

/// Splits an API-key list on newlines, commas, semicolons, or whitespace,
/// trimming blanks and removing duplicates while preserving order. Port of the
/// `keySeparators` + dedupe loop in `providerKeySpecs`.
pub fn split_keys(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for k in raw.split(['\n', '\r', '\t', ' ', ',', ';']) {
        let k = k.trim();
        if k.is_empty() || !seen.insert(k.to_string()) {
            continue;
        }
        out.push(k.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_parse_and_render() {
        assert_eq!(KeyStrategy::parse("failover"), KeyStrategy::Failover);
        assert_eq!(KeyStrategy::parse("sequential"), KeyStrategy::Failover);
        assert_eq!(KeyStrategy::parse("RANDOM"), KeyStrategy::Random);
        assert_eq!(KeyStrategy::parse("weight"), KeyStrategy::Weighted);
        assert_eq!(KeyStrategy::parse("nonsense"), KeyStrategy::RoundRobin);
        assert_eq!(KeyStrategy::parse(""), KeyStrategy::RoundRobin);
        assert_eq!(KeyStrategy::Failover.as_str(), "failover");
        assert_eq!(KeyStrategy::RoundRobin.as_str(), "roundrobin");
        assert_eq!(KeyStrategy::Random.as_str(), "random");
        assert_eq!(KeyStrategy::Weighted.as_str(), "weighted");
    }

    #[test]
    fn single_lane_is_trivial() {
        let p = CredPool::new(KeyStrategy::RoundRobin, vec![]);
        assert_eq!(p.order(1, 0), vec![0]);
        assert_eq!(p.order(0, 0), Vec::<usize>::new());
    }

    #[test]
    fn round_robin_advances_first_choice() {
        let p = CredPool::new(KeyStrategy::RoundRobin, vec![]);
        assert_eq!(p.order(3, 0), vec![0, 1, 2]);
        assert_eq!(p.order(3, 0), vec![1, 2, 0]);
        assert_eq!(p.order(3, 0), vec![2, 0, 1]);
        assert_eq!(p.order(3, 0), vec![0, 1, 2]);
    }

    #[test]
    fn failover_is_natural_order() {
        let p = CredPool::new(KeyStrategy::Failover, vec![]);
        assert_eq!(p.order(3, 0), vec![0, 1, 2]);
        assert_eq!(p.order(3, 0), vec![0, 1, 2]);
    }

    #[test]
    fn random_uses_injected_start() {
        let p = CredPool::new(KeyStrategy::Random, vec![]);
        assert_eq!(p.order(4, 2), vec![2, 3, 0, 1]);
        assert_eq!(p.order(4, 5), vec![1, 2, 3, 0]); // 5 % 4 == 1
    }

    #[test]
    fn weighted_biases_first_choice_by_weight() {
        // Lane 0 weight 3, lane 1 weight 1 → over 4 picks, lane 0 first ~3×.
        let p = CredPool::new(KeyStrategy::Weighted, vec![3, 1]);
        let firsts: Vec<usize> = (0..4).map(|_| p.order(2, 0)[0]).collect();
        let zeros = firsts.iter().filter(|&&x| x == 0).count();
        assert_eq!(zeros, 3);
    }

    #[test]
    fn provider_base_strips_suffix() {
        assert_eq!(provider_base("minimax#1"), "minimax");
        assert_eq!(provider_base("codex"), "codex");
    }

    #[test]
    fn split_keys_dedupes_and_trims() {
        let keys = split_keys("a, b\nc;a  d");
        assert_eq!(keys, vec!["a", "b", "c", "d"]);
        assert!(split_keys("   ").is_empty());
    }
}
