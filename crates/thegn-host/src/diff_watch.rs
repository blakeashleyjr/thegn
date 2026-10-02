//! The active worktree's diff fs-watcher, and the **change generation** it
//! maintains (THE-718).
//!
//! [`build_diff_watcher`] is the registration + event-filter body that used to
//! live inside `hydrate::retarget_diff_watcher`'s thread (moved, then
//! extended). It additionally keeps a monotonically increasing *generation* for
//! the watched path: every event that could alter what `status` / `diff` /
//! `stash list` print bumps it, whether or not the 500 ms refresh throttle lets
//! the event drive an immediate rehydrate. Hydration compares the generation
//! with the one its last snapshot was taken at ([`current_print`]) and skips
//! re-running those reads when nothing moved, instead of forking git every
//! 5 s tick.
//!
//! The claim is deliberately conservative: **fail toward today's behaviour,
//! never toward staleness**. No generation is published (so hydration reads
//! every tick, as before) unless registration was complete, and a published one
//! is withdrawn the moment the watcher can no longer vouch for the tree (a
//! rescan/error is a bump; a directory appearing under a non-recursively
//! watched parent, or a `.gitignore` / `info/exclude` edit, withdraws it for the
//! rest of the watcher's life). It is also withdrawn when the watcher is
//! dropped. thegn's own git mutations additionally bump a process-wide write
//! epoch ([`thegn_core::util::git_write_epoch`]) so a refresh the user triggers
//! straight after an in-app write never races the asynchronous inotify event.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use tokio::sync::mpsc as tokio_mpsc;

use crate::hydrate::RefreshKind;

/// Process-wide source of generation values: unique across watchers, so a
/// snapshot from an earlier watcher on the same path can never match a later
/// one's.
static NEXT_GEN: AtomicU64 = AtomicU64::new(1);

/// One watcher's claim to cover its worktree.
pub(crate) struct Coverage {
    generation: AtomicU64,
    live: AtomicBool,
}

impl Coverage {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(NEXT_GEN.fetch_add(1, Ordering::SeqCst)),
            live: AtomicBool::new(false),
        }
    }

    fn bump(&self) {
        self.generation
            .store(NEXT_GEN.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
    }

    fn withdraw(&self) {
        self.live.store(false, Ordering::SeqCst);
    }

    /// Classify one watcher callback result; see the module docs.
    fn observe(
        &self,
        res: &notify::Result<Event>,
        nonrec: &std::collections::HashSet<PathBuf>,
        ignore: &ignore::gitignore::Gitignore,
    ) {
        use notify::EventKind;
        let ev = match res {
            Ok(ev) => ev,
            Err(_) => {
                self.bump(); // the backend lost track of something
                return;
            }
        };
        if matches!(ev.kind, EventKind::Access(_)) {
            return; // opens/closes without a write cannot change any answer
        }
        self.bump();
        if matches!(ev.kind, EventKind::Create(_)) {
            for p in &ev.paths {
                let parent_unrecursed = p.parent().is_some_and(|d| nonrec.contains(d));
                if parent_unrecursed && p.is_dir() && !crate::git_watch::prune_dir(p, ignore) {
                    // A new directory under a non-recursive watch is never
                    // registered, so edits inside it would go unseen.
                    self.withdraw();
                }
            }
        }
        // The prune plan and the exclude rules were derived at registration.
        if ev.paths.iter().any(|p| {
            p.file_name().is_some_and(|n| n == ".gitignore")
                || (p.file_name().is_some_and(|n| n == "exclude")
                    && p.parent()
                        .and_then(Path::file_name)
                        .is_some_and(|d| d == "info"))
        }) {
            self.withdraw();
        }
    }
}

/// Dropped with the watcher's event closure: when the watcher goes away its
/// claim goes with it.
struct CoverGuard {
    cov: Arc<Coverage>,
    path: PathBuf,
}

