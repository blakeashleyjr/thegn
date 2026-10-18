//! Bounded cgroup v2 sampling for container inventory supplied by the caller.

use std::collections::{HashMap, HashSet};
use std::path::{Component, PathBuf};
use std::time::Instant;

use super::cgroup::{CgroupFs, DeviceFilter, read_group};
use crate::collect::model::cgroup::CgroupSample;
use crate::collect::model::containers_stats::ContainerStatsRow;

fn counters_decreased(previous: &CgroupSample, current: &CgroupSample) -> bool {
    let cpu_decreased = previous.cpu.zip(current.cpu).is_some_and(|(before, now)| {
        now.usage_usec < before.usage_usec
            || now.user_usec < before.user_usec
            || now.system_usec < before.system_usec
            || before
                .nr_periods
                .zip(now.nr_periods)
                .is_some_and(|(old, new)| new < old)
            || before
                .nr_throttled
                .zip(now.nr_throttled)
                .is_some_and(|(old, new)| new < old)
            || before
                .throttled_usec
                .zip(now.throttled_usec)
                .is_some_and(|(old, new)| new < old)
    });
    let events_decreased =
        previous
            .mem_events
            .zip(current.mem_events)
            .is_some_and(|(before, now)| {
                before.low.zip(now.low).is_some_and(|(old, new)| new < old)
                    || before
                        .high
                        .zip(now.high)
                        .is_some_and(|(old, new)| new < old)
                    || before.max.zip(now.max).is_some_and(|(old, new)| new < old)
                    || before.oom.zip(now.oom).is_some_and(|(old, new)| new < old)
                    || before
                        .oom_kill
                        .zip(now.oom_kill)
                        .is_some_and(|(old, new)| new < old)
            });
    let io_decreased = previous.io.zip(current.io).is_some_and(|(before, now)| {
        now.rbytes < before.rbytes
            || now.wbytes < before.wbytes
            || now.rios < before.rios
            || now.wios < before.wios
    });
    cpu_decreased
        || events_decreased
        || io_decreased
        || previous
            .mem_peak
            .zip(current.mem_peak)
            .is_some_and(|(before, now)| now < before)
}

pub const MAX_CONTAINER_SCOPES_PER_TICK: usize = 64;
const CPU_PRIORITY_SLOTS: usize = 56;
const SAMPLE_BUFFER_BYTES: usize = 8 * 1024;

/// A caller-resolved container cgroup path, relative to the cgroup v2 mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContainerScope {
    pub id: String,
    pub path: PathBuf,
}

impl ResolvedContainerScope {
    pub fn new(id: impl Into<String>, path: impl Into<PathBuf>) -> Option<Self> {
        let id = id.into();
        let path = path.into();
        if id.is_empty()
            || path.as_os_str().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
        {
            return None;
        }
        Some(Self { id, path })
    }
}

#[derive(Debug, Clone)]
struct PreviousSample {
    sample: CgroupSample,
    at: Instant,
    cpu_pct: f64,
}

/// Reads at most 64 caller-resolved cgroup scopes per tick and derives rates.
pub struct ContainerStatsSampler<F> {
    fs: F,
    filter: DeviceFilter,
    buffer: Vec<u8>,
    sample: CgroupSample,
    path: PathBuf,
    previous: HashMap<String, PreviousSample>,
    rows: Vec<ContainerStatsRow>,
    round_robin: usize,
}

impl<F> ContainerStatsSampler<F> {
    pub fn new(fs: F, filter: DeviceFilter) -> Self {
        Self {
            fs,
            filter,
            buffer: Vec::with_capacity(SAMPLE_BUFFER_BYTES),
            sample: CgroupSample::default(),
            path: PathBuf::new(),
            previous: HashMap::new(),
            rows: Vec::with_capacity(MAX_CONTAINER_SCOPES_PER_TICK),
            round_robin: 0,
        }
    }

    #[cfg(test)]
    fn buffer_capacity(&self) -> usize {
        self.buffer.capacity()
    }

    #[cfg(test)]
    fn previous_capacity(&self) -> usize {
        self.previous.capacity()
    }
}

