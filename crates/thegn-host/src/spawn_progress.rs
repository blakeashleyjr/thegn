//! Spawn-progress cards (THE-213).
//!
//! While a pane waits for its first shell prompt, show *what* the pane is
//! doing instead of a blank cell:
//!
//! ```text
//! resolving environment · 12.4 s
//!   nix print-dev-env 4.2 s
//!   direnv export bash 0.6 s
//! direnv is loading .envrc
//! ```
//!
//! Sources of truth, in order:
//! * thegn's own lifecycle phases (spec resolution → sandbox → fork → prompt)
//! * sampled `/proc` descendants of the pane's shell: argv basenames with
//!   per-process elapsed times (Linux; other platforms show phases only)
//! * a small culprit table mapping common program names to a one-line hint
//!
//! Never shown for instant spawns (grace), never shown after the pane
//! produces output (the drain retires the tracker on first output), and
//! always self-limiting: a 30 s hard bound, and one ticker thread that only
//! exists while something is waiting — the same 0%-idle contract the splash
//! ticker keeps (`thegn//spec/loading`).
//!
//! Redaction: pane argv flows through [`thegn_core::log_redact::redact_argv`]
//! before it is drawn, exactly as the spawn-logging policy demands; nothing
//! else from the environment is ever read.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::panes::Panes;
use crate::platform::proc::{ProcSample, descendants_under};

/// A card appears only once the *current* phase has run this long — instant
/// spawns (fast resolution, immediate prompt) never flash one, even right
/// after a slow phase advanced.
pub(crate) const GRACE: Duration = Duration::from_millis(150);
/// A tracker is dropped this long after its kick (or fork), even with no
/// output: the pane is then assumed live-but-quiet, and the startup
/// watchdog/timeout paths own it from there. Keeps a wedged spawn from
/// pinning a card — and the ticker — forever.
pub(crate) const BOUND: Duration = Duration::from_secs(30);
/// Descendant lines kept per tracker (leaf-most first, sorted by the sampler).
pub(crate) const MAX_PROC_LINES: usize = 3;
/// Args shown per descendant line, then clipped with `…`.
const ARGS_BUDGET: usize = 40;

/// Lifecycle phases, in order. A tracker only ever advances forward
/// (`rank`); a stale advance after a phase moved on is ignored, and `Failed`
/// is terminal (a retried kick replaces the tracker wholesale).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// A materialize kick landed; the spec is resolving off-loop.
    ResolvingEnv,
    /// The resolved spec group targets a sandboxed backend.
    StartingSandbox,
    /// Spec group resolved; the pane's shell is about to be forked.
    StartingShell,
    /// The pane exists; the shell has not printed its prompt yet.
    WaitingForShellPrompt,
    /// Resolution failed; the leaf card holds the reason until retry.
    Failed,
}

impl Phase {
    fn rank(self) -> u8 {
        match self {
            Phase::ResolvingEnv => 0,
            Phase::StartingSandbox => 1,
            Phase::StartingShell => 2,
            Phase::WaitingForShellPrompt => 3,
            Phase::Failed => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Phase::ResolvingEnv => "resolving environment",
            Phase::StartingSandbox => "starting sandbox",
            Phase::StartingShell => "starting shell",
            Phase::WaitingForShellPrompt => "waiting for shell prompt",
            Phase::Failed => "failed",
        }
    }
}

/// What a card renders for one pane: `lines` top-down, then `hint` in the
/// faint style. Equality drives the per-frame `dirty` gate, so it must not
/// embed wall-clock-now (the elapsed values are re-derived each frame).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpawnBlock {
    /// Phase line first, then at most [`MAX_PROC_LINES`] descendant lines.
    pub(crate) lines: Vec<String>,
    /// One-line culprit-table hint (e.g. `direnv is loading .envrc`).
    pub(crate) hint: Option<String>,
}

/// Per-pane tracking state. Born at a materialize kick keyed by the *missing
/// leaf id*, then keyed by the *pane id* once the fork exists (materialize
/// makes the leaf id the pane id, so the move is a re-insert under the same
/// key). Retired by the drain on the pane's first output.
#[derive(Clone, Debug)]
pub struct SpawnProgress {
    phase: Phase,
    /// When the current phase began: elapsed base *and* grace base.
    since: Instant,
    /// When tracking began (kick or fork): the hard-bound base.
    started: Instant,
    /// Latest sampled descendants (leaf-most first, ≤ [`MAX_PROC_LINES`]).
    procs: Vec<ProcSample>,
    /// Why a `Failed` tracker failed (already short; drawing clips further).
    failure: Option<String>,
}

