//! Diff/ref fs-watch event classification + the main-checkout self-heal it drives.
//!
//! The diff watcher ([`crate::hydrate::retarget_diff_watcher`]) fires on every
//! filesystem event under the active worktree. These pure predicates decide which
//! events are worth a panel rehydrate ([`watcher_path_triggers_refresh`] /
//! [`is_git_state_path`]) and which are a branch-ref move ([`is_ref_move_path`])
//! that should fast-forward the canonical main checkout ([`spawn_main_checkout_heal`]).
//! Split out of the (cap-bound) `hydrate` module and unit-tested in isolation.

use tokio::sync::mpsc as tokio_mpsc;

use termwiz::terminal::TerminalWaker;

use crate::hydrate::RefreshKind;

/// True when `p` lies inside a `.git` directory (any path component is
/// `.git`) — used to filter the recursive worktree watcher so git's own
/// metadata churn doesn't drive a refresh loop.
pub(crate) fn in_dot_git(p: &std::path::Path) -> bool {
    p.components().any(|c| c.as_os_str() == ".git")
}

/// True for the subset of `.git`-internal paths that signal a real *git-state*
/// change — a commit, checkout, reset, branch/tag move, or a merge / rebase /
/// cherry-pick / revert progressing. These are the events the panel must react
/// to even though they live under `.git`.
///
/// Deliberately an allowlist, not a blocklist: the high-churn internals —
/// `index` (hydration's own `git status`/`diff` rewrite its stat cache, the
/// ~2 Hz feedback loop that once read as a freeze), the object store, lock
/// files, `COMMIT_EDITMSG` — never match, so they can never drive a refresh
/// loop no matter what new files git starts writing.
pub(crate) fn is_git_state_path(p: &std::path::Path) -> bool {
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".lock") {
        // The transient `*.lock` git writes while preparing a ref/HEAD update;
        // react to the final write that replaces it, not the lock churn.
        return false;
    }
    // `logs/HEAD` (reflog) is appended on commit/checkout/reset/merge/rebase;
    // `refs/…` + `packed-refs` move on branch/tag updates; the rebase-* dirs
    // and *_HEAD pseudo-refs track an in-progress sequencer operation.
    p.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("refs") | Some("logs") | Some("rebase-merge") | Some("rebase-apply")
        )
    }) || matches!(
        name,
        "HEAD"
            | "packed-refs"
            | "MERGE_HEAD"
            | "ORIG_HEAD"
            | "CHERRY_PICK_HEAD"
            | "REVERT_HEAD"
            | "BISECT_LOG"
    )
}

/// Whether a diff-watcher event path is a *branch/tag ref update* — a write to
/// `refs/…` (but not the reflog under `logs/`) or a `packed-refs` rewrite. Drives
/// the main-checkout self-heal ([`RefreshKind::MainRefMoved`]); intentionally a
/// superset ("some ref moved", not specifically `main`) since the heal is a
/// guarded, idempotent no-op whenever the checkout is already coherent.
pub(crate) fn is_ref_move_path(p: &std::path::Path) -> bool {
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".lock") {
        return false; // react to the final write, not the lock churn
    }
    if name == "packed-refs" {
        return true;
    }
    let comps: Vec<&str> = p
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // A loose ref lives at `refs/heads/<branch>`; exclude the reflog mirror under
    // `logs/refs/…`, which is appended on every commit in any worktree.
    comps.contains(&"refs") && !comps.contains(&"logs")
}

/// Whether a diff-watcher event path is a *remote-tracking* ref update
/// (`refs/remotes/…`) — the local signature of a `git push` (or fetch). Drives
/// an immediate PR/CI cache kick so a just-pushed branch's checks appear
/// without waiting for the 20s / `[ci] poll_interval_secs` tickers. Local
/// commits only move `refs/heads/…` and deliberately don't match — they'd
/// churn provider subprocesses on every agent commit.
pub(crate) fn is_remote_ref_path(p: &std::path::Path) -> bool {
    let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".lock") {
        return false; // react to the final write, not the lock churn
    }
    let comps: Vec<&str> = p
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    // Exclude the reflog mirror under `logs/refs/remotes/…`.
    comps.windows(2).any(|w| w == ["refs", "remotes"]) && !comps.contains(&"logs")
}