impl Drop for CoverGuard {
    fn drop(&mut self) {
        self.cov.withdraw();
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        if reg
            .get(&self.path)
            .is_some_and(|c| Arc::ptr_eq(c, &self.cov))
        {
            reg.remove(&self.path);
        }
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Coverage>>> {
    static REG: std::sync::OnceLock<Mutex<HashMap<PathBuf, Arc<Coverage>>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn publish(path: &Path, cov: &Arc<Coverage>) {
    cov.live.store(true, Ordering::SeqCst);
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), cov.clone());
}

/// A point in a worktree's change history: equal prints mean no event and no
/// in-app git write happened in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WatchPrint {
    generation: u64,
    write_epoch: u64,
}

#[cfg(test)]
impl WatchPrint {
    pub(crate) fn for_test(generation: u64, write_epoch: u64) -> Self {
        Self {
            generation,
            write_epoch,
        }
    }
}

/// The current print for `path`, or `None` when no live watcher vouches for it
/// (not the active worktree, registration incomplete or withdrawn, remote loc).
/// Read this BEFORE the git reads it will gate: an event landing during the
/// reads then makes the stored snapshot's print stale, so it is re-derived.
pub(crate) fn current_print(path: &Path) -> Option<WatchPrint> {
    let cov = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned()?;
    if !cov.live.load(Ordering::SeqCst) {
        return None;
    }
    Some(WatchPrint {
        generation: cov.generation.load(Ordering::SeqCst),
        write_epoch: thegn_core::util::git_write_epoch(),
    })
}

/// No tracked file matches an ignore rule (`git ls-files -ci`). Any failure to
/// establish that counts as "not established".
fn no_tracked_ignored(cwd: &Path) -> bool {
    thegn_core::util::git_out_allow_empty(
        cwd,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--ignored",
            "--exclude-standard",
        ],
    )
    .is_some_and(|out| out.is_empty())
}

/// Where a watcher sends its refresh requests, and how it wakes the loop.
pub(crate) struct RefreshSink {
    pub(crate) tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    pub(crate) wake: Arc<dyn Fn() + Send + Sync>,
}

