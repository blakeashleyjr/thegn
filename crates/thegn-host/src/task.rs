//! Lightweight task/test runner substrate for the native host.
//!
//! This is intentionally small and synchronous-at-the-boundary: callers run the
//! functions from `spawn_blocking`, then deliver the returned value to the event
//! loop over a channel. That keeps command execution off the render/input loop
//! while avoiding a daemon or polling thread.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::config::{Config, LimitsConfig, Task, TaskKind};
use thegn_core::remote::GitLoc;

use crate::panel::{self, TestLocation, TestNode, TestNodeKind, TestState, TestTask};

const MAX_CAPTURE_BYTES: usize = 256 * 1024;
pub(crate) const LIVE_TAIL_BYTES: usize = 16 * 1024;
const LIVE_EMIT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobLive {
    pub worktree: String,
    pub name: String,
    pub generation: u64,
    pub tail: String,
}

pub(crate) type JobLiveSink = Arc<dyn Fn(JobLive) + Send + Sync>;

struct CapRun {
    slot: String,
    generation: u64,
    timeout: Option<Duration>,
    live: Option<JobLiveSink>,
    keep_end: bool,
    label: (String, String),
}

#[derive(Default)]
struct ReaderState {
    stdout_capture: Vec<u8>,
    stderr_capture: Vec<u8>,
    produced: usize,
    live_tail: Vec<u8>,
}

impl ReaderState {
    fn push(&mut self, bytes: &[u8], with_live: bool, stdout: bool) {
        self.produced = self.produced.saturating_add(bytes.len());
        let capture = if stdout {
            &mut self.stdout_capture
        } else {
            &mut self.stderr_capture
        };
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(capture.len());
        capture.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        if with_live {
            self.live_tail.extend_from_slice(bytes);
            if self.live_tail.len() > LIVE_TAIL_BYTES {
                let mut start = self.live_tail.len() - LIVE_TAIL_BYTES;
                while start < self.live_tail.len() && self.live_tail[start] & 0xC0 == 0x80 {
                    start += 1;
                }
                if let Some(newline) = self.live_tail[start..].iter().position(|b| *b == b'\n')
                    && start + newline + 1 < self.live_tail.len()
                {
                    start += newline + 1;
                }
                self.live_tail.drain(..start);
            }
        }
    }

    fn tail(&self) -> String {
        let mut tail = String::from_utf8_lossy(&self.live_tail).into_owned();
        if tail.len() > LIVE_TAIL_BYTES {
            let mut start = tail.len() - LIVE_TAIL_BYTES;
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            if let Some(newline) = tail[start..].find('\n')
                && start + newline + 1 < tail.len()
            {
                start += newline + 1;
            }
            tail.drain(..start);
        }
        tail
    }
}

fn head_and_end(head: Vec<u8>, tail: &[u8], produced: usize) -> Vec<u8> {
    if produced <= head.len() {
        return head;
    }

    let missing = produced.saturating_sub(head.len());
    let mut start = tail.len().saturating_sub(missing);
    while start < tail.len() && tail[start] & 0xC0 == 0x80 {
        start += 1;
    }
    let suffix = &tail[start..];
    let omitted = produced
        .saturating_sub(head.len())
        .saturating_sub(suffix.len());
    let marker = format!("\n[output truncated: {omitted} bytes not shown; the end follows]\n");
    let mut text = head;
    text.extend_from_slice(marker.as_bytes());
    text.extend_from_slice(suffix);
    text
}

/// How a capped child gets its CPU/mem ceiling. Resolved once from `PATH`;
/// `wrap_capped` is pure over it so it can be unit-tested deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapBackend {
    /// `systemd-run --user --scope` with `CPUQuota`/`MemoryMax`/`Nice`.
    Systemd,
    /// `nice -n N` (and `ionice -c3` when present) — no hard cgroup cap.
    Nice,
    /// No wrapper available; run bare.
    None,
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

pub fn detect_cap_backend() -> CapBackend {
    if on_path("systemd-run") {
        CapBackend::Systemd
    } else if on_path("nice") {
        CapBackend::Nice
    } else {
        CapBackend::None
    }
}

fn limits_disabled(limits: &LimitsConfig) -> bool {
    limits.test_cpu_quota.trim().is_empty()
        && limits.test_mem_max.trim().is_empty()
        && limits.test_nice == 0
}

/// Wrap `argv` so an explicit test run is CPU/memory-capped and can't pin the
/// machine. Pure over `backend` for testability. Returns `argv` unchanged when
/// all limits are disabled or no backend is available.
pub fn wrap_capped(argv: &[String], limits: &LimitsConfig, backend: CapBackend) -> Vec<String> {
    if limits_disabled(limits) {
        return argv.to_vec();
    }
    match backend {
        CapBackend::Systemd => {
            let mut v = vec![
                "systemd-run".to_string(),
                "--user".into(),
                "--scope".into(),
                "--quiet".into(),
                "--collect".into(),
            ];
            if !limits.test_cpu_quota.trim().is_empty() {
                v.push("-p".into());
                v.push(format!("CPUQuota={}", limits.test_cpu_quota.trim()));
            }
            if !limits.test_mem_max.trim().is_empty() {
                v.push("-p".into());
                v.push(format!("MemoryMax={}", limits.test_mem_max.trim()));
            }
            if limits.test_nice != 0 {
                v.push("--nice".into());
                v.push(limits.test_nice.to_string());
            }
            v.push("--".into());
            v.extend_from_slice(argv);
            v
        }
        CapBackend::Nice => {
            let mut v = Vec::new();
            if on_path("ionice") {
                v.extend(["ionice".into(), "-c3".into()]);
            }
            if limits.test_nice != 0 {
                v.extend(["nice".into(), "-n".into(), limits.test_nice.to_string()]);
            }
            if v.is_empty() {
                return argv.to_vec();
            }
            v.extend_from_slice(argv);
            v
        }
        CapBackend::None => argv.to_vec(),
    }
}

/// Live child registry for single-flight + real cancellation. Keyed by a logical
/// slot (`"<worktree>:test"`, `"<worktree>:disc"`, or
/// `"<worktree>:job:<name>"`); the value is `(generation, group handle)` — a
/// pgid on unix, a kill-on-close Job Object on Windows.
/// Starting a newer job in the same slot kills the older group so a superseded
/// `cargo test` stops burning CPU immediately.
fn registry() -> &'static Mutex<HashMap<String, (u64, crate::platform::GroupHandle)>> {
    static R: OnceLock<Mutex<HashMap<String, (u64, crate::platform::GroupHandle)>>> =
        OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Kill whatever is currently registered in `slot` (used when a newer job
/// supersedes an in-flight one). Public for the supersede path in `run.rs`.
pub fn cancel_slot(slot: &str) {
    if let Ok(mut map) = registry().lock()
        && let Some((_, group)) = map.remove(slot)
    {
        group.terminate();
    }
}

/// True if a thegn-spawned test, discovery, or named job is currently registered
/// for `worktree` — the cleanup guard that keeps an auto/manual
/// `clean` from yanking `target/` out from under a build this process started.
/// (An *external* `cargo build` in a pane is covered separately: `clean_target`
/// prefers `cargo clean`, which takes the build lock and serializes.)
pub fn slot_active(worktree: &std::path::Path) -> bool {
    let prefix = format!("{}:", worktree.display());
    let test = format!("{prefix}test");
    let disc = format!("{}:disc", worktree.display());
    registry()
        .lock()
        .map(|m| {
            m.contains_key(&test)
                || m.contains_key(&disc)
                || m.keys()
                    .any(|slot| slot.starts_with(&format!("{prefix}job:")))
        })
        .unwrap_or(false)
}

pub fn test_slot(worktree: &Path) -> String {
    format!("{}:test", worktree.display())
}

pub fn job_slot(worktree: &Path, name: &str) -> String {
    format!("{}:job:{name}", worktree.display())
}

pub(crate) use crate::panel::confirm::TwoPress as RestartArm;

pub fn cancel_job(worktree: &Path, name: &str) {
    let slot = job_slot(worktree, name);
    if let Ok(map) = registry().lock()
        && let Some((generation, group)) = map.get(&slot)
    {
        if let Ok(mut stopped) = stopped_runs().lock() {
            stopped.insert((slot.clone(), *generation));
        }
        group.terminate();
    }
}

fn stopped_runs() -> &'static Mutex<HashSet<(String, u64)>> {
    static S: OnceLock<Mutex<HashSet<(String, u64)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Outcome of one capped child run.
struct CapOutput {
    exit_code: Option<i32>,
    truncated: bool,
    /// True iff the watchdog killed the process group at the deadline.
    timed_out: bool,
    /// True iff this exact slot generation was cancelled by the user.
    stopped: bool,
    text: String,
}

/// Private `CARGO_TARGET_DIR` for thegn-spawned cargo, so discovery/tests
/// never block on the build-directory lock held by the user's own
/// `cargo`/rust-analyzer. `None` when isolation is disabled.
fn isolated_cargo_target(worktree: &Path, limits: &LimitsConfig) -> Option<PathBuf> {
    limits
        .isolated_target_dir
        .then(|| worktree.join("target").join("thegn"))
}

/// Run `command` in `worktree` under a CPU/mem cap and a single-flight `slot`,
/// capturing bounded combined stdout+stderr. Kills any older job in the slot
/// first. A non-zero `timeout` arms a watchdog that kills the process group at
/// the deadline (so a run wedged on a build lock can't hang the panel forever).
fn run_capped(
    command: &str,
    loc: &GitLoc,
    worktree: &Path,
    limits: &LimitsConfig,
    run: CapRun,
) -> CapOutput {
    // `-c`, NOT `-lc`: a login shell re-sources the user's profile on every
    // spawn — pure overhead on a path meant to be cheap, with surprising
    // side effects. Run the command in a plain non-interactive shell.
    let inner = vec![
        thegn_core::util::shell(),
        "-c".to_string(),
        command.to_string(),
    ];
    run_capped_argv(&inner, loc, worktree, limits, run)
}

/// Build the (unspawned) command for a capped job: program + args, the worktree
/// cwd, piped stdio, an isolated `CARGO_TARGET_DIR`, its own process group, and a
/// **scrubbed git environment**.
///
/// The git scrub is the important bit: a user job (a build, a test suite, a
/// script) shelling out to `git` must operate on the worktree via the job's own
/// `-C`/cwd, never on whatever `GIT_DIR`/`GIT_INDEX_FILE` happened to be in the
/// environment. `main()` already scrubs process-wide, so this is defense in
/// depth — but it makes the guarantee local and explicit (mirrors
/// `util::git_cmd`), so running a job through thegn is a safer place to run
/// commands than a raw shell, and stays correct even if some future code sets a
/// `GIT_*` var in-process.
fn build_capped_command(argv: &[String], worktree: &Path, limits: &LimitsConfig) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in thegn_core::util::GIT_ENV_VARS {
        cmd.env_remove(var);
    }
    if let Some(target) = isolated_cargo_target(worktree, limits) {
        cmd.env("CARGO_TARGET_DIR", target);
    }
    cmd
}

/// Build the `Command` for a task's inner argv, routed by the worktree's
/// location. **Local**: `build_capped_command` over the cap-wrapped argv (host
/// cap wrapper + cwd + isolated `CARGO_TARGET_DIR` + git-env scrub + stdio +
/// process group). **Remote** (ssh/provider): run the argv *inside the env* via
/// the control transport (`GitLoc::sh_command`, which `cd`s into the worktree) —
/// the host cap wrapper and host cargo-target don't apply across the transport,
/// and the env enforces its own limits; stdio + process group are applied here.
fn task_command(loc: &GitLoc, inner: &[String], worktree: &Path, limits: &LimitsConfig) -> Command {
    if loc.is_remote() {
        let mut cmd = loc.sh_command(&thegn_core::util::sh_join(inner));
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        return cmd;
    }
    build_capped_command(
        &wrap_capped(inner, limits, detect_cap_backend()),
        worktree,
        limits,
    )
}

