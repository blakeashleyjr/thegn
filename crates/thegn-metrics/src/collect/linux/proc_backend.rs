//! Linux native backend for process sampling.

use std::time::{Duration, Instant};

use sysinfo::Pid;

use crate::collect::ProcSampleRequest as SampleRequest;
use crate::procs::{ProcOwner, ProcSample, ProcSnapshot};
use crate::procs_fs::ProcFs;
use crate::procs_linux::{ProcTable, RealProcFs, clock_ticks_per_second, kept_max_from_rlimit};

const TOP_N: usize = 32;
const MAX_ANCESTRY: usize = 16;

#[derive(Debug)]
pub(crate) enum ProcBackend {
    Sysinfo,
    Native(Box<NativeBackend<RealProcFs>>),
}

impl ProcBackend {
    pub(crate) fn new() -> Self {
        match RealProcFs::new() {
            Ok(fs) => Self::Native(Box::new(NativeBackend::new(fs))),
            Err(_) => Self::Sysinfo,
        }
    }

    pub(crate) fn reset(&mut self, primed: &mut bool) {
        match self {
            Self::Native(native) => reset_with(native, primed),
            Self::Sysinfo => *primed = false,
        }
    }

    pub(crate) fn sample(
        &mut self,
        request: SampleRequest<'_>,
        primed: &mut bool,
    ) -> Option<ProcSnapshot> {
        let Self::Native(native) = self else {
            return None;
        };
        let (snapshot, _successful) = sample_with(
            native,
            request,
            Instant::now(),
            sysinfo::System::uptime(),
            primed,
        );
        Some(snapshot)
    }
}

pub(crate) struct NativeBackend<F: ProcFs> {
    table: ProcTable<F>,
    boot_time: Option<u64>,
    ticks_per_second: Option<u64>,
    page_size: Option<u64>,
    previous_pids: Vec<u32>,
    pass_time: Duration,
    last_sample: Option<Instant>,
    scratch: Vec<(Pid, f32, u64)>,
}

impl NativeBackend<RealProcFs> {
    fn new(fs: RealProcFs) -> Self {
        Self::with(
            fs,
            kept_max_from_rlimit(),
            read_boot_time(),
            clock_ticks_per_second(),
            page_size(),
        )
    }
}

impl<F: ProcFs> std::fmt::Debug for NativeBackend<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBackend").finish_non_exhaustive()
    }
}

impl<F: ProcFs> NativeBackend<F> {
    fn with(
        fs: F,
        kept_max: usize,
        boot_time: Option<u64>,
        ticks_per_second: Option<u64>,
        page_size: Option<u64>,
    ) -> Self {
        Self {
            table: ProcTable::with_kept_max(fs, kept_max),
            boot_time,
            ticks_per_second,
            page_size,
            previous_pids: Vec::new(),
            pass_time: Duration::ZERO,
            last_sample: None,
            scratch: Vec::new(),
        }
    }

    /// Releases all per-process descriptors and clears CPU sampling state.
    /// The `RealProcFs` root directory descriptor remains open with the backend.
    pub(crate) fn reset(&mut self) {
        self.table.reset();
        self.previous_pids.clear();
        self.pass_time = Duration::ZERO;
        self.last_sample = None;
        self.scratch.clear();
    }
}

fn reset_with<F: ProcFs>(native: &mut NativeBackend<F>, primed: &mut bool) {
    native.reset();
    *primed = false;
}

fn sample_with<F: ProcFs>(
    native: &mut NativeBackend<F>,
    mut request: SampleRequest<'_>,
    now: Instant,
    uptime_secs: u64,
    primed: &mut bool,
) -> (ProcSnapshot, bool) {
    if let Some(previous) = native.last_sample {
        native.pass_time = native
            .pass_time
            .saturating_add(now.saturating_duration_since(previous));
    }
    native.last_sample = Some(now);
    if native
        .table
        .pass(native.pass_time, &native.previous_pids)
        .is_err()
    {
        return (
            ProcSnapshot {
                enabled: true,
                primed: false,
                ..ProcSnapshot::default()
            },
            false,
        );
    }
    request.primed = *primed;
    let snapshot = project_native_table(native, request, uptime_secs).0;
    *primed = true;
    (snapshot, true)
}

