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

use crate::gpu::{GpuReading, IOREG_ARGS, NVIDIA_QUERY_ARGS, parse_ioaccel, parse_nvidia};
use crate::gpu_exec::{ExecFailure, run_bounded};
use crate::gpu_sysfs::{GpuSysfs, RealFs, SysRead};

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
    #[cfg(target_os = "linux")]
    Nvml,
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
    pub nvidia_proc: PathBuf,
    pub pci_devices: PathBuf,
    pub ioreg: HelperCmd,
    pub try_ioreg: bool,
    pub discovery_deadline: Duration,
    pub sample_deadline: Duration,
    pub output_cap: usize,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub absent_retry: Duration,
    pub stale_max: Duration,
    #[cfg(target_os = "linux")]
    nvml_loader: fn() -> Option<Box<dyn crate::gpu_nvml::NvmlApi>>,
}

impl Default for HelperSpec {
    fn default() -> Self {
        Self {
            nvidia: HelperCmd::named("nvidia-smi"),
            nvidia_proc: "/proc/driver/nvidia/gpus".into(),
            pci_devices: "/sys/bus/pci/devices".into(),
            ioreg: HelperCmd::named("ioreg"),
            try_ioreg: cfg!(target_os = "macos"),
            discovery_deadline: Duration::from_secs(3),
            sample_deadline: Duration::from_secs(5),
            output_cap: 64 * 1024,
            backoff_base: Duration::from_secs(30),
            backoff_max: Duration::from_secs(600),
            absent_retry: Duration::from_secs(600),
            stale_max: Duration::from_secs(120),
            #[cfg(target_os = "linux")]
            nvml_loader: crate::gpu_nvml::load,
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
    #[cfg(target_os = "linux")]
    nvml_attempted: bool,
    #[cfg(target_os = "linux")]
    nvml: Option<Box<dyn crate::gpu_nvml::NvmlApi>>,
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
    sysfs: Option<GpuSysfs>,
    shared: Arc<Shared>,
    spec: Arc<HelperSpec>,
}

impl GpuMonitor {
    pub(crate) fn new() -> Self {
        Self::with_spec(Some("/sys/class/drm".into()), HelperSpec::default())
    }

    pub(crate) fn with_spec(sysfs: Option<PathBuf>, spec: HelperSpec) -> Self {
        Self {
            sysfs: sysfs.map(|root| GpuSysfs::new(&root)),
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
                    #[cfg(target_os = "linux")]
                    nvml_attempted: false,
                    #[cfg(target_os = "linux")]
                    nvml: None,
                }),
                idle: Condvar::new(),
            }),
            spec: Arc::new(spec),
        }
    }

    /// One sample. Never blocks on a helper: on the slow tier it may start a
    /// background probe, and always returns the latest good reading.
    pub(crate) fn sample(&self, slow: bool) -> GpuReading {
        if let Some(sysfs) = &self.sysfs
            && sysfs.has_cards()
        {
            return sysfs.sample(slow);
        }
        #[cfg(target_os = "linux")]
        let suspended = slow && nvidia_suspended(&self.spec);
        #[cfg(target_os = "linux")]
        if suspended {
            let retired = {
                let mut state = self.shared.lock();
                state.nvml.take()
            };
            if let Some(mut api) = retired {
                api.shutdown();
            }
        }
        #[cfg(not(target_os = "linux"))]
        let suspended = slow && nvidia_suspended(&self.spec);
        if slow && !suspended {
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
                #[cfg(target_os = "linux")]
                let (backend, nvml, attempted) = {
                    let mut state = shared.lock();
                    (state.backend, state.nvml.take(), state.nvml_attempted)
                };
                #[cfg(not(target_os = "linux"))]
                let backend = shared.lock().backend;
                #[cfg(target_os = "linux")]
                let (probe, nvml, attempted) = run_linux_probe(&spec, backend, nvml, attempted);
                #[cfg(not(target_os = "linux"))]
                let probe = run_probe(&spec, backend);
                let mut state = shared.lock();
                #[cfg(target_os = "linux")]
                {
                    state.nvml = nvml;
                    state.nvml_attempted = attempted;
                }
                apply(&mut state, &spec, probe);
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

#[cfg(target_os = "linux")]
fn run_linux_probe(
    spec: &HelperSpec,
    backend: Backend,
    api: Option<Box<dyn crate::gpu_nvml::NvmlApi>>,
    attempted: bool,
) -> (Probe, Option<Box<dyn crate::gpu_nvml::NvmlApi>>, bool) {
    static NVML_INIT_ATTEMPTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    let first_attempt = backend == Backend::Unresolved
        && !attempted
        && !NVML_INIT_ATTEMPTED.swap(true, std::sync::atomic::Ordering::AcqRel);
    let candidate = if first_attempt {
        (spec.nvml_loader)()
    } else {
        None
    };
    run_linux_probe_with_candidate(
        spec,
        backend,
        api,
        attempted,
        candidate,
        Duration::from_secs(5),
    )
}

#[cfg(target_os = "linux")]
fn run_linux_probe_with_candidate(
    spec: &HelperSpec,
    backend: Backend,
    mut api: Option<Box<dyn crate::gpu_nvml::NvmlApi>>,
    attempted: bool,
    candidate: Option<Box<dyn crate::gpu_nvml::NvmlApi>>,
    init_timeout: Duration,
) -> (Probe, Option<Box<dyn crate::gpu_nvml::NvmlApi>>, bool) {
    use crate::gpu_nvml::{InitFailure, init_with_watchdog, read};

    if backend == Backend::Nvml {
        if let Some(live_api) = api.as_mut() {
            if let Some(reading) = read(live_api.as_mut()) {
                return (Probe::Reading(Backend::Nvml, reading), api, attempted);
            }
            if let Some(mut retired) = api.take() {
                retired.shutdown();
            }
            return (run_probe(spec, Backend::Unresolved), None, attempted);
        }
        // A runtime-suspended device releases its session above. Re-open it
        // lazily on the next active probe; the cold init already used watchdog.
        if let Some(mut api) = (spec.nvml_loader)() {
            if api.init().is_ok()
                && let Some(reading) = read(api.as_mut())
            {
                return (Probe::Reading(Backend::Nvml, reading), Some(api), attempted);
            }
            api.shutdown();
        }
        return (run_probe(spec, Backend::Unresolved), None, attempted);
    }
    if !attempted {
        if let Some(api) = candidate {
            match init_with_watchdog(api, init_timeout) {
                Ok(mut api) => {
                    if let Some(reading) = read(api.as_mut()) {
                        return (Probe::Reading(Backend::Nvml, reading), Some(api), true);
                    }
                    return (nvidia_sample(spec), None, true);
                }
                Err(InitFailure::Timeout | InitFailure::Unavailable) => {
                    return (run_probe(spec, Backend::Unresolved), None, true);
                }
            }
        }
        return (run_probe(spec, Backend::Unresolved), None, true);
    }
    (run_probe(spec, backend), api, attempted)
}

fn nvidia_suspended(spec: &HelperSpec) -> bool {
    let fs = RealFs;
    fs.list(&spec.nvidia_proc).into_iter().any(|slot| {
        let status = spec.pci_devices.join(slot).join("power/runtime_status");
        fs.read_to_string(&status)
            .is_some_and(|value| value.trim() == "suspended")
    })
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
        #[cfg(target_os = "linux")]
        Backend::Nvml => Probe::Failed(ExecFailure::Malformed),
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

    #[cfg(target_os = "linux")]
    struct FakeNvml {
        hang: bool,
        no_reading: bool,
        shutdown: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    #[cfg(target_os = "linux")]
    impl crate::gpu_nvml::NvmlApi for FakeNvml {
        fn init(&mut self) -> Result<(), ()> {
            if self.hang {
                std::thread::sleep(Duration::from_secs(2));
            }
            Ok(())
        }
        fn device_count(&mut self) -> Result<u32, ()> {
            Ok(1)
        }
        fn handle_by_index(&mut self, _: u32) -> Result<usize, ()> {
            Ok(1)
        }
        fn utilization(&mut self, _: usize) -> Option<u32> {
            (!self.no_reading).then_some(71)
        }
        fn memory_info(&mut self, _: usize) -> Option<(u64, u64)> {
            None
        }
        fn temperature(&mut self, _: usize) -> Option<f32> {
            None
        }
        fn power_usage(&mut self, _: usize) -> Option<f32> {
            None
        }
        fn shutdown(&mut self) {
            if let Some(shutdown) = &self.shutdown {
                shutdown.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }

    fn spec_for(dir: &Path, body: &str) -> HelperSpec {
        let s = script(dir, "smi.sh", body);
        HelperSpec {
            nvidia: HelperCmd {
                program: "sh".into(),
                prefix: vec![s.into_os_string()],
            },
            nvidia_proc: dir.join("nvidia-missing"),
            pci_devices: dir.join("pci-devices"),
            try_ioreg: false,
            #[cfg(target_os = "linux")]
            nvml_loader: || None,
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
            #[cfg(target_os = "linux")]
            nvml_loader: || None,
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
            #[cfg(target_os = "linux")]
            nvml_loader: || None,
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
            nvidia_proc: dir.join("nvidia-missing"),
            pci_devices: dir.join("pci-devices"),
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
            #[cfg(target_os = "linux")]
            nvml_loader: || None,
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

    #[test]
    fn suspended_nvidia_slot_suppresses_probe_until_it_wakes() {
        let d = tempfile::tempdir().unwrap();
        let proc = d.path().join("proc-gpus");
        let pci = d.path().join("pci");
        let slot = "0000:01:00.0";
        std::fs::create_dir_all(proc.join(slot)).unwrap();
        std::fs::create_dir_all(pci.join(slot).join("power")).unwrap();
        std::fs::write(pci.join(slot).join("power/runtime_status"), "suspended\n").unwrap();
        let mut spec = spec_for(d.path(), GOOD);
        spec.nvidia_proc = proc;
        spec.pci_devices = pci.clone();
        let monitor = GpuMonitor::with_spec(None, spec);
        for _ in 0..20 {
            monitor.sample(true);
        }
        assert_eq!(monitor.probes_started(), 0);
        std::fs::write(pci.join(slot).join("power/runtime_status"), "active\n").unwrap();
        monitor.sample(true);
        assert_eq!(monitor.probes_started(), 1);
        assert!(monitor.wait_idle(Duration::from_secs(10)));
    }

    #[test]
    fn missing_nvidia_proc_directory_keeps_probe_enabled() {
        let d = tempfile::tempdir().unwrap();
        let monitor = GpuMonitor::with_spec(None, spec_for(d.path(), GOOD));
        monitor.sample(true);
        assert_eq!(monitor.probes_started(), 1);
        assert!(monitor.wait_idle(Duration::from_secs(10)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nvml_read_uses_no_nvidia_smi_spawn() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("smi-ran");
        let body = format!(
            "echo called >> {}\necho '9, 1, 2, 3, 4'\n",
            marker.display()
        );
        let spec = spec_for(d.path(), &body);
        let (probe, _, attempted) = run_linux_probe_with_candidate(
            &spec,
            Backend::Nvml,
            Some(Box::new(FakeNvml {
                hang: false,
                no_reading: false,
                shutdown: None,
            })),
            true,
            None,
            Duration::from_millis(50),
        );
        assert!(matches!(probe, Probe::Reading(Backend::Nvml, _)));
        assert!(attempted);
        assert!(
            !marker.exists(),
            "a live NVML reading must not spawn nvidia-smi"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nvml_init_timeout_falls_back_to_nvidia_smi() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("smi-ran");
        let body = format!(
            "echo called >> {}\ncase \"$1\" in --version) echo v;; *) echo '9, 1, 2, 3, 4';; esac\n",
            marker.display()
        );
        let spec = spec_for(d.path(), &body);
        let (probe, _, attempted) = run_linux_probe_with_candidate(
            &spec,
            Backend::Unresolved,
            None,
            false,
            Some(Box::new(FakeNvml {
                hang: true,
                no_reading: false,
                shutdown: None,
            })),
            Duration::from_millis(20),
        );
        assert!(attempted, "a timed out init is permanently retired");
        assert!(matches!(probe, Probe::Reading(Backend::Nvidia, _)));
        let calls = std::fs::read_to_string(marker).unwrap();
        assert_eq!(calls.lines().count(), 2, "version and sample fallback ran");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nvml_without_a_reading_uses_discovery_and_classifies_missing_smi_absent() {
        let d = tempfile::tempdir().unwrap();
        let mut spec = spec_for(d.path(), "exit 0\n");
        spec.nvidia = HelperCmd::named("/nonexistent/thegn-nvidia-smi");
        let (probe, api, attempted) = run_linux_probe_with_candidate(
            &spec,
            Backend::Unresolved,
            None,
            false,
            Some(Box::new(FakeNvml {
                hang: false,
                no_reading: true,
                shutdown: None,
            })),
            Duration::from_secs(1),
        );
        assert!(matches!(probe, Probe::Absent { permanent: true }));
        assert!(api.is_none());
        assert!(attempted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_suspension_shuts_down_the_cached_nvml_session() {
        let d = tempfile::tempdir().unwrap();
        let nvidia_proc = d.path().join("nvidia-gpus");
        let pci_devices = d.path().join("pci-devices");
        std::fs::create_dir_all(nvidia_proc.join("GPU-0")).unwrap();
        let status = pci_devices.join("GPU-0/power/runtime_status");
        std::fs::create_dir_all(status.parent().unwrap()).unwrap();
        std::fs::write(status, "suspended\n").unwrap();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut spec = spec_for(d.path(), "exit 0\n");
        spec.nvidia_proc = nvidia_proc;
        spec.pci_devices = pci_devices;
        let monitor = GpuMonitor::with_spec(None, spec);
        {
            let mut state = monitor.shared.lock();
            state.backend = Backend::Nvml;
            state.nvml_attempted = true;
            state.nvml = Some(Box::new(FakeNvml {
                hang: false,
                no_reading: false,
                shutdown: Some(std::sync::Arc::clone(&shutdown)),
            }));
        }
        monitor.sample(true);
        assert!(shutdown.load(std::sync::atomic::Ordering::Acquire));
        assert!(monitor.shared.lock().nvml.is_none());
        assert_eq!(monitor.probes_started(), 0);
    }
}