impl SpawnProgress {
    /// A tracker born at the kick of a materialize: phase `ResolvingEnv`.
    pub(crate) fn kicked(now: Instant) -> Self {
        Self {
            phase: Phase::ResolvingEnv,
            since: now,
            started: now,
            procs: Vec::new(),
            failure: None,
        }
    }

    /// Monotonic advance: ignored when `next` ranks at or below the current
    /// phase — a fork's `WaitingForShellPrompt` must never regress a tracker
    /// that already moved past a late spec-landing update.
    pub(crate) fn advance(&mut self, next: Phase, now: Instant) {
        if self.phase != Phase::Failed && next.rank() > self.phase.rank() {
            self.phase = next;
            self.since = now;
        }
    }

    /// Terminal. A retried kick replaces the tracker, so no un-fail path.
    pub(crate) fn fail(&mut self, detail: impl Into<String>, now: Instant) {
        self.phase = Phase::Failed;
        self.since = now;
        self.failure = Some(detail.into());
    }

    /// The latest sampler payload for this tracker's pane. An empty payload
    /// just clears the lines (a vanished descendant tree must not linger).
    pub(crate) fn set_procs(&mut self, mut procs: Vec<ProcSample>) {
        procs.truncate(MAX_PROC_LINES);
        self.procs = procs;
    }

    /// True when the pane is past its hard bound and the tracker must go.
    pub(crate) fn is_past_bound(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) > BOUND
    }

    /// When the current phase began — for the retire-side debug log.
    pub(crate) fn since(&self) -> Instant {
        self.since
    }

    /// The card for this tracker right now, or `None` while the current
    /// phase is younger than [`GRACE`] (instant spawns never flash a card).
    /// `program` is the pane's own program name, when the pane exists.
    pub(crate) fn block(&self, program: Option<&str>, now: Instant) -> Option<SpawnBlock> {
        if now.saturating_duration_since(self.since) < GRACE {
            return None;
        }
        let mut lines = Vec::with_capacity(1 + self.procs.len());
        lines.push(match &self.failure {
            Some(failure) => format!("{}: {}", Phase::Failed.label(), failure),
            None => format!(
                "{} · {}",
                self.phase.label(),
                elapsed_text(now.saturating_duration_since(self.since))
            ),
        });
        for proc in &self.procs {
            lines.push(proc_line(proc, now));
        }
        Some(SpawnBlock {
            lines,
            hint: culprit_hint(program, &self.procs).map(str::to_string),
        })
    }
}

/// Elapsed seconds as `0.4 s` / `2.4 s` under ten, whole above.
fn elapsed_text(elapsed: Duration) -> String {
    if elapsed.as_secs_f32() < 10.0 {
        format!("{:.1} s", elapsed.as_secs_f32())
    } else {
        format!("{} s", elapsed.as_secs())
    }
}

/// One descendant line: `direnv export bash 0.6 s` — argv[0] basename, up to
/// [`ARGS_BUDGET`] chars of the rest (redacted first: this is pane argv, and
/// pane argv never reaches a surface unredacted), then the descendant's age.
fn proc_line(proc: &ProcSample, now: Instant) -> String {
    let argv = thegn_core::log_redact::redact_argv(&proc.argv);
    let name = argv
        .first()
        .map(|first| thegn_core::util::basename(first))
        .unwrap_or_default()
        .to_string();
    let args = if argv.is_empty() {
        String::new()
    } else {
        clip_args(&argv[1..])
    };
    let age = elapsed_text(now.saturating_duration_since(proc.started));
    if args.is_empty() {
        format!("{name} {age}")
    } else {
        format!("{name} {args} {age}")
    }
}

/// `argv[1..]` joined, clipped to [`ARGS_BUDGET`] chars with a `…` marker.
fn clip_args(args: &[String]) -> String {
    let joined = args.join(" ");
    if joined.chars().count() <= ARGS_BUDGET {
        return joined;
    }
    let clipped: String = joined.chars().take(ARGS_BUDGET - 1).collect();
    format!("{clipped}…")
}

