//! Per-process sampling for the monitor's Processes tab.
//!
//! Kept apart from [`crate::sample::StatsSampler`] on purpose, in two ways.
//!
//! **A separate `System` instance.** The main sampler's narrow
//! `refresh_processes_specifics(ProcessesToUpdate::Some(&pids))` path is
//! untouched by anything here, so the duplicate-PID abort analysis documented on
//! `sample::refresh_pid_list` — and its property tests — stay valid unchanged.
//! This module uses [`ProcessesToUpdate::All`], where sysinfo builds the PID
//! list itself from the OS and no caller-supplied list exists, so that hazard is
//! structurally absent rather than merely avoided.
//!
//! **A separate gate and cadence.** Enumerating every process is the one
//! genuinely expensive sample thegn takes (~3–12 ms wall on a ~400-process
//! desktop), so it runs only while the Processes tab is open and unpaused, and
//! never faster than [`MIN_INTERVAL`] — per-process CPU is a delta, and at
//! half-second spacing it is noise rather than data.
//!
//! If a *targeted* refresh is ever needed here, it must route through
//! [`dedup_pids`]. Hand-rolling a second PID-list builder is exactly how that
//! abort comes back.

use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// Never enumerate faster than this: per-process CPU is a delta over the
/// interval, and a shorter one measures scheduler jitter rather than load.
pub const MIN_INTERVAL: Duration = Duration::from_secs(2);

/// How many processes to keep per sort key. The tab shows the union of the top
/// N by CPU and the top N by memory, so flipping the sort column re-sorts an
/// in-memory list instantly instead of waiting for the next sample.
const TOP_N: usize = 32;

/// Depth limit when walking a process's ancestry for pane attribution. Cheap
/// insurance against a malformed parent chain.
const MAX_ANCESTRY: usize = 16;

/// Who a process belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcOwner {
    #[default]
    Other,
    /// The thegn UI process itself.
    ThegnSelf,
    /// The pane daemon.
    ThegnDaemon,
    /// Running inside one of thegn's panes (directly or as a descendant).
    Pane(u32),
}

/// One process's reading.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub ppid: Option<u32>,
    /// Executable name, truncated. Deliberately **not** the full command line:
    /// reading `cmdline` is an extra syscall per process, and a command line can
    /// carry secrets that have no business in a UI list.
    pub name: String,
    /// Per-core sum, unclamped — the same convention as `self_cpu_pct`, so a
    /// busy multi-threaded process reads above 100%.
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    pub run_secs: u64,
    pub owner: ProcOwner,
}

/// One enumeration pass.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcSnapshot {
    /// The union of top-N-by-CPU and top-N-by-memory. Already trimmed.
    pub procs: Vec<ProcSample>,
    /// How many processes existed at sample time.
    pub total: usize,
    /// False on the first pass after the gate opens: CPU is a delta and there is
    /// nothing yet to diff against, so the UI should say so rather than show a
    /// column of confident zeroes.
    pub primed: bool,
    /// Whether process sampling is enabled at all (`[monitor] processes`).
    pub enabled: bool,
}

/// Sort + dedup a PID list.
///
/// **The only sanctioned way to build a `ProcessesToUpdate::Some` list.** A
/// repeated PID hands two rayon workers the same `Process` entry, both can close
/// its cached `/proc` file descriptor, and std aborts the process over the
/// double close. See `sample::refresh_pid_list` for the measured failure rates.
pub fn dedup_pids(mut v: Vec<Pid>) -> Vec<Pid> {
    v.sort_unstable();
    v.dedup();
    v
}

/// Stateful process sampler. Lives on its own thread; never the event loop.
pub struct ProcSampler {
    sys: System,
    /// Reused between passes so the common case allocates nothing: only the
    /// ≤2·[`TOP_N`] survivors get a `ProcSample` (and its `String`) built.
    scratch: Vec<(Pid, f32, u64)>,
    primed: bool,
    last: Option<Instant>,
    /// `(pid, pane_id)` for thegn's own panes, refreshed by the host when the
    /// pane set changes.
    pane_pids: Vec<(u32, u32)>,
    self_pid: u32,
    daemon_pid: Option<u32>,
    rows: usize,
}

