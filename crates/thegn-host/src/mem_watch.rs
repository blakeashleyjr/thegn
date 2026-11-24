//! Per-pane memory watch: the host half (THE-2600 follow-up).
//!
//! Every local interactive pane runs in its own transient systemd scope under
//! `thegn.slice`. This module watches those scopes:
//!
//! - **Sample.** A sampler thread reads each pane scope's `memory.current`,
//!   `memory.max`, `memory.swap.current` and `memory.events` every
//!   [`SAMPLE_EVERY`]. The blocking cgroupfs reads never touch the event loop:
//!   the loop only *publishes* `(pane id, pid, program)` triples into
//!   [`Shared`] (throttled, atomics and a map swap) and the thread resolves each
//!   pid's scope through `/proc/<pid>/cgroup`. The thread exists only while
//!   there are published panes and exits when there are none, like the other
//!   ticker threads — a host with no cgroup v2 never starts it.
//! - **Warn.** Threshold crossings and `oom_kill` increments come from the pure
//!   state machine in [`thegn_core::pane_memwatch`]; they surface as a toast
//!   through the existing [`RefreshKind::Toast`] path.
//! - **Restart.** When a pane's shell exits and that exit coincides with an
//!   `oom_kill`, [`on_pane_exit`] tells the exit handler to keep the pane's
//!   leaf for the materialize pipeline (the same respawn a sole pane already
//!   gets) and [`take_relaunch`] hands the fresh shell the agent's continue
//!   command. The 10-minute cooldown follows the pane slot across the id change.
//!
//! No agent-notification channel exists to deliver the warning through, and the
//! scope-memory notice never types into a PTY, so agents see nothing here; the
//! relaunch above is the only input written, into a shell thegn just spawned.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;
use thegn_core::notification::Priority;
use thegn_core::pane_memwatch as pmw;

use crate::hydrate::RefreshKind;

/// How often each pane scope is sampled.
const SAMPLE_EVERY_MS: u64 = 5_000;
const SAMPLE_EVERY: Duration = Duration::from_millis(SAMPLE_EVERY_MS);
/// Where cgroup v2 is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Automatic restarts are on; there is no config key yet.
const AUTO_RESTART: bool = true;

/// A pane the loop asked the sampler to watch.
struct Target {
    pid: u32,
    program: String,
}

struct Notice {
    tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
}

#[derive(Default)]
struct Shared {
    /// Published by the loop: every pane with a local pid.
    targets: HashMap<u32, Target>,
    /// Pane id to the monotonic ms of its latest `oom_kill` increment.
    last_oom_ms: HashMap<u32, u64>,
    /// Pane id to the monotonic ms of its latest automatic restart.
    last_restart_ms: HashMap<u32, u64>,
    /// Pane id to the command that brings its agent back in the new shell.
    relaunch: HashMap<u32, String>,
    notice: Option<Notice>,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default()))
}

fn lock() -> MutexGuard<'static, Shared> {
    shared().lock().unwrap_or_else(|poison| poison.into_inner())
}

fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    u64::try_from(EPOCH.get_or_init(Instant::now).elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Whether this host has cgroup v2 at all; checked once.
fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| Path::new(CGROUP_ROOT).join("cgroup.controllers").is_file())
}

/// Give the sampler somewhere to send its toasts. Called once at startup.
pub(crate) fn install_notice(
    tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    lock().notice = Some(Notice { tx, waker });
}

/// Publish the live pane set to the sampler. Called from the drain on every loop
/// turn; throttled to once per [`SAMPLE_EVERY`] so the common call is one atomic
/// load. Does no I/O.
pub(crate) fn publish(table: &HashMap<u32, crate::pane::PtyPane>) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    if !supported() {
        return;
    }
    let now = now_ms().saturating_add(1);
    let last = LAST.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < SAMPLE_EVERY_MS {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let targets: HashMap<u32, Target> = table
        .iter()
        .filter_map(|(id, pane)| {
            Some((
                *id,
                Target {
                    pid: pane.live_pid()?,
                    program: pane.program().to_string(),
                },
            ))
        })
        .collect();
    let any = !targets.is_empty();
    lock().targets = targets;
    if any {
        ensure_sampler();
    }
}

fn ensure_sampler() {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let spawned = thegn_core::thread_name::try_spawn_named("thegn-memwatch", || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let mut watches: HashMap<u32, Watch> = HashMap::new();
        loop {
            std::thread::sleep(SAMPLE_EVERY);
            if !tick(&mut watches) {
                RUNNING.store(false, Ordering::Release);
                return;
            }
        }
    });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::Release);
    }
}

/// Sampler-side state for one pane.
struct Watch {
    pid: u32,
    /// The pane scope's cgroup directory once resolved.
    dir: Option<PathBuf>,
    state: pmw::WatchState,
}

