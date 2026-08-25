//! Host-side terminal-activity state machine for the sidebar's live dots.
//!
//! Activity is measured by scanning `/proc` for processes whose cwd sits under
//! a managed worktree and summing their CPU time, OR'd with a second signal the
//! host injects: recent *unsolicited* agent-pane PTY output (an agent blocked on
//! an API response uses ~0% CPU but keeps redrawing its spinner — CPU alone
//! would flip it to `waiting` mid-turn). A worktree with either signal is
//! `active` (filled white dot — working); one that was active and has gone
//! quiet is `waiting` (filled **red** dot — "stuck, look at me", an *unread*
//! alert); focusing its tab marks it `read` (hollow red dot — seen but still
//! stuck) via [`ack`]. A red worktree is **sticky**: it only leaves red when
//! work *genuinely resumes* — sustained busy over the resume grace, not a
//! one-window blip from a stray watcher or a stale output stamp.
//!
//!   none ─── busy ─────────────────────────────────▶ active
//!   active ─ confirmed quiet, has agent ──────────▶ waiting  (filled red, unread)
//!   active ─ confirmed quiet, NO agent ───────────▶ none     (no dot)
//!   waiting ─ ack(tab) ───────────────────────────▶ read     (hollow red, seen)
//!   waiting/read ─ busy ≥ resume grace ───────────▶ active    (work resumed)
//!   waiting/read ─ quiet, NO agent ───────────────▶ none      (self-heal)
//!
//! Two rules keep the dots honest, both added after the alerts proved noisy;
//! The `activity_step` module holds them and explains them at length:
//!
//! * **Red requires an agent.** A bare shell that ran a command and went quiet
//!   returns to no dot. Any CPU under the worktree path clears the ~3%-of-a-core
//!   threshold — `git status`, `direnv`, an LSP, shell autosuggestions — so
//!   without this gate a plain terminal latched a permanent "look at me" dot.
//! * **Quiet must be confirmed.** Arming red needs two consecutive non-busy
//!   observations plus the grace, so a single quiet poll mid-turn (an agent
//!   thinking at ~0% CPU) cannot flip the dot.
//!
//! Every threshold is configurable — see [`crate::config_activity`].
//!
//! State persists in `~/.thegn/activity.json` (ephemeral, self-healing; kept
//! out of the SQLite DB so frequent polling never contends on the WAL). This
//! used to be the `thegn activity` CLI command; the native host now owns the
//! FSM in-process. Never errors on scan problems — a partial/empty scan just
//! holds the current state (a stuck worktree stays red).

use crate::activity_step;
use crate::config_activity::ActivityConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Serializes in-process load→mutate→save cycles against `activity.json`. The
/// ack path (focusing a red tab) and the ~2s hydrate poll both read-modify-write
/// the same file; without this lock a poll can load a pre-ack snapshot and then
/// overwrite the ack with its stale `waiting` copy (reverting the dot to unread).
/// Cross-process safety still rests on the tmp+rename in [`save`]; this only
/// covers the two writers that live in the same host process.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Monotonic per-write counter so concurrent writers in the same process never
/// collide on the tmp path (pid alone is not enough — the ack + poll threads
/// share a pid). See [`save`].
static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Polls closer together than this reuse the previous scan.
const MIN_SCAN_INTERVAL_SECS: f64 = 1.0;

/// DB `agent` values that are placeholders rather than a real agent: a plain
/// terminal worktree carries one of these, and treating it as an agent is what
/// let a bare shell arm a red "needs you" dot. Shared by every caller that asks
/// "does this worktree have an agent?" — the list used to be copy-pasted in
/// three places and drifted between them.
const AGENT_SENTINELS: [&str; 2] = ["shell", "local"];

/// Whether a DB `agent` value names a real agent rather than the `"shell"` /
/// `"local"` placeholders (or nothing at all).
///
/// Tool drawers (yazi/lazygit/…) are *not* filtered here: that needs the config's
/// `tool_command` lookup, which this crate's callers have and this function does
/// not. Callers gate on both.
pub fn is_real_agent(name: &str) -> bool {
    !name.is_empty() && !AGENT_SENTINELS.contains(&name)
}

/// A managed worktree the scanner should track: `(path, tab_name)`.
#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub worktree: String,
    pub tab: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    version: u32,
    /// Unix seconds of the last *scan* (not ack).
    #[serde(default)]
    polled_at: f64,
    /// Keyed by worktree path.
    #[serde(default)]
    worktrees: BTreeMap<String, Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    tab: String,
    /// Summed CPU counter for the worktree. Still the busy signal on the
    /// remote/bridge path, which reports one number per path rather than per
    /// process; local worktrees use [`Entry::cpu_baselines`].
    #[serde(default)]
    cpu_jiffies: u64,
    /// Last sample's `pid -> jiffies` for this worktree, so the next poll can
    /// take a per-process delta instead of differencing two sums over a
    /// changing process set. See [`cpu_delta`] for why the sum lies. Empty on a
    /// snapshot written before this field existed — the next poll re-baselines.
    #[serde(default)]
    cpu_baselines: BTreeMap<u32, u64>,
    /// Per-PID "has held a core continuously" streaks. Lives beside the
    /// baselines because it is derived from exactly the same two samples; see
    /// [`track_runaways`].
    #[serde(default)]
    hot_pids: BTreeMap<u32, HotPid>,
    state: String, // "none" | "active" | "waiting" | "read"
    #[serde(default)]
    quiet_since: Option<f64>,
    #[serde(default)]
    last_active_at: Option<f64>,
    /// When the current uninterrupted busy streak began (CPU ≥ threshold every
    /// poll since). `None` while idle. Gates `waiting`/`read` → `active` so a
    /// momentary blip can't clear a stuck dot.
    #[serde(default)]
    busy_since: Option<f64>,
}

/// Path to the activity snapshot.
fn state_path() -> PathBuf {
    crate::util::thegn_dir().join("activity.json")
}

/// Read the latest activity states as `tab_name -> state` (`"active"`,
/// `"waiting"`, `"read"`, `"none"`). Empty on any read/parse failure.
pub fn read_states() -> BTreeMap<String, String> {
    read_states_at(&state_path())
}

/// [`read_states`] against an explicit snapshot path (testable, no global env).
pub fn read_states_at(path: &Path) -> BTreeMap<String, String> {
    load(path)
        .worktrees
        .into_values()
        .map(|e| (e.tab, e.state))
        .collect()
}

/// One worktree's full activity entry, keyed by **worktree path** (unlike
/// [`read_states`], which keys by tab name). Exposes the FSM's timestamps so
/// consumers (the attention model) can rank by how long a state has held.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityEntry {
    pub tab: String,
    /// `"none" | "active" | "waiting" | "read"`.
    pub state: String,
    /// When the state went quiet (unix seconds) — set for waiting/read.
    pub quiet_since: Option<f64>,
    /// When the current busy streak began — set while busy.
    pub busy_since: Option<f64>,
    /// Unix seconds of the last poll at which this worktree was busy (CPU or
    /// fresh output). Monotonic while the worktree lives; frozen once it goes
    /// idle; `None` if it has never been active. The recency key for the
    /// sidebar's `Live` sort (most-recently-active first). Distinct from
    /// `busy_since` (start of the *current* streak, cleared on idle).
    pub last_active_at: Option<f64>,
}

/// Read the latest activity entries as `worktree_path -> entry`. Empty on any
/// read/parse failure (same self-healing contract as [`read_states`]).
pub fn read_entries() -> BTreeMap<String, ActivityEntry> {
    read_entries_at(&state_path())
}

/// [`read_entries`] against an explicit snapshot path (testable).
pub fn read_entries_at(path: &Path) -> BTreeMap<String, ActivityEntry> {
    load(path)
        .worktrees
        .into_iter()
        .map(|(wt, e)| {
            (
                wt,
                ActivityEntry {
                    tab: e.tab,
                    state: e.state,
                    quiet_since: e.quiet_since,
                    busy_since: e.busy_since,
                    last_active_at: e.last_active_at,
                },
            )
        })
        .collect()
}

/// Everything one FSM step needs beyond the managed-worktree list.
///
/// A struct rather than more positional parameters: the signature had already
/// grown to five and every new signal (agentness, tuning) would have forced
/// another edit at every call site and in every test literal.
pub struct PollInputs<'a> {
    /// Per-worktree jiffies that **override** the local `/proc` scan for those
    /// paths — remote/provider worktrees whose real processes run in the env,
    /// not on this host (the host gathers them over the resident bridge,
    /// `proc.list`). Local worktrees are absent and scanned as usual.
    pub extra: &'a BTreeMap<String, u64>,
    /// Unix seconds of the last *unsolicited* agent-pane PTY output per worktree
    /// path. A fresh stamp counts as busy alongside CPU, so an agent idling on
    /// network I/O but still animating its spinner never falsely flips to
    /// `waiting`. Worktrees absent here rely on CPU alone.
    pub output_hints: &'a BTreeMap<String, f64>,
    /// `worktree path -> has a real (non-shell, non-tool) agent`. A missing key
    /// means "unknown", which keeps the pre-gate behaviour; `false` is a
    /// positive claim that suppresses red. See
    /// `activity_step::Agentness`.
    pub agents: &'a BTreeMap<String, bool>,
    /// Thresholds and graces. `None` ⇒ [`ActivityConfig::default`].
    pub cfg: Option<&'a ActivityConfig>,
}

/// Empty borrows for [`PollInputs`]'s [`Default`]. `BTreeMap::new` is `const`,
/// so these cost nothing and let call sites pass only the maps they have.
static NO_JIFFIES: BTreeMap<String, u64> = BTreeMap::new();
static NO_HINTS: BTreeMap<String, f64> = BTreeMap::new();
static NO_AGENTS: BTreeMap<String, bool> = BTreeMap::new();

impl Default for PollInputs<'_> {
    fn default() -> Self {
        Self {
            extra: &NO_JIFFIES,
            output_hints: &NO_HINTS,
            agents: &NO_AGENTS,
            cfg: None,
        }
    }
}

