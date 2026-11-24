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
//!   state machine in [`thegn_core::pane_memwatch`]. They go through the
//!   notification path ([`crate::notify`], kind `resource_alert`, Alert), so
//!   they reach the inbox, the red flag and the desktop, and name the worktree.
//!   While a pane is at or above the warn threshold its card title carries a
//!   badge ([`badge`]), cleared once usage drops below the re-arm threshold.
//! - **Restart.** When a pane's shell exits and that exit coincides with an
//!   `oom_kill` and the user did not type into the pane after it (that exit is
//!   the user closing the pane), [`on_pane_exit`] tells the exit handler to
//!   keep the pane's leaf for the materialize pipeline. If the pane was running
//!   a recognised agent (cached on every sample, since it is gone by exit time)
//!   with a fixed continue command, [`carry_over`] queues that command for the
//!   fresh shell and [`deliver_relaunches`] types it only once the pane is in
//!   the foreground tab, within [`RELAUNCH_TTL`]. Nothing reconstructed from
//!   the dead process's command line is ever typed. The 10-minute cooldown
//!   follows the pane slot across the id change.
//!
//! The scope-memory notice never types into a PTY, so agents see nothing here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;
use thegn_core::pane_memwatch as pmw;

use crate::hydrate::RefreshKind;

/// How often each pane scope is sampled.
const SAMPLE_EVERY_MS: u64 = 5_000;
const SAMPLE_EVERY: Duration = Duration::from_millis(SAMPLE_EVERY_MS);
/// Where cgroup v2 is mounted.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Automatic restarts are on; there is no config key yet.
const AUTO_RESTART: bool = true;
/// A queued relaunch not delivered (the pane never reached the foreground tab)
/// within this long is dropped.
pub(crate) const RELAUNCH_TTL: Duration = Duration::from_secs(120);
/// The cached agent is only trusted at exit when it was seen this recently.
const AGENT_FRESH: Duration = Duration::from_secs(30);

/// A pane the loop asked the sampler to watch.
struct Target {
    pid: u32,
    program: String,
    /// The owning worktree's display name; empty when unknown.
    worktree: String,
}

/// An observed `oom_kill`: monotonic ms (for the window) and the instant (to
/// compare with the pane's last user input).
#[derive(Clone, Copy)]
struct OomMark {
    ms: u64,
    at: Instant,
}

/// The last recognised agent in a pane, refreshed on every sample.
#[derive(Clone)]
struct AgentCache {
    seen: pmw::AgentSeen,
    /// The agent process's working directory when readable.
    cwd: Option<String>,
    seen_at: Instant,
}

/// A line waiting to be typed into a respawned pane.
struct Pending {
    line: String,
    at: Instant,
}

struct Notice {
    tx: tokio::sync::mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
}

