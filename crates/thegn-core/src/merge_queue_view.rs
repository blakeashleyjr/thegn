//! Pure policy for compact merge-queue views.
//!
//! The host owns the terminal-specific token layout. This module only decides
//! which queue tier is worth showing and whether a measured token fits.

use crate::attention::MqStatus;

/// The highest active merge-queue tier represented by a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqTier {
    /// The queue is blocked or needs a human decision.
    Blocked,
    /// The queue is actively folding, verifying, or running an agent.
    Working,
    /// The queue has entries waiting to be processed or landed.
    Populated,
}

/// A workspace's compact merge-queue summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqRollup {
    pub tier: MqTier,
    pub count: usize,
}

/// Fold queue statuses to the highest-priority non-silent tier.
pub fn rollup(statuses: impl IntoIterator<Item = MqStatus>) -> Option<MqRollup> {
    let mut best: Option<MqRollup> = None;
    for status in statuses {
        let tier = match status {
            MqStatus::Deferred
            | MqStatus::GateFailed
            | MqStatus::GateError
            | MqStatus::NeedsHuman => MqTier::Blocked,
            MqStatus::Folding | MqStatus::Verifying | MqStatus::AgentRunning => MqTier::Working,
            MqStatus::Queued | MqStatus::Ready => MqTier::Populated,
            MqStatus::Landed => continue,
        };

        match &mut best {
            Some(current) if current.tier == tier => current.count += 1,
            Some(current) if tier_priority(tier) < tier_priority(current.tier) => {
                *current = MqRollup { tier, count: 1 };
            }
            Some(_) => {}
            None => best = Some(MqRollup { tier, count: 1 }),
        }
    }
    best
}

fn tier_priority(tier: MqTier) -> u8 {
    match tier {
        MqTier::Blocked => 0,
        MqTier::Working => 1,
        MqTier::Populated => 2,
    }
}

/// How much of a compact queue token can be painted in its measured space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqTokenFit {
    Full,
    MarkerOnly,
    Hidden,
}

/// Choose the fullest token representation that fits the available width.
pub fn fit_token(available: usize, full_width: usize, marker_width: usize) -> MqTokenFit {
    if available >= full_width {
        MqTokenFit::Full
    } else if available >= marker_width {
        MqTokenFit::MarkerOnly
    } else {
        MqTokenFit::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_status_maps_to_its_expected_tier() {
        let blocked = [
            MqStatus::Deferred,
            MqStatus::GateFailed,
            MqStatus::GateError,
            MqStatus::NeedsHuman,
        ];
        for status in blocked {
            assert_eq!(
                rollup([status]),
                Some(MqRollup {
                    tier: MqTier::Blocked,
                    count: 1
                })
            );
        }

        let working = [
            MqStatus::Folding,
            MqStatus::Verifying,
            MqStatus::AgentRunning,
        ];
        for status in working {
            assert_eq!(
                rollup([status]),
                Some(MqRollup {
                    tier: MqTier::Working,
                    count: 1
                })
            );
        }

        let populated = [MqStatus::Queued, MqStatus::Ready];
        for status in populated {
            assert_eq!(
                rollup([status]),
                Some(MqRollup {
                    tier: MqTier::Populated,
                    count: 1
                })
            );
        }
    }

    #[test]
    fn priority_discards_lower_tiers_but_counts_the_highest() {
        assert_eq!(
            rollup([
                MqStatus::Queued,
                MqStatus::Folding,
                MqStatus::Ready,
                MqStatus::GateFailed,
                MqStatus::Deferred,
            ]),
            Some(MqRollup {
                tier: MqTier::Blocked,
                count: 2
            })
        );
        assert_eq!(
            rollup([MqStatus::Queued, MqStatus::Folding, MqStatus::Verifying]),
            Some(MqRollup {
                tier: MqTier::Working,
                count: 2
            })
        );
    }

    #[test]
    fn empty_landed_only_and_unparsed_statuses_are_silent() {
        assert_eq!(rollup(std::iter::empty()), None);
        assert_eq!(rollup([MqStatus::Landed, MqStatus::Landed]), None);
        assert_eq!(MqStatus::parse("unknown"), None);
    }

    #[test]
    fn token_fit_uses_full_then_marker_then_hidden_at_boundaries() {
        assert_eq!(fit_token(8, 8, 2), MqTokenFit::Full);
        assert_eq!(fit_token(7, 8, 2), MqTokenFit::MarkerOnly);
        assert_eq!(fit_token(2, 8, 2), MqTokenFit::MarkerOnly);
        assert_eq!(fit_token(1, 8, 2), MqTokenFit::Hidden);
        assert_eq!(fit_token(0, 0, 0), MqTokenFit::Full);
    }
}
