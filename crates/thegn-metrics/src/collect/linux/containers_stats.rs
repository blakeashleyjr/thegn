//! Bounded cgroup v2 sampling for container inventory supplied by the caller.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use super::cgroup::{CgroupFs, DeviceFilter, RealFs, read_group};
use crate::collect::env::Env;
use crate::collect::frame::MonitorFrame;
use crate::collect::health::{Absent, Health};
use crate::collect::model::cgroup::CgroupSample;
use crate::collect::model::containers_stats::ContainerStatsRow;
use crate::collect::schedule::Cadence;
use crate::collect::{CollectError, Collector, CollectorId, TickCtx};

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
// Reserve 56 reads for CPU-ranked scopes and 8 for the rotating remainder.
const CPU_PRIORITY_SLOTS: usize = 56;
const SAMPLE_BUFFER_BYTES: usize = 8 * 1024;

/// A caller-resolved container cgroup path, relative to the cgroup v2 mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedContainerScope {
    pub id: std::sync::Arc<str>,
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
        Some(Self {
            id: std::sync::Arc::from(id),
            path,
        })
    }
}

#[derive(Debug, Clone)]
struct PreviousSample {
    sample: CgroupSample,
    at: Instant,
    cpu_pct: f64,
    seen_generation: u64,
}

/// Reads at most 64 caller-resolved cgroup scopes per tick and derives rates.
pub struct ContainerStatsSampler<F> {
    fs: F,
    filter: DeviceFilter,
    root: PathBuf,
    buffer: Vec<u8>,
    sample: CgroupSample,
    path: PathBuf,
    previous: HashMap<std::sync::Arc<str>, PreviousSample>,
    rows: Vec<ContainerStatsRow>,
    prioritized: Vec<usize>,
    selected: Vec<usize>,
    priority: Vec<bool>,
    round_robin: usize,
    generation: u64,
}