impl<F: CgroupFs> ContainerStatsSampler<F> {
    /// Sample the selected scopes at `at`; callers pass a monotonic timestamp.
    /// The returned rows contain this tick's successful reads only.
    pub fn sample(
        &mut self,
        scopes: &[ResolvedContainerScope],
        at: Instant,
    ) -> &[ContainerStatsRow] {
        let active_ids: HashSet<&str> = scopes.iter().map(|scope| scope.id.as_str()).collect();
        self.previous
            .retain(|id, _| active_ids.contains(id.as_str()));
        self.rows.clear();

        let selected = self.select_scopes(scopes);
        for index in selected {
            let scope = &scopes[index];
            self.path.clone_from(&scope.path);
            read_group(
                &mut self.fs,
                &self.filter,
                &mut self.buffer,
                &mut self.sample,
                &mut self.path,
            );
            let Some(cpu) = self.sample.cpu else {
                self.previous.remove(&scope.id);
                continue;
            };
            let old = self.previous.get(&scope.id);
            let dt = old.map(|previous| at.saturating_duration_since(previous.at).as_secs_f64());
            let counter_decreased =
                old.is_some_and(|previous| counters_decreased(&previous.sample, &self.sample));
            let valid_dt = dt.filter(|seconds| *seconds > 0.0);
            let cpu_pct = old
                .and_then(|previous| {
                    valid_dt.map(|seconds| {
                        (cpu.usage_usec.saturating_sub(
                            previous
                                .sample
                                .cpu
                                .map_or(cpu.usage_usec, |value| value.usage_usec),
                        ) as f64)
                            / (seconds * 1_000_000.0)
                            * 100.0
                    })
                })
                .filter(|_| !counter_decreased);
            let (io_read_bps, io_write_bps) =
                match (old, self.sample.io, valid_dt, counter_decreased) {
                    (Some(previous), Some(current), Some(seconds), false) => {
                        previous.sample.io.map_or((None, None), |before| {
                            (
                                Some(current.rbytes.saturating_sub(before.rbytes) as f64 / seconds),
                                Some(current.wbytes.saturating_sub(before.wbytes) as f64 / seconds),
                            )
                        })
                    }
                    _ => (None, None),
                };
            let row_gap = counter_decreased;
            self.rows.push(ContainerStatsRow {
                id: scope.id.clone(),
                cpu_pct,
                mem_current: self.sample.mem_current,
                mem_peak: self.sample.mem_peak,
                pids: self.sample.pids_current,
                io_read_bps,
                io_write_bps,
                gap: row_gap,
            });
            self.previous.insert(
                scope.id.clone(),
                PreviousSample {
                    sample: self.sample.clone(),
                    at,
                    cpu_pct: cpu_pct.unwrap_or(0.0),
                },
            );
        }
        &self.rows
    }

    fn select_scopes(&mut self, scopes: &[ResolvedContainerScope]) -> Vec<usize> {
        let mut prioritized: Vec<usize> = (0..scopes.len()).collect();
        prioritized.sort_by(|left, right| {
            let left_cpu = self
                .previous
                .get(&scopes[*left].id)
                .map_or(0.0, |previous| previous.cpu_pct);
            let right_cpu = self
                .previous
                .get(&scopes[*right].id)
                .map_or(0.0, |previous| previous.cpu_pct);
            right_cpu.total_cmp(&left_cpu).then_with(|| left.cmp(right))
        });
        let priority_count = scopes.len().min(CPU_PRIORITY_SLOTS);
        let priority: HashSet<usize> = prioritized.iter().take(priority_count).copied().collect();
        let mut selected: Vec<usize> = prioritized.into_iter().take(priority_count).collect();
        let remainder_count = scopes.len().saturating_sub(priority_count);
        if remainder_count > 0 {
            let needed = MAX_CONTAINER_SCOPES_PER_TICK.saturating_sub(selected.len());
            let start = self.round_robin % remainder_count;
            for offset in 0..remainder_count.min(needed) {
                let remainder_index = (start + offset) % remainder_count;
                let mut seen = 0;
                for index in 0..scopes.len() {
                    if !priority.contains(&index) && seen == remainder_index {
                        selected.push(index);
                        break;
                    }
                    if !priority.contains(&index) {
                        seen += 1;
                    }
                }
            }
            self.round_robin = (start + needed.min(remainder_count)) % remainder_count;
        }
        selected
    }
}

#[cfg(test)]
#[path = "containers_stats_tests.rs"]
mod tests;
