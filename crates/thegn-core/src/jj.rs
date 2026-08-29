//! Jujutsu (`jj`) **coexistence** — detection and degradation, not a VCS backend.
//!
//! thegn's worktree model ("each git worktree is a tab") has no mechanical
//! mapping onto jj, which does not support `git worktree` at all — so a real
//! `VcsBackend` seam is deliberately out of scope (see the change's design §5).
//! What users hit *today* is the colocated case: a `.jj/` directory beside
//! `.git/`, where jj's docs ask external tools to "mostly use read-only git
//! commands". thegn's reads are already safe there; the hazards are cosmetic or
//! advisory (detached HEAD is jj's normal state, jj ignores the git index, a
//! background `git fetch` can interleave with jj's auto-snapshot).
//!
//! This module is the single source of that detection + the pure policy
//! decisions built on it. **No `jj` process is ever spawned** — detection is a
//! directory-existence check.

use std::path::Path;

/// Whether `repo_root` is colocated with jujutsu: a `.jj` directory beside its
/// `.git`. A cheap `stat`, never a `jj` subprocess.
pub fn is_colocated(repo_root: &Path) -> bool {
    repo_root.join(".jj").is_dir()
}

/// Whether a background `auto_fetch` should run in a repo, given whether it is
/// jj-colocated and the `[git] auto_fetch_colocated` opt-in. Pure so the skip
/// decision is unit-tested without touching a repo:
///
/// - fetch is off ⇒ never;
/// - not colocated ⇒ follow `auto_fetch`;
/// - colocated ⇒ only when the user opted in via `auto_fetch_colocated`.
pub fn should_auto_fetch(auto_fetch: bool, colocated: bool, auto_fetch_colocated: bool) -> bool {
    auto_fetch && (!colocated || auto_fetch_colocated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colocation_is_a_sibling_dot_jj_dir() {
        let dir = std::env::temp_dir().join(format!("tg-jj-{}", std::process::id()));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        assert!(!is_colocated(&dir), "git-only repo is not colocated");
        std::fs::create_dir_all(dir.join(".jj")).unwrap();
        assert!(is_colocated(&dir), "a .jj sibling means colocated");
        // A `.jj` *file* (not a dir) does not count.
        let f = std::env::temp_dir().join(format!("tg-jjf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&f); // best-effort: test setup: fresh scratch dir
        std::fs::create_dir_all(&f).unwrap();
        std::fs::write(f.join(".jj"), "x").unwrap();
        assert!(!is_colocated(&f));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&f); // best-effort: test cleanup: scratch removal must never fail the test
    }

    #[test]
    fn auto_fetch_skip_decision() {
        // Off always skips.
        assert!(!should_auto_fetch(false, false, false));
        assert!(!should_auto_fetch(false, true, true));
        // On, non-colocated: always fetch.
        assert!(should_auto_fetch(true, false, false));
        // On, colocated, default: skip (stay out of jj's way).
        assert!(!should_auto_fetch(true, true, false));
        // On, colocated, opted in: fetch.
        assert!(should_auto_fetch(true, true, true));
    }
}
