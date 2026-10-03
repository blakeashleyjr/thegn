//! GPU sampling that can never stall the sampler.
//!
//! sysfs (amdgpu/i915) is a couple of file reads and is read inline. The helper
//! backends (`nvidia-smi`, `ioreg`) are subprocesses; a wedged one used to freeze
//! the whole refresh ticker, because discovery ran inside `StatsSampler::new` and
//! the slow-tier sample ran inline. Here every helper run — discovery included —
//! happens on one short-lived background thread, under [`run_bounded`]:
//!
//! * at most one probe in flight; the flag is held until the helper is reaped;
//! * [`GpuMonitor::sample`] never blocks and serves the latest good reading;
//! * failures back off exponentially, absence is retried rarely — a missing or
//!   failing helper is not re-spawned every slow tick;
//! * a failed or malformed run never overwrites the last good reading, and a
//!   reading older than `stale_max` is dropped rather than shown as live.
//!   Nothing here ever fabricates a zero.
//!
//! No timer is added: probes are only kicked by the sampler's existing slow tick.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::gpu::{
    GpuReading, IOREG_ARGS, NVIDIA_QUERY_ARGS, find_sysfs, parse_ioaccel, parse_nvidia, read_sysfs,
};
use crate::gpu_exec::{ExecFailure, run_bounded};

/// Observable state of the helper backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuHealth {
    /// No probe has completed yet.
    Pending,
    /// The latest probe succeeded.
    Live,
    /// The latest probe failed; the last good reading (if any) is stale.
    Failing(ExecFailure),
    /// No helper answers on this machine.
    Absent,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backend {
    Unresolved,
    Nvidia,
    IoAccel,
}

/// A command plus fixed leading args (lets tests run `sh script`).
#[derive(Clone)]
pub(crate) struct HelperCmd {
    pub program: OsString,
    pub prefix: Vec<OsString>,
}

impl HelperCmd {
    fn named(p: &str) -> Self {
        Self {
            program: p.into(),
            prefix: Vec::new(),
        }
    }
    fn run(&self, extra: &[&str], deadline: Duration, cap: usize) -> Result<Vec<u8>, ExecFailure> {
        let mut args: Vec<&std::ffi::OsStr> = self.prefix.iter().map(|a| a.as_os_str()).collect();
        args.extend(extra.iter().map(std::ffi::OsStr::new));
        run_bounded(&self.program, &args, deadline, cap)
    }
}

/// Bounds and commands; `Default` is production.
#[derive(Clone)]
pub(crate) struct HelperSpec {
    pub nvidia: HelperCmd,
    pub ioreg: HelperCmd,
    pub try_ioreg: bool,
    pub discovery_deadline: Duration,
    pub sample_deadline: Duration,
    pub output_cap: usize,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub absent_retry: Duration,
    pub stale_max: Duration,
}

impl Default for HelperSpec {
    fn default() -> Self {
        Self {
            nvidia: HelperCmd::named("nvidia-smi"),
            ioreg: HelperCmd::named("ioreg"),
            try_ioreg: cfg!(target_os = "macos"),
            discovery_deadline: Duration::from_secs(3),
            sample_deadline: Duration::from_secs(5),
            output_cap: 64 * 1024,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(600),
            absent_retry: Duration::from_secs(600),
            stale_max: Duration::from_secs(120),
        }
    }
}

struct State {
    backend: Backend,
    last_good: Option<(GpuReading, Instant)>,
    health: GpuHealth,
    in_flight: bool,
    /// The helper is not installed (ENOENT): cached for the process lifetime,
    /// exactly like the old probe-once behaviour — no retry, no spawn.
    gave_up: bool,
    next_attempt: Option<Instant>,
    failures: u32,
    /// Probes started so far — the generation identity of the latest probe.
    probes_started: u64,
}

struct Shared {
    state: Mutex<State>,
    idle: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Clears `in_flight` on every exit path, including a panic in the probe.
struct InFlight {
    shared: Arc<Shared>,
    spec: Arc<HelperSpec>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut st = self.shared.lock();
        if std::thread::panicking() {
            note_failure(&mut st, &self.spec, ExecFailure::Panicked);
        }
        st.in_flight = false;
        self.shared.idle.notify_all();
    }
}