impl Watch {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            dir: None,
            state: pmw::WatchState::default(),
        }
    }
}

fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// One reading of a scope directory; `None` when `memory.current` is gone (the
/// scope ended).
fn read_sample(dir: &Path) -> Option<pmw::MemSample> {
    let current = pmw::parse_u64(&read_file(&dir.join("memory.current"))?)?;
    Some(pmw::MemSample {
        current,
        max: read_file(&dir.join("memory.max")).and_then(|text| pmw::parse_limit(&text)),
        swap_current: read_file(&dir.join("memory.swap.current"))
            .and_then(|text| pmw::parse_u64(&text))
            .unwrap_or(0),
        events: read_file(&dir.join("memory.events"))
            .map(|text| pmw::parse_events(&text))
            .unwrap_or_default(),
    })
}

/// The scope directory of `pid`, from its `/proc/<pid>/cgroup`. `Err` means the
/// process is gone; `Ok(None)` means it is alive but not in a pane scope.
fn resolve_dir(pid: u32) -> Result<Option<PathBuf>, ()> {
    let body = read_file(Path::new(&format!("/proc/{pid}/cgroup"))).ok_or(())?;
    Ok(pmw::pane_scope_path(&body)
        .map(|path| Path::new(CGROUP_ROOT).join(path.trim_start_matches('/'))))
}

/// One sampling round. `false` ends the thread (nothing left to watch).
fn tick(watches: &mut HashMap<u32, Watch>) -> bool {
    let targets: Vec<(u32, u32, String)> = lock()
        .targets
        .iter()
        .map(|(id, target)| (*id, target.pid, target.program.clone()))
        .collect();
    if targets.is_empty() {
        return false;
    }
    watches.retain(|id, _| targets.iter().any(|(live, _, _)| live == id));
    let mut gone: Vec<u32> = Vec::new();
    for (id, pid, program) in targets {
        let watch = watches.entry(id).or_insert_with(|| Watch::new(pid));
        if watch.pid != pid {
            *watch = Watch::new(pid);
        }
        if watch.dir.is_none() {
            match resolve_dir(pid) {
                Ok(dir) => watch.dir = dir,
                Err(()) => {
                    gone.push(id);
                    continue;
                }
            }
        }
        let Some(dir) = watch.dir.clone() else {
            continue;
        };
        let Some(sample) = read_sample(&dir) else {
            // The scope ended or moved: re-resolve on the next round.
            watch.dir = None;
            continue;
        };
        for signal in pmw::step(&mut watch.state, &sample) {
            announce(id, pid, &program, &sample, signal);
        }
    }
    if !gone.is_empty() {
        let mut guard = lock();
        for id in gone {
            guard.targets.remove(&id);
        }
    }
    true
}

fn label(program: &str) -> &str {
    if program.is_empty() { "pane" } else { program }
}

fn announce(id: u32, pid: u32, program: &str, sample: &pmw::MemSample, signal: pmw::Signal) {
    match signal {
        pmw::Signal::Warn { near_cap, .. } => {
            let why = if near_cap {
                "near the cap"
            } else {
                "throttled at the soft limit"
            };
            toast(
                format!("{} {why}: {}", label(program), pmw::warn_text(sample)),
                Priority::Notice,
            );
        }
        pmw::Signal::OomKilled { kills } => {
            // The pane's foreground program is read NOW, while it is still alive:
            // by the time the shell exits there is nothing left to inspect.
            let relaunch = foreground_argv(pid).and_then(|argv| pmw::relaunch_command(&argv));
            {
                let mut guard = lock();
                guard.last_oom_ms.insert(id, now_ms());
                match relaunch {
                    Some(cmd) => {
                        guard.relaunch.insert(id, cmd);
                    }
                    None => {
                        guard.relaunch.remove(&id);
                    }
                }
            }
            tracing::info!(target: "thegn::memwatch", pane = id, kills, "pane scope OOM-killed a process");
            toast(
                format!("{}: {}", label(program), pmw::oom_text(sample.max)),
                Priority::Alert,
            );
        }
    }
}

fn toast(message: String, priority: Priority) {
    let guard = lock();
    let Some(notice) = guard.notice.as_ref() else {
        return;
    };
    if let Err(error) = notice.tx.send(RefreshKind::Toast { message, priority }) {
        tracing::debug!(target: "thegn::memwatch", %error, "failed to queue memory toast");
    }
    if let Err(error) = notice.waker.wake() {
        tracing::debug!(target: "thegn::memwatch", %error, "failed to wake for memory toast");
    }
}

