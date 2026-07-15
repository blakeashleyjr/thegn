//! `LoadingTracker` — the loop-side owner of the per-tab loading-step map.
//!
//! Producers (the materialize observer, the eager provisioner, worktree
//! creation, the spec drain) send whole stamp-free `Vec<LoadStep>`s; the
//! tracker is the ONE insert path, and it stamps/carries/freezes each step's
//! `started_at` across full-Vec replacement so per-step elapsed time survives
//! however chatty a producer is. Matching is label-first with a unique-kind
//! fallback, so a step that refines its label (`"container"` →
//! `"container (podman)"`) keeps its clock.
//!
//! Reads go through `Deref<Target = HashMap>` — the pure helpers in
//! `loading/mod.rs` keep taking `&HashMap` untouched. Writes MUST go through
//! [`LoadingTracker::set`] / [`LoadingTracker::remove`] /
//! [`LoadingTracker::rename`] (no `DerefMut`), which is what makes the
//! timing guarantee hold.

use std::collections::HashMap;
use std::time::Instant;

use crate::chrome::{LoadStep, StepKind, StepState};

/// A `loading_state` key: `(group_name, tab_index)`.
pub(crate) type Key = (String, usize);

#[derive(Debug, Default)]
pub(crate) struct LoadingTracker {
    map: HashMap<Key, Vec<LoadStep>>,
    /// Per-tab env override for the splash's context block: the host the
    /// creation wizard actually chose, captured at `TabOpened` before the DB
    /// row is written. `loading_context` prefers this over the (not-yet-set)
    /// `effective_env` lookup, so a local pick against a provider ambient
    /// default doesn't briefly show the provider. Cleared with the steps.
    env: HashMap<Key, String>,
}

impl std::ops::Deref for LoadingTracker {
    type Target = HashMap<Key, Vec<LoadStep>>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

/// Find `s`'s predecessor in `old`: exact label match first; else, when `s`
/// carries a real kind, the unique step of that kind (a relabel). Ambiguous
/// kind matches (two `Provision` steps) are refused — better a reset clock
/// than the wrong step's clock.
fn matching<'a>(old: &'a [LoadStep], s: &LoadStep) -> Option<&'a LoadStep> {
    old.iter().find(|p| p.label == s.label).or_else(|| {
        if s.kind == StepKind::Other {
            return None;
        }
        let mut it = old.iter().filter(|p| p.kind == s.kind);
        match (it.next(), it.next()) {
            (Some(p), None) => Some(p),
            _ => None,
        }
    })
}

impl LoadingTracker {
    /// Replace `key`'s steps, carrying timing across the replacement:
    /// same-state steps keep their stamp, a step that just went `Active` is
    /// stamped now, and a step leaving `Active` (→ `Done`/`Failed`) keeps its
    /// frozen stamp so the splash can show how long it took.
    pub(crate) fn set(&mut self, key: Key, mut steps: Vec<LoadStep>) {
        let now = Instant::now();
        let old = self.map.get(&key);
        for s in &mut steps {
            let prev = old.and_then(|o| matching(o, s));
            match (prev, s.state) {
                // No transition: both stamps ride along.
                (Some(p), st) if p.state == st => {
                    s.started_at = p.started_at;
                    s.took = p.took;
                }
                // (Re)entered Active: fresh clock, no finished duration.
                (_, StepState::Active) => {
                    s.started_at = Some(now);
                    s.took = None;
                }
                // Left Active for a terminal state: freeze the start stamp and
                // measure how long the step ran (the elapsed column's value
                // for finished steps).
                (Some(p), StepState::Done | StepState::Failed) if p.state == StepState::Active => {
                    s.started_at = p.started_at;
                    s.took = p.started_at.map(|t| now.duration_since(t));
                }
                (Some(p), _) => {
                    s.started_at = p.started_at;
                    s.took = p.took;
                }
                (None, _) => {}
            }
        }
        self.map.insert(key, steps);
    }

    pub(crate) fn remove(&mut self, key: &Key) -> Option<Vec<LoadStep>> {
        self.env.remove(key);
        self.map.remove(key)
    }

    /// Move an entry to a new key verbatim (worktree-creation settles its
    /// optimistic name); stamps ride along untouched.
    pub(crate) fn rename(&mut self, old: &Key, new: Key) {
        if let Some(env) = self.env.remove(old) {
            self.env.insert(new.clone(), env);
        }
        if let Some(steps) = self.map.remove(old) {
            self.map.insert(new, steps);
        }
    }

