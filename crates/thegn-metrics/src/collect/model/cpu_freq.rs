//! Aggregated CPU frequency policies and power controls.
use super::cpu_policy::{CpuPolicy, GovernorSummary, PowerPolicy, SysToken};

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
    use crate::collect::model::cpu_policy::{CpuPolicy, GovernorSummary, SysToken};
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