/// Like [`run_capped`] but runs a prebuilt argv directly (no shell), so callers
/// passing user/pattern data — e.g. the ripgrep source scan — never have to
/// shell-quote. `inner[0]` is the program; the cap wrapper is prepended.
// off-loop: only reached via run_task/discover_tests, which the loop always
// dispatches on spawn_blocking (spawn_test_run_task / spawn_test_discovery).
#[expect(clippy::disallowed_methods)]
fn run_capped_argv(
    inner: &[String],
    loc: &GitLoc,
    worktree: &Path,
    limits: &LimitsConfig,
    run: CapRun,
) -> CapOutput {
    // Supersede any older job in this slot.
    cancel_slot(&run.slot);
    let keep_end = run.keep_end;

    let mut cmd = task_command(loc, inner, worktree, limits);

    // Own group/job so cancellation reaps the whole tree, not just the runner.
    let (mut child, group) = match crate::platform::spawn_grouped(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            return CapOutput {
                exit_code: None,
                truncated: false,
                timed_out: false,
                stopped: false,
                text: format!("failed to spawn task: {e}"),
            };
        }
    };
    if let Ok(mut map) = registry().lock() {
        map.insert(run.slot.clone(), (run.generation, group.clone()));
    }

    // Watchdog: kill the whole process group if the deadline passes before the
    // child exits. `done` lets the main thread retire the watchdog promptly.
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog = run.timeout.filter(|d| !d.is_zero()).map(|deadline| {
        let done = done.clone();
        let timed_out = timed_out.clone();
        let group = group.clone();
        thegn_core::thread_name::spawn_named("thegn-task-wdog", move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let end = Instant::now() + deadline;
            while Instant::now() < end {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if !done.load(Ordering::Relaxed) {
                timed_out.store(true, Ordering::Relaxed);
                group.terminate();
            }
        })
    });

    // Read both streams concurrently to EOF: dropping either pipe at the
    // capture limit would SIGPIPE a chatty child. Retained capture and live
    // tail stay bounded even while every byte is drained.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let state = Arc::new(Mutex::new(ReaderState::default()));
    let (signal_tx, signal_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let out_h = spawn_output_reader(
        stdout,
        state.clone(),
        signal_tx.clone(),
        run.live.is_some() || keep_end,
        run.live.is_some(),
        true,
        "stdout",
    );
    let err_h = spawn_output_reader(
        stderr,
        state.clone(),
        signal_tx,
        run.live.is_some() || keep_end,
        run.live.is_some(),
        false,
        "stderr",
    );
    let forwarder = run.live.and_then(|sink| {
        let state = state.clone();
        let label = run.label.clone();
        let generation = run.generation;
        match std::thread::Builder::new()
            .name("thegn-job-live".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                let mut last_emit = Instant::now();
                while signal_rx.recv().is_ok() {
                    let until = last_emit + LIVE_EMIT_INTERVAL;
                    while let Some(delay) = until.checked_duration_since(Instant::now()) {
                        match signal_rx.recv_timeout(delay) {
                            Ok(()) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                        }
                    }
                    let tail = state.lock().map(|state| state.tail());
                    if let Ok(tail) = tail {
                        sink(JobLive {
                            worktree: label.0.clone(),
                            name: label.1.clone(),
                            generation,
                            tail,
                        });
                    }
                    last_emit = Instant::now();
                }
            }) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    tracing::debug!(target: "thegn::tasks", %error, "failed to start job live forwarder");
                    None
                }
            }
    });
    let status = child.wait();
    done.store(true, Ordering::Relaxed);
    if let Some(w) = watchdog {
        let _ = w.join(); // best-effort: thread join: a panicked helper loses its output, not the caller
    }
    let out_ok = out_h.join();
    let err_ok = err_h.join();
    if let Some(forwarder) = forwarder
        && let Err(error) = forwarder.join()
    {
        tracing::debug!(target: "thegn::tasks", ?error, "job live forwarder panicked");
    }
    if let Err(error) = out_ok {
        tracing::debug!(target: "thegn::tasks", ?error, "stdout reader panicked");
    }
    if let Err(error) = err_ok {
        tracing::debug!(target: "thegn::tasks", ?error, "stderr reader panicked");
    }
    let (buf, truncated) = match state.lock() {
        Ok(state) => {
            let mut capture = state.stdout_capture.clone();
            let remaining = MAX_CAPTURE_BYTES.saturating_sub(capture.len());
            capture.extend_from_slice(
                &state.stderr_capture[..state.stderr_capture.len().min(remaining)],
            );
            let truncated = state.produced > capture.len();
            let text = if keep_end && truncated {
                head_and_end(capture, &state.live_tail, state.produced)
            } else {
                capture
            };
            (text, truncated)
        }
        Err(error) => {
            tracing::debug!(target: "thegn::tasks", %error, "job output state lock poisoned");
            (Vec::new(), false)
        }
    };

    let stopped = finish_slot(&run.slot, run.generation);

    CapOutput {
        exit_code: match status {
            Ok(status) => status.code(),
            Err(error) => {
                tracing::debug!(target: "thegn::tasks", %error, "child wait failed");
                None
            }
        },
        truncated,
        timed_out: timed_out.load(Ordering::Relaxed),
        stopped,
        text: String::from_utf8_lossy(&buf).into_owned(),
    }
}

fn spawn_output_reader(
    stream: Option<impl Read + Send + 'static>,
    state: Arc<Mutex<ReaderState>>,
    signal: std::sync::mpsc::SyncSender<()>,
    with_tail: bool,
    live: bool,
    stdout: bool,
    label: &'static str,
) -> std::thread::JoinHandle<()> {
    thegn_core::thread_name::spawn_named("thegn-task-pipe", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let Some(mut stream) = stream else { return };
        let mut bytes = [0_u8; 8 * 1024];
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    if let Ok(mut state) = state.lock() {
                        state.push(&bytes[..count], with_tail, stdout);
                    }
                    if live {
                        match signal.try_send(()) {
                            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => {}
                            Err(std::sync::mpsc::TrySendError::Disconnected(())) => {}
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::debug!(target: "thegn::tasks", %error, stream = label, "job output read failed");
                    break;
                }
            }
        }
    })
}

/// Consume this run's stop mark while holding the registry lock used to decide
/// whether it may deregister its slot. `cancel_job` takes these locks in the
/// same order, so a cancellation cannot leave a mark behind after this run
/// exits.
fn finish_slot(slot: &str, generation: u64) -> bool {
    if let Ok(mut map) = registry().lock() {
        let owns_slot = map
            .get(slot)
            .map(|(current, _)| *current == generation)
            .unwrap_or(false);
        let marked_stopped = if let Ok(mut marks) = stopped_runs().lock() {
            marks.remove(&(slot.to_string(), generation))
        } else {
            false
        };
        if owns_slot {
            map.remove(slot);
        }
        owns_slot && marked_stopped
    } else {
        false
    }
}

/// Translate a `LimitsConfig` timeout knob (seconds, 0 = disabled) to a
/// `Duration` the watchdog understands.
fn secs_to_timeout(secs: u64) -> Option<Duration> {
    (secs > 0).then(|| Duration::from_secs(secs))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunEnd {
    #[default]
    Exited,
    TimedOut {
        secs: u64,
    },
    Stopped,
}

#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub worktree: String,
    pub generation: u64,
    pub task: TestTask,
    pub exit_code: Option<i32>,
    pub duration_ms: u128,
    pub truncated: bool,
    pub stdout_stderr: String,
    pub end: RunEnd,
}

pub fn failed_test_run_outcome(
    worktree: PathBuf,
    generation: u64,
    task: TestTask,
    error: impl std::fmt::Display,
) -> TaskOutcome {
    TaskOutcome {
        worktree: worktree.to_string_lossy().into_owned(),
        generation,
        task,
        exit_code: None,
        duration_ms: 0,
        truncated: false,
        stdout_stderr: format!("test run did not complete: {error}"),
        end: RunEnd::Exited,
    }
}

