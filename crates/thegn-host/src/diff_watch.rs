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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{Config, Event, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher};
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
    /// Sticky: set by the first withdrawal and never cleared. A withdrawal seen
    /// DURING registration (before [`publish`]) must survive publication.
    withdrawn: AtomicBool,
}

/// What the event classifier needs to know about the registration.
pub(crate) struct Rules {
    /// Non-recursively watched directories (plan entries + git roots): a
    /// directory appearing directly under one is never registered.
    nonrec: std::collections::HashSet<PathBuf>,
    /// Paths whose removal/rename means the watched identity itself is gone or
    /// replaced: the worktree root, every plan entry, every git root.
    protected: std::collections::HashSet<PathBuf>,
    ignore: IgnoreRules,
    /// Canonical git dir + common dir.
    roots: Vec<PathBuf>,
}

/// Directories git creates directly under a git dir during ordinary operations
/// (sequencer state, rerere) that nothing gated on the generation reads, so
/// their appearance must not withdraw the claim for the rest of the watcher's
/// life. `logs/`, `refs/`, `info/`, `modules/` and anything unknown still do.
const EXEMPT_GIT_DIR_CHILDREN: &[&str] = &["rebase-merge", "rebase-apply", "sequencer", "rr-cache"];

/// A directory directly under a git root that git creates/removes as part of
/// ordinary operations. `on_removal` also exempts `worktrees/` (a linked worktree
/// being pruned), which on creation still withdraws.
fn is_exempt_git_child(p: &Path, rules: &Rules, on_removal: bool) -> bool {
    p.parent().is_some_and(|d| rules.is_git_root(d))
        && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
            EXEMPT_GIT_DIR_CHILDREN.contains(&n) || (on_removal && n == "worktrees")
        })
}