    /// Record the host env the creation wizard chose for `key`, so the splash's
    /// context block reflects the actual pick before the DB row exists. Empty
    /// is a no-op (no override → fall back to the DB lookup).
    pub(crate) fn set_env(&mut self, key: Key, env: &str) {
        if env.trim().is_empty() {
            return;
        }
        self.env.insert(key, env.to_string());
    }

    /// The wizard-chosen env override for `key`, if any.
    pub(crate) fn env_for(&self, key: &Key) -> Option<&str> {
        self.env.get(key).map(String::as_str)
    }

    /// The splash context block (env / placement / sandbox / connect / workdir)
    /// for the active tab: empty when there are no load steps, else resolved via
    /// [`loading_context`], preferring this tracker's wizard-chosen env (`akey`)
    /// over the DB `effective_env` — which isn't written until the create
    /// worker's Register step, so a local pick against a provider ambient default
    /// would otherwise briefly show the provider. Called from the event loop; the
    /// resolution lives here (not inline in run.rs) to keep that god-file flat.
    pub(crate) fn load_context(
        &self,
        steps: &[LoadStep],
        worktree: &str,
        cfg: &thegn_core::config::Config,
        akey: Option<&Key>,
    ) -> Vec<(String, String)> {
        if steps.is_empty() {
            return Vec::new();
        }
        let selected = akey.and_then(|k| self.env_for(k));
        loading_context(cfg, worktree, selected)
    }

    /// Provisioning finished; only the shell attach remains. Advance the
    /// existing plan — every non-shell step goes `Done`, the trailing shell
    /// step goes `Active` — rather than replacing it, so a rich backend-aware
    /// plan keeps its rows (and their timings). A missing/empty/short entry
    /// falls back to the classic `[sandbox, container (backend), shell]`
    /// shape so the splash never loses its story.
    pub(crate) fn advance_to_shell(&mut self, key: Key, backend: &str) {
        let steps = match self.map.get(&key) {
            Some(cur) if cur.len() >= 2 && crate::loading::is_shell_wait(cur) => cur
                .iter()
                .map(|s| {
                    let mut s = s.clone();
                    s.state = if s.kind == StepKind::Shell || s.label == "shell" {
                        StepState::Active
                    } else {
                        StepState::Done
                    };
                    s
                })
                .collect(),
            _ => vec![
                LoadStep::done("sandbox").with_kind(StepKind::Resolve),
                LoadStep::done(format!("container ({backend})")).with_kind(StepKind::Create),
                LoadStep::active("shell").with_kind(StepKind::Shell),
            ],
        };
        self.set(key, steps);
    }

    /// A bring-up failed: mark the step that was running `Failed` and attach
    /// the error as its sub-line. Falls back to a single failed step when the
    /// tab has no live plan (the classic `[sandbox failed]` shape).
    pub(crate) fn fail_active(&mut self, key: Key, err: &str) {
        let steps = match self.map.get(&key) {
            Some(cur) if !cur.is_empty() => {
                let at = cur
                    .iter()
                    .position(|s| s.state == StepState::Active)
                    .or_else(|| cur.iter().position(|s| s.state == StepState::Pending))
                    .unwrap_or(cur.len() - 1);
                cur.iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let mut s = s.clone();
                        if i == at {
                            s.state = StepState::Failed;
                            s = s.with_detail(err);
                        }
                        s
                    })
                    .collect()
            }
            _ => vec![
                LoadStep::failed("sandbox")
                    .with_kind(StepKind::Resolve)
                    .with_detail(err),
            ],
        };
        self.set(key, steps);
    }
}