enum Probe {
    Reading(Backend, GpuReading),
    /// No helper answers. `permanent` = not installed at all (never retried);
    /// otherwise present-but-unusable, retried after `absent_retry`.
    Absent {
        permanent: bool,
    },
    Failed(ExecFailure),
}

pub(crate) struct GpuMonitor {
    sysfs: Option<PathBuf>,
    shared: Arc<Shared>,
    spec: Arc<HelperSpec>,
}

impl GpuMonitor {
    pub(crate) fn new() -> Self {
        Self::with_spec(find_sysfs(), HelperSpec::default())
    }

    pub(crate) fn with_spec(sysfs: Option<PathBuf>, spec: HelperSpec) -> Self {
        Self {
            sysfs,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    backend: Backend::Unresolved,
                    last_good: None,
                    health: GpuHealth::Pending,
                    in_flight: false,
                    gave_up: false,
                    next_attempt: None,
                    failures: 0,
                    probes_started: 0,
                }),
                idle: Condvar::new(),
            }),
            spec: Arc::new(spec),
        }
    }

    /// One sample. Never blocks on a helper: on the slow tier it may start a
    /// background probe, and always returns the latest good reading.
    pub(crate) fn sample(&self, slow: bool) -> GpuReading {
        if let Some(p) = &self.sysfs {
            return read_sysfs(p);
        }
        if slow {
            self.kick();
        }
        self.latest()
    }

    pub(crate) fn latest(&self) -> GpuReading {
        let st = self.shared.lock();
        match &st.last_good {
            Some((r, at)) if at.elapsed() <= self.spec.stale_max => r.clone(),
            _ => GpuReading::default(),
        }
    }

    /// Backend health (last-good/stale/error is observable here).
    #[allow(dead_code)] // consumed by tests today; the host seam for a stale badge
    pub(crate) fn health(&self) -> GpuHealth {
        self.shared.lock().health
    }

    fn kick(&self) {
        {
            let mut st = self.shared.lock();
            if st.in_flight || st.gave_up || st.next_attempt.is_some_and(|t| Instant::now() < t) {
                return;
            }
            st.in_flight = true;
            st.probes_started += 1;
        }
        let shared = Arc::clone(&self.shared);
        let spec = Arc::clone(&self.spec);
        let spawned = std::thread::Builder::new()
            .name("thegn-gpu-probe".into())
            .spawn(move || {
                let _guard = InFlight {
                    shared: Arc::clone(&shared),
                    spec: Arc::clone(&spec),
                };
                let backend = shared.lock().backend;
                let probe = run_probe(&spec, backend);
                apply(&mut shared.lock(), &spec, probe);
            });
        if spawned.is_err() {
            let mut st = self.shared.lock();
            note_failure(&mut st, &self.spec, ExecFailure::Spawn { not_found: false });
            st.in_flight = false;
            self.shared.idle.notify_all();
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn probes_started(&self) -> u64 {
        self.shared.lock().probes_started
    }

    /// Test helper: block until no probe is in flight.
    #[cfg(all(test, unix))]
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let st = self.shared.lock();
        let (st, res) = self
            .shared
            .idle
            .wait_timeout_while(st, timeout, |s| s.in_flight)
            .unwrap_or_else(|p| p.into_inner());
        drop(st);
        !res.timed_out()
    }
}

fn nvidia_sample(spec: &HelperSpec) -> Probe {
    match spec
        .nvidia
        .run(&NVIDIA_QUERY_ARGS, spec.sample_deadline, spec.output_cap)
        .and_then(|o| parse_nvidia(&String::from_utf8_lossy(&o)).ok_or(ExecFailure::Malformed))
    {
        Ok(r) => Probe::Reading(Backend::Nvidia, r),
        Err(e) => Probe::Failed(e),
    }
}

fn ioreg_sample(spec: &HelperSpec, deadline: Duration) -> Probe {
    match spec
        .ioreg
        .run(&IOREG_ARGS, deadline, spec.output_cap)
        .and_then(|o| parse_ioaccel(&String::from_utf8_lossy(&o)).ok_or(ExecFailure::Malformed))
    {
        Ok(r) => Probe::Reading(Backend::IoAccel, r),
        Err(e) => Probe::Failed(e),
    }
}