pub fn failed_test_discovery_outcome(
    worktree: PathBuf,
    generation: u64,
    task: TestTask,
    error: impl std::fmt::Display,
) -> DiscoveryOutcome {
    DiscoveryOutcome {
        worktree: worktree.to_string_lossy().into_owned(),
        generation,
        task,
        nodes: Vec::new(),
        error: Some(format!("test discovery did not complete: {error}")),
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveryOutcome {
    pub worktree: String,
    pub generation: u64,
    pub task: TestTask,
    pub nodes: Vec<TestNode>,
    pub error: Option<String>,
}

pub fn configured_test_tasks(cfg: &Config) -> Vec<TestTask> {
    cfg.tasks
        .iter()
        .filter(|t| t.kind == TaskKind::Test)
        .map(test_task_from_config)
        .collect()
}

pub fn test_task_from_config(t: &Task) -> TestTask {
    let mut command = t.command.clone();
    if !t.args.is_empty() {
        command.push(' ');
        command.push_str(&t.args.join(" "));
    }
    let matcher = t
        .matcher
        .clone()
        .unwrap_or_else(|| infer_matcher(&t.command));
    let ingestion = ingestion_for_matcher(&matcher);
    TestTask {
        name: t.name.clone(),
        command,
        matcher,
        ingestion,
        report_glob: None,
    }
}

/// Default ingestion mode for a matcher id. Text is the safe baseline; JSON/
/// Report matchers (added in later phases) override this.
fn ingestion_for_matcher(matcher: &str) -> crate::panel::Ingestion {
    use crate::panel::Ingestion;
    match matcher {
        "nextest" | "libtest-json" | "dart" | "flutter" | "deno" | "bun" | "rspec" => {
            Ingestion::Json
        }
        "junit" | "trx" | "nunit" | "gradle" | "maven" | "dotnet" | "sbt" => Ingestion::Report,
        "tap" | "bats" | "prove" | "busted" | "pgtap" => Ingestion::Tap,
        _ => Ingestion::Text,
    }
}

/// Build manifests whose change should invalidate discovery (deps/targets may
/// have shifted). Source-file edits don't appear here — those mark results
/// stale via the fs-watch and are picked up by an explicit refresh/run.
const FINGERPRINT_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "go.mod",
    "go.sum",
    "pyproject.toml",
    "pytest.ini",
    "tox.ini",
    "package.json",
    "pubspec.yaml",
    "mix.exs",
    "build.zig",
    "CMakeLists.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "build.sbt",
    "deno.json",
    "deno.jsonc",
    "rebar.config",
    "dune-project",
    "gleam.toml",
    "flake.nix",
    "justfile",
];

/// A cheap, stable fingerprint of the worktree's build manifests (name + size +
/// mtime). Two equal fingerprints mean discovery can be safely reused without
/// re-spawning any subprocess. Reads metadata only — no file contents.
pub fn manifest_fingerprint(worktree: &Path) -> String {
    let mut parts = Vec::new();
    for name in FINGERPRINT_MANIFESTS {
        let Ok(meta) = std::fs::metadata(worktree.join(name)) else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        parts.push(format!("{name}:{}:{mtime}", meta.len()));
    }
    parts.join("|")
}

pub fn detect_test_task(worktree: &Path, cfg: &Config) -> Option<TestTask> {
    configured_test_tasks(cfg)
        .into_iter()
        .next()
        .or_else(|| detect_fallback(worktree))
}

fn detect_fallback(worktree: &Path) -> Option<TestTask> {
    use crate::panel::Ingestion;
    let has = |f: &str| worktree.join(f).exists();

    if has_just_test(worktree) {
        return Some(TestTask::new("just test", "just test", "generic"));
    }
    if has("Cargo.toml") {
        // Prefer nextest (structured JSON + timing) when it's installed.
        if on_path("cargo-nextest") {
            // libtest-json is gated behind an experimental env flag in nextest;
            // the inline assignment is fine because we run via `sh -lc`.
            return Some(
                TestTask::new(
                    "cargo nextest",
                    "NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 \
                     cargo nextest run --message-format libtest-json",
                    "nextest",
                )
                .with_ingestion(Ingestion::Json),
            );
        }
        return Some(TestTask::new(
            "cargo test",
            "cargo test --workspace",
            "cargo-test",
        ));
    }
    if has("go.mod") {
        return Some(TestTask::new("go test", "go test ./...", "go-test"));
    }
    if has("pyproject.toml") || has("pytest.ini") || has("tox.ini") {
        return Some(TestTask::new("pytest", "pytest", "pytest"));
    }
    if has("pubspec.yaml") {
        let pubspec = std::fs::read_to_string(worktree.join("pubspec.yaml")).unwrap_or_default();
        let is_flutter = pubspec.contains("flutter:") || pubspec.contains("sdk: flutter");
        return Some(if is_flutter {
            TestTask::new("flutter test", "flutter test --reporter json", "flutter")
                .with_ingestion(Ingestion::Json)
        } else {
            TestTask::new("dart test", "dart test --reporter json", "dart")
                .with_ingestion(Ingestion::Json)
        });
    }
    if has("Package.swift") {
        return Some(TestTask::new("swift test", "swift test", "swift"));
    }
    if has("mix.exs") {
        return Some(TestTask::new("mix test", "mix test", "elixir"));
    }
    if has("CMakeLists.txt") {
        // CTest runs the suite; ctest's stdout is a stable per-test text format.
        return Some(TestTask::new("ctest", "ctest --output-on-failure", "ctest"));
    }
    // JVM / .NET: stdout is unusable, but each writes machine report files we
    // parse after the run (JUnit XML / TRX).
    if has("pom.xml") {
        return Some(
            TestTask::new("maven", "mvn -q test", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("target/surefire-reports/*.xml"),
        );
    }
    if has("build.gradle") || has("build.gradle.kts") {
        return Some(
            TestTask::new("gradle", "gradle test", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("build/test-results/**/*.xml"),
        );
    }
    if has("build.sbt") {
        return Some(
            TestTask::new("sbt", "sbt test", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("target/test-reports/*.xml"),
        );
    }
    if has_dotnet_project(worktree) {
        return Some(
            TestTask::new(
                "dotnet test",
                "dotnet test --logger \"trx;LogFileName=tg.trx\"",
                "trx",
            )
            .with_ingestion(Ingestion::Report)
            .with_report_glob("TestResults/*.trx"),
        );
    }
    // Tier C — text scrapers / lighter integrations.
    if has("rebar.config") {
        return Some(TestTask::new("rebar3 eunit", "rebar3 eunit", "erlang"));
    }
    if has("build.zig") {
        return Some(TestTask::new("zig build test", "zig build test", "zig"));
    }
    if has(".rspec") {
        return Some(
            TestTask::new("rspec", "rspec --format json", "rspec").with_ingestion(Ingestion::Json),
        );
    }
    if has("phpunit.xml") || has("phpunit.xml.dist") {
        return Some(
            TestTask::new("phpunit", "phpunit --log-junit target/phpunit.xml", "junit")
                .with_ingestion(Ingestion::Report)
                .with_report_glob("target/phpunit.xml"),
        );
    }
    if has("Gemfile") {
        // No `.rspec` → assume minitest via rake (sparse text; synthetic result).
        return Some(TestTask::new("rake test", "rake test", "ruby"));
    }
    if has("dub.json") || has("dub.sdl") {
        return Some(TestTask::new("dub test", "dub test", "d"));
    }
    // TAP-emitting ecosystems (one parser covers them all).
    if has_test_file_ext(worktree, ".bats") {
        return Some(
            TestTask::new("bats", "bats --formatter tap .", "bats").with_ingestion(Ingestion::Tap),
        );
    }
    if has_test_file_ext(worktree, "_spec.lua") || has(".busted") {
        return Some(
            TestTask::new("busted", "busted -o TAP", "busted").with_ingestion(Ingestion::Tap),
        );
    }
    if has("cpanfile") || has("Makefile.PL") || has("dist.ini") || has_test_file_ext(worktree, ".t")
    {
        // prove -v echoes each test's raw TAP inline (plus a summary we ignore).
        return Some(TestTask::new("prove", "prove -v t/", "prove").with_ingestion(Ingestion::Tap));
    }
    // PowerShell / Pester → NUnit XML report on stdout.
    if has_test_file_ext(worktree, ".Tests.ps1") {
        return Some(
            TestTask::new(
                "pester",
                "pwsh -NoProfile -Command \"Invoke-Pester -CI -Output Minimal\"",
                "nunit",
            )
            .with_ingestion(Ingestion::Report)
            .with_report_glob("testResults.xml"),
        );
    }
    // OCaml / dune and Gleam: sparse text → synthetic result from exit code.
    if has("dune-project") {
        return Some(TestTask::new("dune test", "dune runtest", "ocaml"));
    }
    if has("gleam.toml") {
        return Some(TestTask::new("gleam test", "gleam test", "gleam"));
    }
    if has("deno.json") || has("deno.jsonc") {
        // deno writes a JUnit report to stdout (no file), parsed via the Report
        // path's stdout fallback.
        return Some(
            TestTask::new("deno test", "deno test --reporter junit", "junit")
                .with_ingestion(Ingestion::Report),
        );
    }
    if has("package.json") {
        let pkg = std::fs::read_to_string(worktree.join("package.json")).unwrap_or_default();
        if pkg.contains("vitest") {
            return Some(TestTask::new("vitest", "npm test -- --run", "vitest"));
        }
        if pkg.contains("jest") {
            return Some(TestTask::new("jest", "npm test -- --runInBand", "jest"));
        }
        return Some(TestTask::new("npm test", "npm test", "javascript"));
    }
    // Lowest priority: a pure-Nix repo with no language manifest. A polyglot
    // repo (e.g. Cargo.toml + flake.nix) still uses its language runner above.
    if has("flake.nix") {
        return Some(TestTask::new(
            "flake checks",
            "nix flake check -L",
            "nix-flake",
        ));
    }
    None
}

fn has_dotnet_project(worktree: &Path) -> bool {
    std::fs::read_dir(worktree)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.path()
                    .extension()
                    .and_then(|s| s.to_str())
                    // C#, F#, and VB.NET project/solution files.
                    .map(|ext| matches!(ext, "sln" | "csproj" | "fsproj" | "vbproj"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Whether any file under `worktree` (root + common test dirs) has `ext`.
fn has_test_file_ext(worktree: &Path, ext: &str) -> bool {
    for sub in ["", "test", "tests", "spec", "t"] {
        let dir = if sub.is_empty() {
            worktree.to_path_buf()
        } else {
            worktree.join(sub)
        };
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file() && p.to_string_lossy().ends_with(ext) {
                    return true;
                }
            }
        }
    }
    false
}

fn has_just_test(worktree: &Path) -> bool {
    for name in ["justfile", "Justfile", ".justfile"] {
        let path = worktree.join(name);
        if let Ok(s) = std::fs::read_to_string(path)
            && s.lines().any(|l| l.trim_start().starts_with("test:"))
        {
            return true;
        }
    }
    false
}

fn infer_matcher(command: &str) -> String {
    let cmd = command.to_ascii_lowercase();
    if cmd.contains("cargo") {
        "cargo-test"
    } else if cmd.contains("pytest") {
        "pytest"
    } else if cmd.contains("go test") {
        "go-test"
    } else if cmd.contains("vitest") {
        "vitest"
    } else if cmd.contains("jest") {
        "jest"
    } else {
        "generic"
    }
    .into()
}

pub fn run_task(
    worktree: PathBuf,
    loc: &GitLoc,
    generation: u64,
    task: TestTask,
    limits: &LimitsConfig,
) -> TaskOutcome {
    let label = (String::new(), task.name.clone());
    let mut outcome = run_task_in(
        worktree.clone(),
        loc,
        generation,
        task,
        limits,
        CapRun {
            slot: test_slot(&worktree),
            generation,
            timeout: secs_to_timeout(limits.test_timeout_secs),
            live: None,
            keep_end: false,
            label,
        },
    );
    if let RunEnd::TimedOut { secs } = outcome.end {
        outcome.stdout_stderr = format!(
            "test run exceeded {secs}s and was stopped\n{}",
            outcome.stdout_stderr
        );
    }
    outcome
}

pub(crate) fn run_job(
    worktree: PathBuf,
    loc: &GitLoc,
    generation: u64,
    task: TestTask,
    limits: &LimitsConfig,
    live: Option<JobLiveSink>,
) -> TaskOutcome {
    let slot = job_slot(&worktree, &task.name);
    let label = (worktree.to_string_lossy().into_owned(), task.name.clone());
    let mut outcome = run_task_in(
        worktree,
        loc,
        generation,
        task,
        limits,
        CapRun {
            slot,
            generation,
            timeout: secs_to_timeout(limits.test_timeout_secs),
            live,
            keep_end: true,
            label,
        },
    );
    match outcome.end {
        RunEnd::TimedOut { secs } => {
            outcome.stdout_stderr = format!(
                "job exceeded {secs}s ([limits] test_timeout_secs) and was stopped\n{}",
                outcome.stdout_stderr
            );
        }
        RunEnd::Stopped => {
            outcome.stdout_stderr = format!("stopped by user\n{}", outcome.stdout_stderr);
        }
        RunEnd::Exited => {}
    }
    outcome
}

pub(crate) fn job_outcome_is_stale(
    latest_generation: Option<u64>,
    outcome_generation: u64,
) -> bool {
    latest_generation.is_some_and(|latest| outcome_generation < latest)
}

pub(crate) fn apply_job_live(
    runs: &mut HashMap<String, crate::panel::TaskRunRecord>,
    latest: &HashMap<String, u64>,
    active_wt: &str,
    msg: JobLive,
) -> bool {
    if latest.get(&msg.name) != Some(&msg.generation)
        || (!active_wt.is_empty() && active_wt != msg.worktree.as_str())
        || !runs.get(&msg.name).is_some_and(|run| run.running)
    {
        return false;
    }
    let Some(run) = runs.get_mut(&msg.name) else {
        return false;
    };
    if run.output_tail == msg.tail {
        return false;
    }
    run.output_tail = msg.tail;
    true
}

fn run_task_in(
    worktree: PathBuf,
    loc: &GitLoc,
    generation: u64,
    task: TestTask,
    limits: &LimitsConfig,
    run: CapRun,
) -> TaskOutcome {
    let started = Instant::now();
    let out = run_capped(&task.command, loc, &worktree, limits, run);
    let end = if out.timed_out {
        RunEnd::TimedOut {
            secs: limits.test_timeout_secs,
        }
    } else if out.stopped {
        RunEnd::Stopped
    } else {
        RunEnd::Exited
    };
    TaskOutcome {
        worktree: worktree.to_string_lossy().into_owned(),
        generation,
        task,
        // A timeout kill is a failure, not a clean exit.
        exit_code: if out.timed_out {
            Some(124)
        } else {
            out.exit_code
        },
        duration_ms: started.elapsed().as_millis(),
        truncated: out.truncated,
        stdout_stderr: out.text,
        end,
    }
}

pub(crate) fn job_status_line(name: &str, end: RunEnd, exit: i32, elapsed_s: f64) -> String {
    match end {
        RunEnd::Exited if exit == 0 => format!("Task '{name}' done in {elapsed_s:.1}s"),
        RunEnd::Exited => format!("Task '{name}' failed (exit {exit}) in {elapsed_s:.1}s"),
        RunEnd::TimedOut { secs } => format!("Task '{name}' timed out after {secs}s"),
        RunEnd::Stopped => format!("Task '{name}' stopped after {elapsed_s:.1}s"),
    }
}

/// A no-compile source scan for one ecosystem: file globs to search and the
/// regexes that mark a test declaration. The matched name is pulled out by
/// [`extract_test_name`], so a single extractor serves every language.
struct ScanRule {
    globs: &'static [&'static str],
    /// Patterns use POSIX classes (`[[:space:]]`, `[[:alnum:]_]`) so the same
    /// string works for both ripgrep (Rust regex) and the GNU `grep -E`
    /// fallback.
    patterns: &'static [&'static str],
}

/// The ripgrep source-scan ruleset, keyed by matcher. This is the general form
/// of the cargo `metadata` fix: enumerate tests by reading source — no compile,
/// no build lock — for toolchains that would otherwise compile to list (Go,
/// Swift) or that had no discovery at all (JS, Elixir, Zig, Ruby).
fn scan_rule(matcher: &str) -> Option<ScanRule> {
    let rule = match matcher {
        "go-test" => ScanRule {
            globs: &["*_test.go"],
            patterns: &["^[[:space:]]*func[[:space:]]+(Test|Benchmark|Fuzz|Example)[[:alnum:]_]*"],
        },
        "swift" => ScanRule {
            globs: &["*Tests.swift", "*Test.swift"],
            patterns: &["func[[:space:]]+test[[:alnum:]_]*[[:space:]]*\\("],
        },
        "jest" | "vitest" | "javascript" => ScanRule {
            globs: &[
                "*.test.js",
                "*.test.ts",
                "*.test.jsx",
                "*.test.tsx",
                "*.test.mjs",
                "*.spec.js",
                "*.spec.ts",
                "*.spec.jsx",
                "*.spec.tsx",
            ],
            patterns: &["\\b(it|test)[[:space:]]*\\([[:space:]]*[\"'`]"],
        },
        "elixir" => ScanRule {
            globs: &["*_test.exs", "*_test.ex"],
            patterns: &["^[[:space:]]*test[[:space:]]+\""],
        },
        "zig" => ScanRule {
            globs: &["*.zig"],
            patterns: &["^[[:space:]]*test[[:space:]]+\""],
        },
        "ruby" | "rspec" => ScanRule {
            globs: &["*_spec.rb", "*_test.rb"],
            patterns: &[
                "^[[:space:]]*it[[:space:]]+[\"']",
                "^[[:space:]]*def[[:space:]]+test_[[:alnum:]_]*",
            ],
        },
        _ => return None,
    };
    Some(rule)
}

/// Build the argv for a source scan: ripgrep when available (respects
/// `.gitignore`, fast), else a GNU `grep -rnE` fallback. Both emit
/// `path:line:text` so [`parse_scan_output`] is backend-agnostic.
fn build_scan_argv(rule: &ScanRule) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    if on_path("rg") {
        argv.extend(
            [
                "rg",
                "--no-heading",
                "--line-number",
                "--no-messages",
                "--color=never",
            ]
            .map(String::from),
        );
        for g in rule.globs {
            argv.push("-g".into());
            argv.push((*g).into());
        }
        for p in rule.patterns {
            argv.push("-e".into());
            argv.push((*p).into());
        }
        argv.push(".".into());
    } else {
        argv.extend(["grep", "-rnE"].map(String::from));
        for g in rule.globs {
            argv.push(format!("--include={g}"));
        }
        for p in rule.patterns {
            argv.push("-e".into());
            argv.push((*p).into());
        }
        argv.push(".".into());
    }
    argv
}

/// Pull a test name out of one matched source line. Handles both declaration
/// shapes with no per-language config: `<keyword> <ident>` (Go `func`, Python
/// `def`/`class`, Swift `func`, …) and `<keyword> "name"` / `it('name')`
/// (Elixir/Zig `test "…"`, JS `it`/`test`). Tries the identifier form first,
/// then the quoted form.
fn extract_test_name(line: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &["func", "def", "class", "function", "fn", "sub"];
    // `<keyword> <ident>` — the token right after a declaration keyword.
    let toks: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|t| !t.is_empty())
        .collect();
    for (i, t) in toks.iter().enumerate() {
        if KEYWORDS.contains(t)
            && let Some(raw) = toks.get(i + 1)
        {
            let name: String = raw
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // `<keyword> "name"` / `it('name')` — the first quoted string on the line.
    let first_quote = ['"', '\'', '`']
        .iter()
        .filter_map(|q| line.find(*q).map(|i| (i, *q)))
        .min_by_key(|(i, _)| *i)?;
    let (start, q) = first_quote;
    let rest = &line[start + 1..];
    let end = rest.find(q)?;
    let name = &rest[..end];
    (!name.is_empty()).then(|| name.to_string())
}

/// Parse `path:line:text` scan output into jumpable placeholder nodes (grouped
/// by file, superseded by a real run).
fn parse_scan_output(text: &str) -> Vec<TestNode> {
    let mut seen = std::collections::BTreeSet::new();
    let mut nodes = Vec::new();
    for line in text.lines() {
        let mut it = line.splitn(3, ':');
        let (Some(path), Some(lineno), Some(rest)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let Ok(lineno) = lineno.parse::<usize>() else {
            continue;
        };
        let Some(name) = extract_test_name(rest) else {
            continue;
        };
        let path = path.trim_start_matches("./").to_string();
        let id = format!("{path}::{name}");
        if !seen.insert(id.clone()) {
            continue;
        }
        nodes.push(TestNode {
            id,
            label: name,
            depth: 0,
            kind: TestNodeKind::Test,
            state: TestState::Unknown,
            location: Some(TestLocation {
                path,
                line: lineno,
                column: None,
            }),
            message: None,
            placeholder: true,
        });
    }
    panel::tree_from_flat_tests(nodes)
}

pub fn discover_tests(
    worktree: PathBuf,
    loc: &GitLoc,
    generation: u64,
    task: TestTask,
    limits: &LimitsConfig,
) -> DiscoveryOutcome {
    let wt = worktree.to_string_lossy().into_owned();
    let slot = format!("{}:disc", worktree.display());
    let timeout = secs_to_timeout(limits.discover_timeout_secs);

    // Source-scan ecosystems (no compile, no build lock): grep source for test
    // declarations. Local worktrees scan in-process via fff (no subprocess);
    // remote/provider worktrees keep the rg/grep-over-transport path since their
    // files are not on this host.
    if let Some(rule) = scan_rule(task.matcher.as_str()) {
        if matches!(loc, GitLoc::Local(_)) {
            // ~parity with the old `--max-count`-less scan: cap generously.
            const SCAN_LIMIT: usize = 10_000;
            let text = crate::fff_backend::scan(&worktree, rule.globs, rule.patterns, SCAN_LIMIT);
            return DiscoveryOutcome {
                worktree: wt,
                generation,
                task,
                nodes: parse_scan_output(&text),
                error: None,
            };
        }
        let argv = build_scan_argv(&rule);
        let out = run_capped_argv(
            &argv,
            loc,
            &worktree,
            limits,
            CapRun {
                slot: slot.clone(),
                generation,
                timeout,
                live: None,
                keep_end: false,
                label: (wt.clone(), task.name.clone()),
            },
        );
        if out.timed_out {
            return DiscoveryOutcome {
                worktree: wt,
                generation,
                task,
                nodes: Vec::new(),
                error: Some(format!(
                    "test discovery timed out after {}s",
                    limits.discover_timeout_secs
                )),
            };
        }
        return DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes: parse_scan_output(&out.text),
            error: None,
        };
    }

    // Cargo: enumerate test *targets* from `cargo metadata`. This reads only the
    // manifests — no compile, no build-directory lock — so discovery is instant
    // regardless of build state. Per-test results fill in on the first run.
    let command = if matches!(task.matcher.as_str(), "cargo-test" | "nextest") {
        "cargo metadata --no-deps --format-version 1"
    } else if let Some(c) = discovery_command(&task) {
        c
    } else {
        return DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes: Vec::new(),
            error: Some("target discovery is not available for this test command".into()),
        };
    };

    let out = run_capped(
        command,
        loc,
        &worktree,
        limits,
        CapRun {
            slot,
            generation,
            timeout,
            live: None,
            keep_end: false,
            label: (wt.clone(), task.name.clone()),
        },
    );
    if out.timed_out {
        return DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes: Vec::new(),
            error: Some(format!(
                "test discovery timed out after {}s — another build may be holding the cargo lock",
                limits.discover_timeout_secs
            )),
        };
    }
    if out.exit_code == Some(0) {
        let nodes = match task.matcher.as_str() {
            "cargo-test" | "nextest" => parse_cargo_metadata_targets(&out.text),
            "nix-flake" => crate::testkit::json::parse_nix_flake_show(&out.text),
            _ => discovery_output_to_nodes(&task.matcher, &out.text),
        };
        DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes,
            error: None,
        }
    } else {
        DiscoveryOutcome {
            worktree: wt,
            generation,
            task,
            nodes: Vec::new(),
            error: Some(out.text.trim().chars().take(200).collect()),
        }
    }
}