/// Whether `p` lies inside a git dir (a `.git` component or under a git root).
fn in_git_dir(p: &Path, roots: &[PathBuf]) -> bool {
    crate::git_watch::in_dot_git(p) || roots.iter().any(|r| p.starts_with(r))
}

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
            withdrawn: AtomicBool::new(false),
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
        self.withdrawn.store(true, Ordering::SeqCst);
        self.live.store(false, Ordering::SeqCst);
    }

    /// Whether this claim may be relied on right now.
    fn vouches(&self) -> bool {
        self.live.load(Ordering::SeqCst) && !self.withdrawn.load(Ordering::SeqCst)
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
        // Only INSIDE a git dir: `src/refs/x.rs` in the tree is not a ref move.
        if ev
            .paths
            .iter()
            .any(|p| in_git_dir(p, &rules.roots) && crate::git_watch::is_ref_move_path(p))
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
        // The sequencer/rerere/worktrees dirs git creates and removes in the course
        // of ordinary work are exempt in BOTH directions: a pre-existing one is a
        // plan entry, and its removal must not withdraw the claim for good.
        if (matches!(ev.kind, EventKind::Remove(_)) || name_event)
            && ev
                .paths
                .iter()
                .any(|p| rules.protected.contains(p) && !is_exempt_git_child(p, rules, true))
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
                if !is_exempt_git_child(p, rules, false) {
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
    in_git_dir(p, roots)
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
    // A withdrawal racing the store above (or seen during registration) wins.
    if cov.withdrawn.load(Ordering::SeqCst) {
        cov.live.store(false, Ordering::SeqCst);
    }
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
    if !cov.vouches() {
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
    cov.vouches()
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
fn plan_children_covered(plan: &[crate::git_watch::WatchPlanEntry], ignore: &IgnoreRules) -> bool {
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
    /// How a withdrawn claim is re-established (THE-726); `None` = never.
    pub(crate) revive: Option<Reviver>,
}

/// Most re-registrations one binding of the watcher to a path will make after
/// withdrawals. Reset by a retarget (fresh [`Reviver`]), so a tab switch still
/// restores the budget; an overflow storm or unwatchable filesystem is bounded.
const MAX_REVIVES: u32 = 4;
/// Backoff before revive `n` (0-based) builds: `BASE << n`. Lets an editor's
/// write burst settle (and a `.gitignore` be rewritten fully) before the plan
/// is re-walked. A sleeping off-loop worker: no timer on the loop, no wake.
const REVIVE_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Bumped by every retarget: an in-flight revive for an older binding aborts.
static REVIVE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Invalidate every pending revive (call when the watcher is rebound).
pub(crate) fn cancel_revives() {
    REVIVE_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// Re-establishes a withdrawn claim by building a fresh watcher for the same
/// path and handing it to the loop over the existing adoption channel. Driven
/// by the withdrawal EVENT itself (never a timer or a poll), once per withdrawn
/// watcher, within a per-binding attempt budget.
#[derive(Clone)]
pub(crate) struct Reviver {
    attempts: Arc<AtomicU32>,
    epoch: u64,
    wtx: tokio_mpsc::UnboundedSender<(PathBuf, RecommendedWatcher)>,
}

impl Reviver {
    pub(crate) fn new(wtx: tokio_mpsc::UnboundedSender<(PathBuf, RecommendedWatcher)>) -> Self {
        Self {
            attempts: Arc::new(AtomicU32::new(0)),
            epoch: REVIVE_EPOCH.load(Ordering::SeqCst),
            wtx,
        }
    }

    /// Take one attempt from the budget: the backoff to wait before building,
    /// or `None` when exhausted.
    fn take_attempt(&self) -> Option<Duration> {
        let n = self
            .attempts
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < MAX_REVIVES).then_some(n + 1)
            })
            .ok()?;
        Some(REVIVE_BACKOFF_BASE * (1u32 << n))
    }

    /// Schedule ONE off-loop rebuild of `cwd`'s watcher.
    fn fire(
        &self,
        cwd: &Path,
        tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
        wake: &Arc<dyn Fn() + Send + Sync>,
    ) {
        let Some(backoff) = self.take_attempt() else {
            tracing::debug!(target: "thegn::watch", worktree = %cwd.display(), "revive budget exhausted");
            return;
        };
        let this = self.clone();
        let (cwd, tx, wake) = (cwd.to_path_buf(), tx.clone(), wake.clone());
        // best-effort: a failed spawn just leaves the claim withdrawn, which is
        // today's fall-back-to-periodic-reads behaviour.
        let _ = std::thread::Builder::new()
            .name("diff-watch-revive".into())
            .spawn(move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                std::thread::sleep(backoff);
                if REVIVE_EPOCH.load(Ordering::SeqCst) != this.epoch {
                    return; // retargeted meanwhile: the new binding builds its own
                }
                let sink = RefreshSink {
                    tx,
                    wake: wake.clone(),
                    revive: Some(this.clone()),
                };
                if let Some(nw) = build_diff_watcher(&cwd, sink)
                    && REVIVE_EPOCH.load(Ordering::SeqCst) == this.epoch
                    && this.wtx.send((cwd, nw)).is_ok()
                {
                    wake();
                }
            });
    }
}

#[derive(Clone)]
pub(crate) struct IgnoreRules {
    pub(crate) root: PathBuf,
    root_rules: ignore::gitignore::Gitignore,
    nested: Vec<(PathBuf, ignore::gitignore::Gitignore)>,
}

impl IgnoreRules {
    pub(crate) fn new(root: &Path, root_rules: ignore::gitignore::Gitignore) -> Self {
        Self {
            root: root.to_path_buf(),
            root_rules,
            nested: Vec::new(),
        }
    }

    pub(crate) fn add_nested(&mut self, dir: PathBuf, rules: ignore::gitignore::Gitignore) {
        self.nested.push((dir, rules));
    }

    pub(crate) fn is_ignored(&self, path: &Path) -> bool {
        if !path.starts_with(&self.root) {
            return false;
        }
        let mut result = self.root_rules.matched_path_or_any_parents(path, true);
        let mut nested: Vec<_> = self
            .nested
            .iter()
            .filter(|(dir, _)| path.starts_with(dir))
            .collect();
        nested.sort_by_key(|(dir, _)| dir.components().count());
        for (_, matcher) in nested {
            let matched = matcher.matched_path_or_any_parents(path, true);
            if !matches!(matched, ignore::Match::None) {
                result = matched;
            }
        }
        result.is_ignore()
    }
}

fn add_ignore_source(builder: &mut ignore::gitignore::GitignoreBuilder, path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let source = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(error) = builder.add(&source) {
        tracing::debug!(
            target: "thegn::hydrate",
            source = %source.display(),
            %error,
            "could not fully load git ignore source"
        );
    }
    true
}