impl PollInputs<'_> {
    fn config(&self) -> ActivityConfig {
        self.cfg.cloned().unwrap_or_default()
    }
}

/// Advance the FSM one step over `managed` and persist. Cheap to call on a
/// timer; skips the `/proc` walk if the last scan was under a second ago.
pub fn poll_and_save(managed: &[ManagedWorktree]) {
    poll_and_save_inputs(managed, &PollInputs::default());
}

/// [`poll_and_save`] with injected signals — see [`PollInputs`].
pub fn poll_and_save_inputs(managed: &[ManagedWorktree], inputs: &PollInputs) {
    poll_and_save_at_inputs(&state_path(), managed, inputs, unix_now());
}

/// [`poll_and_save`] against an explicit path/clock (testable).
pub fn poll_and_save_at(path: &Path, managed: &[ManagedWorktree], now: f64) {
    poll_and_save_at_inputs(path, managed, &PollInputs::default(), now);
}

/// [`poll_and_save_inputs`] against an explicit path/clock (testable).
pub fn poll_and_save_at_inputs(
    path: &Path,
    managed: &[ManagedWorktree],
    inputs: &PollInputs,
    now: f64,
) {
    // Serialize against the ack path so a poll can't load a pre-ack snapshot and
    // then save its stale `waiting` copy over a just-written ack (see WRITE_LOCK).
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut snap = load(path);
    if now - snap.polled_at < MIN_SCAN_INTERVAL_SECS {
        return;
    }
    poll(&mut snap, managed, inputs, now);
    save(path, &snap);
}

/// Mark a worktree's tab as read: a `waiting` "look at me" dot (filled red)
/// turns `read` (hollow red) once the user focuses the tab. The dot is *not*
/// cleared — it stays hollow until work genuinely resumes. No-op unless the tab
/// is `waiting`.
pub fn ack(tab: &str) {
    ack_at(&state_path(), tab);
}

/// [`ack`] against an explicit path (testable).
pub fn ack_at(path: &Path, tab: &str) {
    // Serialize the whole load→mutate→save against the poll thread so an ack is
    // never clobbered by a concurrent stale save (see WRITE_LOCK).
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut snap = load(path);
    let mut changed = false;
    for e in snap.worktrees.values_mut() {
        if e.tab == tab && e.state == "waiting" {
            e.state = "read".into();
            changed = true;
        }
    }
    if changed {
        save(path, &snap);
    }
}

/// Drop a deleted worktree's FSM entry. Entries are otherwise carried forward
/// forever (deliberately — dormant workspaces' dots must survive a switch),
/// and `read_states()` re-keys them by TAB name, so a worktree recreated on
/// the same path or tab would inherit the dead one's dot — including a sticky
/// red `waiting` that only 3s of sustained CPU clears. Called from every
/// worktree-forget path.
pub fn forget(worktree_path: &str) {
    forget_at(&state_path(), worktree_path);
}

/// [`forget`] against an explicit path (testable).
pub fn forget_at(path: &Path, worktree_path: &str) {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut snap = load(path);
    if snap.worktrees.remove(worktree_path).is_some() {
        save(path, &snap);
    }
}

/// The settled state a stale running/active dot collapses to at resurrection
/// (no live work; no dot).
pub const SETTLED_STATE: &str = "none";

/// Restore-time stale-state guard (pure). A `"running"`/`"active"` state whose
/// last live signal is older than `grace_ms` collapses to [`SETTLED_STATE`], so a
/// session killed mid-run never resurrects a phantom forever-running dot. Fresh
/// running states and already-settled states (`"waiting"`/`"read"`/`"none"`) pass
/// through unchanged. This is the age-based generalization of the live
/// `RESUME_GRACE_SECS` sticky logic, applied **once** at resurrection; the live
/// `poll` FSM is untouched. Boundary: an age of exactly `grace_ms` is treated
/// as stale (`>=`), matching the `RESUME_GRACE_SECS` convention above.
pub fn coerce_stale(state: &str, age_ms: u64, grace_ms: u64) -> String {
    let running = matches!(state, "active" | "running");
    if running && age_ms >= grace_ms {
        SETTLED_STATE.to_string()
    } else {
        state.to_string()
    }
}

/// Apply [`coerce_stale`] to every persisted entry once at resurrection, so a
/// crash mid-run doesn't resurrect a phantom running/stuck dot. Each entry's age
/// is `now - last_active_at` (falling back to the snapshot's `polled_at` when the
/// entry never recorded an active timestamp). A coerced entry also clears its
/// streak bookkeeping so the next `poll` starts clean; the live FSM then
/// re-derives the true state from fresh CPU deltas. Best-effort: a missing or
/// garbled snapshot is a no-op, and nothing is written unless a state changed.
pub fn coerce_stale_states_at(path: &Path, grace_ms: u64, now: f64) {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut snap = load(path);
    if snap.worktrees.is_empty() {
        return;
    }
    let mut changed = false;
    for e in snap.worktrees.values_mut() {
        let ref_secs = e.last_active_at.unwrap_or(snap.polled_at);
        let age_ms = ((now - ref_secs).max(0.0) * 1000.0) as u64;
        let coerced = coerce_stale(&e.state, age_ms, grace_ms);
        if coerced != e.state {
            e.state = coerced;
            e.quiet_since = None;
            e.busy_since = None;
            changed = true;
        }
    }
    if changed {
        save(path, &snap);
    }
}

/// [`coerce_stale_states_at`] against the default snapshot path + wall clock.
pub fn coerce_stale_states(grace_ms: u64) {
    coerce_stale_states_at(&state_path(), grace_ms, unix_now());
}

/// The CPU advance between two per-PID samples of one worktree.
///
/// Per-PID rather than a single summed counter, because a sum over the
/// *currently alive* set lies in both directions:
///
/// * a process that appeared since the last sample brings its whole accumulated
///   lifetime CPU with it, which reads as one enormous delta — this is why
///   running `ls` or `git status` in a bare shell used to arm a dot;
/// * a busy child that exited makes the sum *drop*, and the old
///   `saturating_sub` floored that at zero — a false idle window in the middle
///   of real work.
///
/// So only PIDs present in **both** samples contribute. A first sighting
/// establishes a baseline and contributes nothing; a vanished PID simply drops
/// out; and a counter that went *backwards* for a known PID means the number was
/// recycled onto a new process, so it re-baselines instead of contributing a
/// bogus advance.
fn cpu_delta(prev: &BTreeMap<u32, u64>, cur: &BTreeMap<u32, u64>) -> f64 {
    let mut sum: u64 = 0;
    for (pid, now_j) in cur {
        if let Some(then_j) = prev.get(pid)
            && now_j >= then_j
        {
            sum += now_j - then_j;
        }
    }
    sum as f64
}

/// Linux `CLK_TCK`: jiffies in one second of one core. `scan_proc` normalizes
/// every platform to this unit (see the macOS and sysinfo arms), so one core-second
/// is 100 wherever this runs.
const JIFFIES_PER_CORE_SEC: f64 = 100.0;

/// How long a PID has been holding a core, and what it has cost so far.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct HotPid {
    /// Continuous seconds at or above the fraction. Resets the moment it drops.
    #[serde(default)]
    held_secs: f64,
    /// Jiffies accumulated across that stretch — the cost to report.
    #[serde(default)]
    jiffies: u64,
    /// Reported already, so a standing runaway is named once rather than every poll.
    #[serde(default)]
    reported: bool,
}

/// A process that has held a core long enough to be worth naming.
#[derive(Debug, Clone, PartialEq)]
pub struct Runaway {
    pub pid: u32,
    /// Continuous seconds spent above the fraction.
    pub held_secs: f64,
    /// CPU consumed over that stretch, in core-seconds.
    pub core_secs: f64,
}

/// Per-PID runaway tracking, folded into the same two samples `cpu_delta` uses.
///
/// The activity FSM already had everything needed except *retention*: it takes a
/// per-PID delta each poll and throws it away, so "this process has burned a core
/// since Tuesday" was unrepresentable. `hot` carries the streak across polls.
///
/// The three cases that keep this honest, mirroring `cpu_delta`'s reasoning:
///   * a PID absent from either sample cannot have a delta — no baseline, no claim;
///   * a counter that went *backwards* means the number was recycled onto a new
///     process, so the streak is dropped rather than inherited by a stranger;
///   * dropping below the fraction for even one poll resets the streak, because
///     the whole signal is *continuity* — a busy build fluctuates, a spin loop
///     does not.
fn track_runaways(
    prev: &BTreeMap<u32, u64>,
    cur: &BTreeMap<u32, u64>,
    hot: &mut BTreeMap<u32, HotPid>,
    wall_secs: f64,
    core_fraction: f64,
    report_after_secs: f64,
) -> Vec<Runaway> {
    let mut out = Vec::new();
    if core_fraction <= 0.0 || report_after_secs <= 0.0 || wall_secs <= 0.0 {
        hot.clear();
        return out;
    }
    // Jiffies that constitute "holding the fraction" over this window.
    let bar = JIFFIES_PER_CORE_SEC * wall_secs * core_fraction;
    for (pid, now_j) in cur {
        let Some(then_j) = prev.get(pid) else {
            hot.remove(pid); // first sighting: baseline only
            continue;
        };
        if now_j < then_j {
            hot.remove(pid); // recycled PID — not the same process
            continue;
        }
        let delta = (now_j - then_j) as f64;
        if delta < bar {
            hot.remove(pid);
            continue;
        }
        let e = hot.entry(*pid).or_default();
        e.held_secs += wall_secs;
        e.jiffies += now_j - then_j;
        if !e.reported && e.held_secs >= report_after_secs {
            e.reported = true;
            out.push(Runaway {
                pid: *pid,
                held_secs: e.held_secs,
                core_secs: e.jiffies as f64 / JIFFIES_PER_CORE_SEC,
            });
        }
    }
    // A PID that vanished stops being tracked; without this the map grows for
    // the life of the snapshot and is serialized on every persist.
    hot.retain(|pid, _| cur.contains_key(pid));
    out
}