/// The argv of the program a pane is actually running: its foreground process,
/// descending through nested shells and sandbox/remote wrappers (the same walk
/// as `PtyPane::foreground_program`, but returning the whole command line).
fn foreground_argv(pane_pid: u32) -> Option<Vec<String>> {
    const MAX_DEPTH: usize = 8;
    let mut pid = pane_pid;
    for _ in 0..MAX_DEPTH {
        let child = crate::platform::proc::foreground_child(pid)?;
        let argv = crate::platform::proc::cmdline(child)?;
        let name = Path::new(argv.first()?)
            .file_stem()?
            .to_string_lossy()
            .into_owned();
        if crate::pane::is_interactive_shell(&name) || crate::pane::is_runtime_wrapper(&name) {
            pid = child;
            continue;
        }
        return Some(argv);
    }
    None
}

/// The exit handler's question: should this exiting pane be restarted?
///
/// Consumes the pane's `oom_kill` mark either way (it belongs to the dead pane)
/// and, on [`pmw::RestartVerdict::Restart`], stamps the cooldown.
pub(crate) fn on_pane_exit(id: u32, exit_code: Option<i32>) -> pmw::RestartVerdict {
    let now = now_ms();
    let mut guard = lock();
    let oom = guard.last_oom_ms.remove(&id);
    let verdict = pmw::restart_verdict(
        AUTO_RESTART,
        exit_code,
        oom,
        guard.last_restart_ms.get(&id).copied(),
        now,
    );
    guard
        .last_restart_ms
        .retain(|_, at| now.saturating_sub(*at) < pmw::RESTART_COOLDOWN_MS);
    if verdict == pmw::RestartVerdict::Restart {
        guard.last_restart_ms.insert(id, now);
    } else {
        guard.relaunch.remove(&id);
    }
    verdict
}

/// Status line for a memory exit.
pub(crate) fn exit_status(verdict: pmw::RestartVerdict) -> Option<&'static str> {
    match verdict {
        pmw::RestartVerdict::Restart => Some("Pane lost to an OOM kill; restarting it…"),
        pmw::RestartVerdict::RateLimited => {
            Some("Pane lost to an OOM kill again within 10 minutes; not auto-restarting")
        }
        pmw::RestartVerdict::NotOom => None,
    }
}

/// Called by materialize for each respawned leaf: hands over the agent relaunch
/// command recorded for dead pane `old`, and moves its restart stamp onto
/// `fresh` so the cooldown follows the pane slot rather than the pane id.
pub(crate) fn take_relaunch(old: u32, fresh: u32) -> Option<String> {
    let mut guard = lock();
    if let Some(at) = guard.last_restart_ms.get(&old).copied() {
        guard.last_restart_ms.insert(fresh, at);
    }
    guard.relaunch.remove(&old)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_consumes_the_oom_mark_and_stamps_the_cooldown() {
        // Ids far from anything another test in this process could use.
        let id = 9_000_001;
        lock().last_oom_ms.insert(id, now_ms());
        assert_eq!(on_pane_exit(id, Some(137)), pmw::RestartVerdict::Restart);
        // The mark is spent: the same exit again is not a memory exit.
        assert_eq!(on_pane_exit(id, Some(137)), pmw::RestartVerdict::NotOom);
        // A second OOM exit inside the cooldown is limited.
        lock().last_oom_ms.insert(id, now_ms());
        assert_eq!(
            on_pane_exit(id, Some(137)),
            pmw::RestartVerdict::RateLimited
        );
    }

    #[test]
    fn clean_exit_with_a_mark_is_not_restarted() {
        let id = 9_000_002;
        lock().last_oom_ms.insert(id, now_ms());
        lock().relaunch.insert(id, "claude --continue".into());
        assert_eq!(on_pane_exit(id, Some(0)), pmw::RestartVerdict::NotOom);
        assert_eq!(take_relaunch(id, id + 1), None);
    }

    #[test]
    fn cooldown_and_plan_follow_the_pane_slot() {
        let (old, fresh) = (9_000_003, 9_000_004);
        lock().last_oom_ms.insert(old, now_ms());
        lock().relaunch.insert(old, "pi --continue".into());
        assert_eq!(on_pane_exit(old, None), pmw::RestartVerdict::Restart);
        assert_eq!(take_relaunch(old, fresh).as_deref(), Some("pi --continue"));
        // The replacement pane inherits the cooldown.
        lock().last_oom_ms.insert(fresh, now_ms());
        assert_eq!(
            on_pane_exit(fresh, Some(137)),
            pmw::RestartVerdict::RateLimited
        );
    }

    #[test]
    fn status_text_only_for_memory_exits() {
        assert!(exit_status(pmw::RestartVerdict::Restart).is_some());
        assert!(exit_status(pmw::RestartVerdict::RateLimited).is_some());
        assert!(exit_status(pmw::RestartVerdict::NotOom).is_none());
    }
}