impl Default for ProcSampler {
    fn default() -> Self {
        Self::new(TOP_N)
    }
}

impl ProcSampler {
    /// `rows` is `[monitor] proc_rows` — how many the UI will show; the sampler
    /// keeps at least that many per sort key.
    pub fn new(rows: usize) -> Self {
        ProcSampler {
            // Nothing but processes: this instance never reads cpu/mem/disk
            // globals, which the main sampler already covers.
            sys: System::new_with_specifics(RefreshKind::nothing()),
            scratch: Vec::new(),
            primed: false,
            last: None,
            pane_pids: Vec::new(),
            self_pid: std::process::id(),
            daemon_pid: None,
            rows: rows.clamp(1, 200),
        }
    }

    /// Point pane attribution at the current pane set.
    pub fn set_pane_pids(&mut self, pids: Vec<(u32, u32)>) {
        self.pane_pids = pids;
    }

    pub fn set_daemon_pid(&mut self, pid: Option<u32>) {
        self.daemon_pid = pid;
    }

    /// Release the process table and forget the CPU baseline.
    ///
    /// Called when the gate closes, so a monitor the user shut costs exactly
    /// nothing — not a stale few hundred KB of process map.
    pub fn reset(&mut self) {
        self.sys = System::new_with_specifics(RefreshKind::nothing());
        self.scratch = Vec::new();
        self.primed = false;
        self.last = None;
    }