/// Parse `cargo metadata --no-deps` JSON into one placeholder node per testable
/// target (libs, bins, and integration tests). Granularity is target-level: the
/// individual `#[test]` functions are populated by an actual run. Nodes are
/// grouped by package and carry the target's source file so "open" jumps there.
fn parse_cargo_metadata_targets(json: &str) -> Vec<TestNode> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(packages) = root.get("packages").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for pkg in packages {
        let pkg_name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("crate");
        let Some(targets) = pkg.get("targets").and_then(|t| t.as_array()) else {
            continue;
        };
        for target in targets {
            // `test: true` marks targets that produce a test binary (lib, bin,
            // and `[[test]]` integration targets); examples/benches are false.
            if !target
                .get("test")
                .and_then(|t| t.as_bool())
                .unwrap_or(false)
            {
                continue;
            }
            let tname = target.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let kind = target
                .get("kind")
                .and_then(|k| k.as_array())
                .and_then(|a| a.first())
                .and_then(|k| k.as_str())
                .unwrap_or("test");
            // Group by package; label distinguishes the target within it.
            let label = if kind == "lib" || tname == pkg_name {
                format!("{kind} tests")
            } else {
                tname.to_string()
            };
            let id = format!("{pkg_name}::{label}");
            if !seen.insert(id.clone()) {
                continue;
            }
            let location = target
                .get("src_path")
                .and_then(|p| p.as_str())
                .map(|p| TestLocation {
                    path: p.to_string(),
                    line: 1,
                    column: None,
                });
            nodes.push(TestNode {
                id,
                label,
                depth: 0,
                kind: TestNodeKind::Test,
                state: TestState::Unknown,
                location,
                message: None,
                placeholder: true,
            });
        }
    }
    panel::tree_from_flat_tests(nodes)
}

fn discovery_command(task: &TestTask) -> Option<&'static str> {
    match task.matcher.as_str() {
        // NB: cargo (`cargo-test`/`nextest`) is handled in `discover_tests` via
        // `cargo metadata`, and the source-scan ecosystems (go, swift, js,
        // elixir, zig, ruby) via `scan_rule` — both no-compile, before here.
        // pytest collection imports modules but neither compiles nor locks.
        "pytest" => Some("pytest --collect-only -q"),
        // `ctest -N` ("show only") prints `Test #N: name` without running.
        "ctest" => Some("ctest -N"),
        // Enumerate flake checks as targets (JSON, parsed specially).
        "nix-flake" => Some("nix flake show --json"),
        _ => None,
    }
}

fn discovery_output_to_nodes(matcher: &str, text: &str) -> Vec<TestNode> {
    let mut seen = std::collections::BTreeSet::new();
    let mut nodes = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some(name) = discovery_test_name(matcher, line) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        nodes.push(TestNode {
            id: name.clone(),
            label: name,
            depth: 0,
            kind: TestNodeKind::Test,
            state: TestState::Unknown,
            location: None,
            message: None,
            placeholder: false,
        });
    }
    panel::tree_from_flat_tests(nodes)
}

/// Pull one test target name out of a discovery line, or `None` for headers and
/// summaries. Each framework's `--list`/`--collect-only` format differs.
fn discovery_test_name(matcher: &str, line: &str) -> Option<String> {
    match matcher {
        // `cargo test -- --list` prints `path::to::test: test` (and `…: benchmark`),
        // plus `N tests, M benchmarks` summaries and blank lines.
        "cargo-test" | "nextest" => line
            .strip_suffix(": test")
            .or_else(|| line.strip_suffix(": benchmark"))
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
        // `swift test --list-tests` prints `Suite/testMethod` per line.
        "swift" => (line.contains('/') && !line.contains(' ')).then(|| line.to_string()),
        // `ctest -N` prints `  Test #3: suite.name`; ignore the `Total Tests` line.
        "ctest" => line
            .split_once(':')
            .filter(|(head, _)| head.trim_start().starts_with("Test #"))
            .map(|(_, name)| name.trim().to_string())
            .filter(|n| !n.is_empty()),
        // `go test -list` prints one bare test name per line plus `ok …`/`?  …`
        // build/result lines.
        "go-test" => {
            let first = line.split_whitespace().next().unwrap_or("");
            (first.starts_with("Test")
                || first.starts_with("Benchmark")
                || first.starts_with("Example"))
            .then(|| first.to_string())
        }
        // `pytest --collect-only -q` prints `path::Class::test` node ids and a
        // trailing `N tests collected` summary.
        "pytest" => (line.contains("::") && !line.contains(" collected")).then(|| line.to_string()),
        _ => None,
    }
}

pub fn parse_task_outcome(outcome: &TaskOutcome) -> Vec<TestNode> {
    use crate::panel::Ingestion;
    // Structured ingestion is preferred where the runner provides it; text
    // scraping is the fragile fallback. Report-file ingestion lands in a later
    // phase (parsed from disk by the caller, not from captured stdout).
    if outcome.task.ingestion == Ingestion::Json {
        let nodes = crate::testkit::json::parse(&outcome.task.matcher, &outcome.stdout_stderr);
        if !nodes.is_empty() {
            return nodes;
        }
        // Fall through to the synthetic single-node result below on empty JSON
        // (e.g. the runner failed before emitting any events).
    }
    if outcome.task.ingestion == Ingestion::Tap {
        let nodes = crate::testkit::tap::parse(&outcome.stdout_stderr);
        if !nodes.is_empty() {
            return nodes;
        }
        // No TAP lines parsed: fall through to the synthetic node.
    }
    if outcome.task.ingestion == Ingestion::Report {
        // File-based report (Maven/Gradle/sbt/.NET/PHP) when a glob is set;
        // otherwise the runner wrote the report to stdout (e.g. deno --reporter
        // junit), so parse the captured output directly.
        let nodes = match &outcome.task.report_glob {
            Some(glob) => crate::testkit::report::parse_glob(Path::new(&outcome.worktree), glob),
            None => crate::testkit::report::parse_report(&outcome.stdout_stderr),
        };
        if !nodes.is_empty() {
            return nodes;
        }
        // No report (build failed before producing one): fall through to the
        // synthetic node so the user still sees the failure.
    }
    let mut nodes = panel::parse_test_output(&outcome.stdout_stderr);
    if nodes.is_empty() {
        let state = if outcome.exit_code == Some(0) {
            TestState::Pass
        } else {
            TestState::Fail
        };
        nodes.push(TestNode {
            id: outcome.task.name.clone(),
            label: outcome.task.name.clone(),
            depth: 0,
            kind: TestNodeKind::Test,
            state,
            location: first_location(&outcome.stdout_stderr),
            message: panel::first_failure_message(&outcome.stdout_stderr),
            placeholder: false,
        });
    }
    panel::tree_from_flat_tests(nodes)
}

fn first_location(output: &str) -> Option<TestLocation> {
    panel::extract_locations(output).into_iter().next()
}

