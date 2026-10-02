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
    /// Bumped only by branch/tag/packed-refs moves: lets the loop notice, in O(1)
    /// on its existing model tick, that a ref moved even when the event's own
    /// `MainRefMoved` was lost to the 500 ms refresh throttle.
    ref_generation: AtomicU64,
    live: AtomicBool,
}

/// What the event classifier needs to know about the registration.
pub(crate) struct Rules {
    /// Non-recursively watched directories (plan entries + git roots): a
    /// directory appearing directly under one is never registered.
    nonrec: std::collections::HashSet<PathBuf>,
    /// Paths whose removal/rename means the watched identity itself is gone or
    /// replaced: the worktree root, every plan entry, every git root.
    protected: std::collections::HashSet<PathBuf>,
    ignore: ignore::gitignore::Gitignore,
    /// Canonical git dir + common dir.
    roots: Vec<PathBuf>,
}

/// Directories git creates directly under a git dir during ordinary operations
/// (sequencer state, rerere) that nothing gated on the generation reads, so
/// their appearance must not withdraw the claim for the rest of the watcher's
/// life. `logs/`, `refs/`, `info/`, `modules/` and anything unknown still do.
const EXEMPT_GIT_DIR_CHILDREN: &[&str] = &["rebase-merge", "rebase-apply", "sequencer", "rr-cache"];

impl Rules {
    fn is_git_root(&self, p: &Path) -> bool {
        self.roots.iter().any(|r| r == p) || p.file_name().is_some_and(|n| n == ".git")
    }
}

