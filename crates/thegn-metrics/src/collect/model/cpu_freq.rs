//! Aggregated CPU frequency policies and power controls.
use super::cpu_policy::{CoreClass, CpuPolicy, GovernorSummary, PowerPolicy, SysToken};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CpuFreqPart {
    pub policies: Vec<CpuPolicy>,
    pub power: PowerPolicy,
}
impl crate::collect::frame::Part for CpuFreqPart {
    fn clear(&mut self) {
        self.policies.clear();
        self.power = PowerPolicy::default();
    }
}

pub fn mean_mhz(khz: &[u64]) -> Option<u64> {
    if khz.is_empty() {
        return None;
    }
    Some(khz.iter().sum::<u64>() / khz.len() as u64 / 1000)
}
impl CpuFreqPart {
    pub fn mean_cur_mhz(&self) -> Option<u64> {
        let (sum, count) = self
            .policies
            .iter()
            .filter_map(|policy| policy.cur_khz)
            .fold((0_u64, 0_u64), |(sum, count), value| {
                (sum + value, count + 1)
            });
        (count > 0).then_some(sum / count / 1000)
    }
    /// Mean current frequency (MHz) over the policies of `class` that report
    /// one; `None` when no such policy has a current value.
    pub fn class_mean_cur_mhz(&self, class: CoreClass) -> Option<u64> {
        let (sum, count) = self
            .policies
            .iter()
            .filter(|policy| policy.core_class == class)
            .filter_map(|policy| policy.cur_khz)
            .fold((0_u64, 0_u64), |(sum, count), value| {
                (sum.saturating_add(value), count + 1)
            });
        (count > 0).then_some(sum / count / 1000)
    }
    pub fn max_cur_khz(&self) -> Option<u64> {
        self.policies.iter().filter_map(|p| p.cur_khz).max()
    }
    pub fn any_capped(&self) -> bool {
        self.policies.iter().any(CpuPolicy::capped)
    }
    pub fn governor_summary(&self) -> Option<GovernorSummary> {
        let mut values = self.policies.iter().map(|policy| policy.governor);
        let first = values.next()?;
        if values.any(|value| value != first) {
            return Some(GovernorSummary::Mixed);
        }
        first.map(GovernorSummary::Common)
    }
    /// Number of (performance, efficiency) policies; `Unknown` counts in neither.
    pub fn core_class_counts(&self) -> (usize, usize) {
        self.policies
            .iter()
            .fold((0, 0), |(perf, eff), policy| match policy.core_class {
                CoreClass::Performance => (perf + 1, eff),
                CoreClass::Efficiency => (perf, eff + 1),
                CoreClass::Unknown => (perf, eff),
            })
    }
    pub fn governor_token(&self) -> Option<SysToken> {
        match self.governor_summary()? {
            GovernorSummary::Common(token) => Some(token),
            GovernorSummary::Mixed => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CpuFreqPart, mean_mhz};
    use crate::collect::frame::Part;
    use crate::collect::model::cpu_policy::{CoreClass, CpuPolicy, GovernorSummary, SysToken};

    #[test]
    fn core_class_counts_split_performance_and_efficiency() {
        assert_eq!(CpuFreqPart::default().core_class_counts(), (0, 0));
        let with = |core_class| CpuPolicy {
            core_class,
            ..CpuPolicy::default()
        };
        let part = CpuFreqPart {
            policies: vec![
                with(CoreClass::Performance),
                with(CoreClass::Efficiency),
                with(CoreClass::Efficiency),
                with(CoreClass::Unknown),
                with(CoreClass::Performance),
                with(CoreClass::Efficiency),
            ],
            ..CpuFreqPart::default()
        };
        assert_eq!(part.core_class_counts(), (2, 3));
        let unknown = CpuFreqPart {
            policies: vec![CpuPolicy::default(); 4],
            ..CpuFreqPart::default()
        };
        assert_eq!(unknown.core_class_counts(), (0, 0));
    }
    #[test]
    fn class_mean_cur_mhz_averages_per_class() {
        let with = |core_class, cur_khz| CpuPolicy {
            core_class,
            cur_khz,
            ..CpuPolicy::default()
        };
        let part = CpuFreqPart {
            policies: vec![
                with(CoreClass::Performance, Some(4_000_000)),
                with(CoreClass::Performance, Some(4_200_000)),
                with(CoreClass::Performance, None),
                with(CoreClass::Efficiency, Some(2_000_000)),
                with(CoreClass::Efficiency, Some(2_400_000)),
                with(CoreClass::Unknown, Some(9_000_000)),
            ],
            ..CpuFreqPart::default()
        };
        assert_eq!(part.class_mean_cur_mhz(CoreClass::Performance), Some(4100));
        assert_eq!(part.class_mean_cur_mhz(CoreClass::Efficiency), Some(2200));
        assert_eq!(part.class_mean_cur_mhz(CoreClass::Unknown), Some(9000));
        assert_eq!(
            CpuFreqPart::default().class_mean_cur_mhz(CoreClass::Performance),
            None
        );
        let none = CpuFreqPart {
            policies: vec![with(CoreClass::Efficiency, None)],
            ..CpuFreqPart::default()
        };
        assert_eq!(none.class_mean_cur_mhz(CoreClass::Efficiency), None);
        assert_eq!(none.class_mean_cur_mhz(CoreClass::Performance), None);
    }
    #[test]
    fn class_mean_cur_mhz_does_not_overflow_on_huge_values() {
        let part = CpuFreqPart {
            policies: vec![
                CpuPolicy {
                    core_class: CoreClass::Performance,
                    cur_khz: Some(u64::MAX),
                    ..CpuPolicy::default()
                },
                CpuPolicy {
                    core_class: CoreClass::Performance,
                    cur_khz: Some(u64::MAX),
                    ..CpuPolicy::default()
                },
            ],
            ..CpuFreqPart::default()
        };
        // Sum saturates at u64::MAX; mean = MAX / 2 / 1000.
        assert_eq!(
            part.class_mean_cur_mhz(CoreClass::Performance),
            Some(u64::MAX / 2 / 1000)
        );
    }
    #[test]
    fn part_clears_values_and_retains_policy_capacity() {
        let mut part = CpuFreqPart::default();
        part.policies.reserve(4);
        let capacity = part.policies.capacity();
        part.policies.push(CpuPolicy::default());
        part.clear();
        assert_eq!(part.policies.capacity(), capacity);
        assert!(part.policies.is_empty());
        assert_eq!(mean_mhz(&[]), None);
        assert_eq!(mean_mhz(&[1_000_000, 2_000_000]), Some(1500));
    }

    #[test]
    fn governor_summary_reports_common_and_mixed_values() {
        let token = SysToken::new(b"schedutil").unwrap();
        let mut part = CpuFreqPart {
            policies: vec![
                CpuPolicy {
                    governor: Some(token),
                    ..CpuPolicy::default()
                },
                CpuPolicy {
                    governor: Some(token),
                    ..CpuPolicy::default()
                },
            ],
            ..CpuFreqPart::default()
        };
        assert_eq!(
            part.governor_summary(),
            Some(GovernorSummary::Common(token))
        );
        part.policies[1].governor = Some(SysToken::new(b"performance").unwrap());
        assert_eq!(part.governor_summary(), Some(GovernorSummary::Mixed));
    }
}