#[derive(Default)]
struct Shared {
    /// Published by the loop: every pane with a local pid.
    targets: HashMap<u32, Target>,
    /// Pane id to its latest `oom_kill` increment.
    oom: HashMap<u32, OomMark>,
    /// Pane id to the monotonic ms of its latest automatic restart.
    last_restart_ms: HashMap<u32, u64>,
    /// Pane id to the last agent recognised in it.
    agent: HashMap<u32, AgentCache>,
    /// Pane id (the dead pane's, then the respawned pane's) to the continue
    /// command that brings its agent back.
    relaunch: HashMap<u32, Pending>,
    /// Pane id to its usage percent while at or above the warn threshold.
    badge: HashMap<u32, u64>,
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

/// Set by the sampler thread when the host has no cgroup v2: `publish` then
/// stops spawning it.
static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// Whether this host has cgroup v2 at all; checked once, on the sampler thread
/// (it is a blocking `stat`, which must stay off the event loop).
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
pub(crate) fn publish(
    table: &HashMap<u32, crate::pane::PtyPane>,
    session: &crate::session::Session,
) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    if UNSUPPORTED.load(Ordering::Relaxed) {
        return;
    }
    let now = now_ms().saturating_add(1);
    let last = LAST.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < SAMPLE_EVERY_MS {
        return;
    }
    LAST.store(now, Ordering::Relaxed);
    let mut worktree_of: HashMap<u32, &str> = HashMap::new();
    for group in &session.worktrees {
        for tab in &group.tabs {
            for pane in tab.center.pane_ids() {
                worktree_of.insert(pane, group.name.as_str());
            }
        }
    }
    let targets: HashMap<u32, Target> = table
        .iter()
        .filter_map(|(id, pane)| {
            Some((
                *id,
                Target {
                    pid: pane.live_pid()?,
                    program: pane.program().to_string(),
                    worktree: worktree_of.get(id).copied().unwrap_or_default().to_string(),
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
        if !supported() {
            UNSUPPORTED.store(true, Ordering::Relaxed);
            RUNNING.store(false, Ordering::Release);
            return;
        }
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
    /// The badge state last handed to the loop.
    badged: bool,
}

impl Watch {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            dir: None,
            state: pmw::WatchState::default(),
            badged: false,
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
    let targets: Vec<(u32, u32, String, String)> = lock()
        .targets
        .iter()
        .map(|(id, target)| {
            (
                *id,
                target.pid,
                target.program.clone(),
                target.worktree.clone(),
            )
        })
        .collect();
    if targets.is_empty() {
        return false;
    }
    watches.retain(|id, _| targets.iter().any(|(live, _, _, _)| live == id));
    let mut gone: Vec<u32> = Vec::new();
    // Applied under one lock at the end of the round.
    let mut agents: Vec<(u32, Option<AgentCache>)> = Vec::new();
    let mut badges: Vec<(u32, Option<u64>)> = Vec::new();
    for (id, pid, program, worktree) in &targets {
        let (id, pid) = (*id, *pid);
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
        // Remember the agent while it is alive: by the time the shell exits the
        // agent is long gone. A *different* foreground program clears it; an
        // idle prompt (agent just killed) keeps the last sighting.
        let mut agent_name: Option<String> = None;
        if let Some((child, argv)) = foreground(pid) {
            if let Some(seen) = pmw::recognise_agent(&argv) {
                agent_name = Some(seen.name.clone());
                let cwd = crate::platform::proc::cwd_of(child)
                    .and_then(|path| path.to_str().map(str::to_string));
                agents.push((
                    id,
                    Some(AgentCache {
                        seen,
                        cwd,
                        seen_at: Instant::now(),
                    }),
                ));
            } else {
                agents.push((id, None));
            }
        }
        let Some(sample) = read_sample(&dir) else {
            // The scope ended or moved: re-resolve on the next round.
            watch.dir = None;
            continue;
        };
        let who = who(worktree, agent_name.as_deref().unwrap_or(program));
        let mut warned = false;
        for signal in pmw::step(&mut watch.state, &sample) {
            warned |= matches!(signal, pmw::Signal::Warn { .. });
            announce(id, &who, worktree, &sample, signal);
        }
        let percent = sample.percent_of_max().unwrap_or(0);
        let badged = watch.state.badged();
        if badged {
            badges.push((id, Some(percent)));
        }
        if badged != watch.badged {
            watch.badged = badged;
            if !badged {
                badges.push((id, None));
                toast(
                    format!("{who}: memory back to {percent}% of the cap"),
                    thegn_core::notification::Priority::Info,
                );
            } else if !warned {
                // The one-shot warning was already spent on a `high` counter
                // rise; the badge still needs a repaint and a word.
                toast(
                    format!("{who} near the memory cap: {}", pmw::warn_text(&sample)),
                    thegn_core::notification::Priority::Notice,
                );
            }
        }
    }
    let now = now_ms();
    let mut guard = lock();
    for id in gone {
        guard.targets.remove(&id);
    }
    for (id, agent) in agents {
        match agent {
            Some(agent) => {
                guard.agent.insert(id, agent);
            }
            None => {
                guard.agent.remove(&id);
            }
        }
    }
    for (id, percent) in badges {
        match percent {
            Some(percent) => {
                guard.badge.insert(id, percent);
            }
            None => {
                guard.badge.remove(&id);
            }
        }
    }
    // Expire what outlived its purpose: marks past the exit window, cooldown
    // stamps, undelivered relaunches, and per-pane caches of panes that are gone.
    let live: HashSet<u32> = guard.targets.keys().copied().collect();
    guard
        .oom
        .retain(|_, mark| now.saturating_sub(mark.ms) <= pmw::OOM_EXIT_WINDOW_MS);
    guard
        .last_restart_ms
        .retain(|_, at| now.saturating_sub(*at) < pmw::RESTART_COOLDOWN_MS);
    guard
        .relaunch
        .retain(|_, pending| pending.at.elapsed() < RELAUNCH_TTL);
    guard.agent.retain(|id, _| live.contains(id));
    guard.badge.retain(|id, _| live.contains(id));
    true
}

fn label(program: &str) -> &str {
    if program.is_empty() { "pane" } else { program }
}

/// `feature/x · claude`: the worktree and the pane's program, for messages.
fn who(worktree: &str, program: &str) -> String {
    let program = label(program);
    if worktree.is_empty() {
        format!("pane {program}")
    } else {
        format!("{worktree} \u{00b7} {program}")
    }
}

fn announce(id: u32, who: &str, worktree: &str, sample: &pmw::MemSample, signal: pmw::Signal) {
    match signal {
        pmw::Signal::Warn { near_cap, .. } => {
            let why = if near_cap {
                "near the memory cap"
            } else {
                "throttled at the soft limit"
            };
            notify_alert(
                id,
                worktree,
                format!("{who} {why}: {}", pmw::warn_text(sample)),
            );
        }
        pmw::Signal::OomKilled { kills } => {
            lock().oom.insert(
                id,
                OomMark {
                    ms: now_ms(),
                    at: Instant::now(),
                },
            );
            tracing::info!(target: "thegn::memwatch", pane = id, kills, "pane scope OOM-killed a process");
            notify_alert(
                id,
                worktree,
                format!("{who}: {}", pmw::oom_text(sample.max)),
            );
        }
    }
}

/// Route an Alert through the notification path (inbox row, red flag, desktop,
/// toast per the user's routing rules). Runs on the sampler thread, so opening
/// the DB is fine. Falls back to the transient channels without a DB, and to a
/// bare toast before the notifier is installed.
fn notify_alert(id: u32, worktree: &str, message: String) {
    let kind = thegn_core::notification::NotificationKind::ResourceAlert.as_str();
    let source = format!("memwatch:{id}");
    let Some(state) = crate::notify::global() else {
        toast(message, thegn_core::notification::Priority::Alert);
        return;
    };
    match thegn_core::db::Db::open() {
        Ok(db) => {
            crate::notify::record(&db, &state, kind, &source, &message, worktree);
        }
        Err(error) => {
            tracing::debug!(target: "thegn::memwatch", %error, "notification db unavailable");
            crate::notify::route(&state, kind, &source, &message, worktree);
        }
    }
}

fn toast(message: String, priority: thegn_core::notification::Priority) {
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
/// as `PtyPane::foreground_program`, but returning the process id and the whole
/// command line). The argv is only inspected for the program's identity.
fn foreground(pane_pid: u32) -> Option<(u32, Vec<String>)> {
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
        return Some((child, argv));
    }
    None
}

/// The usage percent to badge `id`'s card with, while it is near its cap.
pub(crate) fn badge(id: u32) -> Option<u64> {
    lock().badge.get(&id).copied()
}

/// The exit handler's question: should this exiting pane be restarted? Also
/// returns the agent that was running in it (name only), for the status line.
///
/// `last_input` is the pane's last user input ([`crate::pane::PtyPane::output_stamps`]).
/// Input after the `oom_kill` means the user was driving the pane, so an exit
/// is their own `exit`/Ctrl-D (whose status is the last command's, often 137),
/// never an OOM loss.
///
/// Consumes the pane's `oom_kill` mark and agent cache either way and, on
/// [`pmw::RestartVerdict::Restart`], stamps the cooldown and queues the
/// relaunch line.
pub(crate) fn on_pane_exit(
    id: u32,
    exit_code: Option<i32>,
    last_input: Option<Instant>,
) -> (pmw::RestartVerdict, Option<String>) {
    let now = now_ms();
    let mut guard = lock();
    let mark = guard.oom.remove(&id);
    let agent = guard
        .agent
        .remove(&id)
        .filter(|agent| agent.seen_at.elapsed() < AGENT_FRESH);
    let oom_ms = mark
        .filter(|mark| !last_input.is_some_and(|input| input > mark.at))
        .map(|mark| mark.ms);
    let verdict = pmw::restart_verdict(
        AUTO_RESTART,
        exit_code,
        oom_ms,
        guard.last_restart_ms.get(&id).copied(),
        now,
    );
    guard
        .last_restart_ms
        .retain(|_, at| now.saturating_sub(*at) < pmw::RESTART_COOLDOWN_MS);
    if verdict == pmw::RestartVerdict::Restart {
        guard.last_restart_ms.insert(id, now);
        if let Some(agent) = &agent
            && let Some(line) = pmw::typed_relaunch(&agent.seen, agent.cwd.as_deref())
        {
            guard.relaunch.insert(
                id,
                Pending {
                    line,
                    at: Instant::now(),
                },
            );
        }
    } else {
        guard.relaunch.remove(&id);
    }
    (verdict, agent.map(|agent| agent.seen.name))
}

/// Status line for a memory exit; `ran` is the agent that was running.
pub(crate) fn exit_status(verdict: pmw::RestartVerdict, ran: Option<&str>) -> Option<String> {
    let was = ran
        .map(|name| format!(" (was running {name})"))
        .unwrap_or_default();
    match verdict {
        pmw::RestartVerdict::Restart => Some(format!(
            "Pane lost to an OOM kill{was}; restarting it\u{2026}"
        )),
        pmw::RestartVerdict::RateLimited => Some(format!(
            "Pane lost to an OOM kill again within 10 minutes{was}; not auto-restarting"
        )),
        pmw::RestartVerdict::NotOom => None,
    }
}

/// Called by materialize for each respawned leaf: moves the restart stamp from
/// dead pane `old` onto `fresh` so the cooldown follows the pane slot, and
/// hands the queued relaunch line to `fresh` (host panes only; anything else
/// drops it). Typing happens later, in [`deliver_relaunches`].
pub(crate) fn carry_over(old: u32, fresh: u32, host: bool) {
    let mut guard = lock();
    if let Some(at) = guard.last_restart_ms.get(&old).copied() {
        guard.last_restart_ms.insert(fresh, at);
    }
    if let Some(pending) = guard.relaunch.remove(&old)
        && host
    {
        guard.relaunch.insert(fresh, pending);
    }
}

/// Type each queued relaunch line into its respawned pane, but only while that
/// pane is in the foreground tab (`visible`): never into a background tab hours
/// later. Lines older than [`RELAUNCH_TTL`] are dropped. One lock and an
/// `is_empty` check when nothing is queued.
pub(crate) fn deliver_relaunches(
    table: &mut HashMap<u32, crate::pane::PtyPane>,
    visible: &HashSet<u32>,
) {
    let mut guard = lock();
    if guard.relaunch.is_empty() {
        return;
    }
    guard
        .relaunch
        .retain(|_, pending| pending.at.elapsed() < RELAUNCH_TTL);
    let ready: Vec<u32> = guard
        .relaunch
        .keys()
        .copied()
        .filter(|id| visible.contains(id))
        .collect();
    for id in ready {
        let Some(pane) = table.get_mut(&id) else {
            continue;
        };
        let Some(pending) = guard.relaunch.remove(&id) else {
            continue;
        };
        // The Enter-to-relaunch offer is superseded by the typed command.
        pane.set_pending_relaunch(None);
        if pane
            .write_input(format!("{}\r", pending.line).as_bytes())
            .is_err()
        {
            tracing::debug!(
                target: "thegn::memwatch",
                "agent relaunch input not accepted by the fresh pane"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark() -> OomMark {
        OomMark {
            ms: now_ms(),
            at: Instant::now(),
        }
    }

    fn claude() -> AgentCache {
        AgentCache {
            seen: pmw::recognise_agent(&["claude".to_string()]).unwrap(),
            cwd: None,
            seen_at: Instant::now(),
        }
    }

    // Ids far from anything another test in this process could use.

    #[test]
    fn exit_consumes_the_oom_mark_and_stamps_the_cooldown() {
        let id = 9_000_001;
        lock().oom.insert(id, mark());
        assert_eq!(
            on_pane_exit(id, Some(137), None).0,
            pmw::RestartVerdict::Restart
        );
        // The mark is spent: the same exit again is not a memory exit.
        assert_eq!(
            on_pane_exit(id, Some(137), None).0,
            pmw::RestartVerdict::NotOom
        );
        // A second OOM exit inside the cooldown is limited.
        lock().oom.insert(id, mark());
        assert_eq!(
            on_pane_exit(id, Some(137), None).0,
            pmw::RestartVerdict::RateLimited
        );
    }

    #[test]
    fn clean_exit_with_a_mark_is_not_restarted() {
        let id = 9_000_002;
        lock().oom.insert(id, mark());
        lock().agent.insert(id, claude());
        assert_eq!(
            on_pane_exit(id, Some(0), None).0,
            pmw::RestartVerdict::NotOom
        );
        assert!(!lock().relaunch.contains_key(&id));
    }

    #[test]
    fn user_input_after_the_oom_makes_the_exit_the_users_own() {
        let id = 9_000_005;
        let oom = mark();
        lock().oom.insert(id, oom);
        lock().agent.insert(id, claude());
        // The user typed (`exit`) after the kill: status 137 is the last
        // command's, not a lost pane.
        let typed = Some(oom.at + Duration::from_secs(1));
        assert_eq!(
            on_pane_exit(id, Some(137), typed).0,
            pmw::RestartVerdict::NotOom
        );
        assert!(!lock().relaunch.contains_key(&id));
        // Input from BEFORE the kill does not count.
        let id = 9_000_006;
        let oom = mark();
        lock().oom.insert(id, oom);
        let before = oom.at.checked_sub(Duration::from_secs(1));
        assert_eq!(
            on_pane_exit(id, Some(137), before).0,
            pmw::RestartVerdict::Restart
        );
    }

    #[test]
    fn the_cached_agent_and_slot_cooldown_follow_the_respawn() {
        let (old, fresh) = (9_000_003, 9_000_004);
        lock().oom.insert(old, mark());
        lock().agent.insert(old, claude());
        let (verdict, ran) = on_pane_exit(old, None, None);
        assert_eq!(verdict, pmw::RestartVerdict::Restart);
        assert_eq!(ran.as_deref(), Some("claude"));
        carry_over(old, fresh, true);
        assert_eq!(
            lock()
                .relaunch
                .get(&fresh)
                .map(|p| p.line.clone())
                .as_deref(),
            Some("claude --continue")
        );
        // The replacement pane inherits the cooldown.
        lock().oom.insert(fresh, mark());
        assert_eq!(
            on_pane_exit(fresh, Some(137), None).0,
            pmw::RestartVerdict::RateLimited
        );
    }

    #[test]
    fn a_stale_agent_or_a_non_host_pane_gets_no_typed_relaunch() {
        let id = 9_000_007;
        let mut stale = claude();
        stale.seen_at = Instant::now()
            .checked_sub(AGENT_FRESH + Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        lock().oom.insert(id, mark());
        lock().agent.insert(id, stale);
        assert_eq!(on_pane_exit(id, None, None).0, pmw::RestartVerdict::Restart);
        assert!(!lock().relaunch.contains_key(&id));
        // A fresh sighting, but the respawned pane is not a host pane.
        let (old, fresh) = (9_000_008, 9_000_009);
        lock().oom.insert(old, mark());
        lock().agent.insert(old, claude());
        on_pane_exit(old, None, None);
        carry_over(old, fresh, false);
        assert!(!lock().relaunch.contains_key(&old));
        assert!(!lock().relaunch.contains_key(&fresh));
    }

    #[test]
    fn status_text_only_for_memory_exits() {
        assert!(exit_status(pmw::RestartVerdict::Restart, None).is_some());
        assert!(
            exit_status(pmw::RestartVerdict::RateLimited, Some("codex"))
                .unwrap()
                .contains("was running codex")
        );
        assert!(exit_status(pmw::RestartVerdict::NotOom, None).is_none());
    }

    #[test]
    fn messages_name_the_worktree_and_program() {
        assert_eq!(who("app/feat", "claude"), "app/feat \u{00b7} claude");
        assert_eq!(who("", ""), "pane pane");
    }
}