/// Whether a single diff-watcher event path should drive a model re-hydration.
/// Three cases, in precedence order:
/// 1. `.git`-internal paths (inside a `.git` component, or under the resolved
///    gitdir/common-dir `roots`) refresh ONLY for real git-state changes —
///    commits, checkouts, branch/tag moves, in-progress merge/rebase — gated by
///    [`is_git_state_path`] so index/object-store churn can't drive a loop.
/// 2. Otherwise, gitignored worktree paths (build artifacts like `target/`)
///    never refresh: they can't appear in `git diff HEAD`, so a rebuild would be
///    pure waste — and a cargo/agent running in the tree churns them constantly.
/// 3. Everything else (real edits to tracked/untracked source files) refreshes.
///
/// Pure (given a prebuilt matcher), so the precedence is unit-tested.
pub(crate) fn watcher_path_triggers_refresh(
    p: &std::path::Path,
    roots: &[std::path::PathBuf],
    ignore: &ignore::gitignore::Gitignore,
) -> bool {
    if in_dot_git(p) || roots.iter().any(|r| p.starts_with(r)) {
        is_git_state_path(p)
    } else {
        // Case 2 vs 3: gitignored build churn is dropped; everything else (real
        // source edits) refreshes.
        !ignore
            .matched_path_or_any_parents(p, p.is_dir())
            .is_ignore()
    }
}

