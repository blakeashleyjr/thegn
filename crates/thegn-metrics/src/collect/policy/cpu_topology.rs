//! Pure CPU topology classification and cpulist helpers.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CoreType {
    Performance,
    Efficiency,
    #[default]
    Uniform,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuClassInput {
    pub intel_core: bool,
    pub intel_atom: bool,
    pub arm_capacity: Option<u32>,
    pub amd_max_freq_khz: Option<u64>,
    pub amd_highest_perf: Option<u32>,
}

pub fn parse_cpulist(text: &str) -> Option<Vec<u16>> {
    if text.trim().is_empty() {
        return Some(Vec::new());
    }
    let mut cpus = BTreeSet::new();
    for item in text.trim().split(',') {
        if item.is_empty() {
            return None;
        }
        let mut bounds = item.split('-');
        let start = bounds.next()?.parse::<u16>().ok()?;
        let end = match bounds.next() {
            Some(value) => value.parse::<u16>().ok()?,
            None => start,
        };
        if bounds.next().is_some() || start > end {
            return None;
        }
        for cpu in start..=end {
            cpus.insert(cpu);
        }
    }
    Some(cpus.into_iter().collect())
}

pub fn classify_core_types(inputs: &[(u16, CpuClassInput)]) -> Vec<(u16, CoreType)> {
    if inputs
        .iter()
        .any(|(_, input)| input.intel_core || input.intel_atom)
    {
        return inputs
            .iter()
            .map(|(cpu, input)| {
                let kind = match (input.intel_core, input.intel_atom) {
                    (true, false) => CoreType::Performance,
                    (false, true) => CoreType::Efficiency,
                    _ => CoreType::Uniform,
                };
                (*cpu, kind)
            })
            .collect();
    }
    let capacities: BTreeSet<_> = inputs
        .iter()
        .filter_map(|(_, input)| input.arm_capacity)
        .collect();
    if capacities.len() > 1 {
        let min = capacities.first().copied();
        let max = capacities.last().copied();
        return inputs
            .iter()
            .map(|(cpu, input)| {
                let kind = match (input.arm_capacity, min, max) {
                    (Some(value), Some(low), Some(high)) if value == high && high != low => {
                        CoreType::Performance
                    }
                    (Some(value), Some(low), Some(high)) if value == low && high != low => {
                        CoreType::Efficiency
                    }
                    _ => CoreType::Uniform,
                };
                (*cpu, kind)
            })
            .collect();
    }
    let amd: BTreeSet<_> = inputs
        .iter()
        .filter_map(|(_, input)| input.amd_max_freq_khz.zip(input.amd_highest_perf))
        .collect();
    if amd.len() == 2 {
        let min = amd.first().copied();
        let max = amd.last().copied();
        let has_gap = match (min, max) {
            (Some((low_freq, _)), Some((high_freq, _))) if low_freq > 0 => {
                high_freq as f64 / low_freq as f64 >= 1.15
            }
            _ => false,
        };
        if !has_gap {
            return inputs
                .iter()
                .map(|(cpu, _)| (*cpu, CoreType::Uniform))
                .collect();
        }
        return inputs
            .iter()
            .map(|(cpu, input)| {
                let pair = input.amd_max_freq_khz.zip(input.amd_highest_perf);
                let kind = match (pair, min, max) {
                    (Some(value), Some(low), Some(high)) if value == high && high != low => {
                        CoreType::Performance
                    }
                    (Some(value), Some(low), Some(high)) if value == low && high != low => {
                        CoreType::Efficiency
                    }
                    _ => CoreType::Uniform,
                };
                (*cpu, kind)
            })
            .collect();
    }
    inputs
        .iter()
        .map(|(cpu, _)| (*cpu, CoreType::Uniform))
        .collect()
}