/// A one-line, copyable debug-launch descriptor for the selected test. This is
/// the handoff shape a future DAP client (AQ 525–528) consumes; today it just
/// surfaces the exact command a user could run under a debugger. Pure/testable.
pub fn dap_launch_descriptor(task: &TestTask, target_id: &str) -> String {
    let runner = task.command.split_whitespace().next().unwrap_or("test");
    match task.matcher.as_str() {
        "cargo-test" | "nextest" => {
            format!("debug: cargo test {target_id} (adapter: codelldb)")
        }
        "pytest" => format!("debug: pytest {target_id} (adapter: debugpy)"),
        "go-test" => format!("debug: dlv test -run {target_id} (adapter: delve)"),
        _ => format!("debug: {runner} {target_id} (adapter: tbd)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_temp_dir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("thegn-task-runner-")
            .tempdir()
            .unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tg-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn build_capped_command_scrubs_git_env_and_sets_cwd() {
        // A user job must run with a clean git environment so its own git calls
        // (e.g. a test suite) can't be misdirected at thegn's repo by an
        // inherited GIT_DIR/GIT_INDEX_FILE. `env_remove` shows up in get_envs as
        // (key, None). Thread-safe: inspects the Command, never mutates env.
        let wt = temp_dir("capped-cmd");
        let cmd = build_capped_command(
            &["true".to_string()],
            &wt,
            &thegn_core::config::LimitsConfig::default(),
        );
        let removed: std::collections::HashSet<&std::ffi::OsStr> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k)
            .collect();
        for var in thegn_core::util::GIT_ENV_VARS {
            assert!(
                removed.contains(std::ffi::OsStr::new(var)),
                "capped job command must scrub {var}"
            );
        }
        assert_eq!(cmd.get_current_dir(), Some(wt.as_path()));
        let _ = std::fs::remove_dir_all(&wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// Caps fully disabled → runs bare, so e2e is deterministic and doesn't
    /// depend on systemd-run/nice being present in the test sandbox.
    fn uncapped() -> LimitsConfig {
        LimitsConfig {
            test_cpu_quota: String::new(),
            test_mem_max: String::new(),
            test_nice: 0,
            test_max_parallel: 1,
            ..LimitsConfig::default()
        }
    }

    #[test]
    fn task_command_routes_remote_through_env_and_local_directly() {
        let inner = vec!["sh".to_string(), "-c".to_string(), "cargo test".to_string()];
        // Provider loc → the command runs IN the env via the control prefix,
        // cd'd into the worktree (no host cap wrapper).
        let prov = GitLoc::provider(
            vec!["sprite".into(), "exec".into(), "--".into()],
            "/workspace",
        );
        let cmd = task_command(&prov, &inner, std::path::Path::new("/ignored"), &uncapped());
        assert_eq!(cmd.get_program().to_string_lossy(), "sprite");
        let joined: String = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("cd /workspace &&"),
            "cd into env workdir: {joined}"
        );
        assert!(
            joined.contains("cargo test"),
            "carries the command: {joined}"
        );

        // Local loc → run directly on the host at the worktree cwd (uncapped ⇒
        // the program is the inner argv[0], not a transport).
        let wt = temp_dir("task-cmd-local");
        let local = GitLoc::Local(wt.clone());
        let cmd = task_command(&local, &inner, &wt, &uncapped());
        assert_eq!(cmd.get_program().to_string_lossy(), "sh");
        let _ = std::fs::remove_dir_all(&wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn configured_test_task_beats_manifest_fallback() {
        let wt = temp_dir("configured");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'",
        )
        .unwrap();
        let mut cfg = Config::default();
        cfg.tasks.push(Task {
            name: "unit".into(),
            command: "just".into(),
            args: vec!["unit".into()],
            cwd: None,
            env: Default::default(),
            kind: TaskKind::Test,
            matcher: Some("cargo-test".into()),
            scope: None,
        });
        let task = detect_test_task(&wt, &cfg).unwrap();
        assert_eq!(task.name, "unit");
        assert_eq!(task.command, "just unit");
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn detects_tier_a_manifests_with_right_matcher_and_ingestion() {
        use crate::panel::Ingestion;
        let cases: &[(&str, &str, &str, Ingestion)] = &[
            ("pubspec.yaml", "name: x\n", "dart", Ingestion::Json),
            (
                "pubspec.yaml",
                "name: x\ndependencies:\n  flutter:\n    sdk: flutter\n",
                "flutter",
                Ingestion::Json,
            ),
            (
                "Package.swift",
                "// swift-tools-version:5.9\n",
                "swift",
                Ingestion::Text,
            ),
            (
                "mix.exs",
                "defmodule X do\nend\n",
                "elixir",
                Ingestion::Text,
            ),
            ("CMakeLists.txt", "project(x)\n", "ctest", Ingestion::Text),
            ("pom.xml", "<project/>\n", "junit", Ingestion::Report),
            (
                "build.gradle.kts",
                "plugins {}\n",
                "junit",
                Ingestion::Report,
            ),
            ("build.sbt", "name := \"x\"\n", "junit", Ingestion::Report),
            (
                "rebar.config",
                "{erl_opts, []}.\n",
                "erlang",
                Ingestion::Text,
            ),
            (
                "build.zig",
                "pub fn build() void {}\n",
                "zig",
                Ingestion::Text,
            ),
            (
                ".rspec",
                "--require spec_helper\n",
                "rspec",
                Ingestion::Json,
            ),
            ("phpunit.xml", "<phpunit/>\n", "junit", Ingestion::Report),
            ("dub.json", "{\"name\":\"x\"}\n", "d", Ingestion::Text),
            ("deno.json", "{}\n", "junit", Ingestion::Report),
            (
                "dune-project",
                "(lang dune 3.0)\n",
                "ocaml",
                Ingestion::Text,
            ),
            ("gleam.toml", "name = \"x\"\n", "gleam", Ingestion::Text),
        ];
        for (file, body, matcher, ingestion) in cases {
            let wt = temp_dir(&format!("detect-{}-{}", matcher, file.replace('.', "_")));
            std::fs::write(wt.join(file), body).unwrap();
            let task = detect_test_task(&wt, &Config::default()).unwrap();
            assert_eq!(&task.matcher, matcher, "matcher for {file}");
            assert_eq!(task.ingestion, *ingestion, "ingestion for {file}");
            let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }
    }

    #[test]
    fn detects_file_scan_ecosystems_with_tap_where_possible() {
        use crate::panel::Ingestion;
        // (file under a test dir, matcher, ingestion)
        let cases: &[(&str, &str, Ingestion)] = &[
            ("test/calc.bats", "bats", Ingestion::Tap),
            ("spec/calc_spec.lua", "busted", Ingestion::Tap),
            ("t/basic.t", "prove", Ingestion::Tap),
            ("Calc.Tests.ps1", "nunit", Ingestion::Report),
            ("App.fsproj", "trx", Ingestion::Report),
        ];
        for (rel, matcher, ingestion) in cases {
            let wt = temp_dir(&format!("fscan-{matcher}"));
            let p = wt.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
            let task = detect_test_task(&wt, &Config::default()).unwrap();
            assert_eq!(&task.matcher, matcher, "matcher for {rel}");
            assert_eq!(task.ingestion, *ingestion, "ingestion for {rel}");
            let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }
    }

    #[test]
    fn flake_nix_is_lowest_priority_and_polyglot_prefers_language() {
        // Pure-nix repo → flake checks.
        let nixonly = temp_dir("nixonly");
        std::fs::write(nixonly.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        assert_eq!(
            detect_test_task(&nixonly, &Config::default())
                .unwrap()
                .matcher,
            "nix-flake"
        );
        let _ = std::fs::remove_dir_all(nixonly); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller

        // Cargo + flake → the language runner wins (cargo/nextest), not nix.
        let poly = temp_dir("polyglot");
        std::fs::write(poly.join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
        std::fs::write(
            poly.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'",
        )
        .unwrap();
        let m = detect_test_task(&poly, &Config::default()).unwrap().matcher;
        assert!(matches!(m.as_str(), "cargo-test" | "nextest"), "got {m}");
        let _ = std::fs::remove_dir_all(poly); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn cargo_manifest_fallback_detects_test_task() {
        let wt = temp_dir("cargo");
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'",
        )
        .unwrap();
        let task = detect_test_task(&wt, &Config::default()).unwrap();
        // nextest is preferred when installed; otherwise plain cargo test.
        assert!(
            matches!(task.matcher.as_str(), "cargo-test" | "nextest"),
            "matcher was {}",
            task.matcher
        );
        assert!(task.command.contains("cargo"));
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn parse_outcome_falls_back_to_command_result() {
        let outcome = TaskOutcome {
            worktree: "/tmp/wt".into(),
            generation: 1,
            task: TestTask::new("unit", "false", "generic"),
            exit_code: Some(1),
            duration_ms: 1,
            truncated: false,
            stdout_stderr: "error at src/lib.rs:7:1".into(),
            end: RunEnd::Exited,
        };
        let nodes = parse_task_outcome(&outcome);
        assert!(nodes.iter().any(|n| n.state == TestState::Fail));
    }

    fn on_path(bin: &str) -> bool {
        std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p).any(|d| {
                    let f = d.join(bin);
                    f.is_file()
                })
            })
            .unwrap_or(false)
    }

    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn git_init(dir: &Path) {
        // `git -C dir init` with GIT_DIR/GIT_WORK_TREE/etc. scrubbed: a raw
        // `git init <dir>` inheriting a commit hook's GIT_WORK_TREE writes
        // core.worktree into the OUTER repo's shared config (the pollution bug).
        let _ = thegn_core::util::git_cmd(dir) // best-effort: test setup: a failed init fails the asserted git calls below
            .arg("init")
            .arg("-q")
            .output();
    }

    /// End-to-end against a REAL cargo project in a REAL git repo: detect the
    /// task, run the actual `cargo test`, and parse the real output into a
    /// pass + a fail with a jumpable file:line. Skipped if cargo is absent.
    #[test]
    fn e2e_cargo_real_repo_runs_and_parses_pass_and_fail() {
        if !on_path("cargo") {
            return; // cargo unavailable: skip this real-repo e2e
        }
        let wt = temp_dir("e2e-cargo");
        git_init(&wt);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"tge2e\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n\
             #[cfg(test)]\nmod tests {\n  use super::*;\n\
             #[test] fn passes() { assert_eq!(add(2, 2), 4); }\n\
             #[test] fn fails() { assert_eq!(add(2, 2), 5); }\n}\n",
        )
        .unwrap();

        // Detection picks up the real Cargo.toml (nextest if installed).
        let task = detect_test_task(&wt, &Config::default()).unwrap();
        assert!(matches!(task.matcher.as_str(), "cargo-test" | "nextest"));

        // Run the real test command (single-threaded for stable output).
        //
        // The empty `RUSTC_WRAPPER=`/`CARGO_BUILD_RUSTC_WRAPPER=` make the
        // nested compile hermetic: the dev shell wires `sccache` as the rustc
        // wrapper (both via env and cargo config), which the spawned
        // `cargo test` would otherwise inherit. A flaky/unservable sccache
        // (e.g. the read-only-$HOME sandbox this repo often runs in, where
        // sccache "failed to spawn Command") then kills the compile, so there
        // are no per-test lines to parse — spuriously failing this e2e even
        // though the parser under test is correct. Note: the vars must be set
        // *empty*, not unset — unsetting `RUSTC_WRAPPER` makes cargo fall back
        // to the config's `build.rustc-wrapper` (still sccache); an empty value
        // disables the wrapper outright. This keeps the test about
        // `parse_task_outcome`, not the developer's compiler cache.
        let run_task_spec = TestTask::new(
            "cargo test",
            "env RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= cargo test -- --test-threads=1",
            "cargo-test",
        );
        let outcome = run_task(
            wt.clone(),
            &GitLoc::Local(wt.clone()),
            1,
            run_task_spec,
            &uncapped(),
        );
        let nodes = parse_task_outcome(&outcome);

        assert_eq!(outcome.exit_code, Some(101), "real cargo test should fail");
        assert!(
            nodes
                .iter()
                .any(|n| n.id.contains("passes") && n.state == TestState::Pass),
            "expected a passing test node: {nodes:?}"
        );
        let failed = nodes
            .iter()
            .find(|n| n.id.contains("fails") && n.state == TestState::Fail)
            .expect("expected a failing test node");
        let loc = failed.location.as_ref().expect("failure has a location");
        assert!(loc.path.ends_with("src/lib.rs"), "loc was {loc:?}");

        // Discovery is metadata-based (no compile, no build lock): it lists the
        // crate's testable *targets*, marked as placeholders until a run fills
        // in the per-test results.
        let disc = discover_tests(wt.clone(), &GitLoc::Local(wt.clone()), 2, task, &uncapped());
        assert!(disc.error.is_none(), "discovery error: {:?}", disc.error);
        let target = disc
            .nodes
            .iter()
            .find(|n| n.kind == TestNodeKind::Test)
            .expect("discovery should list at least one test target");
        assert!(target.placeholder, "discovered targets are placeholders");
        assert!(
            target.id.contains("tge2e"),
            "target id should name the package: {}",
            target.id
        );
        assert!(
            target
                .location
                .as_ref()
                .is_some_and(|l| l.path.ends_with("src/lib.rs")),
            "target should point at its source file: {:?}",
            target.location
        );

        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// End-to-end with a real `just` recipe driving an arbitrary test command:
    /// confirms the generic path runs a real process and reflects its exit code.
    #[test]
    fn e2e_generic_shell_task_runs_real_process() {
        let wt = temp_dir("e2e-generic");
        let task = TestTask::new("echo-pass", "echo '✓ widget::works' && true", "generic");
        let outcome = run_task(wt.clone(), &GitLoc::Local(wt.clone()), 1, task, &uncapped());
        assert_eq!(outcome.exit_code, Some(0));
        let nodes = parse_task_outcome(&outcome);
        assert!(
            nodes.iter().any(|n| n.state == TestState::Pass),
            "generic ✓ line should parse as pass: {nodes:?}"
        );
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// End-to-end JSON ingestion against REAL nextest: scaffold a cargo project
    /// with a pass + a fail, run nextest's experimental libtest-json, and confirm
    /// the JSON parser maps results. Skips unless nextest emits that format.
    #[test]
    fn e2e_nextest_json_real_repo() {
        if !on_path("cargo") || !on_path("cargo-nextest") {
            return;
        }
        let wt = temp_dir("e2e-nextest");
        git_init(&wt);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(
            wt.join("Cargo.toml"),
            "[package]\nname = \"tgjson\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )
        .unwrap();
        std::fs::write(
            wt.join("src/lib.rs"),
            "#[cfg(test)]\nmod tests {\n\
             #[test] fn passes() { assert!(true); }\n\
             #[test] fn fails() { assert!(false); }\n}\n",
        )
        .unwrap();

        // Both intentional outcomes must run, even with one nextest worker.
        let task = TestTask::new(
            "cargo nextest",
            "NEXTEST_EXPERIMENTAL_LIBTEST_JSON=1 cargo nextest run --no-fail-fast --message-format libtest-json",
            "nextest",
        )
        .with_ingestion(crate::panel::Ingestion::Json);
        let outcome = run_task(wt.clone(), &GitLoc::Local(wt.clone()), 1, task, &uncapped());

        // Only assert structured results if this nextest version actually emitted
        // libtest-json `"type":"test"` events; otherwise the experimental format
        // isn't supported here and we skip without failing.
        if outcome.stdout_stderr.contains("\"type\": \"test\"")
            || outcome.stdout_stderr.contains("\"type\":\"test\"")
        {
            let nodes = parse_task_outcome(&outcome);
            assert!(
                nodes
                    .iter()
                    .any(|n| n.id.contains("passes") && n.state == TestState::Pass),
                "nextest json should yield a pass node: {nodes:?}"
            );
            assert!(
                nodes
                    .iter()
                    .any(|n| n.id.contains("fails") && n.state == TestState::Fail),
                "nextest json should yield a fail node: {nodes:?}"
            );
        }
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    /// Report-file ingestion through the real dispatcher: a JUnit file on disk
    /// (as Maven/Gradle would write) is parsed when the task's ingestion=Report.
    #[test]
    fn report_ingestion_reads_junit_from_disk() {
        let wt = temp_dir("report");
        let reports = wt.join("target/surefire-reports");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(
            reports.join("TEST-MathTests.xml"),
            r#"<testsuite><testcase name="adds" classname="com.x.M"/>
               <testcase name="broken" classname="com.x.M"><failure>at com.x.M.broken(M.java:9)</failure></testcase>
               </testsuite>"#,
        )
        .unwrap();
        let task = TestTask::new("maven", "mvn -q test", "junit")
            .with_ingestion(crate::panel::Ingestion::Report)
            .with_report_glob("target/surefire-reports/*.xml");
        let outcome = TaskOutcome {
            worktree: wt.to_string_lossy().into_owned(),
            generation: 1,
            task,
            exit_code: Some(1),
            duration_ms: 1,
            truncated: false,
            stdout_stderr: String::new(),
            end: RunEnd::Exited,
        };
        let nodes = parse_task_outcome(&outcome);
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "com.x.M::adds" && n.state == TestState::Pass)
        );
        let f = nodes.iter().find(|n| n.id == "com.x.M::broken").unwrap();
        assert_eq!(f.state, TestState::Fail);
        assert_eq!(f.location.as_ref().unwrap().line, 9);
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn dap_descriptor_is_runner_specific() {
        let cargo = TestTask::new("cargo test", "cargo test --workspace", "cargo-test");
        assert!(dap_launch_descriptor(&cargo, "mod::it").contains("cargo test mod::it"));
        let py = TestTask::new("pytest", "pytest", "pytest");
        assert!(dap_launch_descriptor(&py, "t::k").contains("debugpy"));
        let go = TestTask::new("go test", "go test ./...", "go-test");
        assert!(dap_launch_descriptor(&go, "TestX").contains("dlv"));
    }

    // --- Phase 0 resource-discipline tests ---------------------------------

    #[test]
    fn wrap_capped_disabled_is_passthrough() {
        let argv = vec!["sh".into(), "-lc".into(), "true".into()];
        // Even with a systemd backend, all-disabled limits must not wrap.
        let out = wrap_capped(&argv, &uncapped(), CapBackend::Systemd);
        assert_eq!(out, argv);
    }

    #[test]
    fn wrap_capped_systemd_sets_cpu_mem_nice_scope() {
        let argv = vec!["sh".into(), "-lc".into(), "cargo test".into()];
        let limits = LimitsConfig {
            test_cpu_quota: "150%".into(),
            test_mem_max: "4G".into(),
            test_nice: 10,
            ..LimitsConfig::default()
        };
        let out = wrap_capped(&argv, &limits, CapBackend::Systemd);
        assert_eq!(out[0], "systemd-run");
        assert!(out.contains(&"--scope".to_string()));
        assert!(out.contains(&"CPUQuota=150%".to_string()));
        assert!(out.contains(&"MemoryMax=4G".to_string()));
        assert!(out.windows(2).any(|w| w[0] == "--nice" && w[1] == "10"));
        // The real command is preserved after the `--` separator.
        let sep = out.iter().position(|s| s == "--").unwrap();
        assert_eq!(&out[sep + 1..], &argv[..]);
    }

    #[test]
    fn wrap_capped_nice_prefixes_when_no_systemd() {
        let argv = vec!["sh".into(), "-lc".into(), "go test ./...".into()];
        let limits = LimitsConfig {
            test_cpu_quota: String::new(),
            test_mem_max: String::new(),
            test_nice: 5,
            ..LimitsConfig::default()
        };
        let out = wrap_capped(&argv, &limits, CapBackend::Nice);
        let nice = out.iter().position(|s| s == "nice").expect("nice prefix");
        assert_eq!(out[nice + 1], "-n");
        assert_eq!(out[nice + 2], "5");
        assert!(out.ends_with(&argv));
    }

    /// Single-flight + real cancellation: a long run is killed when a newer job
    /// supersedes it (or `cancel_slot` is called), so it stops burning CPU.
    #[test]
    fn run_capped_cancel_kills_the_process_group() {
        let wt = temp_dir("cancel");
        let slot = format!("{}:run", wt.display());
        let slot2 = slot.clone();
        let handle = std::thread::spawn(move || {
            // Sleeps 30s unless its process group is killed.
            run_capped(
                "sleep 30",
                &GitLoc::Local(wt.clone()),
                &wt,
                &uncapped(),
                CapRun {
                    slot: slot2,
                    generation: 1,
                    timeout: None,
                    live: None,
                    keep_end: false,
                    label: (String::new(), String::new()),
                },
            )
        });
        // Wait for the child to register, then supersede it.
        let mut waited = 0;
        while registry().lock().unwrap().get(&slot).is_none() && waited < 100 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            waited += 1;
        }
        cancel_slot(&slot);
        let start = Instant::now();
        let out = handle.join().unwrap();
        assert!(
            start.elapsed().as_secs() < 10,
            "cancelled run should return promptly, not after the full sleep"
        );
        assert!(!out.timed_out, "this run was cancelled, not timed out");
    }

    #[test]
    fn jobs_have_independent_slots_and_tests_never_kill_a_job() {
        let _dir = private_temp_dir();
        let wt = _dir.path().to_path_buf();
        let slow_slot = job_slot(&wt, "slow");
        let other_slot = job_slot(&wt, "other");
        let wt_thread = wt.clone();
        let slow = std::thread::spawn(move || {
            run_job(
                wt_thread.clone(),
                &GitLoc::Local(wt_thread),
                1,
                TestTask::new("slow", "sleep 30", "generic"),
                &uncapped(),
                None,
            )
        });
        let wt_thread = wt.clone();
        let other = std::thread::spawn(move || {
            run_job(
                wt_thread.clone(),
                &GitLoc::Local(wt_thread),
                4,
                TestTask::new("other", "sleep 30", "generic"),
                &uncapped(),
                None,
            )
        });
        let mut waited = 0;
        while {
            let map = registry().lock().unwrap();
            map.get(&slow_slot).is_none() || map.get(&other_slot).is_none()
        } && waited < 100
        {
            std::thread::sleep(Duration::from_millis(20));
            waited += 1;
        }
        let both_registered = {
            let map = registry().lock().unwrap();
            map.contains_key(&slow_slot) && map.contains_key(&other_slot)
        };
        let fast = run_job(
            wt.clone(),
            &GitLoc::Local(wt.clone()),
            2,
            TestTask::new("fast", "true", "generic"),
            &uncapped(),
            None,
        );
        assert_eq!(fast.exit_code, Some(0));
        let test = run_task(
            wt.clone(),
            &GitLoc::Local(wt.clone()),
            3,
            TestTask::new("unit", "true", "generic"),
            &uncapped(),
        );
        assert_eq!(test.exit_code, Some(0));
        let both_survived_starts = {
            let map = registry().lock().unwrap();
            map.contains_key(&slow_slot) && map.contains_key(&other_slot)
        };
        cancel_job(&wt, "slow");
        let started = Instant::now();
        while !slow.is_finished() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let slow_finished = slow.is_finished();
        let slow_outcome = slow.join().unwrap();
        let other_remained_after_stopping_slow =
            registry().lock().unwrap().contains_key(&other_slot);
        cancel_job(&wt, "other");
        let started = Instant::now();
        while !other.is_finished() && started.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let other_finished = other.is_finished();
        let other_outcome = other.join().unwrap();
        assert!(both_registered, "both jobs should occupy separate slots");
        assert!(
            both_survived_starts,
            "tests and other jobs leave slow running"
        );
        assert!(slow_finished, "cancelled job should finish promptly");
        assert!(
            other_remained_after_stopping_slow,
            "stopping slow leaves other running"
        );
        assert!(
            other_finished,
            "the second job should finish after its own cancellation"
        );
        assert_ne!(slow_outcome.exit_code, Some(0));
        assert_ne!(other_outcome.exit_code, Some(0));
    }

    #[test]
    fn slot_active_counts_tests_discovery_and_jobs_only_for_this_worktree() {
        let _dir = private_temp_dir();
        let wt = _dir.path().join("wt");
        let sibling = PathBuf::from(format!("{}c", wt.display()));
        let key = job_slot(&wt, "x");
        registry().lock().unwrap().insert(
            key.clone(),
            (1, crate::platform::GroupHandle::from_pid(4242)),
        );
        assert!(slot_active(&wt));
        registry().lock().unwrap().remove(&key);
        let sibling_key = job_slot(&sibling, "x");
        registry().lock().unwrap().insert(
            sibling_key.clone(),
            (1, crate::platform::GroupHandle::from_pid(4242)),
        );
        assert!(!slot_active(&wt));
        registry().lock().unwrap().remove(&sibling_key);
        let key = format!("{}:disc", wt.display());
        registry().lock().unwrap().insert(
            key.clone(),
            (1, crate::platform::GroupHandle::from_pid(4242)),
        );
        assert!(slot_active(&wt));
        registry().lock().unwrap().remove(&key);
        let key = test_slot(&wt);
        registry().lock().unwrap().insert(
            key.clone(),
            (1, crate::platform::GroupHandle::from_pid(4242)),
        );
        assert!(slot_active(&wt));
        assert!(!slot_active(&sibling));
        registry().lock().unwrap().remove(&key);
    }

    #[test]
    fn restart_arm_needs_two_presses_on_the_same_task_within_the_window() {
        let t0 = Instant::now();
        let mut arm = RestartArm::default();
        assert!(!arm.confirm("a", t0));
        assert!(arm.confirm("a", t0 + Duration::from_secs(1)));
        assert!(!arm.confirm("a", t0 + Duration::from_secs(2)));
        assert!(!arm.confirm("b", t0 + Duration::from_secs(3)));
        assert!(!arm.confirm("a", t0 + Duration::from_secs(4)));
        assert!(!arm.confirm("a", t0 + Duration::from_secs(8)));
    }

    #[test]
    fn older_job_outcome_is_stale_after_restart() {
        assert!(job_outcome_is_stale(Some(2), 1));
        assert!(!job_outcome_is_stale(Some(2), 2));
        assert!(!job_outcome_is_stale(None, 1));
    }

    #[test]
    fn panicking_test_worker_produces_a_failure_outcome() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let join_error = runtime.block_on(async {
            tokio::spawn(async { panic!("worker panic") })
                .await
                .unwrap_err()
        });
        let outcome = failed_test_run_outcome(
            PathBuf::from("/wt/a"),
            4,
            TestTask::new("unit", "cargo test", "cargo-test"),
            join_error,
        );
        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.duration_ms, 0);
        assert!(!outcome.truncated);
        assert!(
            outcome
                .stdout_stderr
                .starts_with("test run did not complete:")
        );
    }

    /// The watchdog kills a run that overruns its deadline and flags it as a
    /// timeout — the no-hang-forever guarantee.
    #[test]
    fn run_capped_timeout_kills_and_flags() {
        let wt = temp_dir("timeout");
        let slot = test_slot(&wt);
        let start = Instant::now();
        let out = run_capped(
            "sleep 30",
            &GitLoc::Local(wt.clone()),
            &wt,
            &uncapped(),
            CapRun {
                slot,
                generation: 1,
                timeout: Some(Duration::from_millis(300)),
                live: None,
                keep_end: false,
                label: (String::new(), String::new()),
            },
        );
        assert!(
            start.elapsed().as_secs() < 10,
            "timed-out run should return at the deadline, not after the full sleep"
        );
        assert!(out.timed_out, "the deadline should mark the run timed out");
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn jobs_inherit_the_test_timeout_limit() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let limits = LimitsConfig {
            test_timeout_secs: 1,
            ..uncapped()
        };
        let outcome = run_job(
            wt.clone(),
            &GitLoc::Local(wt),
            1,
            TestTask::new("slow", "sleep 2", "generic"),
            &limits,
            None,
        );

        assert_eq!(outcome.exit_code, Some(124));
        assert_eq!(outcome.end, RunEnd::TimedOut { secs: 1 });
        assert!(outcome.stdout_stderr.starts_with("job exceeded 1s"));
    }

    #[test]
    fn live_sink_sees_output_while_the_job_runs() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let messages = Arc::new(Mutex::new(Vec::<JobLive>::new()));
        let sink_messages = messages.clone();
        let sink: JobLiveSink = Arc::new(move |msg| {
            sink_messages.lock().unwrap().push(msg);
        });
        let run_wt = wt.clone();
        let handle = std::thread::spawn(move || {
            run_job(
                run_wt.clone(),
                &GitLoc::Local(run_wt),
                91,
                TestTask::new("live", "printf 'first\\n'; sleep 5", "generic"),
                &uncapped(),
                Some(sink),
            )
        });
        let mut saw_live = false;
        for _ in 0..200 {
            if messages
                .lock()
                .unwrap()
                .iter()
                .any(|msg| msg.tail.contains("first"))
                && !handle.is_finished()
            {
                saw_live = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(saw_live, "live output should arrive before the job exits");
        cancel_job(&wt, "live");
        assert_eq!(handle.join().unwrap().end, RunEnd::Stopped);
    }

    #[test]
    fn live_sink_flushes_output_that_arrives_during_coalescing_wait() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let messages = Arc::new(Mutex::new(Vec::<JobLive>::new()));
        let sink_messages = messages.clone();
        let sink: JobLiveSink = Arc::new(move |msg| {
            sink_messages.lock().unwrap().push(msg);
        });
        let run_wt = wt.clone();
        let handle = std::thread::spawn(move || {
            run_job(
                run_wt.clone(),
                &GitLoc::Local(run_wt),
                95,
                TestTask::new(
                    "coalesced",
                    "printf 'a\\n'; sleep 0.02; printf 'b\\n'; sleep 5",
                    "generic",
                ),
                &uncapped(),
                Some(sink),
            )
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        let mut saw_b_while_running = false;
        while Instant::now() < deadline {
            if messages
                .lock()
                .unwrap()
                .iter()
                .any(|msg| msg.tail.contains("b\n"))
                && !handle.is_finished()
            {
                saw_b_while_running = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            saw_b_while_running,
            "the second line should be flushed while the job is still running"
        );
        cancel_job(&wt, "coalesced");
        assert_eq!(handle.join().unwrap().end, RunEnd::Stopped);
    }

    #[test]
    fn live_tail_is_bounded_and_keeps_the_end() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let messages = Arc::new(Mutex::new(Vec::<JobLive>::new()));
        let sink_messages = messages.clone();
        let sink: JobLiveSink = Arc::new(move |msg| sink_messages.lock().unwrap().push(msg));
        let outcome = run_job(
            wt.clone(),
            &GitLoc::Local(wt),
            92,
            TestTask::new(
                "bounded",
                "head -c 200000 /dev/zero | tr '\\000' x; echo END; sleep 0.2",
                "generic",
            ),
            &uncapped(),
            Some(sink),
        );
        let tails = messages.lock().unwrap();
        assert!(tails.iter().all(|msg| msg.tail.len() <= LIVE_TAIL_BYTES));
        assert!(
            !tails.is_empty(),
            "the emitting job should produce live tails"
        );
        assert!(tails.last().is_some_and(|msg| msg.tail.ends_with("END\n")));
        assert_eq!(outcome.exit_code, Some(0));
    }

    #[test]
    fn head_and_end_returns_head_when_not_truncated() {
        let head = b"complete output".to_vec();
        assert_eq!(
            head_and_end(head.clone(), b"ignored tail", head.len()),
            head
        );
    }

    #[test]
    fn head_and_end_omits_overlap_without_losing_the_end() {
        let head = b"head".to_vec();
        let tail = vec![b'x'; 100];
        let bytes = head_and_end(head.clone(), &tail, head.len() + 10);
        let output = String::from_utf8_lossy(&bytes);
        assert_eq!(
            output,
            "head\n[output truncated: 0 bytes not shown; the end follows]\nxxxxxxxxxx"
        );
    }

    #[test]
    fn head_and_end_starts_suffix_on_utf8_boundary() {
        let head = b"h".to_vec();
        let tail = "x€z".as_bytes();
        let produced = head.len() + 3;
        let output = String::from_utf8(head_and_end(head, tail, produced)).unwrap();
        assert!(output.ends_with('z'));
    }

    #[test]
    fn head_and_end_reports_a_large_gap() {
        let output = String::from_utf8(head_and_end(b"head".to_vec(), b"xy", 20)).unwrap();
        assert!(
            output.starts_with("head\n[output truncated: 14 bytes not shown; the end follows]\n")
        );
        assert!(output.ends_with("xy"));
    }

    #[test]
    fn truncated_job_record_keeps_the_true_end() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let outcome = run_job(
            wt.clone(),
            &GitLoc::Local(wt),
            96,
            TestTask::new(
                "tail",
                "head -c 400000 /dev/zero | tr '\\000' x; echo TRUE_END",
                "generic",
            ),
            &uncapped(),
            None,
        );

        assert!(outcome.truncated);
        assert!(outcome.stdout_stderr.ends_with("TRUE_END\n"));
        assert!(outcome.stdout_stderr.contains("[output truncated:"));
        assert!(outcome.stdout_stderr.len() <= MAX_CAPTURE_BYTES + LIVE_TAIL_BYTES + 128);
    }

    #[test]
    fn untruncated_job_output_has_no_marker() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let outcome = run_job(
            wt.clone(),
            &GitLoc::Local(wt),
            97,
            TestTask::new("small", "echo hi", "generic"),
            &uncapped(),
            None,
        );
        assert_eq!(outcome.stdout_stderr, "hi\n");
    }

    #[test]
    fn truncated_test_run_text_stays_head_only() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let outcome = run_task(
            wt.clone(),
            &GitLoc::Local(wt),
            98,
            TestTask::new(
                "test-output",
                "head -c 400000 /dev/zero | tr '\\000' x; echo TRUE_END",
                "generic",
            ),
            &uncapped(),
        );
        assert!(outcome.truncated);
        assert!(!outcome.stdout_stderr.contains("[output truncated:"));
        assert!(!outcome.stdout_stderr.ends_with("TRUE_END\n"));
    }

    #[test]
    fn chatty_job_past_the_capture_cap_is_not_killed() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let outcome = run_job(
            wt.clone(),
            &GitLoc::Local(wt),
            93,
            TestTask::new("chatty", "head -c 400000 /dev/zero; echo ok", "generic"),
            &uncapped(),
            None,
        );
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.truncated);
    }

    #[test]
    fn live_sink_coalesces_bursts() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink_count = count.clone();
        let sink: JobLiveSink = Arc::new(move |_| {
            sink_count.fetch_add(1, Ordering::Relaxed);
        });
        let started = Instant::now();
        let outcome = run_job(
            wt.clone(),
            &GitLoc::Local(wt),
            94,
            TestTask::new("burst", "seq 1 20000; sleep 0.3", "generic"),
            &uncapped(),
            Some(sink),
        );
        let elapsed_ms = started.elapsed().as_millis();
        let emits = count.load(Ordering::Relaxed);
        assert_eq!(outcome.exit_code, Some(0));
        assert!(emits >= 1, "a non-silent job should emit live output");
        assert!(
            emits as u128 <= elapsed_ms / 100 + 2,
            "{emits} emits in {elapsed_ms}ms"
        );
    }

    #[test]
    fn apply_job_live_fences_generation_worktree_and_finished_runs() {
        let msg = |generation, worktree: &str, tail: &str| JobLive {
            worktree: worktree.into(),
            name: "build".into(),
            generation,
            tail: tail.into(),
        };
        let latest = HashMap::from([("build".to_string(), 7)]);
        for (generation, worktree, running, tail, expected) in [
            (6, "/a", true, "old", false),
            (7, "/b", true, "old", false),
            (7, "/a", false, "old", false),
            (7, "/a", true, "same", false),
            (7, "/a", true, "old", true),
        ] {
            let mut runs = HashMap::from([(
                "build".to_string(),
                crate::panel::TaskRunRecord {
                    running,
                    output_tail: tail.into(),
                    ..Default::default()
                },
            )]);
            let changed =
                apply_job_live(&mut runs, &latest, "/a", msg(generation, worktree, "same"));
            assert_eq!(changed, expected);
            assert_eq!(
                runs["build"].output_tail,
                if expected { "same" } else { tail }
            );
        }
    }

    #[test]
    fn test_run_timeout_keeps_test_run_prefix() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let limits = LimitsConfig {
            test_timeout_secs: 1,
            ..uncapped()
        };
        let outcome = run_task(
            wt.clone(),
            &GitLoc::Local(wt),
            2,
            TestTask::new("slow", "sleep 2", "generic"),
            &limits,
        );
        assert_eq!(outcome.end, RunEnd::TimedOut { secs: 1 });
        assert!(outcome.stdout_stderr.starts_with("test run exceeded 1s"));
    }

    #[test]
    fn stopped_job_reports_stopped_not_failed() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let wt2 = wt.clone();
        let handle = std::thread::spawn(move || {
            run_job(
                wt2.clone(),
                &GitLoc::Local(wt2),
                71,
                TestTask::new("slow", "sleep 30", "generic"),
                &uncapped(),
                None,
            )
        });
        for _ in 0..100 {
            if slot_active(&wt) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            slot_active(&wt),
            "job should register before it is cancelled"
        );
        cancel_job(&wt, "slow");
        let outcome = handle.join().unwrap();
        assert_eq!(outcome.end, RunEnd::Stopped);
        assert!(outcome.stdout_stderr.starts_with("stopped by user"));
        assert!(!slot_active(&wt), "cancelled job should be reaped");
        assert!(
            !stopped_runs()
                .lock()
                .unwrap()
                .contains(&(job_slot(&wt, "slow"), 71))
        );
    }

    #[test]
    fn superseded_run_is_not_marked_stopped() {
        let dir = private_temp_dir();
        let wt = dir.path().to_path_buf();
        let wt2 = wt.clone();
        let slot = job_slot(&wt, "slow");
        let slot2 = slot.clone();
        let handle = std::thread::spawn(move || {
            run_job(
                wt2.clone(),
                &GitLoc::Local(wt2),
                72,
                TestTask::new("slow", "sleep 30", "generic"),
                &uncapped(),
                None,
            )
        });
        for _ in 0..100 {
            if slot_active(&wt) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(slot_active(&wt), "job should register before supersede");
        cancel_slot(&slot);
        let outcome = handle.join().unwrap();
        assert_eq!(outcome.end, RunEnd::Exited);
        assert!(!stopped_runs().lock().unwrap().contains(&(slot2, 72)));
    }

    #[test]
    fn finishing_superseded_generation_clears_its_stop_mark_without_stopping_newer_run() {
        let dir = private_temp_dir();
        let slot = job_slot(dir.path(), "slow");
        registry().lock().unwrap().insert(
            slot.clone(),
            (73, crate::platform::GroupHandle::from_pid(4242)),
        );
        stopped_runs().lock().unwrap().insert((slot.clone(), 72));

        assert!(!finish_slot(&slot, 72));
        assert!(registry().lock().unwrap().contains_key(&slot));
        assert!(!stopped_runs().lock().unwrap().contains(&(slot.clone(), 72)));

        registry().lock().unwrap().remove(&slot);
    }

    #[test]
    fn job_status_line_formats_each_end_kind() {
        let cases = [
            (RunEnd::Exited, 0, "Task 'build' done in 1.2s"),
            (RunEnd::Exited, 2, "Task 'build' failed (exit 2) in 1.2s"),
            (
                RunEnd::TimedOut { secs: 1800 },
                124,
                "Task 'build' timed out after 1800s",
            ),
            (RunEnd::Stopped, -1, "Task 'build' stopped after 1.2s"),
        ];
        for (end, exit, expected) in cases {
            assert_eq!(job_status_line("build", end, exit, 1.2), expected);
        }
    }

    /// Metadata-based cargo discovery parses targets without compiling.
    #[test]
    fn cargo_metadata_targets_parse_to_placeholders() {
        let json = r#"{
            "packages": [
                {"name": "demo", "targets": [
                    {"name": "demo", "kind": ["lib"], "test": true, "src_path": "/r/src/lib.rs"},
                    {"name": "it", "kind": ["test"], "test": true, "src_path": "/r/tests/it.rs"},
                    {"name": "ex", "kind": ["example"], "test": false, "src_path": "/r/examples/ex.rs"}
                ]}
            ]
        }"#;
        let nodes = parse_cargo_metadata_targets(json);
        let tests: Vec<_> = nodes
            .iter()
            .filter(|n| n.kind == TestNodeKind::Test)
            .collect();
        assert_eq!(
            tests.len(),
            2,
            "lib + integration, not the example: {tests:?}"
        );
        assert!(tests.iter().all(|n| n.placeholder));
        assert!(tests.iter().any(|n| n.id == "demo::lib tests"));
        assert!(tests.iter().any(|n| n.id == "demo::it"));
    }

    #[test]
    fn extract_test_name_handles_every_declaration_shape() {
        // <keyword> <ident>
        assert_eq!(
            extract_test_name("func TestAdd(t *testing.T) {").as_deref(),
            Some("TestAdd")
        );
        assert_eq!(
            extract_test_name("    func testParsesJson() throws {").as_deref(),
            Some("testParsesJson")
        );
        assert_eq!(
            extract_test_name("    def test_widget_renders(self):").as_deref(),
            Some("test_widget_renders")
        );
        assert_eq!(
            extract_test_name("class TestMath:").as_deref(),
            Some("TestMath")
        );
        // <keyword> "name" / it('name')
        assert_eq!(
            extract_test_name("  test \"adds two numbers\" do").as_deref(),
            Some("adds two numbers")
        );
        assert_eq!(
            extract_test_name("  it('renders the header', () => {").as_deref(),
            Some("renders the header")
        );
        assert_eq!(
            extract_test_name("test(`templated name`, async () => {").as_deref(),
            Some("templated name")
        );
        // Non-test lines yield nothing.
        assert_eq!(extract_test_name("let x = 3;"), None);
    }

    #[test]
    fn parse_scan_output_builds_jumpable_placeholders_grouped_by_file() {
        let out = "./pkg/math_test.go:12:func TestAdd(t *testing.T) {\n\
                   ./pkg/math_test.go:20:func TestSub(t *testing.T) {\n\
                   not-a-match-line\n";
        let nodes = parse_scan_output(out);
        let tests: Vec<_> = nodes
            .iter()
            .filter(|n| n.kind == TestNodeKind::Test)
            .collect();
        assert_eq!(tests.len(), 2, "two go tests: {tests:?}");
        assert!(tests.iter().all(|n| n.placeholder));
        let add = tests.iter().find(|n| n.label == "TestAdd").unwrap();
        assert_eq!(add.id, "pkg/math_test.go::TestAdd");
        let loc = add.location.as_ref().expect("jumpable location");
        assert_eq!((loc.path.as_str(), loc.line), ("pkg/math_test.go", 12));
    }

    /// End-to-end source-scan discovery against a REAL Go module: no `go`
    /// invocation, no compile — just an in-process fff grep over `*_test.go`.
    /// Verifies the generalized (cargo-metadata-style) instant discovery on
    /// another toolchain. No `rg`/`grep` binary needed (local `GitLoc` → fff).
    #[test]
    fn e2e_go_scan_discovers_without_compiling() {
        let wt = temp_dir("e2e-go-scan");
        std::fs::write(wt.join("go.mod"), "module demo\n\ngo 1.21\n").unwrap();
        std::fs::write(
            wt.join("math_test.go"),
            "package demo\n\nimport \"testing\"\n\n\
             func TestAdd(t *testing.T) { if 2+2 != 4 { t.Fail() } }\n\
             func TestSub(t *testing.T) {}\n\
             func helper() {}\n",
        )
        .unwrap();

        let task = detect_test_task(&wt, &Config::default()).unwrap();
        assert_eq!(task.matcher, "go-test");
        let disc = discover_tests(wt.clone(), &GitLoc::Local(wt.clone()), 1, task, &uncapped());
        assert!(disc.error.is_none(), "discovery error: {:?}", disc.error);
        let labels: Vec<&str> = disc
            .nodes
            .iter()
            .filter(|n| n.kind == TestNodeKind::Test)
            .map(|n| n.label.as_str())
            .collect();
        assert!(
            labels.contains(&"TestAdd") && labels.contains(&"TestSub"),
            "scan should find both test funcs (not the helper): {labels:?}"
        );
        assert!(
            !labels.contains(&"helper"),
            "non-test funcs excluded: {labels:?}"
        );
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}