/// Off-loop, guarded fast-forward of the canonical main checkout after its branch
/// ref moved (an external `git update-ref`, or a fold-actor CAS land in another
/// process). Resolves the canonical from `from` (any worktree in the repo) via
/// `--git-common-dir`, then runs [`thegn_core::util::heal_main_checkout_worktree`]
/// — which only fast-forwards a clean, same-branch, strictly-forward checkout and
/// otherwise no-ops. If it actually healed, pulses a `Model` refresh so the panel
/// reflects the new tip at once. Cheap when already coherent (the common case: a
/// few `git` probes) so it is safe to call on every `MainRefMoved` (throttled by
/// the caller). Never touches a checkout with real uncommitted work.
pub(crate) fn spawn_main_checkout_heal(
    from: std::path::PathBuf,
    refresh_tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    tokio::task::spawn_blocking(move || {
        // off-loop: inside spawn_blocking
        #[expect(clippy::disallowed_methods)]
        let Some(common_parent) = thegn_core::util::git_cmd(&from)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| std::path::PathBuf::from(s.trim()))
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        else {
            return;
        };
        if thegn_core::util::heal_main_checkout_worktree(&common_parent)
            && refresh_tx.send(RefreshKind::Model).is_ok()
        {
            let _ = waker.wake();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_state_paths_signal_commits_and_branch_moves() {
        let yes = |p: &str| is_git_state_path(std::path::Path::new(p));
        // Main checkout: state files live under `<wt>/.git`.
        assert!(yes("/repo/.git/HEAD"));
        assert!(yes("/repo/.git/logs/HEAD")); // reflog — commit/checkout/reset
        assert!(yes("/repo/.git/refs/heads/main")); // branch move
        assert!(yes("/repo/.git/packed-refs"));
        assert!(yes("/repo/.git/MERGE_HEAD"));
        assert!(yes("/repo/.git/ORIG_HEAD"));
        assert!(yes("/repo/.git/rebase-merge/done")); // rebase in progress
        // Linked worktree: state lives in the main repo's external gitdir.
        assert!(yes("/repo/.git/worktrees/feat/HEAD"));
        assert!(yes("/repo/.git/worktrees/feat/logs/HEAD"));
    }

    #[test]
    fn git_state_path_ignores_churn_that_caused_the_refresh_storm() {
        let no = |p: &str| !is_git_state_path(std::path::Path::new(p));
        // The index stat-cache — hydration's own `git status`/`diff` rewrite it,
        // the ~2 Hz self-sustaining loop the allowlist exists to prevent.
        assert!(no("/repo/.git/index"));
        // Object store floods on every commit / gc.
        assert!(no("/repo/.git/objects/ab/cdef0123"));
        assert!(no("/repo/.git/objects/pack/pack-deadbeef.pack"));
        // Transient lock files (react to the final write, not the lock).
        assert!(no("/repo/.git/index.lock"));
        assert!(no("/repo/.git/refs/heads/main.lock"));
        assert!(no("/repo/.git/HEAD.lock"));
        // Editor scratch + config — not a state change.
        assert!(no("/repo/.git/COMMIT_EDITMSG"));
        assert!(no("/repo/.git/config"));
    }

    #[test]
    fn ref_move_paths_drive_the_main_checkout_heal() {
        let yes = |p: &str| is_ref_move_path(std::path::Path::new(p));
        // A branch/tag ref write, or a packed-refs rewrite, is a ref move.
        assert!(yes("/repo/.git/refs/heads/main"));
        assert!(yes("/repo/.git/refs/tags/v1"));
        assert!(yes("/repo/.git/packed-refs"));
        // The reflog mirror under `logs/` is appended on every commit in any
        // worktree — NOT a ref move, so it must not kick the heal.
        assert!(!yes("/repo/.git/logs/refs/heads/main"));
        assert!(!yes("/repo/.git/logs/HEAD"));
        // The transient lock is the churn before the final write.
        assert!(!yes("/repo/.git/refs/heads/main.lock"));
        // Ordinary source / index / object writes are never ref moves.
        assert!(!yes("/repo/src/main.rs"));
        assert!(!yes("/repo/.git/index"));
        assert!(!yes("/repo/.git/HEAD"));
    }

    #[test]
    fn remote_ref_paths_signal_a_push() {
        let yes = |p: &str| is_remote_ref_path(std::path::Path::new(p));
        // A remote-tracking ref write is the local signature of a push/fetch.
        assert!(yes("/repo/.git/refs/remotes/origin/main"));
        assert!(yes("/repo/.git/refs/remotes/origin/sz/feat"));
        // Local commits move refs/heads — deliberately NOT a push signal
        // (agents commit constantly; each would cost a provider subprocess).
        assert!(!yes("/repo/.git/refs/heads/main"));
        assert!(!yes("/repo/.git/refs/tags/v1"));
        // Reflog mirror + transient lock churn never fire.
        assert!(!yes("/repo/.git/logs/refs/remotes/origin/main"));
        assert!(!yes("/repo/.git/refs/remotes/origin/main.lock"));
        assert!(!yes("/repo/src/main.rs"));
    }

    #[test]
    fn watcher_drops_gitignored_churn_but_keeps_source_and_git_state() {
        use std::path::{Path, PathBuf};
        // Matcher built like the live watcher, but from inline patterns so the
        // test needs no temp `.gitignore` on disk.
        let mut b = ignore::gitignore::GitignoreBuilder::new("/repo");
        // `/target` is the ROOT-ANCHORED form this repo's own `.gitignore` uses —
        // the fix hinges on the anchored pattern matching via parent lookup.
        b.add_line(None, "/target").unwrap();
        b.add_line(None, "*.log").unwrap();
        let ig = b.build().unwrap();
        let roots: Vec<PathBuf> = vec![PathBuf::from("/repo/.git")];
        let fires = |p: &str| watcher_path_triggers_refresh(Path::new(p), &roots, &ig);

        // Gitignored build churn — the storm this filter exists to kill.
        assert!(!fires("/repo/target/debug/thegn"));
        assert!(!fires("/repo/target/debug/.fingerprint/x"));
        assert!(!fires("/repo/run.log"));
        // Real source edits still refresh the panel.
        assert!(fires("/repo/src/main.rs"));
        assert!(fires("/repo/crates/foo/Cargo.toml"));
        // Git-state changes still refresh (the `.git` branch wins; the gitignore
        // matcher never even sees these).
        assert!(fires("/repo/.git/HEAD"));
        assert!(fires("/repo/.git/refs/heads/main"));
        // Git-internal churn stays dropped (index/objects).
        assert!(!fires("/repo/.git/index"));
        assert!(!fires("/repo/.git/objects/ab/cdef"));
    }

    #[test]
    fn empty_gitignore_matcher_passes_every_worktree_edit() {
        // Remote/provider worktrees (no local `.gitignore`) build an empty
        // matcher; it must not drop any edit — unchanged pre-filter behavior.
        use std::path::{Path, PathBuf};
        let ig = ignore::gitignore::Gitignore::empty();
        let roots: Vec<PathBuf> = vec![];
        assert!(watcher_path_triggers_refresh(
            Path::new("/wt/target/x"),
            &roots,
            &ig
        ));
        assert!(watcher_path_triggers_refresh(
            Path::new("/wt/src/main.rs"),
            &roots,
            &ig
        ));
    }
}