fn run_probe(spec: &HelperSpec, backend: Backend) -> Probe {
    match backend {
        Backend::Nvidia => nvidia_sample(spec),
        Backend::IoAccel => ioreg_sample(spec, spec.sample_deadline),
        Backend::Unresolved => {
            let nvidia_missing =
                match spec
                    .nvidia
                    .run(&["--version"], spec.discovery_deadline, spec.output_cap)
                {
                    Ok(_) => return nvidia_sample(spec),
                    Err(ExecFailure::Timeout) => return Probe::Failed(ExecFailure::Timeout),
                    Err(ExecFailure::Spawn { not_found: true }) => true,
                    // Present but unusable: fall through to the other backend.
                    Err(_) => false,
                };
            if spec.try_ioreg {
                // Accept the backend only if the counter actually parses.
                return match ioreg_sample(spec, spec.discovery_deadline) {
                    Probe::Failed(ExecFailure::Spawn { .. } | ExecFailure::Malformed) => {
                        Probe::Absent {
                            permanent: nvidia_missing,
                        }
                    }
                    other => other,
                };
            }
            Probe::Absent {
                permanent: nvidia_missing,
            }
        }
    }
}

fn apply(st: &mut State, spec: &HelperSpec, probe: Probe) {
    match probe {
        Probe::Reading(b, r) => {
            st.backend = b;
            st.last_good = Some((r, Instant::now()));
            st.health = GpuHealth::Live;
            st.failures = 0;
            st.next_attempt = None;
        }
        Probe::Absent { permanent } => {
            st.backend = Backend::Unresolved;
            st.health = GpuHealth::Absent;
            st.failures = 0;
            st.gave_up = permanent;
            st.next_attempt = Some(Instant::now() + spec.absent_retry);
        }
        Probe::Failed(f) => note_failure(st, spec, f),
    }
}