    /// Whether enough time has passed for another pass to be meaningful.
    pub fn due(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|t| now.duration_since(t) >= MIN_INTERVAL)
    }

    /// Take one reading. Blocking — background thread only.
    pub fn sample(&mut self) -> ProcSnapshot {
        self.last = Some(Instant::now());
        // Only what we display. Every other `with_*` costs a readlink or an
        // extra read per process.
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );

        // Pass 1: fold every process into reused scratch — no allocation.
        self.scratch.clear();
        self.scratch.extend(
            self.sys
                .processes()
                .iter()
                .map(|(pid, p)| (*pid, p.cpu_usage(), p.memory())),
        );
        let total = self.scratch.len();

        // Pass 2: keep the union of the two top-N sets, so the UI can re-sort
        // without waiting for a fresh sample under a new key.
        let keep = self.rows.max(TOP_N).min(total);
        let mut chosen: Vec<Pid> = Vec::with_capacity(keep * 2);
        if keep > 0 {
            let n = keep.min(self.scratch.len());
            self.scratch
                .select_nth_unstable_by(n - 1, |a, b| b.1.total_cmp(&a.1));
            chosen.extend(self.scratch[..n].iter().map(|(p, _, _)| *p));
            self.scratch
                .select_nth_unstable_by(n - 1, |a, b| b.2.cmp(&a.2));
            chosen.extend(self.scratch[..n].iter().map(|(p, _, _)| *p));
        }
        chosen = dedup_pids(chosen);

        let procs = chosen
            .into_iter()
            .filter_map(|pid| {
                let p = self.sys.process(pid)?;
                Some(ProcSample {
                    pid: pid.as_u32(),
                    ppid: p.parent().map(|x| x.as_u32()),
                    name: p.name().to_string_lossy().chars().take(32).collect(),
                    cpu_pct: p.cpu_usage(),
                    rss_bytes: p.memory(),
                    run_secs: p.run_time(),
                    owner: self.owner_of(pid),
                })
            })
            .collect();

        let primed = self.primed;
        self.primed = true;
        ProcSnapshot {
            procs,
            total,
            primed,
            enabled: true,
        }
    }

    /// Attribute a process to thegn, the daemon, or a pane — walking ancestry,
    /// so `cargo` under `zsh` under a pane shell is still that pane's.
    ///
    /// Uses sysinfo's `parent()`, which is free once the table is refreshed;
    /// re-reading `/proc` here would cost a syscall per process and would not
    /// work on macOS at all.
    fn owner_of(&self, pid: Pid) -> ProcOwner {
        let mut cur = pid;
        for _ in 0..MAX_ANCESTRY {
            let raw = cur.as_u32();
            if raw == self.self_pid {
                return ProcOwner::ThegnSelf;
            }
            if Some(raw) == self.daemon_pid {
                return ProcOwner::ThegnDaemon;
            }
            if let Some((_, pane)) = self.pane_pids.iter().find(|(p, _)| *p == raw) {
                return ProcOwner::Pane(*pane);
            }
            match self.sys.process(cur).and_then(|p| p.parent()) {
                // A self-parenting or missing chain terminates the walk.
                Some(next) if next != cur => cur = next,
                _ => break,
            }
        }
        ProcOwner::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_pids_never_repeats() {
        // The invariant a process abort depends on. Property-checked the same
        // way `sample::refresh_pid_list` is.
        for raw in [
            vec![1u32, 1, 1],
            vec![7, 3, 7, 3],
            vec![9],
            vec![],
            vec![5, 4, 3, 2, 1],
        ] {
            let out = dedup_pids(raw.iter().map(|p| Pid::from_u32(*p)).collect());
            let mut seen = std::collections::HashSet::new();
            for p in &out {
                assert!(seen.insert(*p), "repeated pid in {out:?}");
            }
            // Sorted, and preserves the distinct set.
            assert!(out.windows(2).all(|w| w[0] < w[1]), "{out:?}");
            let want: std::collections::HashSet<u32> = raw.into_iter().collect();
            assert_eq!(out.len(), want.len());
        }
    }

    #[test]
    fn a_fresh_sampler_is_unprimed_and_immediately_due() {
        let s = ProcSampler::new(20);
        assert!(!s.primed);
        assert!(s.due(Instant::now()));
    }

    #[test]
    fn rows_are_clamped_to_something_sane() {
        // A hostile or fat-fingered config must not ask for a million rows.
        assert_eq!(ProcSampler::new(0).rows, 1);
        assert_eq!(ProcSampler::new(1_000_000).rows, 200);
        assert_eq!(ProcSampler::new(20).rows, 20);
    }

    #[test]
    fn reset_clears_the_baseline_so_the_next_pass_reports_unprimed() {
        // Closing the tab must drop the process table AND the CPU baseline —
        // a stale delta across a long gap would be meaningless.
        let mut s = ProcSampler::new(20);
        s.primed = true;
        s.last = Some(Instant::now());
        s.reset();
        assert!(!s.primed);
        assert!(s.due(Instant::now()));
    }

    #[test]
    fn owner_resolves_self_and_daemon_without_a_process_table() {
        // Attribution must not depend on the enumeration having run.
        let mut s = ProcSampler::new(20);
        s.set_daemon_pid(Some(4242));
        assert_eq!(
            s.owner_of(Pid::from_u32(std::process::id())),
            ProcOwner::ThegnSelf
        );
        assert_eq!(s.owner_of(Pid::from_u32(4242)), ProcOwner::ThegnDaemon);
        s.set_pane_pids(vec![(777, 3)]);
        assert_eq!(s.owner_of(Pid::from_u32(777)), ProcOwner::Pane(3));
        // An unrelated pid with no parent chain is simply Other, not a panic.
        assert_eq!(s.owner_of(Pid::from_u32(999_999)), ProcOwner::Other);
    }

    #[test]
    fn an_unprimed_snapshot_is_labelled_as_such() {
        let d = ProcSnapshot::default();
        assert!(!d.primed && !d.enabled && d.procs.is_empty());
    }
}