/// One scan + state-machine step over every managed worktree. Observation lives
/// here (the elapsed window, the CPU threshold, the `/proc` scan); the decision
/// lives in `activity_step::step`.
fn poll(snap: &mut Snapshot, managed: &[ManagedWorktree], inputs: &PollInputs, now: f64) {
    // Longest-prefix targets so a worktree nested under its repo root
    // (worktree_mode = "in_repo") wins over the home tab.
    let mut targets: Vec<(PathBuf, String)> = managed
        .iter()
        .map(|w| (PathBuf::from(&w.worktree), w.worktree.clone()))
        .collect();
    targets.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));

    let by_pid = scan_proc(&targets);

    let cfg = inputs.config();
    let wall = (now - snap.polled_at).max(0.0);
    let first_poll = snap.polled_at == 0.0;
    let threshold = cfg.active_jiffies_per_sec() * wall;

    // Start from the prior snapshot so worktrees absent from `managed` this
    // cycle (a transient DB-read gap, a not-yet-persisted tab) carry their state
    // forward unchanged instead of being reset to `none`.
    let mut next = std::mem::take(&mut snap.worktrees);
    for w in managed {
        let empty = BTreeMap::new();
        let cur_pids = by_pid.get(&w.worktree).unwrap_or(&empty);
        // Remote/provider worktrees: the bridge's in-env scan is authoritative
        // (it is inserted even when 0, so a stray host process under a bind path
        // can't masquerade as in-env activity). It reports one summed counter per
        // path, not per PID, so that path keeps the aggregate-delta arithmetic.
        let remote_agg = inputs.extra.get(&w.worktree).copied();
        let cur_agg = remote_agg.unwrap_or_else(|| cur_pids.values().sum());

        let prev_known = next.contains_key(&w.worktree);
        let mut e = next.remove(&w.worktree).unwrap_or(Entry {
            tab: w.tab.clone(),
            cpu_jiffies: cur_agg,
            cpu_baselines: cur_pids.clone(),
            hot_pids: BTreeMap::new(),
            state: "none".into(),
            quiet_since: None,
            last_active_at: None,
            busy_since: None,
        });
        e.tab = w.tab.clone(); // tab renames follow the caller

        // A first sighting (or first-ever poll) records a baseline; deltas only
        // mean something from the second reading on.
        if prev_known && !first_poll {
            let delta = match remote_agg {
                Some(agg) => agg.saturating_sub(e.cpu_jiffies) as f64,
                None => cpu_delta(&e.cpu_baselines, cur_pids),
            };
            // Same two samples, different question: `delta` asks whether THIS
            // WORKTREE is working; this asks whether ONE PROCESS is doing nothing
            // but burn a core, forever. Only the local path has PIDs to
            // attribute — the remote bridge reports a single number per path.
            if remote_agg.is_none() {
                for r in track_runaways(
                    &e.cpu_baselines,
                    cur_pids,
                    &mut e.hot_pids,
                    wall,
                    cfg.runaway_core_fraction,
                    cfg.runaway_secs,
                ) {
                    tracing::warn!(
                        target: "thegn::activity",
                        pid = r.pid,
                        worktree = %w.worktree,
                        held_hours = r.held_secs / 3600.0,
                        core_hours = r.core_secs / 3600.0,
                        "a single process has held a CPU core continuously — \
                         likely a runaway (a spin loop, not a build)"
                    );
                }
            }
            let cpu_busy = delta >= threshold && wall > 0.0;
            // Output within the window counts as busy: a working agent redraws
            // its spinner continuously, so a fresh stamp arrives every poll and
            // the busy streak is sustained — exactly what the sticky-red resume
            // gate requires.
            let out_busy = wall > 0.0
                && inputs
                    .output_hints
                    .get(&w.worktree)
                    .is_some_and(|t| activity_step::output_is_fresh(*t, now, wall, &cfg));

            let marks = activity_step::step(
                activity_step::Marks {
                    state: std::mem::take(&mut e.state),
                    quiet_since: e.quiet_since,
                    busy_since: e.busy_since,
                    last_active_at: e.last_active_at,
                },
                activity_step::Signals {
                    busy: cpu_busy || out_busy,
                    agent: activity_step::Agentness::from_map(inputs.agents, &w.worktree),
                },
                &cfg,
                now,
            );
            e.state = marks.state;
            e.quiet_since = marks.quiet_since;
            e.busy_since = marks.busy_since;
            e.last_active_at = marks.last_active_at;
        }
        e.cpu_jiffies = cur_agg;
        e.cpu_baselines = cur_pids.clone();
        next.insert(w.worktree.clone(), e);
    }

    snap.version = 1;
    snap.polled_at = now;
    snap.worktrees = next;
}

/// Sum utime+stime jiffies for every process whose cwd is under each path —
/// the reusable core of the activity scan. Also served over the resident bridge
/// (`proc.list`) so a remote env's *own* processes drive the activity dots.
/// Longest-prefix wins (a nested worktree over its repo root). Linux reads
/// /proc; every other platform (Windows, macOS) reads via sysinfo.
pub fn cpu_jiffies_by_path(paths: &[String]) -> BTreeMap<String, u64> {
    let mut targets: Vec<(PathBuf, String)> = paths
        .iter()
        .map(|p| (PathBuf::from(p), p.clone()))
        .collect();
    targets.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));
    // The bridge wire format is one counter per path; the local FSM wants the
    // per-PID breakdown, so summing here keeps both honest from one scan.
    scan_proc(&targets)
        .into_iter()
        .map(|(wt, pids)| (wt, pids.values().sum()))
        .collect()
}

/// utime+stime jiffies **per PID** per managed worktree, for every process whose
/// cwd is under it. Unreadable PIDs (races, permissions) are skipped silently.
///
/// Per-PID rather than pre-summed so the caller can difference matching
/// processes instead of two sums over a changing set — see [`cpu_delta`].
#[cfg(target_os = "linux")]
fn scan_proc(targets: &[(PathBuf, String)]) -> BTreeMap<String, BTreeMap<u32, u64>> {
    let mut out: BTreeMap<String, BTreeMap<u32, u64>> = BTreeMap::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in proc_dir.flatten() {
        let name = ent.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        let Some((_, wt)) = targets.iter().find(|(p, _)| cwd.starts_with(p)) else {
            continue;
        };
        if let Some(j) = stat_jiffies(Path::new("/proc").join(pid.to_string()).join("stat")) {
            out.entry(wt.clone()).or_default().insert(pid, j);
        }
    }
    out
}

/// macOS: the same contract straight off libproc.
///
/// This ran through `sysinfo` like Windows, which was ~5 syscalls and a multi-KB
/// allocation **per process** for the two fields below. sysinfo constructs each
/// process by reading `KERN_PROCARGS2` (two `sysctl`s plus a `Vec` sized from
/// `ARG_MAX`) *before* consulting the refresh kind, so
/// `ProcessRefreshKind::nothing().with_cwd().with_cpu()` did not buy out of it —
/// and because a fresh `System` is built on every call, every process took that
/// path every time. On ~500 processes that is ~2500-3000 syscalls, at up to 1 Hz
/// (`MIN_SCAN_INTERVAL_SECS`), forever.
///
/// Here it is one `proc_listallpids`, then one `proc_pidinfo` per pid for the
/// cwd, and a second **only for the processes that actually matched a worktree**
/// — the shape `platform/proc.rs` already argues for in its module doc ("sysinfo
/// would refresh the whole process table").
///
/// Deliberately still a full-process-table scan rather than a walk of thegn's
/// own descendants: an agent or daemon that reparents to launchd leaves the tree
/// but keeps its cwd, and its worktree's activity dot must not go dark. Same
/// contract as the Linux `/proc` walk.
///
/// macOS only exposes another process's cwd to the same user (or root), which is
/// exactly the set thegn cares about: every pane it spawns is its own child.
#[cfg(target_os = "macos")]
fn scan_proc(targets: &[(PathBuf, String)]) -> BTreeMap<String, BTreeMap<u32, u64>> {
    let mut out: BTreeMap<String, BTreeMap<u32, u64>> = BTreeMap::new();

    // Size the pid buffer from the kernel's own answer, then re-ask with room to
    // spare: the count can grow between the two calls.
    // SAFETY: the sizing call takes a null buffer and a zero size, which is the
    // documented way to ask `proc_listallpids` how much space it needs.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return out;
    }
    let cap = (count as usize).saturating_add(64);
    let mut pids: Vec<libc::pid_t> = vec![0; cap];
    let bytes = (cap * size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: `pids` owns `cap` pid_t slots and we pass exactly that byte count.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    if n <= 0 {
        return out;
    }
    pids.truncate(n as usize);

    for pid in pids {
        if pid <= 0 {
            continue;
        }
        // Cheap filter first: cwd decides whether this process matters at all.
        let Some(cwd) = macos_cwd(pid) else { continue };
        let Some((_, wt)) = targets.iter().find(|(p, _)| cwd.starts_with(p)) else {
            continue;
        };
        // Only now pay for CPU time.
        // Keyed by pid, not summed: main tracks the live process SET so a
        // pane that exited stops contributing (see the linux arm).
        if let Some(jiffies) = macos_cpu_jiffies(pid) {
            out.entry(wt.clone())
                .or_default()
                .insert(pid as u32, jiffies);
        }
    }
    out
}

/// A process's cwd via `proc_pidinfo(PROC_PIDVNODEPATHINFO)`. `None` for a dead
/// pid or one we may not inspect (other users, protected processes) — the same
/// silent skip the Linux arm gives an unreadable `/proc/<pid>/cwd`.
#[cfg(target_os = "macos")]
fn macos_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    // SAFETY: `info` is a correctly-sized, fully-owned `proc_vnodepathinfo` and
    // the kernel is given its exact size; it never writes past `buffersize`.
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if n < size {
        return None;
    }
    // SAFETY: the call returned a full-size write, so `info` is initialised.
    let info = unsafe { info.assume_init() };
    // `vip_path` is `[[c_char; 32]; 32]`, not `[c_char; 1024]` (a libc
    // workaround for old rustc) — flatten it back to bytes.
    let raw: &[u8] = unsafe {
        std::slice::from_raw_parts(
            info.pvi_cdir.vip_path.as_ptr().cast::<u8>(),
            size_of_val(&info.pvi_cdir.vip_path),
        )
    };
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    (end > 0).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&raw[..end])))
}