pub fn l3_groups(cpus: &[u16], shared_lists: &[(u16, Vec<u16>)]) -> Vec<Vec<u16>> {
    let mut groups = Vec::<Vec<u16>>::new();
    for cpu in cpus {
        if let Some((_, shared)) = shared_lists.iter().find(|(owner, _)| owner == cpu) {
            let group: Vec<_> = shared
                .iter()
                .copied()
                .filter(|id| cpus.contains(id))
                .collect();
            if !group.is_empty() && !groups.contains(&group) {
                groups.push(group);
                continue;
            }
        }
        if !groups.iter().any(|group| group.contains(cpu)) {
            groups.push(vec![*cpu]);
        }
    }
    groups
}

pub fn effective_cpus(online: usize, available_parallelism: usize) -> usize {
    online.min(available_parallelism)
}

#[cfg(test)]
mod tests {
    use super::{
        CoreType, CpuClassInput, classify_core_types, effective_cpus, l3_groups, parse_cpulist,
    };

    #[test]
    fn cpulist_parses_ranges_and_rejects_malformed_values() {
        assert_eq!(
            parse_cpulist("0-3,12-15"),
            Some(vec![0, 1, 2, 3, 12, 13, 14, 15])
        );
        assert_eq!(parse_cpulist(""), Some(vec![]));
        assert_eq!(parse_cpulist("3-1"), None);
        assert_eq!(parse_cpulist("1,,2"), None);
        assert_eq!(parse_cpulist("1-2-3"), None);
    }

    #[test]
    fn classifies_intel_arm_amd_and_uniform_fixtures() {
        let intel = vec![
            (
                0,
                CpuClassInput {
                    intel_core: true,
                    ..CpuClassInput::default()
                },
            ),
            (
                1,
                CpuClassInput {
                    intel_atom: true,
                    ..CpuClassInput::default()
                },
            ),
        ];
        assert_eq!(
            classify_core_types(&intel),
            vec![(0, CoreType::Performance), (1, CoreType::Efficiency)]
        );
        let arm = vec![
            (
                0,
                CpuClassInput {
                    arm_capacity: Some(512),
                    ..CpuClassInput::default()
                },
            ),
            (
                1,
                CpuClassInput {
                    arm_capacity: Some(1024),
                    ..CpuClassInput::default()
                },
            ),
        ];
        assert_eq!(
            classify_core_types(&arm),
            vec![(0, CoreType::Efficiency), (1, CoreType::Performance)]
        );
        let amd = vec![
            (
                0,
                CpuClassInput {
                    amd_max_freq_khz: Some(4_000_000),
                    amd_highest_perf: Some(200),
                    ..CpuClassInput::default()
                },
            ),
            (
                1,
                CpuClassInput {
                    amd_max_freq_khz: Some(3_000_000),
                    amd_highest_perf: Some(150),
                    ..CpuClassInput::default()
                },
            ),
        ];
        assert_eq!(
            classify_core_types(&amd),
            vec![(0, CoreType::Performance), (1, CoreType::Efficiency)]
        );
        let preferred_core_ranking = vec![
            (0, 4_000_000, 166),
            (1, 4_050_000, 171),
            (2, 4_100_000, 176),
        ]
        .into_iter()
        .map(|(cpu, freq, perf)| {
            (
                cpu,
                CpuClassInput {
                    amd_max_freq_khz: Some(freq),
                    amd_highest_perf: Some(perf),
                    ..CpuClassInput::default()
                },
            )
        })
        .collect::<Vec<_>>();
        assert_eq!(
            classify_core_types(&preferred_core_ranking),
            vec![
                (0, CoreType::Uniform),
                (1, CoreType::Uniform),
                (2, CoreType::Uniform)
            ]
        );
        assert_eq!(
            classify_core_types(&[(0, CpuClassInput::default())]),
            vec![(0, CoreType::Uniform)]
        );
    }

    #[test]
    fn l3_groups_and_effective_count_are_bounded_by_online_and_affinity() {
        assert_eq!(
            l3_groups(&[0, 1, 2], &[(0, vec![0, 1]), (2, vec![2])]),
            vec![vec![0, 1], vec![2]]
        );
        assert_eq!(effective_cpus(8, 3), 3);
        assert_eq!(effective_cpus(2, 8), 2);
    }
}
