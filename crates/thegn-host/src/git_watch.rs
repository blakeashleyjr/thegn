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

/// A path in the form the fs-watcher will report it in.
///
/// macOS's FSEvents backend canonicalizes the watched root and always delivers
/// **fully resolved** paths — `/private/tmp/wt/…`, never `/tmp/wt/…`. A filter
/// built from the session's un-canonicalized cwd then matches none of them:
/// `roots.starts_with` misses, the gitignore matcher misses, and every write
/// under `target/` drives a full model rebuild — precisely the ~Hz churn case 2
/// of [`watcher_path_triggers_refresh`] exists to drop. inotify reports paths as
/// they were registered, so this is a no-op on Linux.
///
/// (Same `/tmp` → `/private/tmp` root cause as the already-fixed macOS repo
/// resolution bug; this is the fs-watcher's copy of it. It bites any worktree
/// under `/tmp` or `/var`, which includes every `mktemp -d` fixture, plus any
/// user symlink such as `~/code → /Volumes/…`.)
///
/// Falls back to the input when the path can't be resolved — it may not exist
/// yet, and an un-canonicalized filter is merely the status quo, whereas
/// dropping the root would be worse.
pub(crate) fn watch_canonical(p: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// How deep [`plan_watches`] will descend to prune before giving up and taking
/// one recursive watch on the remainder.
///
/// Shallow on purpose. Descending costs correctness, not just time: a directory
/// we watch NON-recursively does not auto-register subdirectories created under
/// it later, so every level we descend adds a place where a brand-new source
/// directory goes unwatched until the safety-net ticker or the next retarget.
/// The build outputs that motivate pruning at all (`target/`, `node_modules/`,
/// `.claude/worktrees/`) sit at or near the top of a repo, so a small budget
/// captures effectively all of the win.
const MAX_PRUNE_DEPTH: usize = 4;

/// One entry of the diff watcher's registration plan: a directory, and whether
/// `notify` should recurse into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchPlanEntry {
    pub(crate) path: std::path::PathBuf,
    pub(crate) recursive: bool,
}

/// Whether a directory must be kept out of the watch registration entirely.
///
/// Two sources, both of which the *event* filter already discards:
/// gitignored build output (case 2 of [`watcher_path_triggers_refresh`]), and
/// the object store, which [`is_git_state_path`]'s allowlist never matches and
/// which floods on every commit/gc.
fn prune_dir(p: &std::path::Path, ignore: &ignore::gitignore::Gitignore) -> bool {
    // `.git/objects` — the same subtree the linked-worktree gitdir watches
    // already refuse to descend into, applied to the main checkout too.
    if p.file_name().and_then(|s| s.to_str()) == Some("objects") && in_dot_git(p) {
        return true;
    }
    // `matched_path_or_any_parents` panics on a path outside the matcher's root
    // (see `watcher_path_triggers_refresh` for the full note). Registration runs
    // on a background thread where a panic would leave the panel with no
    // watcher at all, so check the precondition and decline to prune instead.
    if ignore.is_empty() || !p.starts_with(ignore.path()) {
        return false;
    }
    ignore.matched_path_or_any_parents(p, true).is_ignore()
}

/// Plan the diff watcher's registration over `root`, pruning gitignored
/// subtrees instead of taking one blanket recursive watch.
///
/// `notify`'s "recursive" mode is a userspace walk — on Linux it takes one
/// inotify watch per directory — so a recursive watch on a worktree root
/// registers every build artifact directory under it. On this repo that was
/// 86,972 directories (25,076 in `target/`, 60,668 in `.claude/worktrees/`) for
/// 114,701 watches, and every rustc write then woke the watcher thread to pay a
/// gitignore match on an event [`watcher_path_triggers_refresh`] discards. The
/// matcher was consulted at event time but never at registration time; this is
/// that same matcher, applied one layer earlier.
///
/// A directory with no pruned children needs no further descent — one recursive
/// watch covers it, and `notify` keeps auto-registering new subdirectories under
/// it, which is the property that makes the shallow [`MAX_PRUNE_DEPTH`] safe.
///
/// Reads the filesystem (so not pure), but deterministic given a tree — the
/// pruning precedence is unit-tested against real temp dirs.
pub(crate) fn plan_watches(
    root: &std::path::Path,
    ignore: &ignore::gitignore::Gitignore,
) -> Vec<WatchPlanEntry> {
    let mut out = Vec::new();
    plan_descend(root, ignore, MAX_PRUNE_DEPTH, &mut out);
    out
}

