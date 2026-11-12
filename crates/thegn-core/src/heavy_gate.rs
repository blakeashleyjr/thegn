//! Pure admission policy for host-local heavyweight gate runners.
//!
//! The host owns the filesystem locks and load-average sampling. Keeping the
//! decision here makes the policy testable without a substrate or an event
//! loop.

/// Heavy-gate settings resolved from `[sandbox.limits]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Number of concurrent gate holders. Zero disables slot limiting.
    pub slots: usize,
    /// Absolute one-minute load-average ceiling. Zero disables load limiting.
    pub max_load: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            slots: 0,
            max_load: 0.0,
        }
    }
}

/// The result of evaluating the current host state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Admit,
    WaitLoad,
    WaitSlot,
}

/// Decide whether a heavyweight gate may start.
pub fn decide(
    slots: usize,
    max_load: f64,
    holders: usize,
    loadavg1: f64,
    _cores: usize,
) -> Decision {
    // `max_load` is deliberately absolute. `_cores` is accepted at this seam
    // so callers can report the host capacity without accidentally normalizing
    // the configured threshold per core.
    if max_load > 0.0 && loadavg1 >= max_load {
        return Decision::WaitLoad;
    }
    if slots > 0 && holders >= slots {
        return Decision::WaitSlot;
    }
    Decision::Admit
}

impl From<&crate::config::SandboxLimits> for Config {
    fn from(limits: &crate::config::SandboxLimits) -> Self {
        Self {
            slots: limits.heavy_slots,
            max_load: limits.max_load,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Decision, decide};

    #[test]
    fn policy_table_covers_disabled_load_slots_and_contention() {
        let cases = [
            (0, 0.0, 0, 99.0, Decision::Admit),
            (0, 2.0, 0, 1.99, Decision::Admit),
            (0, 2.0, 0, 2.0, Decision::WaitLoad),
            (0, 2.0, 0, 7.0, Decision::WaitLoad),
            (2, 0.0, 0, 99.0, Decision::Admit),
            (2, 0.0, 1, 99.0, Decision::Admit),
            (2, 0.0, 2, 0.0, Decision::WaitSlot),
            (2, 0.0, 4, 0.0, Decision::WaitSlot),
            (2, 2.0, 2, 3.0, Decision::WaitLoad),
            (2, 2.0, 2, 1.0, Decision::WaitSlot),
        ];
        for (slots, max_load, holders, load, expected) in cases {
            assert_eq!(decide(slots, max_load, holders, load, 16), expected);
        }
    }

    #[test]
    fn zero_and_negative_thresholds_are_inert() {
        assert_eq!(Config::default().slots, 0);
        let limits = crate::config::SandboxLimits {
            heavy_slots: 3,
            max_load: 4.5,
            ..Default::default()
        };
        assert_eq!(
            Config::from(&limits),
            Config {
                slots: 3,
                max_load: 4.5
            }
        );
        assert_eq!(decide(0, 0.0, 0, 0.0, 0), Decision::Admit);
        assert_eq!(decide(0, -1.0, 0, 100.0, 1), Decision::Admit);
    }
}