fn project_native_table<F: ProcFs>(
    native: &mut NativeBackend<F>,
    request: SampleRequest<'_>,
    uptime_secs: u64,
) -> (ProcSnapshot, Vec<Pid>) {
    let table = &native.table;
    let live = table.entries().iter().filter(|entry| !entry.ghost).count();
    native.scratch.clear();
    native.scratch.reserve(live);
    for entry in table.entries().iter().filter(|entry| !entry.ghost) {
        native.scratch.push((
            Pid::from_u32(entry.id.pid),
            entry
                .cpu_delta
                .and_then(|delta| delta.percent)
                .unwrap_or(0.0) as f32,
            native
                .page_size
                .map(|size| entry.stat.rss_pages.max(0) as u64 * size)
                .unwrap_or(0),
        ));
    }
    let chosen = choose_top(&mut native.scratch, request.rows.max(TOP_N));
    let procs = chosen
        .iter()
        .filter_map(|pid| {
            let index = table
                .entries()
                .binary_search_by_key(&pid.as_u32(), |entry| entry.id.pid)
                .ok()?;
            let entry = &table.entries()[index];
            if entry.ghost {
                return None;
            }
            let name = entry.stat.comm.to_string_lossy();
            let start_time = native
                .boot_time
                .and_then(|btime| {
                    native.ticks_per_second.and_then(|hz| {
                        crate::procs_parse::start_unix_secs(entry.stat.starttime, hz, btime)
                    })
                })
                .unwrap_or(0);
            let run_secs = native
                .ticks_per_second
                .filter(|hz| *hz > 0)
                .map(|hz| uptime_secs.saturating_sub(entry.stat.starttime / hz))
                .unwrap_or(0);
            Some(ProcSample {
                pid: entry.id.pid,
                ppid: (entry.stat.ppid != 0).then_some(entry.stat.ppid),
                start_time,
                start_ticks: Some(entry.stat.starttime),
                name: name.chars().take(32).collect(),
                cpu_pct: entry
                    .cpu_delta
                    .and_then(|delta| delta.percent)
                    .unwrap_or(0.0) as f32,
                rss_bytes: native
                    .page_size
                    .map(|size| entry.stat.rss_pages.max(0) as u64 * size)
                    .unwrap_or(0),
                run_secs,
                owner: owner_of_table(
                    table,
                    entry.id.pid,
                    request.self_pid,
                    request.daemon_pid,
                    request.pane_pids,
                ),
            })
        })
        .collect();
    native.previous_pids.clear();
    native
        .previous_pids
        .extend(chosen.iter().map(|pid| pid.as_u32()));
    (
        ProcSnapshot {
            procs,
            total: live,
            primed: request.primed,
            enabled: true,
        },
        chosen,
    )
}

fn owner_of_table<F: ProcFs>(
    table: &ProcTable<F>,
    pid: u32,
    self_pid: u32,
    daemon_pid: Option<u32>,
    pane_pids: &[(u32, u32)],
) -> ProcOwner {
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY {
        if current == self_pid {
            return ProcOwner::ThegnSelf;
        }
        if Some(current) == daemon_pid {
            return ProcOwner::ThegnDaemon;
        }
        if let Some((_, pane)) = pane_pids.iter().find(|(pane_pid, _)| *pane_pid == current) {
            return ProcOwner::Pane(*pane);
        }
        let parent = table
            .entries()
            .binary_search_by_key(&current, |entry| entry.id.pid)
            .ok()
            .and_then(|index| {
                let entry = &table.entries()[index];
                (!entry.ghost).then_some(entry.stat.ppid)
            });
        match parent {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    ProcOwner::Other
}

fn read_boot_time() -> Option<u64> {
    let contents = match std::fs::read("/proc/stat") {
        Ok(contents) => contents,
        Err(_) => return None,
    };
    contents.split(|byte| *byte == b'\n').find_map(|line| {
        let mut fields = line.split(|byte| byte.is_ascii_whitespace());
        (fields.next() == Some(b"btime".as_slice()))
            .then(|| fields.find(|field| !field.is_empty()))
            .flatten()
            .and_then(|field| std::str::from_utf8(field).ok()?.parse().ok())
    })
}

fn page_size() -> Option<u64> {
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(value).ok().filter(|value| *value > 0)
}

fn choose_top(scratch: &mut [(Pid, f32, u64)], keep: usize) -> Vec<Pid> {
    let n = keep.min(scratch.len());
    let mut chosen = Vec::with_capacity(n * 2);
    if n > 0 {
        scratch.select_nth_unstable_by(n - 1, |a, b| {
            b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0))
        });
        chosen.extend(scratch[..n].iter().map(|(pid, _, _)| *pid));
        scratch.select_nth_unstable_by(n - 1, |a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        chosen.extend(scratch[..n].iter().map(|(pid, _, _)| *pid));
    }
    crate::procs::dedup_pids(chosen)
}

#[cfg(test)]
#[path = "proc_backend_tests.rs"]
mod tests;