fn plan_descend(
    dir: &std::path::Path,
    ignore: &ignore::gitignore::Gitignore,
    depth: usize,
    out: &mut Vec<WatchPlanEntry>,
) {
    let mut keep: Vec<std::path::PathBuf> = Vec::new();
    let mut pruned_any = false;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            // Only directories carry watches; a file is covered by its parent.
            // `file_type` on the dirent avoids a stat per entry, and a symlink
            // is deliberately not followed — recursing through one can leave the
            // tree, and `notify` does not follow them either.
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let p = e.path();
            if prune_dir(&p, ignore) {
                pruned_any = true;
            } else {
                keep.push(p);
            }
        }
    }
    // Nothing to prune below here (or no budget left to look): one recursive
    // watch, and `notify` owns new subdirectories from now on.
    if !pruned_any || depth == 0 {
        out.push(WatchPlanEntry {
            path: dir.to_path_buf(),
            recursive: true,
        });
        return;
    }
    out.push(WatchPlanEntry {
        path: dir.to_path_buf(),
        recursive: false,
    });
    for c in keep {
        plan_descend(&c, ignore, depth - 1, out);
    }
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
        // `matched_path_or_any_parents` PANICS on a path outside its root (a
        // documented precondition, asserted unless the matcher is empty) — and
        // this runs on the notify callback thread, where a panic takes the
        // watcher down and the panel silently stops updating. Callers give us a
        // canonicalized root (see `watch_canonical`) so the precondition holds,
        // but a filter is the wrong place to bet a thread on that: check it, and
        // treat an out-of-root path as a plain edit — the pre-filter behavior.
        if !ignore.is_empty() && !p.starts_with(ignore.path()) {
            return true;
        }
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
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
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
        assert!(yes("/repo/.git/refs/remotes/origin/tg/feat"));
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

    /// A worktree shaped like this repo: gitignored build output at the top,
    /// real source beside it, and a `.git` with an object store.
    fn plan_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        for d in [
            "target/debug/.fingerprint",
            ".claude/worktrees/agent-1/target/debug",
            "crates/thegn-host/src",
            "docs/help",
            ".git/objects/ab",
            ".git/refs/heads",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        (tmp, root)
    }

    fn plan_matcher(root: &std::path::Path) -> ignore::gitignore::Gitignore {
        let mut b = ignore::gitignore::GitignoreBuilder::new(root);
        // The root-anchored forms this repo's own `.gitignore` uses.
        b.add_line(None, "/target").unwrap();
        b.add_line(None, ".claude/*").unwrap();
        b.build().unwrap()
    }

    #[test]
    fn plan_prunes_build_output_and_the_object_store() {
        let (_tmp, root) = plan_fixture();
        let plan = plan_watches(&root, &plan_matcher(&root));
        let watched = |rel: &str| {
            let p = root.join(rel);
            plan.iter().any(|e| e.path == p)
        };
        // The whole point: no watch is ever taken on gitignored build output.
        // Before this planner these were 85,744 of the 86,972 directories the
        // blanket recursive watch registered on the real repo.
        assert!(!watched("target"), "target/ must never be watched");
        // `.claude/*` ignores the CONTENTS, not the directory (the real
        // `.gitignore` uses that form so `!.claude/settings.json` can be
        // re-included). So `.claude/` is descended into and `worktrees/` — the
        // 60,668-directory half of the storm — is pruned one level down.
        assert!(
            !watched(".claude/worktrees"),
            ".claude/worktrees (agent build trees) must never be watched"
        );
        assert!(
            !watched(".claude/worktrees/agent-1/target"),
            "nothing below a pruned directory may be watched"
        );
        // The object store floods on every commit/gc and no event from it can
        // pass `is_git_state_path`, so it is pruned too.
        assert!(!watched(".git/objects"), ".git/objects must be pruned");
        // Real source is still covered, and `.git` is still reachable so
        // ref moves / reflog writes keep firing.
        assert!(watched("crates") || watched("crates/thegn-host"));
        assert!(watched(".git"));
    }

    #[test]
    fn plan_takes_one_recursive_watch_on_clean_subtrees() {
        let (_tmp, root) = plan_fixture();
        let plan = plan_watches(&root, &plan_matcher(&root));
        // `crates/` has nothing pruned beneath it, so it must be covered by a
        // single RECURSIVE entry rather than descended into. That is what keeps
        // `notify` auto-registering new subdirectories under it — the property
        // that makes the shallow MAX_PRUNE_DEPTH safe.
        let crates = plan.iter().find(|e| e.path == root.join("crates"));
        let crates = crates.expect("crates/ should be planned");
        assert!(crates.recursive, "a clean subtree needs only one watch");
        assert!(
            !plan
                .iter()
                .any(|e| e.path == root.join("crates/thegn-host")),
            "must not descend below a subtree already watched recursively"
        );
        // The root itself has pruned children, so it is non-recursive.
        let r = plan.iter().find(|e| e.path == root).expect("root planned");
        assert!(!r.recursive);
    }

    #[test]
    fn plan_without_ignores_is_a_single_recursive_watch() {
        // No `.gitignore` (remote/provider worktrees) => nothing to prune =>
        // exactly the pre-existing behavior: one recursive watch on the root.
        let (_tmp, root) = plan_fixture();
        let plan = plan_watches(&root, &ignore::gitignore::Gitignore::empty());
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].path, root);
        assert!(plan[0].recursive);
    }

    #[cfg(unix)]
    #[test]
    fn filter_survives_a_symlinked_worktree_prefix() {
        // The macOS bug in miniature: FSEvents canonicalizes, so events arrive
        // as `<real>/target/…` while a filter built from the session's cwd is
        // rooted at `<link>/…`. Un-canonicalized, BOTH branches miss — the
        // gitdir root and every ignore pattern — so all build churn refreshes.
        //
        // Built from a real symlink rather than literal strings on purpose: the
        // existing cases all hand-write both sides, which is exactly why this
        // class of drift was invisible to them.
        use std::path::PathBuf;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("target/debug")).unwrap();
        std::fs::create_dir_all(real.join(".git")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Everything the live watcher derives from the session cwd (`link`),
        // canonicalized the way `retarget_diff_watcher` now does.
        let root = watch_canonical(&link);
        assert_eq!(root, std::fs::canonicalize(&real).unwrap());
        let roots: Vec<PathBuf> = vec![watch_canonical(&link.join(".git"))];
        let mut b = ignore::gitignore::GitignoreBuilder::new(&root);
        b.add_line(None, "/target").unwrap();
        let ig = b.build().unwrap();

        // Event paths as the watcher delivers them: resolved through the link.
        let ev = |rel: &str| root.join(rel);
        assert!(
            !watcher_path_triggers_refresh(&ev("target/debug/thegn"), &roots, &ig),
            "gitignored build churn must stay dropped through a symlinked prefix"
        );
        assert!(
            !watcher_path_triggers_refresh(&ev(".git/index"), &roots, &ig),
            "git-internal churn must stay dropped through a symlinked prefix"
        );
        assert!(watcher_path_triggers_refresh(
            &ev("src/main.rs"),
            &roots,
            &ig
        ));
        assert!(watcher_path_triggers_refresh(&ev(".git/HEAD"), &roots, &ig));

        // Negative control: rooted at the LINK, the same events fall outside the
        // matcher root. Before the guard that was a PANIC on the notify callback
        // thread (verified against `ignore` 0.4.27 for `/tmp`→`/private/tmp`,
        // `/var/folders`→`/private/var/folders`, and a differing-basename user
        // symlink alike) — the watcher died and the panel went quiet until the
        // safety-net tick. It must now degrade to "this is an edit" instead.
        let mut b2 = ignore::gitignore::GitignoreBuilder::new(&link);
        b2.add_line(None, "/target").unwrap();
        let stale_ig = b2.build().unwrap();
        let stale_roots: Vec<PathBuf> = vec![link.join(".git")];
        assert!(
            watcher_path_triggers_refresh(&ev("target/debug/thegn"), &stale_roots, &stale_ig),
            "an out-of-root path must degrade to a refresh, never panic"
        );
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