/// `mach_timebase_info`, read once. Converts mach absolute units to nanoseconds
/// as `ticks * numer / denom`.
///
/// This is the trap in the whole conversion: `proc_taskinfo`'s CPU fields are in
/// **mach absolute units, not nanoseconds**. On Apple silicon numer/denom is
/// 125/3, so reading them as nanoseconds understates CPU by ~41× — every pane
/// would look idle and no activity dot would ever light.
#[cfg(target_os = "macos")]
fn mach_timebase() -> (u64, u64) {
    // Declared here rather than used from `libc`, whose binding is deprecated in
    // favour of the `mach2` crate — not worth a new dependency for two `u32`s.
    // Same hand-declaration pattern as `platform/qos.rs` and `mem.rs`.
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }

    static TIMEBASE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *TIMEBASE.get_or_init(|| {
        let mut tb = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: fills a fully-owned, correctly-laid-out struct; the kernel
        // writes exactly two u32s.
        let rc = unsafe { mach_timebase_info(&raw mut tb) };
        if rc != 0 || tb.denom == 0 {
            (1, 1) // 1:1 is the x86_64 ratio and a safe fallback.
        } else {
            (u64::from(tb.numer), u64::from(tb.denom))
        }
    })
}

/// A process's total (user + system) CPU as Linux-style jiffies, so the shared
/// `ACTIVE_JIFFIES_PER_SEC` threshold means the same fraction of a core here as
/// it does on Linux. `None` for a dead or inaccessible pid.
#[cfg(target_os = "macos")]
fn macos_cpu_jiffies(pid: libc::pid_t) -> Option<u64> {
    // SAFETY: a correctly-sized, fully-owned `proc_taskinfo` handed to the
    // kernel with its exact size.
    let mut ti = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
    let n =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, ti.as_mut_ptr().cast(), size) };
    if n < size {
        return None;
    }
    // SAFETY: full-size write above.
    let ti = unsafe { ti.assume_init() };
    let ticks = (ti.pti_total_user as u128).saturating_add(ti.pti_total_system as u128);
    let (numer, denom) = mach_timebase();
    // ticks → ns → jiffies (CLK_TCK = 100, so 10ms = 10_000_000ns each).
    let nanos = ticks.saturating_mul(numer as u128) / (denom as u128).max(1);
    Some((nanos / 10_000_000) as u64)
}

/// Windows (and any other non-Linux, non-macOS unix): same contract via sysinfo
/// (per-process cwd + accumulated CPU time) — the PEB read on Windows.
/// sysinfo reports milliseconds; we divide by 10 to convert to jiffy-equivalents
/// (CLK_TCK = 100) so the shared busy threshold — which is expressed in jiffies —
/// means the same fraction of a core here as it does on Linux.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn scan_proc(targets: &[(PathBuf, String)]) -> BTreeMap<String, BTreeMap<u32, u64>> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};
    let mut out: BTreeMap<String, BTreeMap<u32, u64>> = BTreeMap::new();
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cwd(UpdateKind::Always)
            .with_cpu(),
    );
    for proc in sys.processes().values() {
        // Elevated/protected processes (and, on macOS, other users') hide their
        // cwd — skipped, same as unreadable /proc entries on Linux.
        let Some(cwd) = proc.cwd() else { continue };
        let Some((_, wt)) = targets.iter().find(|(p, _)| cwd.starts_with(p)) else {
            continue;
        };
        // sysinfo reports accumulated CPU time in **milliseconds**, but the FSM's
        // busy threshold (`[activity] cpu_percent`) is in Linux jiffies
        // (CLK_TCK = 100, i.e. 10ms each). Convert ms → jiffy-equivalents so the
        // same threshold means the same fraction of a core on both platforms;
        // otherwise Windows would flip 'busy' at 1/10th the intended CPU.
        out.entry(wt.clone())
            .or_default()
            .insert(proc.pid().as_u32(), proc.accumulated_cpu_time() / 10);
    }
    out
}

/// utime+stime from /proc/PID/stat. comm (field 2) may contain spaces and
/// parens, so parse from the LAST ')' — after it, fields resume at 3 (state),
/// so utime/stime (fields 14/15) are tokens 11/12.
#[cfg(target_os = "linux")]
fn stat_jiffies(path: PathBuf) -> Option<u64> {
    let s = std::fs::read_to_string(path).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    let mut it = rest.split_whitespace().skip(11);
    let utime: u64 = it.next()?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some(utime + stime)
}