/// (program-name set, one-line hint) rows, in priority order: the first row
/// matching *any* candidate name wins, so `direnv` outranks a coincidental
/// `git`. Candidates are argv[0] basenames — the pane's own program first,
/// then the sampled descendants leaf-most (a pane running `ssh` answers
/// `connecting` before any descendant is even sampled).
pub(crate) const CULPRITS: &[(&[&str], &str)] = &[
    (
        &["direnv", "nix-direnv", "nix", "devenv"],
        "direnv is loading .envrc",
    ),
    (
        &["mise", "asdf", "pyenv", "rbenv", "nvm", "conda"],
        "version manager init",
    ),
    (
        &["starship", "oh-my-zsh", "powerlevel10k", "compinit"],
        "prompt framework init",
    ),
    (&["git"], "prompt git status"),
    (
        &["gpg-agent", "ssh-agent", "keychain"],
        "agent startup or key prompt",
    ),
    (&["ssh", "mosh"], "connecting"),
    (&["sudo", "passwd", "askpass"], "waiting for input"),
];

/// First matching row's hint for this pane, or `None` when nothing on the
/// table applies. Two passes: the pane's *own* program is the strongest
/// signal (an `ssh` pane answers `connecting` even while a descendant `git`
/// runs), then the sampled descendants with row priority — `direnv` outranks
/// a coincidental `git`. Candidates are argv[0] basenames.
pub(crate) fn culprit_hint(program: Option<&str>, procs: &[ProcSample]) -> Option<&'static str> {
    if let Some(program) = program {
        for (keys, hint) in CULPRITS {
            if keys.contains(&thegn_core::util::basename(program)) {
                return Some(hint);
            }
        }
    }
    for (keys, hint) in CULPRITS {
        if procs.iter().any(|proc| {
            proc.argv
                .first()
                .is_some_and(|first| keys.contains(&thegn_core::util::basename(first)))
        }) {
            return Some(hint);
        }
    }
    None
}

/// Prune dead trackers and derive the visible cards for the displayed tab.
///
/// Two maps, one lifecycle:
/// * `panes.spawn_progress` — keyed by *pane id* after the fork. Retired by
///   the drain on first output; pruned here when the pane is gone, already
///   printed (belt against a raced retire), or past the hard bound.
/// * `panes.spawn_pending` — keyed by *missing leaf id* between the kick and
///   the fork. Pruned here once the leaf went live through any other path
///   (a raced direct spawn, a restore) or past the bound.
///
/// Only ids in `leaves` — the displayed tab's pane ids, missing leaves and
/// live panes alike — get a card, so the result is display-tab scoped and
/// background tabs can never collide on shared fallback leaf ids. The
/// returned map's identity drives the frame `dirty` gate; the caller owns
/// `model.spawn_hints` because the model owns everything the renderer reads.
pub(crate) fn derive_hints(
    panes: &mut Panes,
    leaves: &std::collections::HashSet<u32>,
    now: Instant,
) -> HashMap<u32, SpawnBlock> {
    let mut hints = HashMap::new();
    panes.spawn_progress.retain(|&pane_id, tracker| {
        if tracker.is_past_bound(now) {
            return false;
        }
        let Some(pane) = panes.table.get(&pane_id) else {
            return false;
        };
        // Belt: the drain retires on first output; if that retire raced a
        // render, the tracker dies here so a card can never outlive output.
        if pane.output_stamps().0.is_some() {
            return false;
        }
        if leaves.contains(&pane_id) {
            if let Some(block) = tracker.block(Some(pane.program()), now) {
                hints.insert(pane_id, block);
            }
        }
        true
    });
    panes.spawn_pending.retain(|&leaf, tracker| {
        if tracker.is_past_bound(now) {
            return false;
        }
        // The leaf went live through another path (a raced direct spawn, a
        // restore): that pane is not waiting on this materialize anymore.
        if panes.table.contains_key(&leaf) {
            return false;
        }
        if leaves.contains(&leaf) {
            if let Some(block) = tracker.block(None, now) {
                hints.insert(leaf, block);
            }
        }
        true
    });
    hints
}

/// Live sampler targets for every waiting pane: `pane id → (shell) pid`.
/// Panes with no host pid are kept (sampling skips them) so the hub's
/// payload map stays pane-keyed for the loop-side merge.
pub(crate) fn sampler_targets(panes: &Panes) -> HashMap<u32, Option<u32>> {
    panes
        .spawn_progress
        .keys()
        .filter_map(|&pane_id| {
            let pid = panes.table.get(&pane_id)?.live_pid();
            Some((pane_id, pid))
        })
        .collect()
}

/// One sampler slot per pane: the pid to walk and the last payload the
/// ticker published. The loop reads payloads through [`SpawnSamplerHub`].
#[derive(Default)]
struct TargetSlot {
    pid: Option<u32>,
    latest: Option<Option<Vec<ProcSample>>>,
}

