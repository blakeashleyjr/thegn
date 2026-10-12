//! Cross-workspace prefetch policy + the prefetch in-flight guard.
//!
//! The switch cache (`handlers::switch_cache`) is path-keyed and global, but
//! the warm loops only ever fed it the ACTIVE workspace's worktrees — so a
//! cross-WORKSPACE switch was a guaranteed cache miss: blank tab-bar chips +
//! a panel skeleton + a 100-500ms wait on an interactive `build_panel`, every
//! time. The policy here picks a small set of other-workspace destinations to
//! keep warm; the guard stops a rapid switch burst from re-spawning the same
//! prefetch set on every keystroke (there was previously no in-flight
//! tracking at all, and 8 concurrent duplicate workers were exactly the WAL
//! write contention that stalled the loop's own DB opens).
//!
//! Pure decisions + a tiny state holder; the loop owns the instance.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How many non-active workspaces to keep warm, in sidebar order (the sidebar
/// is recency/pin-ordered, so the first few are the likeliest destinations).
pub(crate) const WARM_WORKSPACES: usize = 2;
/// How many paths per warmed workspace (its repo root first — that is where a
/// bare workspace switch lands — then its first registered worktrees).
pub(crate) const WARM_PATHS_PER_WORKSPACE: usize = 3;

/// The other-workspace paths worth prefetching, from model state the loop
/// already holds (`model.sidebar_workspaces` order + the all-workspace
/// `model.sidebar_db_worktrees` registry) — zero I/O.
///
/// `workspaces` rows are `(slug, display, kind, repo_path)`; live fallbacks
/// (empty `repo_path`) and the terminals pseudo-workspace are skipped.
pub(crate) fn cross_workspace_targets(
    workspaces: &[(String, String, String, String)],
    db_worktrees: &[crate::sidebar::DbWorktree],
    active_repo: &str,
    limit_ws: usize,
    limit_per_ws: usize,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut picked_ws = 0usize;
    for (_, _, _, repo_path) in workspaces {
        if picked_ws >= limit_ws {
            break;
        }
        if repo_path.is_empty() || repo_path == active_repo || repo_path == "terminal" {
            continue;
        }
        picked_ws += 1;
        let mut n = 0usize;
        if n < limit_per_ws {
            out.push(PathBuf::from(repo_path));
            n += 1;
        }
        for wt in db_worktrees.iter().filter(|w| w.repo_path == *repo_path) {
            if n >= limit_per_ws {
                break;
            }
            if wt.path.is_empty() || wt.path == *repo_path {
                continue;
            }
            let p = PathBuf::from(&wt.path);
            if !out.contains(&p) {
                out.push(p);
                n += 1;
            }
        }
    }
    out
}

/// Dedupe guard for `spawn_panel_prefetch`: at most one in-flight prefetch per
/// path. Entries expire after [`PrefetchInflight::STALE`] so a worker that
/// early-returned without sending a result (vanished dir, DB open failure)
/// can't wedge its path out of warming forever.
pub(crate) struct PrefetchInflight {
    inflight: HashMap<PathBuf, (Instant, u64)>,
    next_generation: u64,
}

impl PrefetchInflight {
    /// Longer than any healthy `build_panel`; far shorter than FRESH_TTL.
    const STALE: Duration = Duration::from_secs(10);

    pub(crate) fn new() -> Self {
        PrefetchInflight {
            inflight: HashMap::new(),
            next_generation: 0,
        }
    }

    /// Whether a prefetch for `path` should spawn; records it when yes.
    pub(crate) fn try_begin(&mut self, path: &Path) -> bool {
        match self.inflight.get(path) {
            Some((t, _)) if t.elapsed() < Self::STALE => false,
            _ => {
                self.next_generation = self.next_generation.wrapping_add(1);
                self.inflight
                    .insert(path.to_path_buf(), (Instant::now(), self.next_generation));
                true
            }
        }
    }

    /// Generation captured by the just-admitted worker.
    pub(crate) fn generation(&self, path: &Path) -> Option<u64> {
        self.inflight.get(path).map(|(_, generation)| *generation)
    }

    /// A result for the matching request arrived. An old completion cannot
    /// release a newer request that replaced a stale entry.
    pub(crate) fn finish(&mut self, path: &Path, generation: u64) -> bool {
        if self.generation(path) == Some(generation) {
            self.inflight.remove(path);
            return true;
        }
        false
    }