/// `(key, value)` facts about where a worktree's pane is coming up, for the
/// loading screen's context block: env, placement, provider/sandbox, connect
/// mode, shell strategy, workdir. Loop-safe (a DB read + pure config resolution,
/// no network/subprocess). Empty for a plain local env (nothing to show).
/// (Relocated out of the `agent` god-file — it's loading-screen state.)
pub(crate) fn loading_context(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    selected: Option<&str>,
) -> Vec<(String, String)> {
    use std::path::{Path, PathBuf};
    use thegn_core::db::Db;
    use thegn_core::placement::Placement;
    use thegn_core::remote::GitLoc;
    use thegn_core::repo;
    use thegn_core::store::WorkspaceStore;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    // Prefer an explicit override (the creation wizard's pick, captured before
    // the DB env row is written) over the persisted `effective_env`.
    let selected = selected.map(str::to_owned).or_else(|| {
        Db::open()
            .ok()
            .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()))
    });
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), selected.as_deref());
    if env.placement.is_local() {
        return Vec::new();
    }
    let mut out = vec![
        ("env".to_string(), env.name.clone()),
        ("placement".to_string(), env.placement.label()),
    ];
    if let Placement::Provider(_) = &env.placement
        && let Some(ec) = cfg.env.get(&env.name)
    {
        let pc = &ec.provider;
        if !pc.provider.trim().is_empty() {
            out.push(("provider".to_string(), pc.provider.clone()));
        }
        if let Some(id) = crate::provider_factory::provider_sandbox_name(cfg, worktree, &env.name)
            .filter(|s| !s.is_empty())
        {
            out.push(("sandbox".to_string(), id));
        }
        out.push((
            "connect".to_string(),
            format!("{:?}", pc.connect).to_lowercase(),
        ));
        let wd = pc.sync_workdir();
        if !wd.trim().is_empty() {
            out.push(("workdir".to_string(), wd));
        }
    }
    let strategy = format!("{:?}", env.sandbox.home.strategy).to_lowercase();
    out.push(("shell".to_string(), strategy));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Key {
        ("wt".to_string(), 0)
    }

    #[test]
    fn stamps_on_pending_to_active_and_carries_across_replacement() {
        let mut t = LoadingTracker::default();
        t.set(key(), vec![LoadStep::active("a"), LoadStep::pending("b")]);
        let first = t.get(&key()).unwrap()[0]
            .started_at
            .expect("active stamped");
        assert!(
            t.get(&key()).unwrap()[1].started_at.is_none(),
            "pending unstamped"
        );
        // Whole-Vec replacement, same states: the stamp carries.
        t.set(key(), vec![LoadStep::active("a"), LoadStep::pending("b")]);
        assert_eq!(t.get(&key()).unwrap()[0].started_at, Some(first));
        // b goes active: stamped fresh; a goes done: stamp frozen.
        t.set(key(), vec![LoadStep::done("a"), LoadStep::active("b")]);
        let steps = t.get(&key()).unwrap();
        assert_eq!(steps[0].started_at, Some(first), "done freezes the stamp");
        assert!(steps[1].started_at.is_some(), "newly active stamped");
    }

    #[test]
    fn carries_across_a_kind_tagged_relabel() {
        let mut t = LoadingTracker::default();
        t.set(
            key(),
            vec![LoadStep::active("container").with_kind(StepKind::Create)],
        );
        let stamp = t.get(&key()).unwrap()[0].started_at;
        t.set(
            key(),
            vec![LoadStep::active("container (podman)").with_kind(StepKind::Create)],
        );
        assert_eq!(
            t.get(&key()).unwrap()[0].started_at,
            stamp,
            "unique-kind match carries the clock through a relabel"
        );
    }

    #[test]
    fn ambiguous_kind_match_is_refused() {
        let mut t = LoadingTracker::default();
        t.set(
            key(),
            vec![
                LoadStep::done("clone").with_kind(StepKind::Provision),
                LoadStep::active("nix").with_kind(StepKind::Provision),
            ],
        );
        // A relabeled step whose kind matches TWO old steps gets a fresh clock
        // (label match still works for the untouched sibling).
        t.set(
            key(),
            vec![
                LoadStep::done("clone").with_kind(StepKind::Provision),
                LoadStep::active("nix develop").with_kind(StepKind::Provision),
            ],
        );
        let steps = t.get(&key()).unwrap();
        assert!(steps[1].started_at.is_some(), "restamped, not carried");
    }

    #[test]
    fn took_is_measured_at_the_terminal_transition() {
        let mut t = LoadingTracker::default();
        t.set(key(), vec![LoadStep::active("a"), LoadStep::pending("b")]);
        assert!(t.get(&key()).unwrap()[0].took.is_none(), "running: no took");
        t.set(key(), vec![LoadStep::done("a"), LoadStep::active("b")]);
        let took = t.get(&key()).unwrap()[0].took.expect("measured on →Done");
        // Carried verbatim on further replacements.
        t.set(key(), vec![LoadStep::done("a"), LoadStep::active("b")]);
        assert_eq!(t.get(&key()).unwrap()[0].took, Some(took));
        // A failed step measures the same way.
        t.set(key(), vec![LoadStep::done("a"), LoadStep::failed("b")]);
        assert!(t.get(&key()).unwrap()[1].took.is_some());
        // A step that jumps straight to Done without ever being Active has
        // nothing to measure.
        t.set(
            key(),
            vec![
                LoadStep::done("a"),
                LoadStep::done("b"),
                LoadStep::done("c"),
            ],
        );
        assert!(t.get(&key()).unwrap()[2].took.is_none());
    }

    #[test]
    fn fresh_key_stamps_its_active_step() {
        let mut t = LoadingTracker::default();
        t.set(key(), vec![LoadStep::pending("x"), LoadStep::active("y")]);
        let steps = t.get(&key()).unwrap();
        assert!(steps[0].started_at.is_none());
        assert!(steps[1].started_at.is_some());
    }

    #[test]
    fn advance_to_shell_preserves_a_rich_plan() {
        let mut t = LoadingTracker::default();
        t.set(
            key(),
            vec![
                LoadStep::done("connect host").with_kind(StepKind::Connect),
                LoadStep::active("image x").with_kind(StepKind::Image),
                LoadStep::pending("container (podman)").with_kind(StepKind::Create),
                LoadStep::pending("shell").with_kind(StepKind::Shell),
            ],
        );
        t.advance_to_shell(key(), "podman");
        let steps = t.get(&key()).unwrap();
        assert_eq!(steps.len(), 4, "rich plan keeps its rows");
        assert!(steps[..3].iter().all(|s| s.state == StepState::Done));
        assert_eq!(steps[3].label, "shell");
        assert_eq!(steps[3].state, StepState::Active);
        assert!(crate::loading::is_shell_wait(steps));
    }

    #[test]
    fn advance_to_shell_falls_back_to_the_classic_shape() {
        let mut t = LoadingTracker::default();
        t.advance_to_shell(key(), "docker");
        let steps = t.get(&key()).unwrap();
        assert_eq!(
            steps.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(),
            vec!["sandbox", "container (docker)", "shell"]
        );
        assert!(crate::loading::is_shell_wait(steps));
        // An entry NOT ending in shell (live provision steps) is also replaced
        // by the classic shape rather than corrupted mid-flight.
        t.set(key(), vec![LoadStep::active("nix")]);
        t.advance_to_shell(key(), "docker");
        assert!(crate::loading::is_shell_wait(t.get(&key()).unwrap()));
    }

    #[test]
    fn fail_active_marks_the_running_step_with_detail() {
        let mut t = LoadingTracker::default();
        t.set(
            key(),
            vec![
                LoadStep::done("sandbox"),
                LoadStep::active("image x"),
                LoadStep::pending("shell"),
            ],
        );
        t.fail_active(key(), "manifest unknown");
        let steps = t.get(&key()).unwrap();
        assert_eq!(steps[1].state, StepState::Failed);
        assert_eq!(steps[1].detail.as_deref(), Some("manifest unknown"));
        assert_eq!(steps[0].state, StepState::Done, "others untouched");
        assert_eq!(steps[2].state, StepState::Pending);
    }

    #[test]
    fn fail_active_with_no_plan_paints_the_classic_failure() {
        let mut t = LoadingTracker::default();
        t.fail_active(key(), "boom");
        let steps = t.get(&key()).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].label, "sandbox");
        assert_eq!(steps[0].state, StepState::Failed);
        assert_eq!(steps[0].detail.as_deref(), Some("boom"));
    }

    #[test]
    fn env_override_is_set_read_moved_and_cleared() {
        let mut t = LoadingTracker::default();
        t.set(key(), vec![LoadStep::active("a")]);
        // Empty is a no-op; a real pick is stored and readable.
        t.set_env(key(), "");
        assert_eq!(t.env_for(&key()), None);
        t.set_env(key(), "default");
        assert_eq!(t.env_for(&key()), Some("default"));
        // Rename carries the override to the settled key.
        let new = ("settled".to_string(), 0);
        t.rename(&key(), new.clone());
        assert_eq!(t.env_for(&key()), None);
        assert_eq!(t.env_for(&new), Some("default"));
        // Remove clears it alongside the steps.
        t.remove(&new);
        assert_eq!(t.env_for(&new), None);
    }

    #[test]
    fn rename_moves_stamps_verbatim() {
        let mut t = LoadingTracker::default();
        t.set(key(), vec![LoadStep::active("a")]);
        let stamp = t.get(&key()).unwrap()[0].started_at;
        let new = ("settled".to_string(), 0);
        t.rename(&key(), new.clone());
        assert!(t.get(&key()).is_none());
        assert_eq!(t.get(&new).unwrap()[0].started_at, stamp);
    }
}