impl<F> ContainerStatsSampler<F> {
    pub fn new(fs: F, filter: DeviceFilter, root: PathBuf) -> Self {
        Self {
            fs,
            filter,
            root,
            buffer: Vec::with_capacity(SAMPLE_BUFFER_BYTES),
            sample: CgroupSample::default(),
            path: PathBuf::new(),
            previous: HashMap::new(),
            rows: Vec::with_capacity(MAX_CONTAINER_SCOPES_PER_TICK),
            prioritized: Vec::new(),
            selected: Vec::with_capacity(MAX_CONTAINER_SCOPES_PER_TICK),
            priority: Vec::new(),
            round_robin: 0,
            generation: 0,
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
        self.generation = self.generation.wrapping_add(1);
        for scope in scopes {
            if let Some(previous) = self.previous.get_mut(scope.id.as_ref()) {
                previous.seen_generation = self.generation;
            }
        }
        self.previous
            .retain(|_, previous| previous.seen_generation == self.generation);
        self.rows.clear();

        self.select_scopes(scopes);
        for selected_index in 0..self.selected.len() {
            let index = self.selected[selected_index];
            let scope = &scopes[index];
            self.path.clear();
            self.path.push(&self.root);
            self.path.push(&scope.path);
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
                id: std::sync::Arc::clone(&scope.id),
                cpu_pct,
                mem_current: self.sample.mem_current,
                mem_peak: self.sample.mem_peak,
                pids: self.sample.pids_current,
                io_read_bps,
                io_write_bps,
                gap: row_gap,
            });
            let carried_cpu_pct =
                cpu_pct.unwrap_or_else(|| old.map_or(0.0, |previous| previous.cpu_pct));
            if let Some(previous) = self.previous.get_mut(&scope.id) {
                previous.sample.clone_from(&self.sample);
                previous.at = at;
                previous.cpu_pct = carried_cpu_pct;
                previous.seen_generation = self.generation;
            } else {
                self.previous.insert(
                    scope.id.clone(),
                    PreviousSample {
                        sample: self.sample.clone(),
                        at,
                        cpu_pct: carried_cpu_pct,
                        seen_generation: self.generation,
                    },
                );
            }
        }
        &self.rows
    }

    fn select_scopes(&mut self, scopes: &[ResolvedContainerScope]) {
        self.prioritized.clear();
        self.prioritized.extend(0..scopes.len());
        self.prioritized.sort_unstable_by(|left, right| {
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
        self.priority.resize(scopes.len(), false);
        self.priority.fill(false);
        self.selected.clear();
        self.selected
            .extend(self.prioritized.iter().take(priority_count).copied());
        for &index in &self.selected {
            self.priority[index] = true;
        }
        let remainder_count = scopes.len().saturating_sub(priority_count);
        if remainder_count > 0 {
            let needed = MAX_CONTAINER_SCOPES_PER_TICK.saturating_sub(self.selected.len());
            self.prioritized.clear();
            self.prioritized
                .extend((0..scopes.len()).filter(|index| !self.priority[*index]));
            let start = self.round_robin % remainder_count;
            for offset in 0..remainder_count.min(needed) {
                let remainder_index = (start + offset) % remainder_count;
                self.selected.push(self.prioritized[remainder_index]);
            }
            self.round_robin = (start + needed.min(remainder_count)) % remainder_count;
        }
    }
}

const RESCAN_INTERVAL_MS: u64 = 10_000;

pub struct ContainersStatsCollector {
    root: Option<PathBuf>,
    scopes: Vec<ResolvedContainerScope>,
    truncated: bool,
    rescans: u32,
    last_rescan_ms: Option<u64>,
    fs: RealFs,
    sampler: Option<ContainerStatsSampler<RealFs>>,
    directory_fs: Box<dyn super::container_scopes::DirectoryFs + Send>,
}

impl Default for ContainersStatsCollector {
    fn default() -> Self {
        Self::with_directory_fs(Box::new(super::container_scopes::RealDirectoryFs))
    }
}

impl ContainersStatsCollector {
    fn with_directory_fs(
        directory_fs: Box<dyn super::container_scopes::DirectoryFs + Send>,
    ) -> Self {
        Self {
            root: None,
            scopes: Vec::new(),
            truncated: false,
            rescans: 0,
            last_rescan_ms: None,
            fs: RealFs::default(),
            sampler: None,
            directory_fs,
        }
    }

    fn rescan(&mut self, now_ms: u64) {
        let Some(root) = self.root.as_deref() else {
            return;
        };
        let result = super::container_scopes::discover(self.directory_fs.as_ref(), root);
        self.scopes = result.scopes;
        self.truncated = result.truncated;
        self.last_rescan_ms = Some(now_ms);
        self.rescans = self.rescans.saturating_add(1);
    }
}

impl Collector for ContainersStatsCollector {
    fn id(&self) -> CollectorId {
        CollectorId::ContainersStats
    }

    fn cadence(&self) -> Cadence {
        Cadence::Every(1)
    }

    fn discover(&mut self, env: &Env) -> Health {
        self.close();
        let root = env
            .cgroup_root
            .as_deref()
            .unwrap_or_else(|| Path::new("/sys/fs/cgroup"));
        let Ok(root_metadata) = std::fs::symlink_metadata(root) else {
            return Health::Absent(Absent::NoHardware);
        };
        if !root_metadata.file_type().is_dir() {
            return Health::Absent(Absent::NoHardware);
        }
        let Some(root) = super::cgroup::cgroup2_root(root) else {
            return Health::Absent(Absent::NotOnThisOs);
        };
        self.sampler = Some(ContainerStatsSampler::new(
            std::mem::take(&mut self.fs),
            DeviceFilter::from_sys_block(Path::new("/sys/block")),
            root.clone(),
        ));
        self.root = Some(root);
        Health::Ok
    }

    fn sample(&mut self, ctx: &TickCtx, frame: &mut MonitorFrame) -> Result<(), CollectError> {
        if self.root.is_none() || self.sampler.is_none() {
            return Err(CollectError::Gone);
        }
        if self
            .last_rescan_ms
            .is_none_or(|last| ctx.now_ms.saturating_sub(last) >= RESCAN_INTERVAL_MS)
        {
            self.rescan(ctx.now_ms);
        }
        let sampler = self.sampler.as_mut().ok_or(CollectError::Gone)?;
        let rows = sampler.sample(&self.scopes, Instant::now());
        frame.parts.containers_stats.rows.clear();
        frame.parts.containers_stats.rows.extend_from_slice(rows);
        frame.parts.containers_stats.scopes = self.scopes.len().min(u32::MAX as usize) as u32;
        frame.parts.containers_stats.truncated = self.truncated;
        frame.parts.containers_stats.rescans = self.rescans;
        frame.sampled.insert(CollectorId::ContainersStats);
        Ok(())
    }

    fn close(&mut self) {
        self.root = None;
        self.scopes = Vec::new();
        self.sampler = None;
        self.fs = RealFs::default();
        self.truncated = false;
        self.rescans = 0;
        self.last_rescan_ms = None;
    }
}

#[cfg(test)]
#[path = "containers_stats_tests.rs"]
mod tests;

#[cfg(test)]
mod collector_tests {
    use super::ContainersStatsCollector;
    use crate::collect::env::Env;
    use crate::collect::frame::MonitorFrame;
    use crate::collect::health::Health;
    use crate::collect::linux::container_scopes::DirectoryFs;
    use crate::collect::{Collector, TickCtx};
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::io;
    use std::path::{Path, PathBuf};

    #[derive(Default)]
    struct FakeDirs(HashMap<PathBuf, Vec<OsString>>);

    impl DirectoryFs for FakeDirs {
        fn directories(&self, path: &Path) -> io::Result<Vec<OsString>> {
            self.0
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
    }

    #[test]
    fn rescans_are_limited_by_tick_time() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("cgroup.controllers"), b"cpu memory\n").unwrap();
        let mut collector =
            ContainersStatsCollector::with_directory_fs(Box::new(FakeDirs::default()));
        let env = Env {
            cgroup_root: Some(temp.path().to_path_buf()),
            ..Env::default()
        };
        assert_eq!(collector.discover(&env), Health::Ok);
        let mut frame = MonitorFrame::default();
        for now_ms in [0, 9_999, 10_000, 19_999, 20_000] {
            collector
                .sample(&TickCtx { tick: 0, now_ms }, &mut frame)
                .unwrap();
            let expected = if now_ms < 10_000 {
                1
            } else if now_ms < 20_000 {
                2
            } else {
                3
            };
            assert_eq!(frame.parts.containers_stats.rescans, expected);
        }
    }
}
