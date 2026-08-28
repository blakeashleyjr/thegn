//! Startup git heal, off-loop (THE-78).
//!
//! At launch thegn runs a defensive self-heal over the launch dir, each session
//! worktree group and the canonical main checkout: strip a stray `core.worktree`
//! from a shared `.git/config` (which silently retargets every git read — diff
//! panel included — at another worktree) and fast-forward a main checkout left
//! stale by a ref move ([`thegn_core::util::heal_main_checkout_worktree`]). It
//! used to run synchronously on the pre-first-frame launch path — a direct
//! violation of the "no blocking I/O before the first frame" invariant, measured
//! at 14-22 ms on the daily path with a pathological tail (a resync walk over a
//! large checkout) that could hold the first frame for seconds.
//!
//! Now the same sequence runs on its own Background thread (mirrors `crash-scan`)
//! and signals completion through [`HealGate`]. The first git-reading consumer —
//! the initial model hydration — awaits the gate **bounded**
//! ([`BARRIER_TIMEOUT_MS`]): on the daily path the wait returns instantly (the
//! heal is spawned before hydration is), past the timeout the consumer proceeds
//! anyway and the heal's own Model-refresh fixup corrects the pass when it
//! lands. That fixup (send `RefreshKind::Model` + pulse the waker — the
//! `git_watch::spawn_main_checkout_heal` pattern) also repaints when the heal
//! repairs something the first frame already rendered.
//!
//! Spawn failure is fail-safe in the same direction as the cpu-cap wrap: the
//! gate stays uncompleted, waiters fall out at the timeout, launch proceeds.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;

/// Bound on how long the first git-reading consumer waits for the heal.
pub(crate) const BARRIER_TIMEOUT_MS: u64 = 250;

/// Completion signal for the startup heal: a flag + `Condvar`.
pub(crate) struct HealGate {
    done: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl HealGate {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            done: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
        })
    }

    /// True when the heal completed (healed or not) before the deadline. Never
    /// blocks unboundedly: a lost/spawn-failed thread costs one timeout.
    pub(crate) fn wait_bounded(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let (guard, _) = self
                .cv
                .wait_timeout(done, remaining)
                .unwrap_or_else(|e| e.into_inner());
            done = guard;
        }
        true
    }

    fn complete(&self) {
        *self.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }
}

/// Spawn the heal on its own Background thread. The thread body is the old
/// pre-frame heal sequence (launch dir + each worktree group + the canonical
/// checkout probe), unchanged apart from running off-loop; it completes `gate`
/// when done and — if anything healed — sends `RefreshKind::Model` + pulses the
/// waker so the loop repaints the repaired state (the
/// `git_watch::spawn_main_checkout_heal` fixup pattern).
pub(crate) fn spawn(
    cwd: PathBuf,
    group_paths: Vec<PathBuf>,
    start: Instant,
    waker: TerminalWaker,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    gate: std::sync::Arc<HealGate>,
) {
    let spawned = std::thread::Builder::new()
        .name("startup-heal".into())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let (healed, _roots) = run(&cwd, &group_paths, start);
            gate.complete();
            if healed {
                let _ = refresh_tx.send(crate::hydrate::RefreshKind::Model); // best-effort: consumer may be gone
                let _ = waker.wake(); // best-effort: loop may be gone
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(
            target: "thegn::startup",
            error = %e,
            "failed to spawn the startup-heal thread — hydration proceeds at the gate timeout"
        );
    }
}

/// The heal sequence itself, unit-tested hermetically. Returns
/// `(anything_healed, roots_probed)`.
fn run(cwd: &Path, group_paths: &[PathBuf], start: Instant) -> (bool, usize) {
    let t0 = Instant::now();
    // Defensive self-heal: strip any stray `core.worktree` that leaked into a
    // main checkout's shared `.git/config` (which silently retargets every git
    // read — diff panel included — at another worktree), and fast-forward a main
    // checkout left stale by a ref move. No-ops on linked worktrees (whose
    // `.git` is a file). Cheap, once per launch over the roots below.
    let mut healed = thegn_core::util::heal_main_checkout_worktree(cwd);
    let mut roots = 1usize;
    for g in group_paths {
        roots += 1;
        healed |= thegn_core::util::heal_main_checkout_worktree(g);
    }
    // Also heal the canonical checkout that OWNS the shared `.git`, even when we
    // launched from a linked worktree (its path is usually not among cwd or the
    // session worktree paths). `--git-common-dir` resolves to `<canonical>/.git`,
    // whose parent is the main checkout — which is exactly where a stray
    // `core.worktree` actually lands. Scrubbed git env so this probe is itself safe.
    // off-loop: inside the startup-heal thread — see clippy.toml
    #[expect(clippy::disallowed_methods)]
    if let Some(common_parent) = thegn_core::util::git_cmd(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| PathBuf::from(s.trim()))
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        roots += 1;
        healed |= thegn_core::util::heal_main_checkout_worktree(&common_parent);
    }
    tracing::info!(
        target: "thegn::startup",
        since_start_ms = start.elapsed().as_millis() as u64,
        heal_ms = t0.elapsed().as_millis() as u64,
        roots,
        healed,
        "startup git heal"
    );
    (healed, roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_completes_and_wait_returns_true() {
        let gate = HealGate::new();
        let g = std::sync::Arc::clone(&gate);
        std::thread::spawn(move || g.complete());
        assert!(gate.wait_bounded(Duration::from_millis(50)));
    }

    #[test]
    fn wait_times_out_on_uncompleted_gate() {
        let gate = HealGate::new();
        // Uses the param, NOT BARRIER_TIMEOUT_MS — no slow tests.
        assert!(!gate.wait_bounded(Duration::from_millis(5)));
        // A late complete() still lands and is observable afterwards.
        gate.complete();
        assert!(gate.wait_bounded(Duration::from_millis(5)));
    }

    #[test]
    fn run_on_non_repo_dir_probes_one_root_and_heals_nothing() {
        let tmp = std::env::temp_dir().join(format!("thegn-startup-heal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // No `.git`, no groups: the heal no-ops on the launch dir and the
        // common-dir probe fails, so nothing heals and only one root is probed.
        let (healed, roots) = run(&tmp, &[], Instant::now());
        assert!(!healed);
        assert_eq!(roots, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn run_counts_group_roots_even_outside_a_repo() {
        let tmp = std::env::temp_dir().join(format!("thegn-startup-heal-g-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let group = tmp.join("wt-a");
        std::fs::create_dir_all(&group).unwrap();
        let (healed, roots) = run(&tmp, std::slice::from_ref(&group), Instant::now());
        assert!(!healed);
        assert_eq!(roots, 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