impl Coverage {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(NEXT_GEN.fetch_add(1, Ordering::SeqCst)),
            ref_generation: AtomicU64::new(NEXT_GEN.fetch_add(1, Ordering::SeqCst)),
            live: AtomicBool::new(false),
        }
    }

    fn bump(&self) {
        self.generation
            .store(NEXT_GEN.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
    }

    fn bump_refs(&self) {
        self.ref_generation
            .store(NEXT_GEN.fetch_add(1, Ordering::SeqCst), Ordering::SeqCst);
    }

    fn withdraw(&self) {
        self.live.store(false, Ordering::SeqCst);
    }

    /// Classify one watcher callback result; see the module docs. Anything the
    /// watcher cannot PROVE it saw completely withdraws the claim.
    fn observe(&self, res: &notify::Result<Event>, rules: &Rules) {
        use notify::EventKind;
        use notify::event::{ModifyKind, RenameMode};
        let ev = match res {
            Ok(ev) => ev,
            Err(_) => {
                // A backend error (inotify watch limit, a failed watch on a new
                // dir) means events may be missing from now on.
                self.bump();
                self.withdraw();
                return;
            }
        };
        if matches!(ev.kind, EventKind::Access(_)) {
            return; // opens/closes without a write cannot change any answer
        }
        if ev.need_rescan() {
            // Queue overflow: events were dropped, so nothing can be vouched for.
            self.bump();
            self.withdraw();
            return;
        }
        // Two kinds of git-dir file change nothing thegn reads:
        // - `FETCH_HEAD`, the write-only record of the last fetch: git rewrites
        //   it on EVERY fetch, changed refs or not (a fetch that moves a ref also
        //   writes that ref, which does bump);
        // - `*.lock` files: git creates one, then RENAMES it onto the target (the
        //   target's own event bumps) or deletes it unused (no state change; an
        //   up-to-date fetch does exactly that to `refs/remotes/origin/HEAD.lock`).
        // Only inside a git dir: `Cargo.lock` in the tree is a real edit.
        if !ev.paths.is_empty() && ev.paths.iter().all(|p| is_git_bookkeeping(p, &rules.roots)) {
            return;
        }
        self.bump();
        if ev
            .paths
            .iter()
            .any(|p| crate::git_watch::is_ref_move_path(p))
        {
            self.bump_refs();
        }
        // Free when off (a cached-interest check): `THEGN_LOG=thegn::watch=debug`
        // names what moved the generation, which is the first question whenever
        // an idle backstop fires.
        tracing::debug!(
            target: "thegn::watch",
            kind = ?ev.kind,
            path = ?ev.paths.first(),
            "change generation bumped"
        );
        let name_event = matches!(ev.kind, EventKind::Modify(ModifyKind::Name(_)));
        let arrives = matches!(ev.kind, EventKind::Create(_))
            || matches!(
                ev.kind,
                EventKind::Modify(ModifyKind::Name(
                    RenameMode::To | RenameMode::Both | RenameMode::Any | RenameMode::Other
                ))
            );
        // The watched identity itself removed or replaced (worktree deleted and
        // recreated at the same path, a git dir swapped): the watches are on the
        // OLD inodes.
        if (matches!(ev.kind, EventKind::Remove(_)) || name_event)
            && ev.paths.iter().any(|p| rules.protected.contains(p))
        {
            self.withdraw();
        }
        if arrives {
            for p in &ev.paths {
                let parent_unrecursed = p.parent().is_some_and(|d| rules.nonrec.contains(d));
                if !parent_unrecursed || !p.is_dir() {
                    continue;
                }
                if crate::git_watch::prune_dir(p, &rules.ignore) {
                    continue;
                }
                let exempt = p.parent().is_some_and(|d| rules.is_git_root(d))
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| EXEMPT_GIT_DIR_CHILDREN.contains(&n));
                if !exempt {
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

/// A file under a git dir whose change cannot alter any read (see
/// [`Coverage::observe`]): the fetch record and transient `*.lock` files.
fn is_git_bookkeeping(p: &Path, roots: &[PathBuf]) -> bool {
    let in_git_dir = crate::git_watch::in_dot_git(p) || roots.iter().any(|r| p.starts_with(r));
    in_git_dir
        && p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "FETCH_HEAD" || n.ends_with(".lock"))
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

/// The ref-move generation for `path` while a live watcher vouches for it.
pub(crate) fn current_ref_generation(path: &Path) -> Option<u64> {
    let cov = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned()?;
    cov.live
        .load(Ordering::SeqCst)
        .then(|| cov.ref_generation.load(Ordering::SeqCst))
}

/// A consumer's record of the print at which it last COMPLETED a pass over a
/// path, so a periodic backstop can skip a pass when nothing has changed since
/// (the generation is monotonic and covers every event, so equal prints mean an
/// identical tree and git dir). No live print => never skips: callers fall back
/// to their old cadence exactly.
pub(crate) struct Seen(Mutex<Option<HashMap<PathBuf, WatchPrint>>>);

impl Seen {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// The current print for `path` and whether the last completed pass already
    /// saw exactly it. Take the print BEFORE the pass and hand it to [`mark`].
    ///
    /// [`mark`]: Seen::mark
    pub(crate) fn check(&self, path: &Path) -> (Option<WatchPrint>, bool) {
        let now = current_print(path);
        (now, self.matches(path, now))
    }

    /// Whether the last completed pass over `path` saw exactly `now` (a live
    /// print). Split from [`check`](Seen::check) so callers can inject the print.
    pub(crate) fn matches(&self, path: &Path, now: Option<WatchPrint>) -> bool {
        now.is_some_and(|p| {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|m| m.get(path))
                .is_some_and(|seen| *seen == p)
        })
    }

    /// Whether any pass over `path` has completed under a live print yet. A
    /// consumer's FIRST pass under a print is a catch-up for everything that
    /// changed between its earlier print-less pass (startup, before the watcher
    /// registered) and the registration, so it must not be skipped or debounced.
    pub(crate) fn marked(&self, path: &Path) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|m| m.contains_key(path))
    }

    /// Record that a pass over `path` COMPLETED having started at `print`.
    pub(crate) fn mark(&self, path: &Path, print: Option<WatchPrint>) {
        let Some(print) = print else {
            return;
        };
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let m = m.get_or_insert_with(HashMap::new);
        if m.len() >= 16 && !m.contains_key(path) {
            m.clear();
        }
        m.insert(path.to_path_buf(), print);
    }
}

/// After the watches are registered: every directory directly under a
/// non-recursively watched plan entry is itself a plan entry or pruned. One that
/// is neither appeared between the plan walk and registration, so it is
/// unwatched, and the claim must not be made.
fn plan_children_covered(
    plan: &[crate::git_watch::WatchPlanEntry],
    ignore: &ignore::gitignore::Gitignore,
) -> bool {
    let planned: std::collections::HashSet<&Path> = plan.iter().map(|e| e.path.as_path()).collect();
    for entry in plan.iter().filter(|e| !e.recursive) {
        let Ok(rd) = std::fs::read_dir(&entry.path) else {
            return false;
        };
        for child in rd.flatten() {
            if !child.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let p = child.path();
            if !planned.contains(p.as_path()) && !crate::git_watch::prune_dir(&p, ignore) {
                return false;
            }
        }
    }
    true
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
    let protected: std::collections::HashSet<PathBuf> = plan
        .iter()
        .map(|e| e.path.clone())
        .chain(git_roots.iter().cloned())
        .chain(std::iter::once(crate::git_watch::watch_canonical(&cwd)))
        .collect();
    // After registration: every directory directly under a non-recursively
    // watched one must be a plan entry or pruned, else it appeared between the
    // plan walk and the watches and is unwatched.
    let ignore_check = ignore.clone();
    let rules = Rules {
        nonrec,
        protected,
        ignore,
        roots,
    };
    let kick_tx = tx.clone();
    let kick_wake = wake.clone();
    let cov = Arc::new(Coverage::new());
    let guard = CoverGuard {
        cov: cov.clone(),
        path: cwd.clone(),
    };
    let new_watcher = recommended_watcher(move |res: notify::Result<Event>| {
        guard.cov.observe(&res, &rules);
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
                    crate::git_watch::watcher_path_triggers_refresh(p, &rules.roots, &rules.ignore)
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
    // Beyond registration, the claim also needs: the plan to have been complete
    // when the watches landed; an event source that can be trusted on this
    // filesystem; a ref backend whose changes are files; and no submodules (their
    // gitdirs under `<gitdir>/modules` are not watched).
    let unwatched_reason = if !recursive_ok || registered != plan.len() || !external_ok {
        Some("registration incomplete")
    } else if git_dir.is_none() || common_dir.is_none() {
        Some("git dirs unresolved")
    } else if !plan_children_covered(&plan, &ignore_check) {
        Some("a directory appeared between the plan walk and registration")
    } else if crate::platform::fs_kind::unwatchable(&cwd)
        || git_roots
            .iter()
            .any(|r| crate::platform::fs_kind::unwatchable(r))
    {
        Some("network/userspace filesystem")
    } else if thegn_core::git_memo::is_reftable(&cwd) {
        Some("reftable ref backend")
    } else if cwd.join(".gitmodules").exists() {
        Some("submodules")
    } else if !no_tracked_ignored(&cwd) {
        Some("tracked file under an ignore rule")
    } else {
        None
    };
    if let Some(reason) = unwatched_reason {
        tracing::debug!(
            target: "thegn::watch",
            worktree = %cwd.display(),
            reason,
            "no change-generation claim"
        );
    } else {
        publish(&cwd, &cov);
        // Registration completing is an EVENT: everything before it was seen by
        // pre-watcher passes (startup heal, crawl, commit refresh) and anything
        // that changed between those passes and now went unobserved. Ask for the
        // catch-up passes once, now, through the same requests a real change
        // sends (the loop's MainRefMoved arm + a model refresh whose crawl and
        // commit-cache gates treat "first pass under a print" as due), instead
        // of leaving them to the 20 s backstop. One wake, only because work was
        // actually enqueued.
        let sent_ref = kick_tx.send(RefreshKind::MainRefMoved).is_ok(); // best-effort: send: the consumer may be gone
        let sent_model = kick_tx.send(RefreshKind::Model).is_ok(); // best-effort: send: the consumer may be gone
        tracing::debug!(
            target: "thegn::watch",
            worktree = %cwd.display(),
            "claim published; catch-up passes requested"
        );
        if sent_ref || sent_model {
            kick_wake();
        }
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

    // test code: fixture setup, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
        thegn_core::util::git_cmd(dir)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
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

    /// A no-op fetch only rewrites FETCH_HEAD (the startup auto-fetch on an
    /// up-to-date repo): that must not re-arm every backstop; a fetch that moves
    /// a ref must.
    #[test]
    fn fetch_head_alone_is_not_a_change_but_a_moved_ref_is() {
        let base = scratch("fetch");
        let origin = repo(&base, "origin", false);
        let r = base.join("clone");
        git(
            &base,
            &["clone", "-q", origin.to_str().unwrap(), r.to_str().unwrap()],
        );
        let _w = watch(&r);
        assert!(print(&r).is_some());
        let before = settle(&r);
        git(&r, &["fetch", "-q", "origin"]);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            print(&r),
            Some(before),
            "an up-to-date fetch (FETCH_HEAD only) advanced the generation"
        );
        std::fs::write(origin.join("b.txt"), "b\n").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-q", "-m", "more"]);
        advances(&r, "a fetch that moves refs/remotes", || {
            git(&r, &["fetch", "-q", "origin"])
        });
    }

    #[test]
    fn bookkeeping_is_only_git_dir_lock_and_fetch_files() {
        let roots = vec![PathBuf::from("/elsewhere/gitdir")];
        let b = |p: &str| is_git_bookkeeping(Path::new(p), &roots);
        assert!(b("/r/.git/FETCH_HEAD"));
        assert!(b("/r/.git/index.lock"));
        assert!(b("/r/.git/refs/remotes/origin/HEAD.lock"));
        assert!(b("/r/.git/worktrees/w/FETCH_HEAD"));
        assert!(b("/elsewhere/gitdir/refs/heads/x.lock"), "relocated gitdir");
        // The rename target and ordinary files are state.
        assert!(!b("/r/.git/index"));
        assert!(!b("/r/.git/HEAD"));
        assert!(!b("/r/.git/refs/heads/main"));
        // A tracked `Cargo.lock` (or a FETCH_HEAD in the tree) is a real edit.
        assert!(!b("/r/Cargo.lock"));
        assert!(!b("/r/src/FETCH_HEAD"));
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
            let again = crate::panel_git_cache::get(
                &r,
                (
                    at,
                    thegn_core::git_memo::global_git_print().expect("global layer"),
                ),
            );
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
        assert!(
            !git_succeeds(
                &r,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t.t",
                    "merge",
                    "left"
                ],
            ),
            "the merge must conflict"
        );
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
    fn seen_skips_only_an_unchanged_live_print_and_only_after_a_completed_pass() {
        let base = scratch("seen");
        let r = repo(&base, "r", false);
        let seen = Seen::new();
        // No watcher: never skips, nothing to mark.
        assert_eq!(seen.check(&r), (None, false));
        let _w = watch(&r);
        let (p0, unchanged) = seen.check(&r);
        assert!(p0.is_some() && !unchanged, "no pass completed yet");
        let p0 = settle(&r);
        let (p1, unchanged) = seen.check(&r);
        assert_eq!(p1, Some(p0));
        assert!(!unchanged, "a pass that has not completed never skips");
        seen.mark(&r, p1);
        assert!(
            seen.check(&r).1,
            "a completed pass at this print skips the next"
        );
        advances(&r, "an edit", || {
            std::fs::write(r.join("a.txt"), "changed\n").unwrap();
        });
        assert!(!seen.check(&r).1, "a change re-arms the pass");
        // An in-app git write re-arms it without any fs event.
        let (p, _) = seen.check(&r);
        seen.mark(&r, p);
        let settled = settle(&r);
        seen.mark(&r, Some(settled));
        assert!(seen.check(&r).1);
        drop(thegn_core::util::GitWriteScope::begin());
        assert!(!seen.check(&r).1);
    }

    /// Registration completing kicks the catch-up passes once (and only when a
    /// claim was published): MainRefMoved + a model refresh, with one wake.
    #[test]
    fn a_published_claim_kicks_the_catch_up_passes_once() {
        let base = scratch("kick");
        let r = repo(&base, "r", false);
        let (tx, mut rx) = tokio_mpsc::unbounded_channel();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w2 = wakes.clone();
        let _w = build_diff_watcher(
            &r,
            RefreshSink {
                tx,
                wake: Arc::new(move || {
                    w2.fetch_add(1, Ordering::SeqCst);
                }),
            },
        )
        .expect("registers");
        assert!(print(&r).is_some());
        let mut kinds = Vec::new();
        while let Ok(k) = rx.try_recv() {
            kinds.push(format!("{k:?}"));
        }
        assert_eq!(kinds, ["MainRefMoved", "Model"], "exactly the two requests");
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "one wake for the kick");
        // No claim (tracked file under an ignore rule) => no kick, no wake.
        let r2 = repo(&base, "r2", true);
        std::fs::write(r2.join("target/f.txt"), "f").unwrap();
        git(&r2, &["add", "-f", "target/f.txt"]);
        git(&r2, &["commit", "-q", "-m", "f"]);
        let (tx2, mut rx2) = tokio_mpsc::unbounded_channel();
        let wakes2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w3 = wakes2.clone();
        let _w2 = build_diff_watcher(
            &r2,
            RefreshSink {
                tx: tx2,
                wake: Arc::new(move || {
                    w3.fetch_add(1, Ordering::SeqCst);
                }),
            },
        );
        assert!(print(&r2).is_none());
        assert!(rx2.try_recv().is_err());
        assert_eq!(wakes2.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn seen_marked_reports_a_completed_pass() {
        let seen = Seen::new();
        let p = Path::new("/tmp/tg-seen-marked");
        assert!(!seen.marked(p));
        seen.mark(p, None);
        assert!(!seen.marked(p), "a print-less pass is not a marked one");
        seen.mark(p, Some(WatchPrint::for_test(1, 1)));
        assert!(seen.marked(p));
    }

    // ---- classifier: staleness proofs (pure; no inotify) -------------------

    fn live_cov() -> Coverage {
        let c = Coverage::new();
        c.live.store(true, Ordering::SeqCst);
        c
    }

    fn rules_for(base: &Path) -> Rules {
        let root = base.join("wt");
        let gitdir = root.join(".git");
        std::fs::create_dir_all(&gitdir).unwrap();
        let nonrec: std::collections::HashSet<PathBuf> =
            [root.clone(), gitdir.clone()].into_iter().collect();
        let protected: std::collections::HashSet<PathBuf> = [
            root.clone(),
            gitdir.clone(),
            gitdir.join("refs"),
            root.join("src"),
        ]
        .into_iter()
        .collect();
        Rules {
            nonrec,
            protected,
            ignore: ignore::gitignore::Gitignore::empty(),
            roots: vec![gitdir],
        }
    }

    fn ev(kind: notify::EventKind, paths: &[&Path]) -> notify::Result<notify::Event> {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(p.to_path_buf());
        }
        Ok(e)
    }

    #[test]
    fn a_backend_error_or_overflow_withdraws_the_claim() {
        let base = scratch("cls-err");
        let rules = rules_for(&base);
        let c = live_cov();
        c.observe(&Err(notify::Error::generic("inotify watch limit")), &rules);
        assert!(!c.live.load(Ordering::SeqCst), "error must withdraw");
        let c = live_cov();
        let overflow =
            notify::Event::new(notify::EventKind::Other).set_flag(notify::event::Flag::Rescan);
        c.observe(&Ok(overflow), &rules);
        assert!(!c.live.load(Ordering::SeqCst), "Q_OVERFLOW must withdraw");
    }

    #[test]
    fn a_directory_renamed_into_a_non_recursive_parent_withdraws() {
        use notify::EventKind::Modify;
        use notify::event::{ModifyKind::Name, RenameMode};
        let base = scratch("cls-rename");
        let rules = rules_for(&base);
        let root = base.join("wt");
        let moved = root.join("moved_in");
        std::fs::create_dir_all(&moved).unwrap();
        let file = root.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        for mode in [RenameMode::To, RenameMode::Both, RenameMode::Any] {
            let c = live_cov();
            let paths: Vec<&Path> = if mode == RenameMode::Both {
                vec![Path::new("/elsewhere/old"), moved.as_path()]
            } else {
                vec![moved.as_path()]
            };
            c.observe(&ev(Modify(Name(mode)), &paths), &rules);
            assert!(
                !c.live.load(Ordering::SeqCst),
                "{mode:?} of a dir must withdraw"
            );
        }
        // A FILE moved in is an ordinary edit.
        let c = live_cov();
        c.observe(&ev(Modify(Name(RenameMode::To)), &[&file]), &rules);
        assert!(c.live.load(Ordering::SeqCst));
    }

    #[test]
    fn removing_or_replacing_a_watched_root_withdraws() {
        use notify::EventKind::{Modify, Remove};
        use notify::event::{ModifyKind::Name, RemoveKind, RenameMode};
        let base = scratch("cls-remove");
        let rules = rules_for(&base);
        let root = base.join("wt");
        for gone in [
            root.clone(),
            root.join(".git"),
            root.join(".git/refs"),
            root.join("src"),
        ] {
            let c = live_cov();
            c.observe(&ev(Remove(RemoveKind::Folder), &[&gone]), &rules);
            assert!(!c.live.load(Ordering::SeqCst), "remove {gone:?}");
            let c = live_cov();
            c.observe(&ev(Modify(Name(RenameMode::From)), &[&gone]), &rules);
            assert!(!c.live.load(Ordering::SeqCst), "rename-from {gone:?}");
        }
        // An unrelated file removed is just a change.
        let c = live_cov();
        c.observe(
            &ev(Remove(RemoveKind::File), &[&root.join("other.txt")]),
            &rules,
        );
        assert!(c.live.load(Ordering::SeqCst));
    }

    #[test]
    fn sequencer_dirs_under_a_git_dir_do_not_withdraw_but_logs_refs_modules_do() {
        use notify::EventKind::Create;
        use notify::event::CreateKind;
        let base = scratch("cls-exempt");
        let rules = rules_for(&base);
        let gitdir = base.join("wt/.git");
        for ok in ["rebase-merge", "rebase-apply", "sequencer", "rr-cache"] {
            let d = gitdir.join(ok);
            std::fs::create_dir_all(&d).unwrap();
            let c = live_cov();
            c.observe(&ev(Create(CreateKind::Folder), &[&d]), &rules);
            assert!(c.live.load(Ordering::SeqCst), "{ok} must not withdraw");
        }
        for bad in ["logs", "refs2", "info", "modules", "mystery"] {
            let d = gitdir.join(bad);
            std::fs::create_dir_all(&d).unwrap();
            let c = live_cov();
            c.observe(&ev(Create(CreateKind::Folder), &[&d]), &rules);
            assert!(!c.live.load(Ordering::SeqCst), "{bad} must withdraw");
        }
    }

    #[test]
    fn only_ref_moves_advance_the_ref_generation() {
        use notify::EventKind::Modify;
        use notify::event::{DataChange, ModifyKind::Data};
        let base = scratch("cls-refgen");
        let rules = rules_for(&base);
        let c = live_cov();
        let r0 = c.ref_generation.load(Ordering::SeqCst);
        let g0 = c.generation.load(Ordering::SeqCst);
        let edit = Modify(Data(DataChange::Any));
        c.observe(&ev(edit, &[&base.join("wt/src/main.rs")]), &rules);
        assert_ne!(g0, c.generation.load(Ordering::SeqCst));
        assert_eq!(
            r0,
            c.ref_generation.load(Ordering::SeqCst),
            "an edit is no ref move"
        );
        c.observe(&ev(edit, &[&base.join("wt/.git/refs/heads/x")]), &rules);
        assert_ne!(r0, c.ref_generation.load(Ordering::SeqCst));
        let r1 = c.ref_generation.load(Ordering::SeqCst);
        c.observe(&ev(edit, &[&base.join("wt/.git/packed-refs")]), &rules);
        assert_ne!(r1, c.ref_generation.load(Ordering::SeqCst));
    }

    #[test]
    fn a_directory_that_appeared_after_the_plan_walk_is_not_covered() {
        let base = scratch("cls-plancover");
        let root = base.join("wt");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        let ignore = {
            let mut b = ignore::gitignore::GitignoreBuilder::new(&root);
            b.add_line(None, "target/").unwrap();
            b.build().unwrap()
        };
        let plan = vec![
            crate::git_watch::WatchPlanEntry {
                path: root.clone(),
                recursive: false,
            },
            crate::git_watch::WatchPlanEntry {
                path: root.join("src"),
                recursive: true,
            },
        ];
        assert!(
            plan_children_covered(&plan, &ignore),
            "src planned, target pruned"
        );
        std::fs::create_dir_all(root.join("late")).unwrap();
        assert!(
            !plan_children_covered(&plan, &ignore),
            "`late` is neither planned nor pruned"
        );
    }

    #[test]
    fn submodules_and_reftable_withhold_the_claim() {
        let base = scratch("withhold");
        let a = repo(&base, "sub", false);
        std::fs::write(a.join(".gitmodules"), "[submodule \"x\"]\n").unwrap();
        let _wa = watch(&a);
        assert_eq!(print(&a), None, "submodule gitdirs are not watched");
        let b = repo(&base, "rt", false);
        std::fs::create_dir_all(b.join(".git/reftable")).unwrap();
        let _wb = watch(&b);
        assert_eq!(print(&b), None, "reftable refs are not files");
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