fn load(path: &Path) -> Snapshot {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic-ish write (tmp + rename) so concurrent readers never see a torn file.
fn save(path: &Path, snap: &Snapshot) {
    let Ok(json) = serde_json::to_string(snap) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Unique per write: pid + a monotonic counter, so two threads in the same
    // process (ack + poll) never truncate+write the *same* tmp file and publish
    // a torn rename.
    let seq = WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{seq}", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    } else {
        // best-effort: drop a failed tmp so it doesn't accumulate.
        let _ = std::fs::remove_file(&tmp);
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tg-act-{tag}-{}.json", std::process::id()))
    }

    /// Poll with injected per-worktree jiffies (the remote-bridge path).
    fn poll_extra(
        path: &Path,
        managed: &[ManagedWorktree],
        extra: &BTreeMap<String, u64>,
        now: f64,
    ) {
        poll_and_save_at_inputs(
            path,
            managed,
            &PollInputs {
                extra,
                ..Default::default()
            },
            now,
        );
    }

    /// Poll with injected output-hint stamps.
    fn poll_hints(
        path: &Path,
        managed: &[ManagedWorktree],
        output_hints: &BTreeMap<String, f64>,
        now: f64,
    ) {
        poll_and_save_at_inputs(
            path,
            managed,
            &PollInputs {
                output_hints,
                ..Default::default()
            },
            now,
        );
    }

    /// Poll with an agentness verdict for every managed worktree.
    fn poll_agents(
        path: &Path,
        managed: &[ManagedWorktree],
        agents: &BTreeMap<String, bool>,
        now: f64,
    ) {
        poll_and_save_at_inputs(
            path,
            managed,
            &PollInputs {
                agents,
                ..Default::default()
            },
            now,
        );
    }

    #[test]
    fn missing_file_is_empty() {
        let path = tmp("missing");
        let _ = std::fs::remove_file(&path);
        assert!(read_states_at(&path).is_empty());
    }

    #[test]
    fn parses_tab_states_from_disk() {
        let path = tmp("parse");
        let json = r#"{"worktrees":{"/wt/a":{"tab":"app/home","state":"waiting","cpu_jiffies":0},
                        "/wt/b":{"tab":"app/feat","state":"read","cpu_jiffies":0}}}"#;
        std::fs::write(&path, json).unwrap();
        let m = read_states_at(&path);
        assert_eq!(m.get("app/home").map(String::as_str), Some("waiting"));
        assert_eq!(m.get("app/feat").map(String::as_str), Some("read"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn forget_drops_only_the_named_worktree_entry() {
        let path = tmp("forget");
        let json = r#"{"worktrees":{"/wt/a":{"tab":"app/fix","state":"waiting","cpu_jiffies":0},
                        "/wt/b":{"tab":"app/feat","state":"read","cpu_jiffies":0}}}"#;
        std::fs::write(&path, json).unwrap();
        forget_at(&path, "/wt/a");
        let m = read_states_at(&path);
        // The deleted worktree's sticky dot is gone — a recreated worktree on
        // the same path/tab starts clean instead of inheriting red `waiting`.
        assert!(!m.contains_key("app/fix"));
        assert_eq!(m.get("app/feat").map(String::as_str), Some("read"));
        // Forgetting an unknown path is a no-op (and never writes).
        forget_at(&path, "/wt/nope");
        assert_eq!(read_states_at(&path).len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_entries_exposes_timestamps_keyed_by_path() {
        let path = tmp("entries");
        let json = r#"{"worktrees":{
            "/wt/a":{"tab":"app/home","state":"waiting","cpu_jiffies":0,"quiet_since":1234.0},
            "/wt/b":{"tab":"app/feat","state":"active","cpu_jiffies":0,"busy_since":2000.0,"last_active_at":2100.0}}}"#;
        std::fs::write(&path, json).unwrap();
        let m = read_entries_at(&path);
        let a = &m["/wt/a"];
        assert_eq!((a.tab.as_str(), a.state.as_str()), ("app/home", "waiting"));
        assert_eq!(a.quiet_since, Some(1234.0));
        assert_eq!(a.busy_since, None);
        // No `last_active_at` in the JSON → `None` (serde default).
        assert_eq!(a.last_active_at, None);
        let b = &m["/wt/b"];
        assert_eq!(b.busy_since, Some(2000.0));
        // `last_active_at` round-trips from disk — the `Live` recency key.
        assert_eq!(b.last_active_at, Some(2100.0));
        // Missing file → empty map; default-path wrapper never panics.
        let _ = std::fs::remove_file(&path);
        assert!(read_entries_at(&path).is_empty());
        let _ = read_entries();
    }

    #[test]
    fn garbled_file_is_empty() {
        let path = tmp("bad");
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(read_states_at(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_states_uses_default_path_without_panicking() {
        let _ = read_states();
    }

    #[test]
    fn poll_records_baseline_then_waiting_then_read() {
        let path = tmp("fsm");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt".into(),
            tab: "app/home".into(),
        }];
        // First poll: baseline, state "none".
        poll_and_save_at(&path, &managed, 1000.0);
        assert_eq!(
            read_states_at(&path).get("app/home").map(String::as_str),
            Some("none")
        );
        // No CPU advance (path doesn't exist) → stays none, never panics.
        poll_and_save_at(&path, &managed, 1100.0);
        let st = read_states_at(&path);
        assert!(st.contains_key("app/home"));

        // Manually mark waiting, then ack turns it to read (hollow, not cleared).
        let mut snap = load(&path);
        if let Some(e) = snap.worktrees.values_mut().next() {
            e.state = "waiting".into();
            e.quiet_since = Some(1100.0);
        }
        save(&path, &snap);
        ack_at(&path, "app/home");
        assert_eq!(
            read_states_at(&path).get("app/home").map(String::as_str),
            Some("read")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn poll_skips_when_called_too_soon() {
        let path = tmp("skip");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/x".into(),
            tab: "t".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);
        // < MIN_SCAN_INTERVAL_SECS later: no rescan, snapshot unchanged.
        poll_and_save_at(&path, &managed, 1000.5);
        assert_eq!(read_states_at(&path).len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    /// Seed an `active` entry with a known low jiffies baseline so the polls
    /// below (no real CPU advance under the bogus path) see `delta < threshold`,
    /// and check the *confirmed*-quiet edge: the first quiet poll only opens the
    /// streak, and only a second one past the grace arms `waiting`.
    #[test]
    fn active_goes_waiting_after_confirmed_quiet() {
        let path = tmp("waiting");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-waiting".into(),
            tab: "app/q".into(),
        }];
        // Baseline poll establishes prev + polled_at.
        poll_and_save_at(&path, &managed, 1000.0);

        // Hand-edit the entry into the `active` state with an old activity
        // timestamp, then poll again past the grace window with no CPU advance.
        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut("/nonexistent/wt-waiting").unwrap();
            e.state = "active".into();
            e.cpu_jiffies = 0;
            e.last_active_at = Some(1000.0);
            e.quiet_since = None;
        }
        save(&path, &snap);

        // First quiet poll: however wide the window, this only *opens* the quiet
        // streak — one quiet observation must never arm red (it used to).
        poll_and_save_at(&path, &managed, 1010.0);
        assert_eq!(
            read_states_at(&path).get("app/q").map(String::as_str),
            Some("active"),
            "a single quiet poll must not arm red"
        );

        // Second quiet poll, now past the grace measured from the streak start.
        poll_and_save_at(&path, &managed, 1020.0);
        let st = read_states_at(&path);
        assert_eq!(st.get("app/q").map(String::as_str), Some("waiting"));

        // `quiet_since` is the *streak start* (the first quiet poll), not the
        // moment of the flip — that is the honest "waiting since" the attention
        // model ranks and acks on.
        let snap = load(&path);
        assert_eq!(
            snap.worktrees["/nonexistent/wt-waiting"].quiet_since,
            Some(1010.0)
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `waiting`/`read` dot is sticky: an idle poll (no CPU advance) never
    /// clears it, and a worktree absent from `managed` carries its state
    /// forward unchanged instead of resetting to `none`.
    #[test]
    fn waiting_is_sticky_and_survives_absence() {
        let path = tmp("sticky");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-sticky".into(),
            tab: "app/s".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);

        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut("/nonexistent/wt-sticky").unwrap();
            e.state = "waiting".into();
            e.cpu_jiffies = 0;
            e.quiet_since = Some(1000.0);
        }
        save(&path, &snap);

        // Idle poll: no CPU advance under the bogus path → stays waiting.
        poll_and_save_at(&path, &managed, 1100.0);
        assert_eq!(
            read_states_at(&path).get("app/s").map(String::as_str),
            Some("waiting")
        );

        // The worktree drops out of `managed` for a cycle → carried forward.
        poll_and_save_at(&path, &[], 1200.0);
        assert_eq!(
            read_states_at(&path).get("app/s").map(String::as_str),
            Some("waiting")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A real CPU burner under a managed worktree drives the sticky-resume
    /// edge: a `read` dot stays red through a single busy window and only flips
    /// to `active` once the busy streak has lasted `RESUME_GRACE_SECS`.
    #[cfg(target_os = "linux")]
    #[test]
    fn read_resumes_active_only_after_sustained_busy() {
        use std::process::Command;
        let wt = std::env::temp_dir().join(format!("tg-act-resume-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let path = tmp("resume");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: wt.to_string_lossy().into_owned(),
            tab: "app/r".into(),
        }];

        let mut child = Command::new("sh")
            .arg("-c")
            .arg("while :; do :; done")
            .current_dir(&wt)
            .spawn()
            .expect("spawn cpu burner");

        // Baseline records the burner's current jiffies, then seed `read`.
        poll_and_save_at(&path, &managed, 1000.0);
        let mut snap = load(&path);
        {
            let e = snap
                .worktrees
                .get_mut(&wt.to_string_lossy().into_owned())
                .unwrap();
            e.state = "read".into();
            e.busy_since = None;
        }
        save(&path, &snap);

        // First busy window (wall=1s): busy=true but the streak is 0s old, so a
        // sticky red dot must NOT clear yet.
        std::thread::sleep(std::time::Duration::from_millis(250));
        poll_and_save_at(&path, &managed, 1001.0);
        assert_eq!(
            read_states_at(&path).get("app/r").map(String::as_str),
            Some("read"),
            "a single busy window must not clear the sticky dot"
        );

        // Keep burning until the streak exceeds RESUME_GRACE_SECS (3s): resume.
        // wall = 1005 - 1001 = 4s ⇒ threshold = 12 jiffies; burn well past it.
        std::thread::sleep(std::time::Duration::from_millis(500));
        poll_and_save_at(&path, &managed, 1005.0);
        assert_eq!(
            read_states_at(&path).get("app/r").map(String::as_str),
            Some("active"),
            "sustained busy must resume the worktree to active"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// An `active` entry still inside the grace window neither flaps to quiet
    /// nor (without CPU advance) re-arms active — it just holds `active`.
    #[test]
    fn active_holds_within_grace() {
        let path = tmp("hold");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-hold".into(),
            tab: "app/h".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);

        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut("/nonexistent/wt-hold").unwrap();
            e.state = "active".into();
            e.cpu_jiffies = 0;
            e.last_active_at = Some(1000.5);
        }
        save(&path, &snap);

        // 1s wall, only 0.5s since last activity → within QUIET_GRACE_SECS.
        poll_and_save_at(&path, &managed, 1001.0);
        let st = read_states_at(&path);
        assert_eq!(st.get("app/h").map(String::as_str), Some("active"));
        let _ = std::fs::remove_file(&path);
    }

    /// Drive the `active` transition (lines 157-159) and the `/proc` scan
    /// (`scan_proc` + `stat_jiffies`) with a real CPU-burning child process
    /// whose cwd lives under a managed worktree directory.
    #[cfg(target_os = "linux")]
    #[test]
    fn real_cpu_burn_marks_active() {
        use std::process::Command;
        let wt = std::env::temp_dir().join(format!("tg-act-burn-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let path = tmp("burn");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: wt.to_string_lossy().into_owned(),
            tab: "app/burn".into(),
        }];

        // A shell that spins, burning CPU, with cwd inside the worktree so
        // scan_proc attributes its jiffies to this worktree.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("while :; do :; done")
            .current_dir(&wt)
            .spawn()
            .expect("spawn cpu burner");

        // Baseline poll records the burner's current jiffies.
        poll_and_save_at(&path, &managed, 1000.0);

        // Let it accumulate CPU, then poll again far enough apart that the
        // scan actually runs (>= MIN_SCAN_INTERVAL_SECS) and the delta clears
        // the active threshold.
        std::thread::sleep(std::time::Duration::from_millis(400));
        poll_and_save_at(&path, &managed, 1001.0);

        let _ = child.kill();
        let _ = child.wait();

        let st = read_states_at(&path);
        // The burner ran for ~400ms wall against a 1s "wall" the FSM was told,
        // so threshold = 3 jiffies and the delta (tens of jiffies) clears it.
        assert_eq!(st.get("app/burn").map(String::as_str), Some("active"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// `stat_jiffies` parses utime+stime from a synthetic /proc/PID/stat line,
    /// including a `comm` field that itself contains spaces and parens.
    #[cfg(target_os = "linux")]
    #[test]
    fn stat_jiffies_parses_fields() {
        let p = std::env::temp_dir().join(format!("tg-stat-{}.txt", std::process::id()));
        // pid (comm) state ppid pgrp ... fields 14/15 (utime/stime) are the
        // 11th/12th whitespace tokens after the last ')'. Build a line where
        // comm = "(weird cmd)" to exercise the rfind(')') logic.
        // After ')': state(3) ppid(4) pgrp(5) session(6) tty(7) tpgid(8)
        // flags(9) minflt(10) cminflt(11) majflt(12) cmajflt(13) utime(14)
        // stime(15) ...
        let line = "42 ((weird cmd)) R 1 1 1 0 -1 0 0 0 0 0 7 11 0 0";
        std::fs::write(&p, line).unwrap();
        assert_eq!(stat_jiffies(p.clone()), Some(18));
        let _ = std::fs::remove_file(&p);
    }

    /// A malformed stat line (no ')') yields None.
    #[cfg(target_os = "linux")]
    #[test]
    fn stat_jiffies_handles_garbage() {
        let p = std::env::temp_dir().join(format!("tg-stat-bad-{}.txt", std::process::id()));
        std::fs::write(&p, "no parens here").unwrap();
        assert_eq!(stat_jiffies(p.clone()), None);
        let _ = std::fs::remove_file(&p);
        // A missing file also yields None.
        assert_eq!(stat_jiffies(PathBuf::from("/no/such/stat")), None);
    }

    /// The non-Linux scanner attributes a child process to the worktree it is
    /// running in.
    ///
    /// This path (sysinfo — `proc_pidinfo` on macOS, the PEB on Windows) had no
    /// test at all: every scanner test was `#[cfg(target_os = "linux")]`, so the
    /// implementation used by BOTH other platforms was unverified. It also rests
    /// on a load-bearing OS claim — that macOS exposes a process's cwd to the
    /// same user — which decides whether activity dots work there at all. If
    /// that stops being true, the feature silently reports every worktree idle,
    /// and this test is what says so.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_scanner_sees_a_child_processes_cwd() {
        let dir = std::env::temp_dir().join(format!("tg-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Resolve symlinks: macOS hands back /private/var… for /var…, and the
        // scanner compares with `starts_with`.
        let dir = std::fs::canonicalize(&dir).unwrap();

        // A child of THIS process, in that cwd — exactly the shape of a pane.
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "i=0; while [ $i -lt 40 ]; do i=$((i+1)); done; sleep 3",
            ])
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn probe child");
        std::thread::sleep(std::time::Duration::from_millis(300));

        let targets = vec![(dir.clone(), "wt/probe".to_string())];
        let sums = scan_proc(&targets);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            sums.contains_key("wt/probe"),
            "a child running in {} must be attributed to its worktree — if this \
             fails on macOS, the OS no longer exposes a same-user process's cwd \
             and activity dots are dead on this platform (got {sums:?})",
            dir.display()
        );
    }

    /// The mach-timebase conversion, which is the one way this scanner can be
    /// subtly wrong rather than obviously broken.
    ///
    /// `proc_taskinfo`'s CPU fields are **mach absolute units**, not nanoseconds.
    /// On Apple silicon numer/denom is 125/3, so reading them as nanoseconds
    /// understates CPU by ~41× — every pane looks idle, no dot ever lights, and
    /// nothing errors. Burn a known amount of CPU in a child and assert the
    /// scanner sees a plausible fraction of it.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_cpu_time_is_converted_from_mach_units_not_read_as_nanos() {
        let (numer, denom) = mach_timebase();
        assert!(numer > 0 && denom > 0, "timebase must be usable");

        let dir = std::env::temp_dir().join(format!("tg-mach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();

        // Burn ~0.5s of CPU **in the shell itself**, then idle so it stays alive
        // to be measured. Pure builtin arithmetic on purpose: a `$(date)`-driven
        // loop forks a process per iteration, so nearly all the CPU lands in
        // short-lived children and the parent measures as idle — which is how an
        // earlier version of this test failed for a reason that had nothing to
        // do with the conversion under test.
        let mut child = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "i=0; while [ $i -lt 250000 ]; do i=$((i+1)); done; sleep 5",
            ])
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn cpu burner");
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let sums = scan_proc(&[(dir.clone(), "wt/burn".to_string())]);
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);

        let jiffies = sums.get("wt/burn").copied().unwrap_or(0);
        // ~0.5s of a busy core is ~50 jiffies. Assert only the ORDER of
        // magnitude — the bug this guards divides by ~41, landing at ~1.
        assert!(
            jiffies >= 15,
            "a child that burned ~0.5s of CPU reported {jiffies} jiffies — too \
             low to be a real conversion (timebase {numer}/{denom}); mach \
             absolute units were probably read as nanoseconds"
        );
    }

    /// Injected jiffies (the remote-bridge `proc.list` path) drive the FSM
    /// independent of the local `/proc` scan: a bogus path with no local
    /// processes goes `active` purely from the `extra` override advancing.
    #[test]
    fn injected_jiffies_drive_active() {
        let path = tmp("inject");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/remote-wt".into(),
            tab: "app/remote".into(),
        }];
        // Baseline poll establishes prev + polled_at with injected jiffies = 0.
        let mut extra = BTreeMap::new();
        extra.insert("/nonexistent/remote-wt".to_string(), 0u64);
        poll_extra(&path, &managed, &extra, 1000.0);
        assert_eq!(
            read_states_at(&path).get("app/remote").map(String::as_str),
            Some("none")
        );
        // A second poll 1s later with a large jiffies advance (delta well over
        // the 3-jiffies/s threshold) flips it to active — no /proc involvement.
        extra.insert("/nonexistent/remote-wt".to_string(), 500u64);
        poll_extra(&path, &managed, &extra, 1001.0);
        assert_eq!(
            read_states_at(&path).get("app/remote").map(String::as_str),
            Some("active")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `worktree path -> stamp` output-hint map for the tests below.
    fn hint(wt: &str, at: f64) -> BTreeMap<String, f64> {
        let mut m = BTreeMap::new();
        m.insert(wt.to_string(), at);
        m
    }

    /// A fresh output hint counts as busy with zero CPU: `none` wakes to
    /// `active` purely from the hint (an agent streaming output but idling on
    /// network I/O).
    #[test]
    fn output_hint_drives_none_to_active() {
        let path = tmp("hint-wake");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/hint-wt";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/hint".into(),
        }];
        // Baseline poll: state none, no signals yet.
        poll_and_save_at(&path, &managed, 1000.0);
        // Second poll with an output stamp inside the elapsed window and no
        // CPU at all (the path has no processes) → active.
        poll_hints(&path, &managed, &hint(wt, 1001.5), 1002.0);
        assert_eq!(
            read_states_at(&path).get("app/hint").map(String::as_str),
            Some("active")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Continuous fresh hints hold `active` across a stretch far longer than
    /// the quiet grace with zero CPU (the mid-turn API wait that used to falsely
    /// flip the dot to `waiting`); once the hints go stale, confirmed quiet
    /// flips it to `waiting` as usual.
    #[test]
    fn output_hint_keeps_active_then_waiting_when_stale() {
        let path = tmp("hint-hold");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/hint-hold-wt";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/hold".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);
        // Seed an active entry as of t=1000.
        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut(wt).unwrap();
            e.state = "active".into();
            e.last_active_at = Some(1000.0);
        }
        save(&path, &snap);

        // 10s of zero CPU (double QUIET_GRACE) but a fresh hint each poll:
        // stays active, last_active_at keeps advancing.
        poll_hints(&path, &managed, &hint(wt, 1004.5), 1005.0);
        poll_hints(&path, &managed, &hint(wt, 1009.5), 1010.0);
        assert_eq!(
            read_states_at(&path).get("app/hold").map(String::as_str),
            Some("active")
        );
        let snap = load(&path);
        assert_eq!(snap.worktrees[wt].last_active_at, Some(1010.0));

        // Hints go stale (stamp older than the freshness window): the first such
        // poll only opens the quiet streak.
        poll_hints(&path, &managed, &hint(wt, 1005.0), 1020.0);
        assert_eq!(
            read_states_at(&path).get("app/hold").map(String::as_str),
            Some("active"),
            "one stale-hint poll must not arm red"
        );
        // A second confirms the quiet and, past the grace, arms it.
        poll_hints(&path, &managed, &hint(wt, 1005.0), 1030.0);
        assert_eq!(
            read_states_at(&path).get("app/hold").map(String::as_str),
            Some("waiting")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Sticky red holds against output hints exactly like CPU: a red dot needs
    /// a *sustained* hint streak (RESUME_GRACE_SECS) to resume active — a
    /// single fresh stamp (e.g. one stray replay burst) is ignored.
    #[test]
    fn red_resumes_only_after_sustained_output() {
        let path = tmp("hint-red");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/hint-red-wt";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/red".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);
        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut(wt).unwrap();
            e.state = "read".into();
            e.quiet_since = Some(990.0);
        }
        save(&path, &snap);

        // Fresh hint starts the busy streak but the grace hasn't elapsed.
        poll_hints(&path, &managed, &hint(wt, 1000.5), 1001.0);
        assert_eq!(
            read_states_at(&path).get("app/red").map(String::as_str),
            Some("read")
        );
        // Streak continues but still under RESUME_GRACE_SECS.
        poll_hints(&path, &managed, &hint(wt, 1001.9), 1002.0);
        assert_eq!(
            read_states_at(&path).get("app/red").map(String::as_str),
            Some("read")
        );
        // Sustained past the grace → genuinely resumed.
        poll_hints(&path, &managed, &hint(wt, 1004.0), 1004.5);
        assert_eq!(
            read_states_at(&path).get("app/red").map(String::as_str),
            Some("active")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Hint freshness, end to end through `poll`: a stamp inside the window
    /// registers as busy, while a too-old stamp, a far-future one (clock
    /// skew/garbage) and one keyed to another worktree do not.
    ///
    /// The *boundaries* are pinned directly on the pure predicate
    /// (`activity_step::output_freshness_*`); what this test adds is that `poll`
    /// wires the right stamp to the right worktree. "Registered as busy" is read
    /// off `busy_since`/`quiet_since` rather than the state string, because the
    /// state is now behind a confirmed-quiet edge and would need a second poll
    /// per case just to observe the same fact.
    #[test]
    fn stale_future_or_foreign_hints_ignored() {
        let path = tmp("hint-bounds");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/hint-bounds-wt";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/bounds".into(),
        }];
        // Reset to a clean `active` with no streak in either direction, so each
        // case starts from the same place. Clearing `quiet_since` matters: a
        // streak left over from the previous case would make the next flip land
        // a poll early and pass for the wrong reason.
        let seed_active = |at: f64| {
            let mut snap = load(&path);
            let e = snap.worktrees.get_mut(wt).unwrap();
            e.state = "active".into();
            e.last_active_at = Some(at);
            e.busy_since = None;
            e.quiet_since = None;
            save(&path, &snap);
        };
        let busy_registered = || load(&path).worktrees[wt].busy_since.is_some();
        poll_and_save_at(&path, &managed, 1000.0);

        // Fresh: wall = 10, window = max(10, ttl) + slack = 11, and age 10.5 is
        // inside it. Strictly inside on purpose — the horizon itself is the
        // first *stale* age (`activity_step::the_freshness_horizon_is_exclusive`),
        // so a case sitting exactly on it would be asserting the opposite.
        seed_active(1000.0);
        poll_hints(&path, &managed, &hint(wt, 999.5), 1010.0);
        assert!(busy_registered(), "a stamp inside the window is busy");

        // Just past the window → not busy.
        seed_active(1010.0);
        poll_hints(&path, &managed, &hint(wt, 1008.9), 1020.0);
        assert!(!busy_registered(), "a stamp past the window is not busy");

        // A far-future stamp can't pin busy either.
        seed_active(1020.0);
        poll_hints(&path, &managed, &hint(wt, 1050.0), 1030.0);
        assert!(!busy_registered(), "a future stamp must not pin busy");

        // A hint for some other worktree leaves this one alone.
        seed_active(1030.0);
        poll_hints(&path, &managed, &hint("/some/other/wt", 1039.5), 1040.0);
        assert!(!busy_registered(), "a foreign hint must not leak across");

        // And with the quiet confirmed across two polls, it does reach `waiting`.
        poll_hints(&path, &managed, &hint("/some/other/wt", 1049.5), 1050.0);
        assert_eq!(
            read_states_at(&path).get("app/bounds").map(String::as_str),
            Some("waiting")
        );
        let _ = std::fs::remove_file(&path);
    }

    // ── per-PID CPU deltas ──────────────────────────────────────────────────

    fn pids(pairs: &[(u32, u64)]) -> BTreeMap<u32, u64> {
        pairs.iter().copied().collect()
    }

    /// A process seen for the first time contributes nothing — it only
    /// establishes a baseline. Without this, a short-lived `ls` or `git status`
    /// dropped its whole accumulated lifetime CPU into one window and armed a
    /// dot on a bare shell.
    #[test]
    fn new_pid_contributes_zero_on_first_sighting() {
        let prev = pids(&[(10, 100)]);
        let cur = pids(&[(10, 100), (11, 5_000)]);
        assert_eq!(cpu_delta(&prev, &cur), 0.0);
        // From the second reading on, its advance counts as normal.
        let next = pids(&[(10, 100), (11, 5_050)]);
        assert_eq!(cpu_delta(&cur, &next), 50.0);
    }

    /// An exited process must not manufacture an idle window. The old summed
    /// counter dropped when a busy child died and `saturating_sub` floored it at
    /// zero, reading as "not busy" in the middle of real work.
    #[test]
    fn exited_pid_never_produces_a_negative_delta() {
        // A big child (pid 11) exits; pid 10 kept working through the window.
        let prev = pids(&[(10, 100), (11, 9_000)]);
        let cur = pids(&[(10, 160)]);
        assert_eq!(
            cpu_delta(&prev, &cur),
            60.0,
            "the survivor's real advance must still register"
        );
    }

    /// A counter that went backwards for a known PID means the number was
    /// recycled onto a different process, so it re-baselines rather than
    /// contributing a bogus advance.
    #[test]
    fn pid_reuse_resets_the_baseline_instead_of_spiking() {
        let prev = pids(&[(10, 9_000)]);
        let cur = pids(&[(10, 3)]);
        assert_eq!(cpu_delta(&prev, &cur), 0.0);
        // And it counts from the new baseline next time.
        assert_eq!(cpu_delta(&cur, &pids(&[(10, 40)])), 37.0);
    }

    #[test]
    fn a_runaway_is_named_only_after_holding_a_core_continuously() {
        // 10s polls, 90% of a core, report after 60s.
        let (wall, frac, after) = (10.0, 0.9, 60.0);
        let mut hot = BTreeMap::new();
        // A process pegging one core: 100 jiffies per second of core time.
        let mut j = 0u64;
        let mut prev = pids(&[(7, j)]);
        for _ in 0..5 {
            j += 1_000; // a full core for 10s
            let cur = pids(&[(7, j)]);
            let out = track_runaways(&prev, &cur, &mut hot, wall, frac, after);
            assert!(out.is_empty(), "must not fire before the duration is met");
            prev = cur;
        }
        // The 6th poll crosses 60s.
        j += 1_000;
        let cur = pids(&[(7, j)]);
        let out = track_runaways(&prev, &cur, &mut hot, wall, frac, after);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pid, 7);
        assert!((out[0].core_secs - 60.0).abs() < 0.001, "{:?}", out[0]);
        // ...and is named ONCE, not every poll thereafter.
        prev = cur;
        j += 1_000;
        let out = track_runaways(&prev, &pids(&[(7, j)]), &mut hot, wall, frac, after);
        assert!(out.is_empty(), "a standing runaway must not re-report");
    }

    #[test]
    fn a_busy_build_that_ever_dips_is_not_a_runaway() {
        // THE distinction: a build pegs a core too. Only *continuity* separates
        // them, so a single sub-threshold poll must reset the streak entirely.
        let (wall, frac, after) = (10.0, 0.9, 60.0);
        let mut hot = BTreeMap::new();
        let mut j = 0u64;
        let mut prev = pids(&[(7, j)]);
        for i in 0..20 {
            // Every 4th window the process is mostly idle, as real work is.
            j += if i % 4 == 3 { 100 } else { 1_000 };
            let cur = pids(&[(7, j)]);
            assert!(
                track_runaways(&prev, &cur, &mut hot, wall, frac, after).is_empty(),
                "a fluctuating load must never be reported"
            );
            prev = cur;
        }
    }

    #[test]
    fn a_recycled_pid_does_not_inherit_a_streak() {
        let (wall, frac, after) = (10.0, 0.9, 20.0);
        let mut hot = BTreeMap::new();
        let prev = pids(&[(7, 5_000)]);
        // Counter went BACKWARDS: the number is on a new process now. Inheriting
        // the streak would blame a stranger for the old one's hour.
        let cur = pids(&[(7, 20)]);
        assert!(track_runaways(&prev, &cur, &mut hot, wall, frac, after).is_empty());
        assert!(
            !hot.contains_key(&7),
            "the streak must be dropped, not carried"
        );
    }

    #[test]
    fn tracking_is_off_when_disabled_and_prunes_dead_pids() {
        let mut hot = BTreeMap::new();
        // 0 disables either half of the rule.
        let prev = pids(&[(7, 0)]);
        let cur = pids(&[(7, 100_000)]);
        assert!(track_runaways(&prev, &cur, &mut hot, 10.0, 0.0, 60.0).is_empty());
        assert!(track_runaways(&prev, &cur, &mut hot, 10.0, 0.9, 0.0).is_empty());
        assert!(hot.is_empty());

        // A pid that pegged a core and then vanished is forgotten, or the map
        // grows for the life of the snapshot and is persisted every write.
        assert!(track_runaways(&prev, &cur, &mut hot, 10.0, 0.9, 60.0).is_empty());
        assert!(hot.contains_key(&7));
        track_runaways(
            &pids(&[(9, 0)]),
            &pids(&[(9, 10)]),
            &mut hot,
            10.0,
            0.9,
            60.0,
        );
        assert!(!hot.contains_key(&7), "a vanished pid must be pruned");
    }

    #[test]
    fn cpu_delta_handles_empty_samples() {
        let empty = BTreeMap::new();
        assert_eq!(cpu_delta(&empty, &empty), 0.0);
        assert_eq!(cpu_delta(&empty, &pids(&[(1, 500)])), 0.0);
        assert_eq!(cpu_delta(&pids(&[(1, 500)]), &empty), 0.0);
    }

    // ── the agent gate, end to end ──────────────────────────────────────────

    #[test]
    fn is_real_agent_rejects_sentinels_and_blanks() {
        assert!(is_real_agent("claude"));
        assert!(is_real_agent("codex"));
        // The placeholders a plain terminal worktree carries.
        assert!(!is_real_agent("shell"));
        assert!(!is_real_agent("local"));
        assert!(!is_real_agent(""));
    }

    /// The reported bug, through the real `poll`: a worktree we know has no
    /// agent goes quiet back to `none` instead of latching a red dot.
    #[test]
    fn shell_worktree_settles_to_none_instead_of_red() {
        let path = tmp("shell-none");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/wt-shell";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/term".into(),
        }];
        let agents = BTreeMap::from([(wt.to_string(), false)]);
        poll_and_save_at(&path, &managed, 1000.0);
        // Seed the `active` a stray `git status` would have armed.
        let mut snap = load(&path);
        snap.worktrees.get_mut(wt).unwrap().state = "active".into();
        snap.worktrees.get_mut(wt).unwrap().last_active_at = Some(1000.0);
        save(&path, &snap);

        poll_agents(&path, &managed, &agents, 1010.0);
        poll_agents(&path, &managed, &agents, 1020.0);
        assert_eq!(
            read_states_at(&path).get("app/term").map(String::as_str),
            Some("none"),
            "a bare terminal must never show a needs-you dot"
        );
    }

    /// An inherited red dot on a worktree that turns out to have no agent heals
    /// rather than staying stuck forever (a recycled tab name, or a dot armed
    /// before the agent map was populated).
    #[test]
    fn inherited_red_heals_for_a_shell_worktree() {
        let path = tmp("shell-heal");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/wt-heal";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/heal".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);
        let mut snap = load(&path);
        snap.worktrees.get_mut(wt).unwrap().state = "waiting".into();
        snap.worktrees.get_mut(wt).unwrap().quiet_since = Some(900.0);
        save(&path, &snap);

        poll_agents(
            &path,
            &managed,
            &BTreeMap::from([(wt.to_string(), false)]),
            1010.0,
        );
        assert_eq!(
            read_states_at(&path).get("app/heal").map(String::as_str),
            Some("none")
        );
    }

    /// A worktree the caller has *not* classified keeps the pre-gate behaviour,
    /// so a session-overlay worktree with no DB row yet never loses its dot.
    #[test]
    fn unclassified_worktree_still_arms_red() {
        let path = tmp("unknown-agent");
        let _ = std::fs::remove_file(&path);
        let wt = "/nonexistent/wt-unknown";
        let managed = vec![ManagedWorktree {
            worktree: wt.into(),
            tab: "app/unk".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);
        let mut snap = load(&path);
        snap.worktrees.get_mut(wt).unwrap().state = "active".into();
        snap.worktrees.get_mut(wt).unwrap().last_active_at = Some(1000.0);
        save(&path, &snap);

        // An agent map that simply doesn't mention this worktree.
        let others = BTreeMap::from([("/some/other/wt".to_string(), false)]);
        poll_agents(&path, &managed, &others, 1010.0);
        poll_agents(&path, &managed, &others, 1020.0);
        assert_eq!(
            read_states_at(&path).get("app/unk").map(String::as_str),
            Some("waiting")
        );
    }

    /// **The reported bug, end to end against the real `/proc` scanner.**
    ///
    /// A bare terminal worktree where the user keeps running short commands —
    /// the `git status` / `ls` / prompt-hook churn every shell produces — must
    /// never light a "needs you" dot. This drives the real scan (not injected
    /// jiffies) so it also covers the per-PID accounting: previously each
    /// newly-appeared process dumped its whole accumulated lifetime CPU into one
    /// window, which armed `active`, and the quiet grace then turned that red.
    #[cfg(target_os = "linux")]
    #[test]
    fn bare_shell_churn_never_arms_a_red_dot() {
        use std::process::Command;
        let wt = std::env::temp_dir().join(format!("tg-act-churn-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let path = tmp("churn");
        let _ = std::fs::remove_file(&path);
        let wt_s = wt.to_string_lossy().into_owned();
        let managed = vec![ManagedWorktree {
            worktree: wt_s.clone(),
            tab: "app/term".into(),
        }];
        // The worktree is a plain terminal: positively no agent.
        let agents = BTreeMap::from([(wt_s, false)]);

        // Churn: each command is a *new* pid that does a little work and exits,
        // exactly like a shell prompt hook or a one-off `git status`.
        let churn = || {
            let _ = Command::new("sh")
                .arg("-c")
                .arg("for i in 1 2 3 4 5; do echo $i >/dev/null; done")
                .current_dir(&wt)
                .status();
        };

        let state = |p: &Path| {
            read_states_at(p)
                .get("app/term")
                .map(String::as_str)
                .unwrap_or("none")
                .to_string()
        };

        // Part 1 — the churn itself is invisible. Each command is born and dies
        // between polls, so per-PID accounting sees no matched pair and the dot
        // never even reaches `active`, let alone red. (Summing the live set
        // instead, as this used to, credited every new pid's whole lifetime CPU
        // to one window and armed the dot.)
        let mut t = 1000.0;
        poll_agents(&path, &managed, &agents, t);
        for _ in 0..5 {
            churn();
            t += 10.0; // well past the quiet grace every step
            poll_agents(&path, &managed, &agents, t);
            assert_eq!(state(&path), "none", "short-lived churn must be invisible");
        }

        // Part 2 — real sustained work DOES light the dot, and then settles back
        // to nothing. This is the reported bug proper: the worktree was busy,
        // went quiet, and must not be left holding a red "look at me".
        let mut burner = Command::new("sh")
            .arg("-c")
            .arg("while :; do :; done")
            .current_dir(&wt)
            .spawn()
            .expect("spawn cpu burner");
        // Two polls: the first baselines the burner's pid, the second sees its
        // advance (threshold = 3 jiffies/s * 1s wall).
        t += 1.0;
        poll_agents(&path, &managed, &agents, t);
        std::thread::sleep(std::time::Duration::from_millis(400));
        t += 1.0;
        poll_agents(&path, &managed, &agents, t);
        assert_eq!(state(&path), "active", "real CPU must light the white dot");

        let _ = burner.kill();
        let _ = burner.wait();

        // Quiet, confirmed across two polls past the grace.
        t += 10.0;
        poll_agents(&path, &managed, &agents, t);
        t += 10.0;
        poll_agents(&path, &managed, &agents, t);
        assert_eq!(
            state(&path),
            "none",
            "a bare terminal that went quiet must clear, not turn red"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// The public, default-path wrappers must run without panicking, covering
    /// `state_path`/`unix_now` plumbing.
    #[test]
    fn default_path_wrappers_dont_panic() {
        let _ = read_states();
        // poll_and_save against the real default path with no managed worktrees:
        // a no-op step that just persists an empty snapshot.
        poll_and_save(&[]);
        // ack of a tab that isn't quiet anywhere: a harmless no-op.
        ack("definitely-not-a-real-tab");
        // unix_now returns a positive, monotonic-ish wall clock.
        assert!(unix_now() > 0.0);
        // coerce_stale_states against the default path: a no-op unless a stale
        // dot exists — never panics.
        coerce_stale_states(600_000);
    }

    // ── restore-time stale-state guard ────────────────────────────────────────

    #[test]
    fn coerce_stale_downgrades_only_stale_running() {
        // Fresh running stays running.
        assert_eq!(coerce_stale("active", 100, 1000), "active");
        assert_eq!(coerce_stale("running", 100, 1000), "running");
        // Stale running downgrades to the settled state.
        assert_eq!(coerce_stale("active", 5000, 1000), SETTLED_STATE);
        assert_eq!(coerce_stale("running", 5000, 1000), SETTLED_STATE);
    }

    #[test]
    fn coerce_stale_passes_non_running_through() {
        // Non-running states are never coerced, however old.
        for st in ["waiting", "read", "none", "quiet", "weird"] {
            assert_eq!(coerce_stale(st, 10_000_000, 1000), st);
        }
    }

    #[test]
    fn coerce_stale_boundary_is_inclusive() {
        // Exactly at the grace threshold counts as stale (>=).
        assert_eq!(coerce_stale("active", 1000, 1000), SETTLED_STATE);
        // One ms under is still fresh.
        assert_eq!(coerce_stale("active", 999, 1000), "active");
    }

    #[test]
    fn coerce_stale_states_downgrades_phantom_but_keeps_fresh() {
        let path = tmp("coerce");
        let _ = std::fs::remove_file(&path);
        // Two worktrees left "active" by a killed session, plus a genuinely-stuck
        // "waiting" dot. polled_at 1000; one entry was last active long ago, the
        // other just before the (simulated) restart.
        let json = r#"{"polled_at":1000.0,"worktrees":{
            "/wt/phantom":{"tab":"app/phantom","state":"active","cpu_jiffies":0,"last_active_at":1000.0},
            "/wt/fresh":{"tab":"app/fresh","state":"active","cpu_jiffies":0,"last_active_at":1990.0},
            "/wt/stuck":{"tab":"app/stuck","state":"waiting","cpu_jiffies":0}
        }}"#;
        std::fs::write(&path, json).unwrap();

        // Restart at now=2000 with a 600s grace: the phantom (1000s old) collapses;
        // the fresh one (10s old) survives; the stuck red dot is never touched.
        coerce_stale_states_at(&path, 600_000, 2000.0);
        let st = read_states_at(&path);
        assert_eq!(st.get("app/phantom").map(String::as_str), Some("none"));
        assert_eq!(st.get("app/fresh").map(String::as_str), Some("active"));
        assert_eq!(st.get("app/stuck").map(String::as_str), Some("waiting"));
    }

    #[test]
    fn coerce_stale_states_no_snapshot_is_noop() {
        let path = tmp("coerce-missing");
        let _ = std::fs::remove_file(&path);
        // Missing file: no write, no panic.
        coerce_stale_states_at(&path, 600_000, 2000.0);
        assert!(!path.exists());
    }

    /// Regression: concurrent in-process ack + poll must never (a) lose the ack
    /// (revert `read`→`waiting`) nor (b) publish a torn JSON file. Before the
    /// fix, the ack and poll threads shared a pid-keyed tmp path and did
    /// unserialized load→mutate→save, so a poll could clobber a fresh ack with
    /// its stale snapshot and two `fs::write`s could interleave into garbage.
    #[test]
    fn concurrent_ack_and_poll_never_lose_ack_or_tear() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        let path = Arc::new(tmp("race"));
        let _ = std::fs::remove_file(&*path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-race".into(),
            tab: "app/race".into(),
        }];
        // Seed a `waiting` (unread red) entry: the poll under a bogus path sees
        // no CPU, so absent a race it would keep the state as-is on every cycle.
        poll_and_save_at(&path, &managed, 1000.0);
        {
            let mut snap = load(&path);
            let e = snap.worktrees.get_mut("/nonexistent/wt-race").unwrap();
            e.state = "waiting".into();
            e.quiet_since = Some(1000.0);
            save(&path, &snap);
        }

        let stop = Arc::new(AtomicBool::new(false));
        // Poll thread: hammers load→mutate→save on the same file with an
        // advancing clock so the MIN_SCAN_INTERVAL guard never short-circuits it.
        //
        // The tiny per-cycle sleep is load-bearing, not politeness. `WRITE_LOCK`
        // is a std Mutex (not fair): a poller that re-acquires immediately after
        // unlocking barges the waiting main thread, which starved this test for
        // minutes on a many-core box. Pausing between cycles still interleaves
        // thousands of poll/ack pairs — which is what the race is about — while
        // letting the acker make progress.
        let poller = {
            let (path, stop, managed) = (path.clone(), stop.clone(), managed.clone());
            std::thread::spawn(move || {
                let mut t = 1001.0;
                while !stop.load(Ordering::Relaxed) {
                    poll_and_save_at(&path, &managed, t);
                    t += 2.0;
                    std::thread::sleep(std::time::Duration::from_micros(50));
                }
            })
        };
        // Ack the tab; with the FSM sticky, once it is `read` it must stay red
        // (never revert to `waiting`), and every read of the file must parse.
        let mut acked_seen = false;
        for _ in 0..2000 {
            ack_at(&path, "app/race");
            let st = read_states_at(&path); // parses => file was never torn
            match st.get("app/race").map(String::as_str) {
                Some("read") => acked_seen = true,
                // Legal transient states, but never a revert *after* we saw read.
                Some("waiting") | Some("active") | Some("none") | None => {
                    assert!(!acked_seen, "ack was lost: state reverted away from `read`");
                }
                other => panic!("unexpected state {other:?}"),
            }
        }
        stop.store(true, Ordering::Relaxed);
        poller.join().unwrap();
        assert!(acked_seen, "ack never took effect at all");
        let _ = std::fs::remove_file(&*path);
    }

    /// The tmp path used by `save` is unique per write (pid + counter), so two
    /// writes never target the same file. Guards the torn-rename half of the fix.
    #[test]
    fn save_tmp_path_is_unique_per_write() {
        let seq0 = WRITE_SEQ.load(Ordering::Relaxed);
        let _ = seq0; // counter is monotonic; two saves advance it.
        let path = tmp("tmp-unique");
        let _ = std::fs::remove_file(&path);
        let snap = Snapshot::default();
        save(&path, &snap);
        save(&path, &snap);
        let seq1 = WRITE_SEQ.load(Ordering::Relaxed);
        assert!(seq1 >= seq0 + 2, "each save must consume a fresh counter");
        let _ = std::fs::remove_file(&path);
    }
}