/// Add worktree-rooted patterns from git-level excludes files.
fn add_root_ignore_source(builder: &mut ignore::gitignore::GitignoreBuilder, path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut added = false;
    for (line_no, line) in contents.lines().enumerate() {
        let line = if line_no == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        if let Err(error) = builder.add_line(None, line) {
            tracing::debug!(
                target: "thegn::hydrate",
                source = %path.display(),
                %error,
                "could not fully load git ignore source"
            );
        } else {
            added = true;
        }
    }
    added
}

/// Add nested `.gitignore` files while walking only directories not already
/// excluded by the root and git-level ignore sources. Each newly loaded file
/// affects the next level, as Git's per-directory ignore rules do.
fn collect_nested_gitignores(dir: &Path, rules: &mut IgnoreRules, visited: &mut usize) -> bool {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if !entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false) {
                continue;
            }
            let child = entry.path();
            *visited += 1;
            if *visited > crate::git_watch::MAX_DIFF_WATCHES {
                return false;
            }
            if crate::git_watch::in_dot_git(&child) || crate::git_watch::prune_dir(&child, rules) {
                continue;
            }
            let path = child.join(".gitignore");
            let mut builder = ignore::gitignore::GitignoreBuilder::new(&child);
            if add_ignore_source(&mut builder, &path) {
                let matcher = builder
                    .build()
                    .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
                rules.add_nested(child.clone(), matcher);
            }
            if !collect_nested_gitignores(&child, rules, visited) {
                return false;
            }
        }
    }
    true
}

fn global_excludes_path(cwd: &Path) -> Option<PathBuf> {
    if let Some(path) =
        thegn_core::util::git_out(cwd, &["config", "--path", "--get", "core.excludesFile"])
    {
        let path = PathBuf::from(path.trim());
        return (!path.as_os_str().is_empty()).then_some(path);
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    config.map(|path| path.join("git/ignore"))
}

fn build_ignore_rules(
    cwd: &Path,
    canonical_cwd: &Path,
    git_dir: Option<&Path>,
    common_dir: Option<&Path>,
) -> Option<IgnoreRules> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(canonical_cwd);
    // Git gives global excludes the lowest precedence, then info/exclude, then
    // per-directory .gitignore rules. Add root sources in that same order.
    if let Some(path) = global_excludes_path(cwd) {
        add_root_ignore_source(&mut builder, &path);
    }
    let mut excludes = Vec::new();
    if let Some(path) = common_dir {
        excludes.push(path.join("info/exclude"));
    }
    if let Some(path) = git_dir {
        let candidate = path.join("info/exclude");
        if !excludes.contains(&candidate) {
            excludes.push(candidate);
        }
    }
    for path in excludes {
        add_root_ignore_source(&mut builder, &path);
    }
    add_ignore_source(&mut builder, &cwd.join(".gitignore"));
    // The global excludes file is outside this watcher's registered roots;
    // edits to it can leave the plan stale until the next watcher retarget.
    let root_rules = builder
        .build()
        .unwrap_or_else(|_| ignore::gitignore::Gitignore::empty());
    let mut rules = IgnoreRules::new(canonical_cwd, root_rules);
    let mut visited = 1usize; // include the worktree root in the bounded scan
    if !collect_nested_gitignores(canonical_cwd, &mut rules, &mut visited) {
        return None;
    }
    Some(rules)
}