/// Shared between the loop (target updates, payload pickup) and the ticker
/// thread (sampling). The mutex is held for microseconds per operation and
/// never across the procfs walks themselves.
#[derive(Default)]
pub(crate) struct SpawnSamplerHub {
    targets: std::sync::Mutex<HashMap<u32, TargetSlot>>,
}

impl SpawnSamplerHub {
    /// Replace the target set. Called once per frame while anything waits:
    /// cheap — one short mutex hold on a pane-count-sized map.
    pub(crate) fn set_targets(&self, targets: &HashMap<u32, Option<u32>>) {
        let mut guard = self
            .targets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        guard.retain(|&pane_id, _| targets.contains_key(&pane_id));
        for (&pane_id, &pid) in targets {
            guard.entry(pane_id).or_default().pid = pid;
        }
    }

    /// Sample every live target, off-loop, on the ticker thread. Payloads
    /// are snapshots, never deltas; `None` from `descendants_under` clears a
    /// pane's payload so a vanished tree can't linger as a stale line.
    pub(crate) fn sample_all(&self) {
        let targets: Vec<(u32, Option<u32>)> = {
            let guard = self
                .targets
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            guard
                .iter()
                .map(|(&pane_id, slot)| (pane_id, slot.pid))
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        let mut sampled: HashMap<u32, Option<Vec<ProcSample>>> = HashMap::new();
        for (pane_id, pid) in targets {
            if let Some(pid) = pid {
                sampled.insert(pane_id, descendants_under(pid));
            }
        }
        let mut guard = self
            .targets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (pane_id, payload) in sampled {
            guard.entry(pane_id).or_default().latest = Some(payload);
        }
    }

    /// Take the payloads the ticker published since the last pickup.
    pub(crate) fn take_latest(&self) -> Vec<(u32, Option<Vec<ProcSample>>)> {
        let mut guard = self
            .targets
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        guard
            .iter_mut()
            .filter_map(|(&pane_id, slot)| slot.latest.take().map(|latest| (pane_id, latest)))
            .collect()
    }
}

/// The spawn-progress ticker — the exact idle contract the splash ticker
/// keeps (`crate::loading::ticker`): `set_visible(true)` spawns a
/// short-lived `thegn-spawn` thread that fires `tick` every
/// [`PERIOD`] and parks itself out of existence (no send, no wake) within
/// one period of `false`. A session whose panes all print promptly never
/// has the thread at all.
///
/// `tick` runs on the ticker thread — off-loop — and must stay cheap at
/// 4 Hz: sample the hub's targets, send `RefreshKind::SpawnProgressTick`,
/// pulse the loop's `TerminalWaker`. The benign race from the splash ticker
/// is inherited deliberately: a false→true flip landing exactly as the old
/// thread exits can miss a replacement for one frame; the loop calls
/// `set_visible` after every derive pass while a pane waits, so the next
/// frame respawns it — a 250 ms hiccup, nothing more.
pub(crate) struct SpawnTicker {
    active: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    tick: Arc<dyn Fn() + Send + Sync>,
    period: Duration,
}

impl SpawnTicker {
    /// Fast enough that sub-second descendant phases (direnv, agents) land
    /// as lines while they run; slow enough that the sampler stays
    /// invisible in `top`.
    pub(crate) const PERIOD: Duration = Duration::from_millis(250);

    pub(crate) fn new(tick: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            active: Arc::default(),
            running: Arc::default(),
            tick: Arc::new(tick),
            period: Self::PERIOD,
        }
    }

    #[cfg(test)]
    fn with_period(tick: impl Fn() + Send + Sync + 'static, period: Duration) -> Self {
        Self {
            period,
            ..Self::new(tick)
        }
    }

    /// Declare whether any pane is still waiting for its first prompt.
    /// Cheap and idempotent — the loop calls it every frame after deriving.
    /// Spawns the ticker thread on the false→true edge; the thread parks
    /// itself out of existence (no send, no wake) within one period of
    /// `false`.
    pub(crate) fn set_visible(&self, visible: bool) {
        self.active.store(visible, Ordering::Relaxed);
        if visible
            && self
                .running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let active = self.active.clone();
            let running = self.running.clone();
            let tick = self.tick.clone();
            let period = self.period;
            thegn_core::thread_name::try_spawn_named("thegn-spawn", move || {
                loop {
                    std::thread::sleep(period);
                    if !active.load(Ordering::Relaxed) {
                        running.store(false, Ordering::Release);
                        return;
                    }
                    tick();
                }
            })
            .map(|_| ())
            .unwrap_or_else(|_| self.running.store(false, Ordering::Release));
        }
    }
}

#[cfg(test)]
#[path = "spawn_progress_tests.rs"]
mod tests;
