//! Pure due decision shared by model and PR refresh; their cadences are
//! independent, with one PR refresh subsuming a coincident model refresh.

use std::num::NonZeroU64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelRefresh {
    Model,
    Pr,
}

pub(crate) fn model_or_pr(tick: u64, model: NonZeroU64, pr: NonZeroU64) -> Option<ModelRefresh> {
    if tick.is_multiple_of(pr.get()) {
        Some(ModelRefresh::Pr)
    } else if tick.is_multiple_of(model.get()) {
        Some(ModelRefresh::Model)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cadence_cannot_starve_normal_pr_schedule() {
        for model_ms in [0, 500, 1500, 5000, 7000, 1u128 << 61, u128::MAX] {
            let model = thegn_core::time_policy::cadence_millis_slots(model_ms, 500);
            let pr = thegn_core::time_policy::cadence_slots(20, 0, 500);
            let pr_ticks: Vec<_> = (1..=240)
                .filter(|tick| model_or_pr(*tick, model, pr) == Some(ModelRefresh::Pr))
                .collect();
            assert_eq!(pr_ticks, [40, 80, 120, 160, 200, 240]);
        }
    }

    #[test]
    fn coincident_work_is_one_pr_refresh_and_idle_tick_is_none() {
        let model = NonZeroU64::new(10).unwrap();
        let pr = NonZeroU64::new(40).unwrap();
        assert_eq!(model_or_pr(1, model, pr), None);
        assert_eq!(model_or_pr(10, model, pr), Some(ModelRefresh::Model));
        assert_eq!(model_or_pr(40, model, pr), Some(ModelRefresh::Pr));
    }
}