    /// Roll back the path latch when background admission refused the request.
    pub(crate) fn finish_if_refused(
        &mut self,
        path: &Path,
        generation: u64,
        admission: crate::sched::Admission,
    ) {
        if admission == crate::sched::Admission::Admitted {
            return;
        }
        self.finish(path, generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(slug: &str, repo: &str) -> (String, String, String, String) {
        (
            slug.to_string(),
            slug.to_string(),
            "repo".to_string(),
            repo.to_string(),
        )
    }

    fn wt(repo: &str, path: &str) -> crate::sidebar::DbWorktree {
        crate::sidebar::DbWorktree {
            slug: "s".into(),
            branch: "b".into(),
            repo_path: repo.to_string(),
            tab_name: "s/b".into(),
            path: path.to_string(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        }
    }

    #[test]
    fn admission_refusal_releases_latch_for_later_retry() {
        let path = PathBuf::from("/repo/wt");
        let mut inflight = PrefetchInflight::new();
        assert!(inflight.try_begin(&path));
        let generation = inflight.generation(&path).unwrap();
        inflight.finish_if_refused(&path, generation, crate::sched::Admission::Saturated);
        assert!(
            inflight.try_begin(&path),
            "the next trigger can retry immediately"
        );
    }

    #[test]
    fn stale_completion_cannot_clear_replacement_generation() {
        let path = PathBuf::from("/repo/wt");
        let mut inflight = PrefetchInflight::new();
        assert!(inflight.try_begin(&path));
        let old = inflight.generation(&path).unwrap();
        inflight.inflight.insert(
            path.clone(),
            (Instant::now() - PrefetchInflight::STALE, old),
        );
        assert!(inflight.try_begin(&path));
        let current = inflight.generation(&path).unwrap();
        assert_ne!(old, current);
        inflight.finish(&path, old);
        assert_eq!(inflight.generation(&path), Some(current));
    }

    #[test]
    fn targets_skip_active_live_fallback_and_terminal_rows() {
        let workspaces = vec![
            ws("active", "/r/active"),
            ws("live", ""),          // live fallback: no DB row yet
            ws("term", "terminal"),  // terminals pseudo-workspace
            ws("other", "/r/other"), // the one real candidate
        ];
        let wts = vec![wt("/r/other", "/wt/other-a")];
        let got = cross_workspace_targets(&workspaces, &wts, "/r/active", 2, 3);
        assert_eq!(
            got,
            vec![PathBuf::from("/r/other"), PathBuf::from("/wt/other-a")]
        );
    }

    #[test]
    fn targets_bound_by_workspace_and_per_workspace_limits() {
        let workspaces = vec![
            ws("act", "/r/act"),
            ws("a", "/r/a"),
            ws("b", "/r/b"),
            ws("c", "/r/c"), // beyond limit_ws=2
        ];
        let wts = vec![
            wt("/r/a", "/wt/a1"),
            wt("/r/a", "/wt/a2"),
            wt("/r/a", "/wt/a3"), // beyond limit_per_ws=3 (root + 2)
            wt("/r/b", "/wt/b1"),
        ];
        let got = cross_workspace_targets(&workspaces, &wts, "/r/act", 2, 3);
        assert_eq!(
            got,
            vec![
                PathBuf::from("/r/a"),
                PathBuf::from("/wt/a1"),
                PathBuf::from("/wt/a2"),
                PathBuf::from("/r/b"),
                PathBuf::from("/wt/b1"),
            ]
        );
    }

    #[test]
    fn targets_dedupe_worktrees_that_alias_the_repo_root() {
        // A registry row whose path IS the repo root (the home checkout) must
        // not produce a duplicate of the root entry.
        let workspaces = vec![ws("act", "/r/act"), ws("a", "/r/a")];
        let wts = vec![wt("/r/a", "/r/a"), wt("/r/a", "/wt/a1")];
        let got = cross_workspace_targets(&workspaces, &wts, "/r/act", 2, 3);
        assert_eq!(got, vec![PathBuf::from("/r/a"), PathBuf::from("/wt/a1")]);
    }

    #[test]
    fn inflight_guard_dedupes_until_finish_then_rearms() {
        let mut g = PrefetchInflight::new();
        let p = Path::new("/wt/a");
        assert!(g.try_begin(p), "first spawn proceeds");
        let generation = g.generation(p).unwrap();
        assert!(!g.try_begin(p), "duplicate while in flight is suppressed");
        g.finish(p, generation);
        assert!(g.try_begin(p), "re-arms after the result lands");
    }
}
