//! Which headless producers the daemon arms (THE-2208).
//!
//! A kind is armed only when `[daemon] enabled` and `headless_producers` are
//! on AND something needs it (an enabled rule, autopilot, the PR queue, a
//! mention consumer). Arming is by need: with nothing that needs a producer
//! the daemon starts no task, no timer and writes no lease.
//!
//! The decision bodies are filled by b161 lane 5; until then nothing arms.

use crate::producer_lease::ProducerKind;

/// Everything [`armed`] decides from, gathered by the caller.
pub struct ArmInputs<'a> {
    /// `[daemon] enabled`.
    pub daemon_enabled: bool,
    /// `[daemon] headless_producers`.
    pub headless: bool,
    /// Every automation rule (config and Lead-authored).
    pub rules: &'a [crate::automation::AutomationRule],
    /// Any repo has `[autopilot]` enabled.
    pub autopilot_any: bool,
    /// `[pr_queue] enabled`.
    pub pr_queue_enabled: bool,
    /// `[notifications] github_mentions`.
    pub github_mentions: bool,
    /// A push topic is configured.
    pub push_configured: bool,
}

/// True when the daemon should run `kind`'s producer. Stub: never armed
/// (fails closed) until b161 lane 5 fills it.
pub fn armed(_kind: ProducerKind, _i: &ArmInputs<'_>) -> bool {
    false
}

/// Every armed kind, in [`ProducerKind::ALL`] order.
pub fn armed_kinds(i: &ArmInputs<'_>) -> Vec<ProducerKind> {
    ProducerKind::ALL
        .into_iter()
        .filter(|kind| armed(*kind, i))
        .collect()
}

/// How often `kind`'s producer ticks, in seconds. Stub: 300 until b161
/// lane 5 reads the per-kind cadence from config.
pub fn cadence_secs(_kind: ProducerKind, _cfg: &crate::config::Config) -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_arms_with_everything_off() {
        let inputs = ArmInputs {
            daemon_enabled: false,
            headless: false,
            rules: &[],
            autopilot_any: false,
            pr_queue_enabled: false,
            github_mentions: false,
            push_configured: false,
        };
        assert!(armed_kinds(&inputs).is_empty());
    }
}