fn note_failure(st: &mut State, spec: &HelperSpec, f: ExecFailure) {
    st.failures = st.failures.saturating_add(1);
    let shift = (st.failures - 1).min(16);
    let delay = spec
        .backoff_base
        .saturating_mul(1u32 << shift)
        .min(spec.backoff_max);
    st.next_attempt = Some(Instant::now() + delay);
    st.health = GpuHealth::Failing(f);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::gpu_exec::test_support::script;
    use std::path::Path;

    fn spec_for(dir: &Path, body: &str) -> HelperSpec {
        let s = script(dir, "smi.sh", body);
        HelperSpec {
            nvidia: HelperCmd {
                program: "sh".into(),
                prefix: vec![s.into_os_string()],
            },
            try_ioreg: false,
            discovery_deadline: Duration::from_secs(3),
            sample_deadline: Duration::from_secs(3),
            output_cap: 4096,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(1),
            absent_retry: Duration::from_secs(3600),
            stale_max: Duration::from_secs(60),
            ..HelperSpec::default()
        }
    }

    /// Short deadlines — only for tests where the helper is meant to hang.
    fn hang(mut spec: HelperSpec) -> HelperSpec {
        spec.discovery_deadline = Duration::from_millis(250);
        spec.sample_deadline = Duration::from_millis(250);
        spec
    }

    const GOOD: &str =
        "case \"$1\" in --version) echo v;; *) echo '42, 100, 200, 50, 10.5';; esac\n";

    fn probe_once(m: &GpuMonitor) {
        m.sample(true);
        assert!(m.wait_idle(Duration::from_secs(10)));
    }

    #[test]
    fn good_helper_yields_a_live_reading() {
        let d = tempfile::tempdir().unwrap();
        let m = GpuMonitor::with_spec(None, spec_for(d.path(), GOOD));
        // First call returns immediately with nothing — never a fabricated zero.
        assert_eq!(m.sample(true).util_pct, None);
        assert!(m.wait_idle(Duration::from_secs(10)));
        assert_eq!(m.health(), GpuHealth::Live);
        let r = m.sample(false);
        assert_eq!(r.util_pct, Some(42));
        assert_eq!(r.mem_mib, Some((100, 200)));
    }

    #[test]
    fn startup_hang_never_blocks_the_sampler() {
        let d = tempfile::tempdir().unwrap();
        let m = GpuMonitor::with_spec(None, hang(spec_for(d.path(), "sleep 300\n")));
        let t = Instant::now();
        assert_eq!(m.sample(true), GpuReading::default());
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(m.wait_idle(Duration::from_secs(10)));
        assert_eq!(m.health(), GpuHealth::Failing(ExecFailure::Timeout));
    }

    #[test]
    fn sample_hang_keeps_last_good_and_reports_failing() {
        let d = tempfile::tempdir().unwrap();
        let flag = d.path().join("hang");
        let body = format!(
            "case \"$1\" in --version) echo v;; *) [ -e {f} ] && sleep 300; echo '7, 1, 2, 3, 4';; esac\n",
            f = flag.display()
        );
        let m = GpuMonitor::with_spec(None, spec_for(d.path(), &body));
        probe_once(&m);
        assert_eq!(m.latest().util_pct, Some(7));
        std::fs::write(&flag, "").unwrap();
        std::thread::sleep(Duration::from_millis(5)); // backoff_base is 1ms
        probe_once(&m);
        assert_eq!(m.health(), GpuHealth::Failing(ExecFailure::Timeout));
        assert_eq!(m.latest().util_pct, Some(7), "stale value preserved");
    }

    #[test]
    fn unusable_version_probe_means_absent() {
        // `yes` overflows the cap and `exit 2` is nonzero: both make
        // `--version` unusable, so the backend is treated as not present.
        for body in ["yes\n", "exit 2\n"] {
            let d = tempfile::tempdir().unwrap();
            let m = GpuMonitor::with_spec(None, spec_for(d.path(), body));
            probe_once(&m);
            assert_eq!(m.health(), GpuHealth::Absent, "{body}");
            assert_eq!(m.latest(), GpuReading::default());
        }
    }

    #[test]
    fn query_stage_failures_are_typed() {
        for (q, want) in [
            ("yes", ExecFailure::OutputTooLarge),
            ("exit 5", ExecFailure::Status(Some(5))),
            ("echo garbage", ExecFailure::Malformed),
        ] {
            let d = tempfile::tempdir().unwrap();
            let body = format!("case \"$1\" in --version) echo v;; *) {q};; esac\n");
            let m = GpuMonitor::with_spec(None, spec_for(d.path(), &body));
            probe_once(&m);
            assert_eq!(m.health(), GpuHealth::Failing(want));
            assert_eq!(m.latest(), GpuReading::default());
        }
    }

    #[test]
    fn missing_helper_is_absent_and_not_respawned() {
        let mut spec = HelperSpec {
            try_ioreg: false,
            ..HelperSpec::default()
        };
        spec.nvidia = HelperCmd::named("/nonexistent/thegn-nvidia-smi");
        let m = GpuMonitor::with_spec(None, spec);
        probe_once(&m);
        assert_eq!(m.health(), GpuHealth::Absent);
        for _ in 0..20 {
            m.sample(true);
        }
        assert_eq!(
            m.probes_started(),
            1,
            "absence is cached, not retried per tick"
        );
    }

    #[test]
    fn failures_back_off_instead_of_respawning_every_tick() {
        let d = tempfile::tempdir().unwrap();
        let mut spec = hang(spec_for(d.path(), "sleep 300\n"));
        spec.backoff_base = Duration::from_secs(3600);
        spec.backoff_max = Duration::from_secs(3600);
        let m = GpuMonitor::with_spec(None, spec);
        probe_once(&m);
        for _ in 0..20 {
            m.sample(true);
        }
        assert_eq!(m.probes_started(), 1);
    }

    #[test]
    fn only_one_probe_in_flight() {
        let d = tempfile::tempdir().unwrap();
        let mut spec = spec_for(d.path(), "sleep 300\n");
        spec.discovery_deadline = Duration::from_secs(2);
        let m = GpuMonitor::with_spec(None, spec);
        for _ in 0..10 {
            m.sample(true);
        }
        assert_eq!(m.probes_started(), 1);
        assert!(m.wait_idle(Duration::from_secs(10)));
    }

    #[test]
    fn stale_readings_expire_instead_of_posing_as_live() {
        let d = tempfile::tempdir().unwrap();
        let mut spec = spec_for(d.path(), GOOD);
        spec.stale_max = Duration::from_millis(20);
        let m = GpuMonitor::with_spec(None, spec);
        probe_once(&m);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(m.latest(), GpuReading::default());
    }

    #[test]
    fn non_slow_ticks_never_probe() {
        let d = tempfile::tempdir().unwrap();
        let m = GpuMonitor::with_spec(None, spec_for(d.path(), GOOD));
        for _ in 0..5 {
            m.sample(false);
        }
        assert_eq!(m.probes_started(), 0);
    }

    #[test]
    fn not_installed_is_cached_for_the_process_lifetime() {
        let mut spec = HelperSpec {
            try_ioreg: false,
            absent_retry: Duration::from_millis(1),
            ..HelperSpec::default()
        };
        spec.nvidia = HelperCmd::named("/nonexistent/thegn-nvidia-smi");
        let m = GpuMonitor::with_spec(None, spec);
        probe_once(&m);
        std::thread::sleep(Duration::from_millis(10)); // past absent_retry
        for _ in 0..20 {
            m.sample(true);
        }
        assert_eq!(m.probes_started(), 1, "ENOENT must never be re-spawned");
    }

    #[test]
    fn present_but_unusable_is_retried_after_absent_retry() {
        let d = tempfile::tempdir().unwrap();
        let mut spec = spec_for(d.path(), "exit 2\n");
        spec.absent_retry = Duration::from_millis(1);
        let m = GpuMonitor::with_spec(None, spec);
        probe_once(&m);
        std::thread::sleep(Duration::from_millis(10));
        probe_once(&m);
        assert_eq!(m.probes_started(), 2);
    }

    fn ioreg_spec(dir: &Path, body: &str) -> HelperSpec {
        let s = script(dir, "ioreg.sh", body);
        HelperSpec {
            nvidia: HelperCmd::named("/nonexistent/thegn-nvidia-smi"),
            ioreg: HelperCmd {
                program: "sh".into(),
                prefix: vec![s.into_os_string()],
            },
            try_ioreg: true,
            discovery_deadline: Duration::from_secs(3),
            sample_deadline: Duration::from_secs(3),
            output_cap: 4096,
            backoff_base: Duration::from_millis(1),
            backoff_max: Duration::from_millis(1),
            absent_retry: Duration::from_millis(1),
            stale_max: Duration::from_secs(60),
        }
    }

    #[test]
    fn ioreg_discovery_selects_a_parsing_backend() {
        let d = tempfile::tempdir().unwrap();
        let m = GpuMonitor::with_spec(
            None,
            ioreg_spec(d.path(), "echo '\"Device Utilization %\"=55'\n"),
        );
        probe_once(&m);
        assert_eq!(m.health(), GpuHealth::Live);
        assert_eq!(m.latest().util_pct, Some(55));
    }

    #[test]
    fn ioreg_spawn_and_malformed_mean_permanently_absent() {
        let d = tempfile::tempdir().unwrap();
        let mut missing = ioreg_spec(d.path(), "true\n");
        missing.ioreg = HelperCmd::named("/nonexistent/thegn-ioreg");
        for spec in [missing, ioreg_spec(d.path(), "echo garbage\n")] {
            let m = GpuMonitor::with_spec(None, spec);
            probe_once(&m);
            assert_eq!(m.health(), GpuHealth::Absent);
            std::thread::sleep(Duration::from_millis(10));
            m.sample(true);
            assert_eq!(m.probes_started(), 1, "no nvidia and no counter: give up");
        }
    }

    #[test]
    fn ioreg_timeout_is_a_failure_not_absence() {
        let d = tempfile::tempdir().unwrap();
        let m = GpuMonitor::with_spec(None, hang(ioreg_spec(d.path(), "sleep 300\n")));
        probe_once(&m);
        assert_eq!(m.health(), GpuHealth::Failing(ExecFailure::Timeout));
    }
}