// ── Task auto-discovery ───────────────────────────────────────────────────────

/// Merge configured tasks (win by name) with auto-discovered tasks (fill gaps).
/// The final list is: all configured tasks first, then discovered tasks whose
/// names (case-insensitive) don't collide with any configured task.
pub fn merge_tasks(configured: Vec<Task>, discovered: Vec<Task>) -> Vec<Task> {
    let configured_names: std::collections::HashSet<String> = configured
        .iter()
        .map(|t| t.name.to_ascii_lowercase())
        .collect();
    let mut out = configured;
    for t in discovered {
        if !configured_names.contains(&t.name.to_ascii_lowercase()) {
            out.push(t);
        }
    }
    out
}

/// Infer `TaskKind` from a command string. Pure; no subprocess.
fn infer_kind(command: &str) -> TaskKind {
    let c = command.to_ascii_lowercase();
    if c.contains("test") || c.contains("spec") || c.contains("check --") {
        TaskKind::Test
    } else if c.contains("build") || c.contains("compile") || c.contains("make ") || c == "make" {
        TaskKind::Build
    } else if c.contains("lint")
        || c.contains("fmt")
        || c.contains("format")
        || c.contains("clippy")
        || c.contains("check")
    {
        TaskKind::Lint
    } else if c.contains("dev")
        || c.contains("serve")
        || c.contains("start")
        || c.contains("watch")
        || c.starts_with("run ")
        || c == "run"
    {
        TaskKind::Run
    } else {
        TaskKind::Custom
    }
}