/// Build + register the diff fs-watcher for `cwd` (blocking: registration walks
/// the tree and runs two `git rev-parse` calls — call it off the loop). `None`
/// when no usable watcher could be attached.
pub(crate) fn build_diff_watcher(cwd: &Path, sink: RefreshSink) -> Option<RecommendedWatcher> {
    let cwd = cwd.to_path_buf();
    let RefreshSink { tx, wake } = sink;
    // Resolve this worktree's gitdir + common dir. For a *linked* worktree
    // `<cwd>/.git` is a file pointer, so the HEAD / reflog / refs that
    // signal a commit live OUTSIDE the watched tree (in the main repo's
    // `.git/worktrees/<name>` + shared `.git`); we must watch those too or
    // pane-driven commits never reach the panel. For the main checkout both
    // resolve back under `cwd` and the recursive root watch already covers
    // them. `git rev-parse` runs here, off the event loop.
    let git_dir =
        thegn_core::util::git_out(&cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])
            .map(std::path::PathBuf::from);
    let common_dir = thegn_core::util::git_out(
        &cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(std::path::PathBuf::from);
    // Roots used by the event filter to recognise git-internal paths even
    // for bare/relocated gitdirs whose path has no literal `.git` component.
    // Canonicalized because the filter compares them against *event* paths,
    // which FSEvents always reports fully resolved — see `watch_canonical`.
    let git_roots: Vec<std::path::PathBuf> = [git_dir.clone(), common_dir.clone()]
        .into_iter()
        .flatten()
        .map(|p| crate::git_watch::watch_canonical(&p))
        .collect();

    let mut last_send = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let wake = wake.clone();
    let roots = git_roots.clone();
    // Drop watcher events for gitignored paths (`target/`, `node_modules/`,
    // build outputs): a change to an ignored file can never alter
    // `git diff HEAD`, so firing a model rebuild for it is pure waste — yet a
    // cargo/sccache/agent running inside the worktree churns these constantly,
    // which was the dominant source of redundant ~Hz hydrations. Built once
    // per retarget from the worktree's root `.gitignore` (nested `.gitignore`s
    // are rare for the high-churn dirs we care about; revisit only if
    // profiling shows residual churn). A missing/unreadable `.gitignore`
    // yields an empty matcher → every path passes → unchanged behavior, so
    // remote/provider worktrees with no local `.gitignore` are unaffected.
    // NOTE: a force-added (`git add -f`) or negate-pattern (`!keep`) ignored
    // file *can* appear in the diff and would be dropped here; that's rare,
    // and the safety-net ticker still rebuilds the panel within a few seconds.
    // The matcher's ROOT must be canonical (it is matched against event
    // paths) while the `.gitignore` it reads is addressed from the real cwd —
    // on macOS those differ under any symlinked prefix, and a matcher rooted
    // at `/tmp/wt` matches nothing FSEvents delivers from `/private/tmp/wt`.
    let ignore = {
        let mut b =
            ignore::gitignore::GitignoreBuilder::new(crate::git_watch::watch_canonical(&cwd));
        let _ = b.add(cwd.join(".gitignore")); // best-effort: a malformed/missing .gitignore just means fewer ignores; the scan below still works
        b.build()
            .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty())
    };
    // Plan the registration BEFORE the matcher moves into the event closure.
    // This is the filesystem walk the old blanket `RecursiveMode::Recursive`
    // did internally — same shape, minus the gitignored subtrees, so it is
    // strictly cheaper than what it replaces.
    let plan = crate::git_watch::plan_watches(&crate::git_watch::watch_canonical(&cwd), &ignore);
    let nonrec: std::collections::HashSet<PathBuf> = plan
        .iter()
        .filter(|e| !e.recursive)
        .map(|e| e.path.clone())
        .chain(git_roots.iter().cloned())
        .collect();
    let cov = Arc::new(Coverage::new());
    let guard = CoverGuard {
        cov: cov.clone(),
        path: cwd.clone(),
    };
    let new_watcher = recommended_watcher(move |res: notify::Result<Event>| {
        guard.cov.observe(&res, &nonrec, &ignore);
        if let Ok(ev) = res
            && matches!(
                ev.kind,
                notify::EventKind::Modify(_)
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
            )
            // React to real worktree edits (the diffs this watcher exists to
            // track) AND to git-state changes — commits, checkouts, branch
            // moves, rebase/merge progress — wherever they land. The latter
            // are gated through `is_git_state_path` so the index stat-cache
            // that hydration's own `git` reads rewrite (and the object-store
            // churn on commit/gc) never match: that allowlist is what keeps
            // the old self-sustaining ~2 Hz refresh loop — which once read
            // as a freeze — from coming back.
            && (ev.paths.is_empty()
                || ev.paths.iter().any(|p| {
                    crate::git_watch::watcher_path_triggers_refresh(p, &roots, &ignore)
                }))
            && last_send.elapsed() > Duration::from_millis(500)
        {
            if tx.send(RefreshKind::Model).is_ok() {
                wake();
            }
            // The tree just changed under the ACTIVE worktree, so its line
            // count is the one that can actually be wrong. `watch: true`
            // lets the scan bypass the long `[loc] scan_interval_secs` for
            // that single path — bounded by `watch_invalidate_secs`, so a
            // save storm still recounts at most once per window.
            let _ = tx.send(RefreshKind::Loc { watch: true }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            // A branch-ref move also kicks the guarded main-checkout self-heal
            // so a checkout sitting on that branch fast-forwards its own tree
            // (external `update-ref` / a fold-actor CAS land elsewhere) without
            // waiting for a tab switch or restart.
            if ev
                .paths
                .iter()
                .any(|p| crate::git_watch::is_ref_move_path(p))
            {
                let _ = tx.send(RefreshKind::MainRefMoved); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            }
            // A checkout changed the branch: the PR cache row is the OLD
            // branch's, so look the new one up now.
            if ev
                .paths
                .iter()
                .any(|p| crate::git_watch::is_head_move_path(p))
            {
                let _ = tx.send(RefreshKind::Pr); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            }
            // A remote-tracking ref moved — the local signature of a push
            // (or fetch): kick the PR + CI caches now so the just-pushed
            // branch's checks appear without waiting for the tickers.
            // Non-forced, so `[ci] ttl_secs` still bounds subprocess churn.
            if ev
                .paths
                .iter()
                .any(|p| crate::git_watch::is_remote_ref_path(p))
            {
                let _ = tx.send(RefreshKind::Pr); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                let _ = tx.send(RefreshKind::Ci { force: false }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
                // The PR queue cares about exactly this event: a push is
                // what unblocks a PR (or is the teammate the queue must not
                // race), so re-evaluate now rather than up to a minute later.
                // Inert when the queue is off — the pass finds no rows.
                let _ = tx.send(RefreshKind::PrQueue); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            }
            last_send = Instant::now();
        }
    });
    let Ok(mut nw) = new_watcher else {
        tracing::warn!(
            target: "thegn::hydrate",
            worktree = %cwd.display(),
            "failed to construct diff fs-watcher — diff panel falls back to the 2s ticker"
        );
        return None;
    };
    // Register the root watch, pruning gitignored subtrees rather than
    // taking one blanket recursive watch (see `git_watch::plan_watches` for
    // why: `notify`'s recursion is one inotify watch per directory, so the
    // old blanket watch registered every `target/` and `.claude/worktrees/`
    // directory — 114,701 watches on this repo — and then paid a gitignore
    // match per rustc write to discard the event it should never have
    // subscribed to).
    //
    // The plan was walked from the CANONICAL root: the matcher is rooted
    // there (`watch_canonical` above), and on macOS an un-canonicalized walk
    // would match nothing and prune nothing.
    let mut registered = 0usize;
    for entry in &plan {
        let mode = if entry.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        if nw.watch(&entry.path, mode).is_ok() {
            registered += 1;
        }
    }
    tracing::debug!(
        target: "thegn::hydrate",
        worktree = %cwd.display(),
        planned = plan.len(),
        registered,
        "diff fs-watch registered (gitignored subtrees pruned)"
    );
    // Every registration failing is the ENOSPC case: on a Linux machine whose
    // `fs.inotify.max_user_watches` is exhausted (large monorepos, many
    // instances) `watch` fails with ENOSPC — previously the thread just exited
    // silently, and `retarget`'s guard suppressed every retry, so the active
    // worktree lost sub-second diff/ref-move/push detection for the rest of
    // the session with no diagnostic. Fall back to a NON-recursive watch on
    // the worktree root (one watch, not thousands): coarser (top-level edits
    // + git-state paths under it still fire) but keeps the ref-move / push
    // kicks working, and — crucially — still sends a watcher back so the loop
    // adopts it (a later retarget away-and-back re-attempts the full plan).
    let recursive_ok = registered > 0;
    if !recursive_ok {
        let fallback_ok = nw.watch(&cwd, RecursiveMode::NonRecursive).is_ok();
        // The likely cause is OS-specific, and naming the wrong mechanism
        // sends the reader down a dead end: `notify` rides inotify on Linux
        // but FSEvents on macOS (which has no per-watch quota to exhaust —
        // there, a failure is a path/permission problem).
        let hint = if cfg!(target_os = "linux") {
            "inotify watches exhausted?"
        } else {
            "path unreadable or unwatchable?"
        };
        tracing::warn!(
            target: "thegn::hydrate",
            worktree = %cwd.display(),
            fallback_ok,
            "recursive diff fs-watch registration failed ({hint}) — \
             fell back to a non-recursive root watch"
        );
        if !fallback_ok {
            // Nothing attached at all — don't ship a dead watcher; the 2s
            // safety-net ticker covers diff refresh until the next retarget.
            return None;
        }
    }
    // Linked worktree: add targeted watches on the external gitdir's
    // state-bearing subtrees. Non-recursive on the gitdir roots (so we
    // never descend into `objects/`, which floods on every commit/gc);
    // `logs/` (reflog) and `refs/` are small and never written by
    // hydration's read-only git, so a recursive watch there is storm-
    // safe. Any root already under `cwd` is skipped — the recursive
    // root watch above covers the main checkout.
    // An external root whose state-bearing watches did not all register leaves
    // pane-driven ref/index writes unobserved, so it withholds the coverage
    // claim below (the refresh itself keeps today's best-effort behaviour).
    let mut external_ok = true;
    for root in [git_dir.as_ref(), common_dir.as_ref()]
        .into_iter()
        .flatten()
    {
        if root.starts_with(&cwd) {
            continue;
        }
        external_ok &= nw.watch(root, RecursiveMode::NonRecursive).is_ok();
        // Reflog is best-effort (absent when `core.logAllRefUpdates` is off);
        // refs/ carries every ref and `refs/stash`, so it must register.
        let _ = nw.watch(&root.join("logs"), RecursiveMode::Recursive); // best-effort: watch registration: reflog may legitimately not exist; refs/ below is the state-bearing watch
        external_ok &= nw
            .watch(&root.join("refs"), RecursiveMode::Recursive)
            .is_ok();
        // Shared `info/exclude` changes what `status` lists as untracked.
        let _ = nw.watch(&root.join("info"), RecursiveMode::NonRecursive); // best-effort: watch registration: info/ is optional; its absence means no exclude file to watch
    }
    // The generation is a claim "nothing under this worktree changed without an
    // event". It is only made when that is true: the full plan registered
    // (no fallback to the bare root), every external root registered, the gitdir
    // and common dir resolved, and no TRACKED file sits under a gitignore
    // pattern (the plan prunes ignored subtrees, so an edit to a force-added
    // file there would be invisible).
    if recursive_ok
        && registered == plan.len()
        && external_ok
        && git_dir.is_some()
        && common_dir.is_some()
        && no_tracked_ignored(&cwd)
    {
        publish(&cwd, &cov);
    }
    Some(nw)
}

#[cfg(test)]
mod tests {
    use super::*;

    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let ok = thegn_core::util::git_cmd(dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.t",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            ok.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tg-diffwatch-{}-{name}", std::process::id()));
        if p.exists() {
            std::fs::remove_dir_all(&p).unwrap();
        }
        std::fs::create_dir_all(&p).unwrap();
        crate::git_watch::watch_canonical(&p)
    }

    fn repo(base: &Path, name: &str, ignore_target: bool) -> PathBuf {
        let r = base.join(name);
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        if ignore_target {
            std::fs::write(r.join(".gitignore"), "target/\n").unwrap();
            std::fs::create_dir_all(r.join("target/debug")).unwrap();
            std::fs::write(r.join("target/debug/x"), "x").unwrap();
        }
        git(&r, &["add", "."]);
        git(&r, &["commit", "-q", "-m", "init"]);
        r
    }

    fn watch(cwd: &Path) -> RecommendedWatcher {
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        // The receiver is dropped: sends fail, which the callback tolerates.
        build_diff_watcher(
            cwd,
            RefreshSink {
                tx,
                wake: Arc::new(|| {}),
            },
        )
        .expect("watcher registers")
    }

    fn print(cwd: &Path) -> Option<WatchPrint> {
        current_print(cwd)
    }

    /// Wait until the print has not moved for `quiet`, so the previous step's
    /// trailing events cannot be mistaken for the next step's.
    fn settle(cwd: &Path) -> WatchPrint {
        let quiet = Duration::from_millis(400);
        let mut last = print(cwd).expect("covered");
        let mut since = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(10);
        while since.elapsed() < quiet {
            assert!(Instant::now() < deadline, "never settled");
            std::thread::sleep(Duration::from_millis(25));
            let now = print(cwd).expect("covered");
            if now != last {
                last = now;
                since = Instant::now();
            }
        }
        last
    }

    fn advances(cwd: &Path, what: &str, op: impl FnOnce()) {
        let before = settle(cwd);
        op();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if print(cwd).is_some_and(|p| p != before) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "`{what}` did not advance the generation (stale panel)"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The reads hydration itself runs (git_cmd pins GIT_OPTIONAL_LOCKS=0) must
    /// not move the generation, or an idle panel would re-read forever.
    fn hydration_reads(cwd: &Path) {
        for args in [
            &["status", "--porcelain=v1", "-z", "--no-renames"][..],
            &["diff", "--numstat", "HEAD"],
            &["stash", "list", "--format=%h"],
            &["rev-parse", "--abbrev-ref", "HEAD"],
        ] {
            assert!(thegn_core::util::git_out_allow_empty(cwd, args).is_some());
        }
    }

    fn exercise_every_writer(cwd: &Path) {
        let a = cwd.join("a.txt");
        let start = thegn_core::util::git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();
        advances(cwd, "edit a tracked file", || {
            std::fs::write(&a, "one\ntwo\n").unwrap();
        });
        advances(cwd, "create an untracked file", || {
            std::fs::write(cwd.join("new.txt"), "n\n").unwrap();
        });
        advances(cwd, "git add (index write)", || git(cwd, &["add", "a.txt"]));
        advances(cwd, "git restore --staged (index write)", || {
            git(cwd, &["restore", "--staged", "a.txt"])
        });
        advances(cwd, "git stash push", || {
            git(cwd, &["stash", "push", "-u", "-q"])
        });
        advances(cwd, "git stash drop", || git(cwd, &["stash", "drop", "-q"]));
        advances(cwd, "edit after the stash", || {
            std::fs::write(&a, "one\nthree\n").unwrap();
        });
        advances(cwd, "git add before commit", || git(cwd, &["add", "a.txt"]));
        advances(cwd, "git commit", || {
            git(cwd, &["commit", "-q", "-m", "c2"])
        });
        advances(cwd, "git checkout -b", || {
            git(cwd, &["checkout", "-q", "-b", "other"])
        });
        advances(cwd, "git checkout back", || {
            git(cwd, &["checkout", "-q", &start])
        });
        advances(cwd, "delete a file", || std::fs::remove_file(&a).unwrap());
        // Idle: hydration's own reads leave the print alone.
        let before = settle(cwd);
        hydration_reads(cwd);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            print(cwd),
            Some(before),
            "hydration's own git reads advanced the generation (self-sustaining refresh)"
        );
    }

    #[test]
    fn every_writer_advances_the_generation_in_a_main_checkout() {
        let base = scratch("main");
        let r = repo(&base, "r", true);
        let _w = watch(&r);
        assert!(print(&r).is_some(), "registration was complete");
        exercise_every_writer(&r);
    }

    #[test]
    fn every_writer_advances_the_generation_in_a_linked_worktree() {
        let base = scratch("linked");
        let r = repo(&base, "r", true);
        let wt = base.join("wt");
        git(
            &r,
            &["worktree", "add", "-q", "-b", "side", wt.to_str().unwrap()],
        );
        let wt = crate::git_watch::watch_canonical(&wt);
        let _w = watch(&wt);
        assert!(print(&wt).is_some(), "registration was complete");
        exercise_every_writer(&wt);
        // A write made from the MAIN checkout that moves this worktree's branch.
        advances(&wt, "a ref created from another worktree", || {
            git(&r, &["branch", "zzz", "main"])
        });
    }

    #[test]
    fn an_in_app_git_write_advances_the_print_without_any_fs_event() {
        let base = scratch("epoch");
        let r = repo(&base, "r", false);
        let _w = watch(&r);
        let before = settle(&r);
        drop(thegn_core::util::GitWriteScope::begin());
        assert_ne!(print(&r), Some(before));
    }

    #[test]
    fn no_print_without_a_watcher_and_none_after_it_drops() {
        let base = scratch("drop");
        let r = repo(&base, "r", false);
        assert_eq!(print(&r), None, "unwatched path");
        let w = watch(&r);
        assert!(print(&r).is_some());
        drop(w);
        let deadline = Instant::now() + Duration::from_secs(5);
        while print(&r).is_some() {
            assert!(Instant::now() < deadline, "claim outlived its watcher");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_new_directory_under_a_non_recursive_watch_withdraws_the_claim() {
        let base = scratch("newdir");
        // `target/` is ignored, so the root is watched non-recursively.
        let r = repo(&base, "r", true);
        let _w = watch(&r);
        assert!(print(&r).is_some());
        std::fs::create_dir(r.join("brand_new")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while print(&r).is_some() {
            assert!(Instant::now() < deadline, "claim not withdrawn");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn editing_gitignore_withdraws_the_claim() {
        let base = scratch("gitignore");
        let r = repo(&base, "r", true);
        let _w = watch(&r);
        assert!(print(&r).is_some());
        std::fs::write(r.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while print(&r).is_some() {
            assert!(Instant::now() < deadline, "claim not withdrawn");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_tracked_file_under_an_ignore_rule_means_no_claim() {
        let base = scratch("forced");
        let r = repo(&base, "r", true);
        std::fs::write(r.join("target/forced.txt"), "f").unwrap();
        git(&r, &["add", "-f", "target/forced.txt"]);
        git(&r, &["commit", "-q", "-m", "force-add"]);
        let _w = watch(&r);
        assert_eq!(
            print(&r),
            None,
            "an edit to the force-added file would be invisible to the pruned plan"
        );
    }

    /// End to end: while snapshots are being reused, every kind of idle change
    /// still reaches the panel on the next hydration.
    #[test]
    fn build_panel_follows_every_change_while_reusing_snapshots() {
        use crate::panel::Stage;
        let base = scratch("panel");
        let state = base.join("state");
        std::fs::create_dir_all(state.join("thegn")).unwrap();
        let _env = crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", state.to_str().unwrap())]);
        let db = thegn_core::db::Db::open_at(&state.join("thegn/thegn.db")).unwrap();
        let cfg = thegn_core::config::Config::default();
        let hints = crate::hydrate::HydrateHints::default();
        let r = repo(&base, "r", true);
        let _w = watch(&r);
        assert!(print(&r).is_some());

        let panel = |why: &str| {
            let at = settle(&r);
            let p = crate::hydrate::build_panel(&r, &db, &hints, &cfg);
            // The snapshot the build stored is exactly the one a repeat uses.
            let again = crate::panel_git_cache::get(&r, at);
            assert!(
                again.is_some(),
                "{why}: build stored a snapshot at the settled print"
            );
            let twice = crate::hydrate::build_panel(&r, &db, &hints, &cfg);
            assert_eq!(
                twice.changes.len(),
                p.changes.len(),
                "{why}: a reused snapshot is identical"
            );
            p
        };
        let row = |p: &crate::panel::PanelData, path: &str| {
            p.changes.iter().find(|c| c.path == path).map(|c| c.stage)
        };

        assert!(panel("clean").changes.is_empty());
        advances(&r, "edit", || {
            std::fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();
        });
        let p = panel("dirty");
        assert_eq!(
            row(&p, "a.txt"),
            Some(Stage::Unstaged),
            "dirty edit appears"
        );
        std::fs::write(r.join("u.txt"), "u\n").unwrap();
        let p = panel("untracked");
        assert_eq!(row(&p, "u.txt"), Some(Stage::Untracked));
        git(&r, &["add", "a.txt"]);
        let p = panel("staged");
        assert_eq!(row(&p, "a.txt"), Some(Stage::Staged), "git add from a pane");
        git(&r, &["stash", "push", "-q"]);
        let p = panel("stashed");
        assert_eq!(p.stash_count, 1, "stash push");
        assert_eq!(row(&p, "a.txt"), None, "stashed file left the changes list");
        git(&r, &["stash", "drop", "-q"]);
        assert_eq!(panel("dropped").stash_count, 0, "stash drop");
        git(&r, &["checkout", "-q", "-b", "left"]);
        std::fs::write(r.join("a.txt"), "left\n").unwrap();
        git(&r, &["commit", "-qam", "left"]);
        git(&r, &["checkout", "-q", "main"]);
        std::fs::write(r.join("a.txt"), "main\n").unwrap();
        git(&r, &["commit", "-qam", "main"]);
        let merged = thegn_core::util::git_cmd(&r)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.t",
                "merge",
                "left",
            ])
            .output()
            .unwrap();
        assert!(!merged.status.success(), "the merge must conflict");
        let p = panel("conflict");
        assert_eq!(row(&p, "a.txt"), Some(Stage::Conflict), "conflict appears");
        assert!(p.merge.is_some(), "merge banner appears");
        git(&r, &["merge", "--abort"]);
        let p = panel("aborted");
        assert!(
            p.merge.is_none() && row(&p, "a.txt").is_none(),
            "abort clears it"
        );
        // The branch switch is visible too.
        git(&r, &["checkout", "-q", "left"]);
        assert_eq!(panel("switched").branch, "left");
    }

    #[test]
    fn a_missing_path_registers_nothing() {
        let base = scratch("missing");
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        let w = build_diff_watcher(
            &base.join("nope"),
            RefreshSink {
                tx,
                wake: Arc::new(|| {}),
            },
        );
        assert!(w.is_none() || print(&base.join("nope")).is_none());
    }
}