fn resolve_git_dirs(cwd: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    let git_dir =
        thegn_core::util::git_out(cwd, &["rev-parse", "--path-format=absolute", "--git-dir"])
            .map(PathBuf::from);
    let common_dir = thegn_core::util::git_out(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(PathBuf::from);
    (git_dir, common_dir)
}

/// Build + register the diff fs-watcher for `cwd` (blocking: registration walks
/// the tree and runs git commands — call it off the loop). `None` when no usable
/// watcher could be attached or the directory-watch cap selects ticker fallback.
pub(crate) fn build_diff_watcher(cwd: &Path, sink: RefreshSink) -> Option<RecommendedWatcher> {
    let cwd = cwd.to_path_buf();
    let RefreshSink { tx, wake, revive } = sink;
    // Resolve this worktree's gitdir + common dir. For a *linked* worktree
    // `<cwd>/.git` is a file pointer, so the HEAD / reflog / refs that
    // signal a commit live OUTSIDE the watched tree (in the main repo's
    // `.git/worktrees/<name>` + shared `.git`); we must watch those too or
    // pane-driven commits never reach the panel. For the main checkout both
    // resolve back under `cwd` and the recursive root watch already covers
    // them. `git rev-parse` runs here, off the event loop.
    let (git_dir, common_dir) = resolve_git_dirs(&cwd);
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
    // Drop watcher events for every git ignore source: a change to an ignored
    // file cannot alter `git diff HEAD`, and build trees can churn constantly.
    // NOTE: a force-added (`git add -f`) or negate-pattern (`!keep`) ignored
    // file *can* appear in the diff and would be dropped here; that's rare,
    // and the safety-net ticker still rebuilds the panel within a few seconds.
    // The matcher's ROOT must be canonical (it is matched against event
    // paths) while the `.gitignore` it reads is addressed from the real cwd —
    // on macOS those differ under any symlinked prefix, and a matcher rooted
    // at `/tmp/wt` matches nothing FSEvents delivers from `/private/tmp/wt`.
    let canonical_cwd = crate::git_watch::watch_canonical(&cwd);
    let Some(ignore) = build_ignore_rules(
        &cwd,
        &canonical_cwd,
        git_dir.as_deref(),
        common_dir.as_deref(),
    ) else {
        tracing::warn!(
            target: "thegn::hydrate",
            worktree = %cwd.display(),
            cap = crate::git_watch::MAX_DIFF_WATCHES,
            "nested gitignore scan cap reached — using the diff ticker"
        );
        return None;
    };
    // Account for the linked-worktree gitdir watches too: they sit outside the
    // worktree plan but consume the same process-wide inotify quota.
    let mut external_watch_count = 0usize;
    for root in [git_dir.as_ref(), common_dir.as_ref()]
        .into_iter()
        .flatten()
        .map(|p| crate::git_watch::watch_canonical(p))
    {
        if root.starts_with(&canonical_cwd) {
            continue;
        }
        external_watch_count = external_watch_count.saturating_add(1);
        for child in [root.join("logs"), root.join("refs")] {
            if child.is_dir() {
                let Some(count) = crate::git_watch::count_recursive_watches(
                    &child,
                    crate::git_watch::MAX_DIFF_WATCHES.saturating_sub(external_watch_count),
                ) else {
                    external_watch_count = crate::git_watch::MAX_DIFF_WATCHES.saturating_add(1);
                    break;
                };
                external_watch_count = external_watch_count.saturating_add(count);
            }
        }
    }
    if external_watch_count >= crate::git_watch::MAX_DIFF_WATCHES {
        tracing::warn!(
            target: "thegn::hydrate",
            worktree = %cwd.display(),
            cap = crate::git_watch::MAX_DIFF_WATCHES,
            "diff fs-watch directory cap reached — using the diff ticker"
        );
        return None;
    }
    // Plan the registration BEFORE the matcher moves into the event closure.
    // This is the filesystem walk the old blanket `RecursiveMode::Recursive`
    // did internally — same shape, minus the gitignored subtrees, so it is
    // strictly cheaper than what it replaces.
    let Some(plan) = crate::git_watch::plan_watches_with_cap(
        &canonical_cwd,
        &ignore,
        crate::git_watch::MAX_DIFF_WATCHES - external_watch_count,
    ) else {
        tracing::warn!(
            target: "thegn::hydrate",
            worktree = %cwd.display(),
            cap = crate::git_watch::MAX_DIFF_WATCHES,
            "diff fs-watch directory cap reached — using the diff ticker"
        );
        return None;
    };
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
    let revive_cwd = cwd.clone();
    let revive_tx = tx.clone();
    let revive_wake = wake.clone();
    let new_watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let vouched = guard.cov.vouches();
            guard.cov.observe(&res, &rules);
            // The claim was live and this very event withdrew it: re-register once.
            if vouched
                && !guard.cov.vouches()
                && let Some(r) = &revive
            {
                r.fire(&revive_cwd, &revive_tx, &revive_wake);
            }
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
        },
        Config::default().with_event_kinds(EventKindMask::CORE),
    );
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

    #[test]
    fn all_ignore_source_roots_prune_their_worktree_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        let excluded = root.join(".claude/worktrees/agent/node_modules");
        let nested_ignored = root.join("vendor/generated");
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::create_dir_all(&nested_ignored).unwrap();
        std::fs::create_dir_all(root.join("generated-global")).unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), ".claude/worktrees/\n").unwrap();
        std::fs::write(root.join("global-ignore"), "/generated-global/\n").unwrap();
        git(
            &root,
            &[
                "config",
                "core.excludesFile",
                root.join("global-ignore").to_str().unwrap(),
            ],
        );
        std::fs::write(root.join("vendor/.gitignore"), "generated/\n").unwrap();

        let gitdir = root.join(".git");
        let matcher = build_ignore_rules(&root, &root, Some(&gitdir), Some(&gitdir)).unwrap();

        assert!(
            crate::git_watch::prune_dir(&root.join(".claude/worktrees"), &matcher),
            "info/exclude must be rooted at the worktree, not .git/info"
        );
        assert!(
            crate::git_watch::prune_dir(&nested_ignored, &matcher),
            "nested .gitignore patterns must be loaded before watch planning"
        );
        assert!(crate::git_watch::prune_dir(
            &root.join("generated-global"),
            &matcher
        ));
        assert!(
            !crate::git_watch::prune_dir(&root.join("src"), &matcher),
            "unmatched worktree directories remain watchable"
        );

        std::fs::create_dir_all(root.join("vendor/src")).unwrap();
        std::fs::write(root.join("vendor/.gitignore"), "/src\nbuild/out\n").unwrap();
        let rooted = build_ignore_rules(&root, &root, Some(&gitdir), Some(&gitdir)).unwrap();
        assert!(
            !crate::git_watch::prune_dir(&root.join("src"), &rooted),
            "vendor/.gitignore /src must not prune the worktree root src"
        );
        assert!(crate::git_watch::prune_dir(
            &root.join("vendor/src"),
            &rooted
        ));
    }

    #[test]
    fn linked_worktree_ignore_uses_common_git_dir_exclude() {
        let base = scratch("linked-exclude");
        let main = repo(&base, "main", false);
        let linked = base.join("linked");
        git(
            &main,
            &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
        );
        let (gitdir, common) = resolve_git_dirs(&linked);
        let common = common.unwrap();
        let gitdir = gitdir.unwrap();
        std::fs::write(common.join("info/exclude"), "shared-excluded/\n").unwrap();
        std::fs::create_dir_all(linked.join("shared-excluded")).unwrap();
        let canonical = crate::git_watch::watch_canonical(&linked);
        let rules = build_ignore_rules(&linked, &canonical, Some(&gitdir), Some(&common)).unwrap();
        assert!(
            crate::git_watch::prune_dir(&canonical.join("shared-excluded"), &rules),
            "linked worktrees read info/exclude from the common git dir"
        );
    }

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
                revive: None,
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
        // Hermetic git global layer: never read the real ~/.gitconfig, system
        // config, or injected GIT_CONFIG_* of the machine running the test.
        let ghome = base.join("ghome");
        std::fs::create_dir_all(&ghome).unwrap();
        let gsys = base.join("gitconfig-system");
        std::fs::write(&gsys, "").unwrap();
        let state = crate::testenv::IsolatedState::with_env(&[
            (
                "GIT_CONFIG_GLOBAL",
                ghome.join(".gitconfig").to_str().unwrap(),
            ),
            ("GIT_CONFIG_SYSTEM", gsys.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "0"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
        ]);
        std::fs::create_dir_all(state.state_home().join("thegn")).unwrap();
        let db = thegn_core::db::Db::open_at(&state.state_home().join("thegn/thegn.db")).unwrap();
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
                    thegn_core::git_memo::global_git_print(&r).expect("global layer"),
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
                revive: None,
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
                revive: None,
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
            ignore: IgnoreRules::new(&root, ignore::gitignore::Gitignore::empty()),
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
            IgnoreRules::new(&root, b.build().unwrap())
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
    fn a_withdrawal_during_registration_survives_publication() {
        let base = scratch("cls-publish");
        let rules = rules_for(&base);
        let path = base.join("wt");
        for early in 0..3 {
            let c = Arc::new(Coverage::new());
            match early {
                0 => c.observe(&Err(notify::Error::generic("limit")), &rules),
                1 => c.observe(
                    &Ok(notify::Event::new(notify::EventKind::Other)
                        .set_flag(notify::event::Flag::Rescan)),
                    &rules,
                ),
                _ => c.observe(
                    &ev(
                        notify::EventKind::Remove(notify::event::RemoveKind::Folder),
                        &[&path],
                    ),
                    &rules,
                ),
            }
            publish(&path, &c);
            assert_eq!(
                current_print(&path),
                None,
                "case {early}: published an unproven claim"
            );
            assert_eq!(current_ref_generation(&path), None, "case {early}");
        }
    }

    #[test]
    fn removing_git_managed_dirs_does_not_withdraw_but_a_tree_refs_file_is_no_ref_move() {
        use notify::EventKind::{Modify, Remove};
        use notify::event::{DataChange, ModifyKind::Data, RemoveKind};
        let base = scratch("cls-l2");
        let mut rules = rules_for(&base);
        let gitdir = base.join("wt/.git");
        for n in ["rebase-merge", "sequencer", "worktrees"] {
            rules.protected.insert(gitdir.join(n));
            let c = live_cov();
            c.observe(&ev(Remove(RemoveKind::Folder), &[&gitdir.join(n)]), &rules);
            assert!(
                c.live.load(Ordering::SeqCst),
                "removing {n} must not withdraw"
            );
        }
        let c = live_cov();
        let r0 = c.ref_generation.load(Ordering::SeqCst);
        c.observe(
            &ev(
                Modify(Data(DataChange::Any)),
                &[&base.join("wt/src/refs/x.rs")],
            ),
            &rules,
        );
        assert_eq!(
            r0,
            c.ref_generation.load(Ordering::SeqCst),
            "tree path, not a ref"
        );
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
                revive: None,
            },
        );
        assert!(w.is_none() || print(&base.join("nope")).is_none());
    }

    #[test]
    fn revive_budget_is_bounded_and_backs_off() {
        let (wtx, _rx) = tokio_mpsc::unbounded_channel();
        let r = Reviver::new(wtx);
        let waits: Vec<_> = std::iter::from_fn(|| r.take_attempt()).collect();
        assert_eq!(waits.len(), MAX_REVIVES as usize, "bounded");
        assert!(waits.windows(2).all(|w| w[1] > w[0]), "backoff grows");
        assert!(r.take_attempt().is_none(), "stays exhausted");
    }

    #[test]
    fn a_withdrawal_revives_the_claim_without_a_tab_switch() {
        let base = scratch("revive");
        let r = repo(&base, "r", true);
        let (tx, _rx) = tokio_mpsc::unbounded_channel();
        let (wtx, mut wrx) = tokio_mpsc::unbounded_channel();
        let _w = build_diff_watcher(
            &r,
            RefreshSink {
                tx,
                wake: Arc::new(|| {}),
                revive: Some(Reviver::new(wtx)),
            },
        )
        .expect("registers");
        assert!(print(&r).is_some());
        std::fs::write(r.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        // The replacement watcher arrives over the adoption channel and vouches.
        let deadline = Instant::now() + Duration::from_secs(10);
        let (path, _nw) = loop {
            if let Ok(got) = wrx.try_recv() {
                break got;
            }
            assert!(Instant::now() < deadline, "no revived watcher");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(path, r);
        assert!(print(&r).is_some(), "claim re-established");
    }

    #[test]
    fn a_tool_state_edit_still_bumps_the_change_generation() {
        use notify::EventKind::Modify;
        use notify::event::{DataChange, ModifyKind::Data};

        let base = scratch("cls-tool-state-generation");
        let rules = rules_for(&base);
        let c = live_cov();
        let before = c.generation.load(Ordering::SeqCst);
        let path = base.join("wt/.thegn/pipeline/x.md");
        c.observe(&ev(Modify(Data(DataChange::Any)), &[&path]), &rules);

        assert_ne!(before, c.generation.load(Ordering::SeqCst));
        assert!(
            c.vouches(),
            "tool-state edits remain covered by the watcher"
        );
    }

    #[test]
    fn a_pipeline_artifact_edit_does_not_move_the_print() {
        let base = scratch("pipeline-print");
        let r = repo(&base, "r", true);
        std::fs::create_dir_all(r.join(".thegn/pipeline/T")).unwrap();
        std::fs::write(r.join(".thegn/pipeline/T/1.md"), "handoff\n").unwrap();
        git(&r, &["add", ".thegn/pipeline/T/1.md"]);
        git(&r, &["commit", "-q", "-m", "pipeline handoff"]);
        let _w = watch(&r);
        let before = settle(&r);

        std::fs::write(r.join(".thegn/pipeline/T/1.md"), "updated handoff\n").unwrap();
        std::fs::write(r.join(".thegn/pipeline/T/2.md"), "another handoff\n").unwrap();
        std::fs::create_dir_all(r.join(".thegn/pipeline/U")).unwrap();
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            print(&r),
            Some(before),
            "pipeline edits must not move the print"
        );

        advances(&r, "edit a.txt", || {
            std::fs::write(r.join("a.txt"), "source edit\n").unwrap();
        });
    }

    fn panel_test_env(base: &Path) -> crate::testenv::IsolatedState {
        let ghome = base.join("ghome");
        std::fs::create_dir_all(&ghome).unwrap();
        let gsys = base.join("gitconfig-system");
        std::fs::write(&gsys, "").unwrap();
        let global_config = ghome.join(".gitconfig");
        crate::testenv::IsolatedState::with_env(&[
            ("GIT_CONFIG_GLOBAL", global_config.to_str().unwrap()),
            ("GIT_CONFIG_SYSTEM", gsys.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "0"),
            ("GIT_CONFIG_COUNT", "0"),
            ("GIT_CONFIG_PARAMETERS", ""),
        ])
    }

    #[test]
    fn build_panel_hides_pipeline_artifacts_but_keeps_source_and_hooks() {
        let base = scratch("panel-pipeline");
        let state = panel_test_env(&base);
        std::fs::create_dir_all(state.state_home().join("thegn")).unwrap();
        let db = thegn_core::db::Db::open_at(&state.state_home().join("thegn/thegn.db")).unwrap();
        let cfg = thegn_core::config::Config::default();
        let hints = crate::hydrate::HydrateHints::default();
        let r = repo(&base, "r", true);
        std::fs::create_dir_all(r.join(".thegn/pipeline/T")).unwrap();
        std::fs::write(r.join(".thegn/pipeline/T/1.md"), "handoff\n").unwrap();
        std::fs::write(r.join(".thegn/pre-create.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        git(&r, &["add", ".thegn"]);
        git(&r, &["commit", "-q", "-m", "tracked tool files"]);
        let _w = watch(&r);
        assert!(print(&r).is_some());

        std::fs::write(r.join(".thegn/pipeline/T/1.md"), "updated handoff\n").unwrap();
        std::fs::write(r.join(".thegn/pipeline/T/2.md"), "untracked handoff\n").unwrap();
        std::fs::write(r.join(".thegn/pre-create.sh"), "#!/bin/sh\necho hook\n").unwrap();
        std::fs::write(r.join("a.txt"), "source edit\n").unwrap();

        let at = settle(&r);
        let panel = crate::hydrate::build_panel(&r, &db, &hints, &cfg);
        let cached = crate::panel_git_cache::get(
            &r,
            (
                at,
                thegn_core::git_memo::global_git_print(&r).expect("global layer"),
            ),
        );
        assert!(cached.is_some(), "build stores the filtered snapshot");
        let reused = crate::hydrate::build_panel(&r, &db, &hints, &cfg);

        for paths in [
            panel
                .changes
                .iter()
                .map(|row| row.path.as_str())
                .collect::<Vec<_>>(),
            panel
                .files
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
        ] {
            assert!(
                paths.contains(&"a.txt"),
                "source edit remains visible: {paths:?}"
            );
            assert!(
                paths.contains(&".thegn/pre-create.sh"),
                "tracked hook remains visible: {paths:?}"
            );
            assert!(
                paths
                    .iter()
                    .all(|path| !path.starts_with(".thegn/pipeline")),
                "pipeline artifacts are excluded: {paths:?}"
            );
        }
        assert_eq!(
            reused.changes, panel.changes,
            "cached changes stay filtered"
        );
        assert_eq!(reused.files, panel.files, "cached files stay filtered");
    }
}