fn make_task(name: impl Into<String>, command: impl Into<String>, kind: TaskKind) -> Task {
    Task {
        name: name.into(),
        command: command.into(),
        args: Vec::new(),
        cwd: None,
        env: Default::default(),
        kind,
        matcher: None,
        scope: None,
    }
}

fn discover_justfile(worktree: &Path) -> Vec<Task> {
    for name in &["justfile", "Justfile", ".justfile"] {
        let Ok(text) = std::fs::read_to_string(worktree.join(name)) else {
            continue;
        };
        let mut tasks = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            // Recipe header: starts with an identifier immediately followed by ':' or ' '
            // (not indented, not a comment, not a setting).
            if trimmed.starts_with('#') || trimmed.starts_with('@') || !trimmed.contains(':') {
                continue;
            }
            let recipe = trimmed.split([':', ' ', '(']).next().unwrap_or("");
            if recipe.is_empty()
                || recipe.starts_with('_')
                || recipe
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            {
                continue;
            }
            // Skip variables (UPPER_SNAKE or key := value)
            if recipe.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                continue;
            }
            let cmd = format!("just {recipe}");
            let kind = infer_kind(recipe);
            tasks.push(make_task(recipe.to_string(), cmd, kind));
        }
        return tasks;
    }
    Vec::new()
}

fn discover_makefile(worktree: &Path) -> Vec<Task> {
    let Ok(text) = std::fs::read_to_string(worktree.join("Makefile")) else {
        return Vec::new();
    };
    let mut phony: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(".PHONY:") {
            for t in rest.split_whitespace() {
                phony.insert(t);
            }
        }
    }
    let mut tasks = Vec::new();
    for line in text.lines() {
        // Target lines: "targetname:" not indented and not a variable assignment.
        if line.starts_with('\t') || line.starts_with(' ') || line.starts_with('#') {
            continue;
        }
        let Some(colon_pos) = line.find(':') else {
            continue;
        };
        let target = line[..colon_pos].trim();
        if target.is_empty()
            || target.starts_with('.')
            || target.contains('$')
            || target.contains('/')
        {
            continue;
        }
        // Include explicit .PHONY targets and common well-known targets.
        let well_known = matches!(
            target,
            "all" | "build" | "test" | "clean" | "install" | "run" | "fmt" | "lint" | "check"
        );
        if phony.contains(target) || well_known {
            let cmd = format!("make {target}");
            let kind = infer_kind(target);
            tasks.push(make_task(target.to_string(), cmd, kind));
        }
    }
    tasks
}

fn discover_package_json(worktree: &Path) -> Vec<Task> {
    let Ok(text) = std::fs::read_to_string(worktree.join("package.json")) else {
        return Vec::new();
    };
    // Minimal JSON parser: find "scripts": { "name": "cmd", ... }
    let Some(scripts_start) = text.find("\"scripts\"") else {
        return Vec::new();
    };
    let after_scripts = &text[scripts_start..];
    let Some(brace_open) = after_scripts.find('{') else {
        return Vec::new();
    };
    let inner = &after_scripts[brace_open + 1..];
    let Some(brace_close) = inner.find('}') else {
        return Vec::new();
    };
    let scripts_block = &inner[..brace_close];

    let mut tasks = Vec::new();
    // Each entry looks like: "name": "command"
    let mut rest = scripts_block;
    while let Some(q1) = rest.find('"') {
        rest = &rest[q1 + 1..];
        let Some(q2) = rest.find('"') else { break };
        let name = &rest[..q2];
        rest = &rest[q2 + 1..];
        let Some(colon) = rest.find(':') else { break };
        rest = &rest[colon + 1..];
        let rest_trim = rest.trim_start();
        if !rest_trim.starts_with('"') {
            continue;
        }
        rest = &rest_trim[1..];
        let Some(q3) = rest.find('"') else { break };
        let cmd_value = &rest[..q3];
        rest = &rest[q3 + 1..];

        if name.is_empty() || name.starts_with('_') {
            continue;
        }
        let cmd = format!("npm run {name}");
        let kind = infer_kind(name);
        tasks.push(make_task(name.to_string(), cmd, kind));
        // best-effort: drain: bounded read of auxiliary output; failure loses the buffer, not the outcome
        let _ = cmd_value; // available if we need it later
    }
    tasks
}

fn discover_cargo_toml(worktree: &Path) -> Vec<Task> {
    let Ok(text) = std::fs::read_to_string(worktree.join("Cargo.toml")) else {
        return Vec::new();
    };
    let mut tasks = vec![
        make_task("cargo build", "cargo build", TaskKind::Build),
        make_task("cargo test", "cargo test --workspace", TaskKind::Test),
        make_task("cargo clippy", "cargo clippy --workspace", TaskKind::Lint),
        make_task("cargo fmt", "cargo fmt --all", TaskKind::Lint),
        make_task("cargo check", "cargo check --workspace", TaskKind::Lint),
    ];
    // Parse [alias] section
    let mut in_alias = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[alias]" {
            in_alias = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_alias = false;
        }
        if in_alias && let Some(eq) = trimmed.find('=') {
            let name = trimmed[..eq].trim().trim_matches('"');
            let val = trimmed[eq + 1..].trim().trim_matches(['"', '\'']);
            if !name.is_empty() {
                let cmd = format!("cargo {name}");
                let kind = infer_kind(val);
                tasks.push(make_task(format!("cargo {name}"), cmd, kind));
            }
        }
    }
    tasks
}

fn discover_go_mod(worktree: &Path) -> Vec<Task> {
    if !worktree.join("go.mod").exists() {
        return Vec::new();
    }
    vec![
        make_task("go build", "go build ./...", TaskKind::Build),
        make_task("go test", "go test ./...", TaskKind::Test),
        make_task("go vet", "go vet ./...", TaskKind::Lint),
        make_task("gofmt", "gofmt -w .", TaskKind::Lint),
    ]
}

fn discover_pyproject(worktree: &Path) -> Vec<Task> {
    let text = std::fs::read_to_string(worktree.join("pyproject.toml"))
        .or_else(|_| std::fs::read_to_string(worktree.join("tox.ini")))
        .unwrap_or_default();
    if text.is_empty() {
        return Vec::new();
    }
    let mut tasks = Vec::new();
    // taskipy tasks: [tool.taskipy.tasks] or just [tasks]
    let mut in_taskipy = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "[tool.taskipy.tasks]" || trimmed == "[tasks]" {
            in_taskipy = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_taskipy = false;
        }
        if in_taskipy && let Some(eq) = trimmed.find('=') {
            let name = trimmed[..eq].trim().trim_matches('"');
            if !name.is_empty() {
                let cmd = format!("task {name}");
                let kind = infer_kind(name);
                tasks.push(make_task(name.to_string(), cmd, kind));
            }
        }
    }
    // tox envs: [testenv:name]
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("[testenv:") {
            let env_name = rest.trim_end_matches(']');
            if !env_name.is_empty() {
                tasks.push(make_task(
                    format!("tox:{env_name}"),
                    format!("tox -e {env_name}"),
                    infer_kind(env_name),
                ));
            }
        }
    }
    tasks
}

fn discover_docker_compose(worktree: &Path) -> Vec<Task> {
    let text = [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yml",
        "compose.yaml",
    ]
    .iter()
    .find_map(|f| std::fs::read_to_string(worktree.join(f)).ok())
    .unwrap_or_default();
    if text.is_empty() {
        return Vec::new();
    }
    let mut tasks = Vec::new();
    let mut in_services = false;
    for line in text.lines() {
        if line.trim() == "services:" {
            in_services = true;
            continue;
        }
        if in_services {
            // Service names are at exactly 2 spaces of indentation
            if line.starts_with("  ") && !line.starts_with("   ") {
                let svc = line.trim().trim_end_matches(':');
                if !svc.is_empty() && !svc.starts_with('#') {
                    tasks.push(make_task(
                        format!("compose:{svc}"),
                        format!("docker compose up {svc}"),
                        TaskKind::Run,
                    ));
                }
            }
            // Stop at the next top-level section
            if !line.starts_with(' ') && line.contains(':') {
                in_services = false;
            }
        }
    }
    tasks
}

fn discover_procfile(worktree: &Path) -> Vec<Task> {
    let Ok(text) = std::fs::read_to_string(worktree.join("Procfile")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (name, _cmd) = line.split_once(':')?;
            let name = name.trim();
            if name.is_empty() || name.starts_with('#') {
                return None;
            }
            Some(make_task(
                name.to_string(),
                format!("foreman start {name}"),
                TaskKind::Run,
            ))
        })
        .collect()
}

/// Auto-discover tasks from well-known manifests in `worktree`. Does NOT run
/// any subprocesses — reads manifest files only. Results are merged with
/// configured tasks by the caller via [`merge_tasks`].
pub fn discover_all_tasks(worktree: &Path) -> Vec<Task> {
    let mut tasks = Vec::new();
    tasks.extend(discover_justfile(worktree));
    tasks.extend(discover_makefile(worktree));
    tasks.extend(discover_package_json(worktree));
    tasks.extend(discover_cargo_toml(worktree));
    tasks.extend(discover_go_mod(worktree));
    tasks.extend(discover_pyproject(worktree));
    tasks.extend(discover_docker_compose(worktree));
    tasks.extend(discover_procfile(worktree));
    tasks
}

// ── Diagnostic extraction ─────────────────────────────────────────────────────

/// Severity level for a compiler/linter/test diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagSeverity {
    Error = 0,
    Warning = 1,
    Info = 2,
    Hint = 3,
}

/// One structured diagnostic extracted from task output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedDiag {
    pub file: String,
    pub line: u64,
    pub col: Option<u64>,
    pub severity: DiagSeverity,
    pub message: String,
    pub source: String,
    pub code: Option<String>,
}

fn diag_severity(word: &str) -> Option<DiagSeverity> {
    match word.to_ascii_lowercase().as_str() {
        "error" | "err" => Some(DiagSeverity::Error),
        "warning" | "warn" => Some(DiagSeverity::Warning),
        "note" | "info" | "help" => Some(DiagSeverity::Info),
        "hint" => Some(DiagSeverity::Hint),
        _ => None,
    }
}

/// Extract structured diagnostics from task output. Pattern matches GCC/clang/
/// rustc, Go, Python/pytest, and a generic fallback. Results are capped at 500
/// and deduplicated by (file, line, message).
pub fn extract_diagnostics(output: &str, source: &str) -> Vec<ExtractedDiag> {
    let mut out: Vec<ExtractedDiag> = Vec::new();
    let mut seen: std::collections::HashSet<(String, u64, String)> =
        std::collections::HashSet::new();

    for line in output.lines() {
        if out.len() >= 500 {
            break;
        }
        // GCC/clang/rustc/cargo: file:line:col: severity: message
        // Also handles: file:line: severity: message (no col)
        if let Some(d) = parse_gcc_line(line, source) {
            let key = (d.file.clone(), d.line, d.message.clone());
            if seen.insert(key) {
                out.push(d);
            }
            continue;
        }
        // Python/pytest: "FAILED path/test.py::TestClass::test_name"
        if let Some(d) = parse_pytest_line(line, source) {
            let key = (d.file.clone(), d.line, d.message.clone());
            if seen.insert(key) {
                out.push(d);
            }
        }
    }

    out.sort_by_key(|d| d.severity as u8);
    out
}

fn parse_gcc_line(line: &str, source: &str) -> Option<ExtractedDiag> {
    // Pattern: <path>:<line>[:<col>]: <severity>: <message>
    // The path segment must not start with whitespace and must not be a URL.
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Split on ": " to find severity keyword.
    let mut parts = line.splitn(3, ": ");
    let location = parts.next()?;
    let severity_word = parts.next()?;
    let message = parts.next().unwrap_or("").trim();

    let severity = diag_severity(severity_word)?;

    // Parse location: path:line[:col]
    let loc_parts: Vec<&str> = location.rsplitn(3, ':').collect();
    // loc_parts is reversed: [col_or_line, line_or_path, path...]
    let (file, line_no, col) = match loc_parts.len() {
        3 => {
            // path:line:col
            let col: u64 = loc_parts[0].parse().ok()?;
            let ln: u64 = loc_parts[1].parse().ok()?;
            let path = &location[..location.len() - loc_parts[0].len() - loc_parts[1].len() - 2];
            (path, ln, Some(col))
        }
        2 => {
            // path:line
            let ln: u64 = loc_parts[0].parse().ok()?;
            let path = &location[..location.len() - loc_parts[0].len() - 1];
            (path, ln, None)
        }
        _ => return None,
    };

    if file.is_empty() || file.contains("://") || file.starts_with("    ") {
        return None;
    }
    // Reject paths that don't look like file paths (contain spaces but no slash).
    if file.contains(' ') && !file.contains('/') && !file.contains('\\') {
        return None;
    }

    Some(ExtractedDiag {
        file: file.to_string(),
        line: line_no,
        col,
        severity,
        message: message.to_string(),
        source: source.to_string(),
        code: None,
    })
}

fn parse_pytest_line(line: &str, source: &str) -> Option<ExtractedDiag> {
    let line = line.trim();
    // "FAILED tests/test_foo.py::TestClass::test_name - AssertionError"
    if let Some(rest) = line.strip_prefix("FAILED ") {
        let (path_part, msg) = rest.split_once(" - ").unwrap_or((rest, "test failed"));
        let file = path_part.split("::").next().unwrap_or(path_part);
        return Some(ExtractedDiag {
            file: file.to_string(),
            line: 1,
            col: None,
            severity: DiagSeverity::Error,
            message: msg.to_string(),
            source: source.to_string(),
            code: None,
        });
    }
    // "ERROR tests/test_foo.py::TestClass::test_name"
    if let Some(rest) = line.strip_prefix("ERROR ") {
        let file = rest.split("::").next().unwrap_or(rest).trim();
        return Some(ExtractedDiag {
            file: file.to_string(),
            line: 1,
            col: None,
            severity: DiagSeverity::Error,
            message: "test error".to_string(),
            source: source.to_string(),
            code: None,
        });
    }
    None
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    fn temp_dir2(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("tg-disc-{tag}"));
        let _ = std::fs::remove_dir_all(&d); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn merge_tasks_configured_wins() {
        let configured = vec![make_task("test", "my test runner", TaskKind::Test)];
        let discovered = vec![
            make_task("test", "cargo test", TaskKind::Test),
            make_task("build", "cargo build", TaskKind::Build),
        ];
        let merged = merge_tasks(configured, discovered);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].command, "my test runner");
        assert_eq!(merged[1].name, "build");
    }

    #[test]
    fn discover_justfile_recipes() {
        let dir = temp_dir2("just");
        std::fs::write(
            dir.join("Justfile"),
            "build:\n    cargo build\n\ntest:\n    cargo test\n\n_hidden:\n    echo hidden\n",
        )
        .unwrap();
        let tasks = discover_justfile(&dir);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"build"), "{names:?}");
        assert!(names.contains(&"test"), "{names:?}");
        assert!(
            !names.contains(&"_hidden"),
            "hidden recipes excluded: {names:?}"
        );
        let _ = std::fs::remove_dir_all(dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn discover_package_json_scripts() {
        let dir = temp_dir2("npm");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"demo","scripts":{"build":"tsc","test":"jest","lint":"eslint ."}}"#,
        )
        .unwrap();
        let tasks = discover_package_json(&dir);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"build"), "{names:?}");
        assert!(names.contains(&"test"), "{names:?}");
        assert!(names.contains(&"lint"), "{names:?}");
        let _ = std::fs::remove_dir_all(dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn discover_cargo_toml_canonical_tasks() {
        let dir = temp_dir2("cargo");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
        let tasks = discover_cargo_toml(&dir);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"cargo build"), "{names:?}");
        assert!(names.contains(&"cargo test"), "{names:?}");
        assert!(names.contains(&"cargo clippy"), "{names:?}");
        let _ = std::fs::remove_dir_all(dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn extract_diagnostics_gcc_style() {
        let output = "src/main.rs:10:5: error: unused variable `x`\n\
                      src/lib.rs:20:3: warning: deprecated function\n\
                      not a diagnostic line\n";
        let diags = extract_diagnostics(output, "cargo");
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert_eq!(diags[0].severity, DiagSeverity::Error);
        assert_eq!(diags[0].file, "src/main.rs");
        assert_eq!(diags[0].line, 10);
        assert_eq!(diags[1].severity, DiagSeverity::Warning);
    }

    #[test]
    fn extract_diagnostics_pytest_style() {
        let output = "FAILED tests/test_foo.py::TestBar::test_something - AssertionError: 1 != 2\n\
                      ERROR tests/test_baz.py::TestQux::test_other\n";
        let diags = extract_diagnostics(output, "pytest");
        assert_eq!(diags.len(), 2, "{diags:?}");
        assert!(diags.iter().all(|d| d.severity == DiagSeverity::Error));
        assert!(
            diags.iter().any(|d| d.file == "tests/test_foo.py"),
            "{diags:?}"
        );
    }

    #[test]
    fn extract_diagnostics_deduplicates() {
        let line = "src/main.rs:5:1: error: duplicate symbol\n";
        let output = line.repeat(5);
        let diags = extract_diagnostics(&output, "cargo");
        assert_eq!(diags.len(), 1, "should deduplicate: {diags:?}");
    }

    #[test]
    fn slot_active_reflects_registry() {
        let wt = temp_dir2("slot");
        assert!(!slot_active(&wt), "no slot registered yet");
        let slot = test_slot(&wt);
        registry().lock().unwrap().insert(
            slot.clone(),
            (1, crate::platform::GroupHandle::from_pid(4242)),
        );
        assert!(slot_active(&wt), "active while a test slot is registered");
        registry().lock().unwrap().remove(&slot);
        assert!(!slot_active(&wt), "inactive after the slot clears");
        let _ = std::fs::remove_dir_all(wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn discover_makefile_phony_targets() {
        let dir = temp_dir2("make");
        std::fs::write(
            dir.join("Makefile"),
            ".PHONY: build test clean\nbuild:\n\tcargo build\ntest:\n\tcargo test\nclean:\n\trm -rf target\n",
        ).unwrap();
        let tasks = discover_makefile(&dir);
        let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"build"), "{names:?}");
        assert!(names.contains(&"test"), "{names:?}");
        assert!(names.contains(&"clean"), "{names:?}");
        let _ = std::fs::remove_dir_all(dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
