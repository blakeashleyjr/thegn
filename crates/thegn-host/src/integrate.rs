//! The local merge queue ("fold-actor") runner.
//!
//! Folds queued worktree branches onto a repo's `target_branch` entirely in the
//! git object database (no checkout), test-gates the folded tip, and advances the
//! branch with an atomic compare-and-swap. Clean branches land automatically;
//! genuine conflicts are deferred. The pure sequencing lives in
//! [`thegn_core::fold`]; this module is the I/O around it — merge plumbing
//! ([`thegn_svc::git::PlumbingOps`]), the throwaway-worktree gate, and the CAS
//! retry loop.
//!
//! [`run_fold`] is synchronous and side-effecting on the repo; the CLI calls it
//! directly and the host daemon calls it from `spawn_blocking`.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::fold::{self, Branch, ConflictKind, FoldGit, FoldPlan, MergeOutcome};
use thegn_core::gate;
use thegn_core::outln;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;
use thegn_svc::git::{CliGit, GitBackend, MergeTreeOutcome, PlumbingOps};

/// A unique throwaway path under the temp dir. `util::now()` is seconds-resolution,
/// so a process-wide sequence keeps two near-simultaneous throwaway worktrees (two
/// gate runs, or parallel tests) from colliding on the same path.
fn tmp_path(prefix: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}-{n}",
        std::process::id(),
        util::now()
    ))
}

/// Drives the pure fold engine over real git plumbing at one repo root.
struct PlumbingAdapter {
    loc: GitLoc,
    repo_root: PathBuf,
    regenerate_paths: Vec<String>,
    /// Empty disables lockfile regeneration (regenerable conflicts just defer).
    regenerate_command: String,
}

impl FoldGit for PlumbingAdapter {
    fn merge_tree(&self, ours: &str, theirs: &str) -> Result<MergeOutcome> {
        match CliGit.merge_tree(&self.loc, ours, theirs)? {
            MergeTreeOutcome::Clean { tree } => Ok(MergeOutcome::Clean { tree }),
            MergeTreeOutcome::Conflict { paths, .. } => {
                // A conflict confined to regenerable artifacts (e.g. Cargo.lock)
                // isn't a real merge conflict — rebuild them and land it, rather
                // than deferring to a human. Only when a regenerate_command is set.
                if !self.regenerate_command.is_empty()
                    && fold::classify(&paths, &self.regenerate_paths)
                        == fold::ConflictKind::Regenerable
                    && let Some(tree) = regenerate_merge(
                        &self.repo_root,
                        ours,
                        theirs,
                        &self.regenerate_paths,
                        &self.regenerate_command,
                    )
                {
                    return Ok(MergeOutcome::Clean { tree });
                }
                Ok(MergeOutcome::Conflict { paths })
            }
        }
    }
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String> {
        CliGit.commit_tree(&self.loc, tree, parents, msg)
    }
}

/// Resolve a regenerable-only merge by replaying it in a throwaway worktree:
/// merge `theirs` onto `ours`, take the incoming side of each regenerate path,
/// run `regenerate_command` to rebuild them, and write the merged tree. Returns
/// the written tree oid, or `None` if anything fails (caller falls back to
/// deferring). Never leaves a worktree behind.
// off-loop: the fold runs from the CLI (`thegn integrate`) or from
// spawn_fold's spawn_blocking (see the module doc) — never on the loop.
#[expect(clippy::disallowed_methods)]
fn regenerate_merge(
    repo_root: &Path,
    ours: &str,
    theirs: &str,
    regenerate_paths: &[String],
    regenerate_command: &str,
) -> Option<String> {
    let tmp = tmp_path("tg-foldregen");
    let tmp_s = tmp.to_string_lossy().to_string();
    if !util::git_ok(
        repo_root,
        &["worktree", "add", "--detach", "--force", &tmp_s, ours],
    ) {
        return None;
    }
    let tree = (|| -> Option<String> {
        // Merge theirs in (conflicts on the lockfiles are expected → ignore the
        // exit status; we resolve them next).
        let _ = util::git_cmd(&tmp)
            .args(["merge", "--no-commit", "--no-ff", theirs])
            .output()
            .ok()?;
        // Take the incoming version of each regenerate path so it's a valid file
        // (not conflict-marked), then the regen command reconciles it.
        for p in regenerate_paths {
            let _ = util::git_cmd(&tmp)
                .args(["checkout", "--theirs", "--", p])
                .output();
        }
        // Rebuild the regenerate artifacts from the merged manifests.
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(regenerate_command)
            .current_dir(&tmp)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        let _ = util::git_cmd(&tmp).args(["add", "-A"]).output();
        // Bail if any path is still unmerged — we only handle regenerable cases.
        let unmerged =
            util::git_out(&tmp, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        if !unmerged.trim().is_empty() {
            return None;
        }
        let tree = util::git_out(&tmp, &["write-tree"])?;
        let tree = tree.trim().to_string();
        (!tree.is_empty()).then_some(tree)
    })();
    let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &tmp_s]);
    if tree.is_some() {
        thegn_core::msg::info(&format!(
            "merge queue: regenerated {} for a lockfile-only merge",
            regenerate_paths.join(", ")
        ));
    }
    tree
}

/// What the test-gate decided about the folded tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// No gate configured (or nothing landed to gate).
    Skipped,
    /// The folded tip built/tested green.
    Passed,
    /// The gate went red. `offender` names the branch bisect isolated as the
    /// cause, if it could localize one (else the whole batch was held back).
    Failed { offender: Option<String> },
    /// The gate could not RUN (missing binary, unprovisioned worktree, killed).
    /// Distinct from `Failed`: it is a fact about the environment, so no branch
    /// is blamed and no bisect is attempted.
    Errored { reason: String },
}

/// A branch that landed in this fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedReport {
    pub branch: String,
    pub commit: String,
}

/// A branch that did not land, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredReport {
    pub branch: String,
    pub paths: Vec<String>,
    pub kind: ConflictKind,
    /// True when this branch was deferred by the test-gate (bisected offender),
    /// not by a textual merge conflict.
    pub gate_failed: bool,
}

/// The outcome of one `run_fold` call.
#[derive(Debug, Clone)]
pub struct FoldReport {
    pub target_branch: String,
    pub original: String,
    pub final_tip: String,
    pub advanced: bool,
    pub landed: Vec<LandedReport>,
    pub deferred: Vec<DeferredReport>,
    pub gate: GateOutcome,
    /// How many CAS attempts it took (main moving under the fold forces a re-fold).
    pub cas_attempts: u32,
    /// What happened to each live checkout of the target branch when the ref
    /// advanced. Advisory, like `Candidates::skipped_dirty`: the caller reports
    /// the ones we could not fast-forward, so a stale working tree is never a
    /// silent surprise. Empty when nothing advanced.
    pub resyncs: Vec<util::CheckoutResync>,
}

/// Resolve the branch the fold advances. `"auto"` (or empty) → the repo's
/// default branch; otherwise the configured name verbatim.
pub fn resolve_target(cfg: &MergeQueueConfig, repo_root: &Path) -> String {
    if cfg.target_branch.is_empty() || cfg.target_branch == "auto" {
        thegn_core::worktree::default_branch(repo_root)
    } else {
        cfg.target_branch.clone()
    }
}

/// A repo's foldable worktree branches plus the bookkeeping the queue/UI needs.
pub struct Candidates {
    /// Branches to fold, in worktree-list order.
    pub branches: Vec<Branch>,
    /// Branches skipped because their worktree is dirty and `snapshot_dirty` is
    /// off — surfaced so the caller can warn rather than silently dropping work.
    pub skipped_dirty: Vec<String>,
    /// branch name → its worktree path (the DB is keyed by worktree).
    pub worktrees: HashMap<String, String>,
}

/// The main checkout (first `git worktree list` entry) reachable from any path
/// inside the repo. The fold advances the repo's target branch, so it operates
/// from the main checkout regardless of which worktree the caller is in.
pub fn main_checkout(start: &Path) -> Option<PathBuf> {
    let porc = util::git_out(start, &["worktree", "list", "--porcelain"])?;
    porc.lines()
        .find_map(|l| l.strip_prefix("worktree ").map(PathBuf::from))
}

/// One-shot fold of the repo containing `any_path`: resolve the main checkout +
/// target branch, gather candidate branches, fold/gate/CAS-advance, and mirror
/// the outcome into the queue cache. The shared entry point for both the CLI
/// command and the in-app (off-loop) runner.
pub fn fold_active_repo(cfg: &thegn_core::config::Config, any_path: &Path) -> Result<FoldReport> {
    let repo_root = main_checkout(any_path).context("not inside a git repository")?;
    // Resolved here (off the loop — this runs inside spawn_fold's blocking task)
    // because the per-repo `[merge_queue]` layer needs the repo root.
    let mq = &cfg.repo_merge_queue(&repo_root);
    let target = resolve_target(mq, &repo_root);
    let mut cands = candidate_branches(mq, &repo_root, &target)?;
    // The in-app `integrate` action reaches this too, so the opt-in guard lives
    // here rather than in the CLI: one keypress must not be able to land a branch
    // nobody nominated. A DB that won't open means we cannot prove anything was
    // enqueued — fold nothing rather than fold everything.
    if mq.require_enqueue {
        let enqueued = Db::open()
            .map(|db| enqueued_worktrees(&db, &target))
            .unwrap_or_default();
        hold_unenqueued(&mut cands, &enqueued);
    }
    let report = run_fold(mq, &repo_root, cands.branches.clone())?;
    if let Ok(db) = Db::open() {
        let _ = persist(mq, &repo_root, &db, &cands, &report);
    }
    Ok(report)
}

/// Hold back every candidate that was not explicitly enqueued, returning the
/// names withheld (in candidate order) so the caller can name them.
///
/// Pure over the membership set — the DB read is the caller's — because this is
/// the guard that decides whether someone's in-progress branch gets landed, and
/// a guard worth having is a guard worth unit-testing.
///
/// `enqueued` holds worktree PATHS, matching `merge_queue`'s key: a branch can be
/// renamed while its worktree stays put, and the queue row survives that.
pub fn hold_unenqueued(cands: &mut Candidates, enqueued: &HashSet<String>) -> Vec<String> {
    let mut held = Vec::new();
    cands.branches.retain(|b| {
        let queued = cands
            .worktrees
            .get(&b.name)
            .is_some_and(|wt| enqueued.contains(wt));
        if !queued {
            held.push(b.name.clone());
        }
        queued
    });
    held
}

/// The worktree paths currently sitting in this repo's queue awaiting a fold.
///
/// Only `queued` counts. A `landed` row is history, and a `deferred` /
/// `gate_failed` one is a branch that already had its turn and stopped — those
/// re-enter through `thegn merge retry`, which is the explicit "I fixed it, try
/// again" gesture, rather than by being silently retried forever.
pub fn enqueued_worktrees(db: &Db, target_branch: &str) -> HashSet<String> {
    db.list_merge_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.status == "queued" && r.target_branch == target_branch)
        .map(|r| r.worktree)
        .collect()
}

/// Collect a repo's foldable worktree branches: every linked worktree (not the
/// main checkout, not the target branch itself). Dirty worktrees are snapshotted
/// into a commit when `snapshot_dirty`, else skipped.
///
/// NOTE: "eligible" here means only *foldable* — clean, and not already the
/// target. It carries no notion of whether the branch was meant to land. Callers
/// that fold rather than merely enumerate must pass the result through
/// [`hold_unenqueued`] when `require_enqueue` is on.
pub fn candidate_branches(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    target_branch: &str,
) -> Result<Candidates> {
    let porc = util::git_out(repo_root, &["worktree", "list", "--porcelain"])
        .context("git worktree list")?;
    let main = repo_root.to_string_lossy().to_string();
    let mut branches = Vec::new();
    let mut skipped_dirty = Vec::new();
    let mut worktrees = HashMap::new();
    let mut wt_path = String::new();
    for line in porc.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            wt_path = p.to_string();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            let branch = b.to_string();
            if wt_path != main && branch != target_branch {
                let loc = GitLoc::for_worktree(Path::new(&wt_path));
                let tip = if cfg.snapshot_dirty {
                    let msg = format!("snapshot: {branch} (fold-actor)");
                    match CliGit.snapshot_worktree(&loc, &msg)? {
                        Some(new_tip) => new_tip,
                        None => CliGit.rev_parse(&loc, "HEAD")?,
                    }
                } else if CliGit.is_dirty(&loc).unwrap_or(false) {
                    skipped_dirty.push(branch.clone());
                    continue;
                } else {
                    CliGit.rev_parse(&loc, "HEAD")?
                };
                worktrees.insert(branch.clone(), wt_path.clone());
                branches.push(Branch { name: branch, tip });
            }
        }
    }
    Ok(Candidates {
        branches,
        skipped_dirty,
        worktrees,
    })
}

/// Mirror a fold's outcome into the `merge_queue` cache (the panel feed +
/// auto-drain record). Best-effort: keyed by worktree path via `cands.worktrees`.
pub fn persist(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    db: &Db,
    cands: &Candidates,
    report: &FoldReport,
) -> Result<()> {
    use thegn_core::merge_lifecycle::LifecycleEvent;
    // Only branches this fold actually ACTED on get a row. Enqueueing every
    // candidate made the queue a record of what was considered rather than what
    // was nominated, which had two costs: a bystander worktree was filed into
    // `queued_folder` ("Merging") by a command the user thought only read, and
    // `require_enqueue` could not distinguish a human's `merge add` from the
    // previous run's own bookkeeping. A row is still needed BEFORE
    // `update_merge_status`, which updates in place and would no-op otherwise.
    let acted: Vec<&String> = report
        .landed
        .iter()
        .map(|l| &l.branch)
        .chain(report.deferred.iter().map(|d| &d.branch))
        .collect();
    for branch in acted {
        if let Some(wt) = cands.worktrees.get(branch) {
            db.enqueue_merge(wt, branch, &report.target_branch)?;
            crate::merge_lifecycle::apply(cfg, db, repo_root, wt, branch, LifecycleEvent::Enqueued);
        }
    }
    for l in &report.landed {
        if let Some(wt) = cands.worktrees.get(&l.branch) {
            db.update_merge_status(wt, "landed", Some(&l.commit), None, None)?;
            crate::merge_lifecycle::apply(
                cfg,
                db,
                repo_root,
                wt,
                &l.branch,
                LifecycleEvent::Landed,
            );
        }
    }
    for d in &report.deferred {
        if let Some(wt) = cands.worktrees.get(&d.branch) {
            let status = if d.gate_failed {
                "gate_failed"
            } else {
                "deferred"
            };
            let paths = (!d.paths.is_empty()).then(|| d.paths.join("\n"));
            db.update_merge_status(wt, status, None, paths.as_deref(), None)?;
            crate::merge_lifecycle::apply(
                cfg,
                db,
                repo_root,
                wt,
                &d.branch,
                LifecycleEvent::Failed,
            );
        }
    }
    Ok(())
}

/// A stable per-repo directory for the reused gate build cache, keyed on the
/// repo root's absolute path so each repo warms its own worktree + target under
/// `$XDG_STATE_HOME/thegn/gate/` (the same state root as the DB/logs). The
/// `DefaultHasher` seed is fixed, so the key is stable across runs.
fn gate_base(repo_root: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo_root.hash(&mut h);
    let key = h.finish();
    let name = repo_root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".to_string());
    util::xdg_state_home()
        .join("thegn/gate")
        .join(format!("{name}-{key:016x}"))
}

/// Blocking advisory lock serializing gate runs on a reused worktree. The
/// sidecar `<wt>.lock` file is created once and never removed (the worktree dir
/// itself is churned; the lock must survive that). `File::lock` dies with the
/// process, so it can't go stale. Best-effort: `None` (exotic fs / permissions)
/// degrades to the old unserialized path rather than refusing to gate.
#[cfg(unix)]
fn gate_lock(wt: &Path) -> Option<std::fs::File> {
    let lock_path = {
        let mut p = wt.as_os_str().to_owned();
        p.push(".lock");
        PathBuf::from(p)
    };
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    f.lock().ok()?; // blocks until the prior gate releases
    Some(f)
}

#[cfg(not(unix))]
fn gate_lock(_wt: &Path) -> Option<std::fs::File> {
    None
}

/// What one gate invocation established. The distinction between `Failed` and
/// `Error` is load-bearing, not cosmetic: only `Failed` is a verdict about the
/// *branch*. An `Error` (missing binary, non-executable, killed, unprovisioned
/// worktree) says nothing about the code, so it must never reach the fixing
/// agent and must never be bisected — see [`thegn_core::gate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// The gate ran and went green (or no gate was configured).
    Passed,
    /// The gate ran and went red. `log` is the tail of its output.
    Failed { log: String },
    /// The gate could not run. `reason` is the short headline, `log` whatever
    /// output there was.
    Error { reason: String, log: String },
}

impl GateVerdict {
    /// Did the folded tip clear the gate?
    pub(crate) fn passed(&self) -> bool {
        matches!(self, GateVerdict::Passed)
    }
}

/// Build/test the folded tip. By default (`gate_reuse_worktree`) this runs in a
/// stable per-repo worktree kept between folds, with a persistent
/// `CARGO_TARGET_DIR` — so cargo does a warm incremental rebuild instead of a
/// cold from-scratch compile (the local-compute win). With reuse off it falls
/// back to a fresh throwaway `/tmp` worktree removed afterward (concurrency-safe,
/// but always cold). Returns whether `gate_command` exited zero plus its captured
/// combined output (tail-truncated), which the queue driver feeds to a fixing
/// agent on a red gate.
// off-loop: the fold runs from the CLI (`thegn integrate`) or from
// spawn_fold's spawn_blocking (see the module doc) — never on the loop.
#[expect(clippy::disallowed_methods)]
pub(crate) fn gate_tip(repo_root: &Path, oid: &str, cfg: &MergeQueueConfig) -> Result<GateVerdict> {
    // Callers only reach here with a non-empty command (`gate_on` already checks
    // it), but guard anyway: an empty gate is a green no-op, not a worktree churn.
    if cfg.gate_command.is_empty() {
        return Ok(GateVerdict::Passed);
    }

    let reuse = cfg.gate_reuse_worktree;
    // Where the gate builds, and where cargo writes artifacts. Reused: a stable
    // per-repo worktree + target dir kept between folds. Throwaway: a unique /tmp
    // worktree. An explicit `gate_target_dir` overrides the target location in
    // either mode. Reuse depends on drains being serialized (queue design); the
    // per-repo path also keeps concurrent drains of *different* repos apart.
    let (wt, target_dir) = if reuse {
        let base = gate_base(repo_root);
        let td = if cfg.gate_target_dir.is_empty() {
            base.join("target")
        } else {
            PathBuf::from(&cfg.gate_target_dir)
        };
        (base.join("wt"), Some(td))
    } else if cfg.gate_target_dir.is_empty() {
        (tmp_path("tg-foldgate"), None)
    } else {
        (
            tmp_path("tg-foldgate"),
            Some(PathBuf::from(&cfg.gate_target_dir)),
        )
    };
    let wt_s = wt.to_string_lossy().to_string();

    // Serialize concurrent gate runs on the SAME reused worktree: two `land` /
    // `drain` processes (e.g. a CLI land + a running instance's autopilot) would
    // otherwise check out DIFFERENT OIDs into one worktree and clobber each
    // other's checkout + gate. The queue design assumes serialization but nothing
    // enforced it ACROSS processes. A blocking flock on a persistent sidecar lock
    // makes the second run wait its turn (a gate can take minutes — waiting is
    // correct). Reuse mode only; throwaway mode already uses unique /tmp paths.
    // Held for the whole prepare→gate→cleanup below (RAII: released on return).
    let _gate_lock = if reuse { gate_lock(&wt) } else { None };

    // (Re)create the gate worktree fresh at `wt`, pruning any stale registration
    // for a path whose dir was removed out from under us.
    let create = || {
        if let Some(parent) = wt.parent() {
            let _ = std::fs::create_dir_all(parent); // best-effort: create_dir_all is idempotent
        }
        let _ = util::git_ok(repo_root, &["worktree", "prune"]); // best-effort: drop stale registration
        util::git_ok(
            repo_root,
            &["worktree", "add", "--detach", "--force", &wt_s, oid],
        )
    };
    // Materialize the folded OID. Refresh a reused worktree in place — `checkout`
    // only re-touches files that actually changed between folds, so cargo rebuilds
    // just the affected crates. If that fails (stale/corrupt registration) self-heal
    // by recreating it; the sibling `target/` dir survives. New paths create fresh.
    let prepared = if reuse && wt.exists() {
        util::git_ok(&wt, &["checkout", "--detach", "--force", oid]) || {
            let _ = std::fs::remove_dir_all(&wt); // best-effort: self-heal a broken worktree
            create()
        }
    } else {
        create()
    };
    if !prepared {
        anyhow::bail!("merge queue: could not prepare gate worktree at {wt_s}");
    }

    // One shell setup for both the (optional) provisioning step and the gate,
    // so they see an identical environment.
    let spawn = |command: &str| {
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(&wt);
        if let Some(td) = &target_dir {
            let _ = std::fs::create_dir_all(td); // best-effort: create_dir_all is idempotent
            cmd.env("CARGO_TARGET_DIR", td);
        }
        // Scrub the git environment, exactly as `run_agent` does. An inherited
        // GIT_DIR/GIT_INDEX_FILE would otherwise point the gate's own `git` at
        // whatever repo invoked thegn instead of at the gate worktree.
        for var in util::GIT_ENV_VARS {
            cmd.env_remove(var);
        }
        cmd.env("THEGN_GATE", "1");
        cmd.env("THEGN_WORKTREE", &wt);
        cmd.env("THEGN_GATE_OID", oid);
        cmd.output()
    };

    // Provision the worktree first. It is a bare checkout of the folded tip —
    // no node_modules, no venv — so any gate whose entry point is a
    // project-local binary dies instantly without this. Deliberately NOT part of
    // the verdict: a failed setup is an environment failure, so it can neither
    // blame the branch nor wake the fixing agent.
    if !cfg.gate_setup_command.is_empty() {
        match spawn(&cfg.gate_setup_command) {
            Ok(o) if !o.status.success() => {
                let mut log = String::from_utf8_lossy(&o.stdout).into_owned();
                log.push_str(&String::from_utf8_lossy(&o.stderr));
                if !reuse {
                    let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &wt_s]);
                }
                return Ok(GateVerdict::Error {
                    reason: format!(
                        "gate_setup_command failed (exit {})",
                        o.status
                            .code()
                            .map_or_else(|| "signal".to_string(), |c| c.to_string())
                    ),
                    log: tail(&log, 4000),
                });
            }
            Err(e) => {
                if !reuse {
                    let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &wt_s]);
                }
                return Ok(GateVerdict::Error {
                    reason: "gate_setup_command could not be started".to_string(),
                    log: format!("{e}"),
                });
            }
            Ok(_) => {}
        }
    }

    let out = spawn(&cfg.gate_command);

    // A throwaway worktree is always removed; a reused one is kept — its warm
    // target/ is the whole point.
    if !reuse {
        let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &wt_s]);
    }
    // Classify rather than collapsing to a bool: the raw exit status is the only
    // place "the command never ran" is distinguishable from "the tests failed",
    // and dropping it here is what let `turbo: command not found` be recorded as
    // a verdict about the branch. See `thegn_core::gate`.
    Ok(match out {
        Ok(o) => {
            let mut log = String::from_utf8_lossy(&o.stdout).into_owned();
            log.push_str(&String::from_utf8_lossy(&o.stderr));
            let log = tail(&log, 4000);
            match gate::classify_exit(o.status.code(), false) {
                gate::GateClass::Passed => GateVerdict::Passed,
                gate::GateClass::Failed => GateVerdict::Failed { log },
                gate::GateClass::Error => GateVerdict::Error {
                    reason: gate::error_reason(o.status.code(), false).to_string(),
                    log,
                },
            }
        }
        Err(e) => GateVerdict::Error {
            reason: gate::error_reason(None, true).to_string(),
            log: format!("gate command failed to start: {e}"),
        },
    })
}

/// Keep the last `max` bytes of `s` on a char boundary (gate logs can be huge; the
/// tail — where the failure is — is what a fixing agent needs).
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = s.len() - max;
    while cut < s.len() && !s.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…\n{}", &s[cut..])
}

/// On a red gate, re-fold growing prefixes of the landed branches; the first
/// prefix whose gate goes red names its last branch as the offender. Returns
/// `None` when it can't localize one (e.g. a flaky gate), in which case the
/// whole batch is held back.
///
/// Aborts on a [`GateVerdict::Error`]: an environment failure reproduces at
/// every prefix, so bisecting one would burn a full gate run per branch and
/// then blame whichever branch happened to be first. The error is returned so
/// the caller reports the environment, not a branch.
fn bisect_offender(
    repo_root: &Path,
    adapter: &PlumbingAdapter,
    base: &str,
    landed: &[LandedReport],
    cfg: &MergeQueueConfig,
) -> Result<Option<String>> {
    let mut prefix: Vec<Branch> = Vec::new();
    for l in landed {
        // The branch tip is the merge commit's second parent — but we re-fold
        // from the branch's own tip, which `candidate_branches` already gave us
        // via the Landed entry's name; reuse the running adapter to re-merge.
        prefix.push(Branch {
            name: l.branch.clone(),
            tip: branch_tip(repo_root, &l.branch)?,
        });
        let plan = fold::fold(adapter, base, prefix.clone(), &cfg.regenerate_paths)?;
        if plan.advanced() {
            match gate_tip(repo_root, &plan.final_tip, cfg)? {
                GateVerdict::Passed => {}
                GateVerdict::Failed { .. } => return Ok(Some(l.branch.clone())),
                // Environmental: identical at every prefix, so stop rather than
                // blame `l.branch` for a missing binary.
                v @ GateVerdict::Error { .. } => return Err(BisectAborted(v).into()),
            }
        }
    }
    Ok(None)
}

/// A bisect that stopped because the gate could not run. Carries the verdict so
/// the caller can report the environment failure verbatim.
#[derive(Debug)]
pub(crate) struct BisectAborted(pub(crate) GateVerdict);

impl std::fmt::Display for BisectAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            GateVerdict::Error { reason, .. } => write!(f, "{reason}"),
            _ => write!(f, "gate could not run"),
        }
    }
}

impl std::error::Error for BisectAborted {}

fn branch_tip(repo_root: &Path, branch: &str) -> Result<String> {
    let loc = GitLoc::for_worktree(repo_root);
    CliGit.rev_parse(&loc, &format!("refs/heads/{branch}"))
}

/// Compose a [`FoldReport`] from a plan plus any gate offenders. `advanced` is
/// left false; callers set it after a successful CAS.
fn build_report(
    target_branch: &str,
    original: &str,
    plan: &FoldPlan,
    gate_offenders: &[String],
    gate: GateOutcome,
    cas_attempts: u32,
) -> FoldReport {
    let mut deferred: Vec<DeferredReport> = plan
        .deferred
        .iter()
        .map(|d| DeferredReport {
            branch: d.branch.name.clone(),
            paths: d.paths.clone(),
            kind: d.kind,
            gate_failed: false,
        })
        .collect();
    for off in gate_offenders {
        deferred.push(DeferredReport {
            branch: off.clone(),
            paths: Vec::new(),
            kind: ConflictKind::Textual,
            gate_failed: true,
        });
    }
    let landed: Vec<LandedReport> = plan
        .landed
        .iter()
        .map(|l| LandedReport {
            branch: l.branch.name.clone(),
            commit: l.commit.clone(),
        })
        .collect();
    FoldReport {
        resyncs: Vec::new(),
        target_branch: target_branch.to_string(),
        original: original.to_string(),
        final_tip: plan.final_tip.clone(),
        advanced: false,
        landed,
        deferred,
        gate,
        cas_attempts,
    }
}

/// Fold `candidates` onto the repo's target branch: merge clean branches in the
/// object DB, gate the union, and CAS-advance the target ref. Clean branches
/// land; conflicts and gate-offenders are deferred. No working tree is touched
/// except the throwaway gate worktree and — after a successful advance — a
/// guarded fast-forward of the repo's own main checkout (see
/// [`util::resync_ff_checkout`]) so `git status` there stays coherent.
pub fn run_fold(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    candidates: Vec<Branch>,
) -> Result<FoldReport> {
    let loc = GitLoc::for_worktree(repo_root);
    let adapter = PlumbingAdapter {
        loc: loc.clone(),
        repo_root: repo_root.to_path_buf(),
        regenerate_paths: cfg.regenerate_paths.clone(),
        regenerate_command: cfg.regenerate_command.clone(),
    };
    let target_branch = resolve_target(cfg, repo_root);
    let target_ref = format!("refs/heads/{target_branch}");
    let original = CliGit.rev_parse(&loc, &target_ref)?;

    let gate_on = cfg.gate_on && !cfg.gate_command.is_empty();
    let mut excluded: HashSet<String> = HashSet::new();
    let mut gate_offenders: Vec<String> = Vec::new();
    let mut cas_attempts = 0u32;

    loop {
        // Re-read the tip each round so a CAS retry folds onto the moved branch.
        let base = CliGit.rev_parse(&loc, &target_ref)?;
        let to_fold: Vec<Branch> = candidates
            .iter()
            // Skip branches bisect held back, and ones already in the target
            // (an already-merged tip would otherwise produce a no-op merge commit).
            .filter(|b| !excluded.contains(&b.name))
            .filter(|b| !util::git_ok(repo_root, &["merge-base", "--is-ancestor", &b.tip, &base]))
            .cloned()
            .collect();
        let plan = fold::fold(&adapter, &base, to_fold, &cfg.regenerate_paths)?;

        if !plan.advanced() {
            // Nothing merged clean. If bisect held branches back, the gate is the
            // reason nothing advanced; otherwise everything just conflicted.
            let gate = if gate_offenders.is_empty() {
                GateOutcome::Skipped
            } else {
                GateOutcome::Failed { offender: None }
            };
            return Ok(build_report(
                &target_branch,
                &original,
                &plan,
                &gate_offenders,
                gate,
                cas_attempts,
            ));
        }

        // Test-gate the union before blessing it.
        let gate = if gate_on {
            let verdict = gate_tip(repo_root, &plan.final_tip, cfg)?;
            // The gate could not run: report the environment and hold everything
            // back. Never bisect — the failure is identical at every prefix.
            if let GateVerdict::Error { reason, .. } = &verdict {
                return Ok(build_report(
                    &target_branch,
                    &original,
                    &plan,
                    &gate_offenders,
                    GateOutcome::Errored {
                        reason: reason.clone(),
                    },
                    cas_attempts,
                ));
            }
            if verdict.passed() {
                GateOutcome::Passed
            } else if cfg.bisect_on_red {
                let landed: Vec<LandedReport> = plan
                    .landed
                    .iter()
                    .map(|l| LandedReport {
                        branch: l.branch.name.clone(),
                        commit: l.commit.clone(),
                    })
                    .collect();
                let bisected = match bisect_offender(repo_root, &adapter, &base, &landed, cfg) {
                    Ok(o) => o,
                    // The gate stopped being runnable mid-bisect: report the
                    // environment rather than blaming whichever branch was next.
                    Err(e) => {
                        let reason = match e.downcast_ref::<BisectAborted>() {
                            Some(b) => b.to_string(),
                            None => return Err(e),
                        };
                        return Ok(build_report(
                            &target_branch,
                            &original,
                            &plan,
                            &gate_offenders,
                            GateOutcome::Errored { reason },
                            cas_attempts,
                        ));
                    }
                };
                if let Some(off) = bisected {
                    excluded.insert(off.clone());
                    gate_offenders.push(off);
                    continue; // re-fold without the offender
                }
                return Ok(build_report(
                    &target_branch,
                    &original,
                    &plan,
                    &gate_offenders,
                    GateOutcome::Failed { offender: None },
                    cas_attempts,
                ));
            } else {
                return Ok(build_report(
                    &target_branch,
                    &original,
                    &plan,
                    &gate_offenders,
                    GateOutcome::Failed { offender: None },
                    cas_attempts,
                ));
            }
        } else {
            GateOutcome::Skipped
        };

        // Green (or no gate) → atomically advance the target ref.
        cas_attempts += 1;
        if CliGit.update_ref_cas(&loc, &target_ref, &plan.final_tip, &base)? {
            // The fold moved the ref via pure plumbing, so the repo's MAIN
            // checkout (which is *on* this branch) now has a `HEAD` resolving to
            // the new tip while its index+tree still hold `base` — `git status`
            // there shows the folded files as pending, and a read-only sandbox
            // mount of it can't self-heal. Fast-forward it host-side (a safe
            // no-op when the checkout has real uncommitted work; see the guards).
            let resyncs =
                util::resync_branch_checkouts(repo_root, &target_branch, &base, &plan.final_tip);
            for r in &resyncs {
                match &r.outcome {
                    util::ResyncOutcome::Healed => thegn_core::msg::info(&format!(
                        "merge queue: synced {} to {}",
                        r.path.display(),
                        &plan.final_tip[..plan.final_tip.len().min(9)]
                    )),
                    // NOT silent any more — the caller renders these on stdout.
                    util::ResyncOutcome::Skipped(why) => tracing::debug!(
                        target: "thegn::integrate",
                        why,
                        path = %r.path.display(),
                        "left checkout working tree as-is"
                    ),
                    util::ResyncOutcome::Failed => tracing::warn!(
                        target: "thegn::integrate",
                        path = %r.path.display(),
                        "could not fast-forward the checkout"
                    ),
                }
            }
            let mut report = build_report(
                &target_branch,
                &original,
                &plan,
                &gate_offenders,
                gate,
                cas_attempts,
            );
            report.advanced = true;
            report.resyncs = resyncs;
            return Ok(report);
        }
        if cas_attempts >= 5 {
            anyhow::bail!("merge queue: {target_branch} kept moving under the fold");
        }
        // Lost the race — loop, re-read, re-fold.
    }
}

/// Print a warning for every checkout of `branch` the fold could NOT
/// fast-forward, with the exact command that syncs it.
///
/// The ref moved under those working trees, so `git status` there now shows the
/// whole fold as pending deletions — which reads as a catastrophic accidental
/// deletion, and which `git commit` would turn into a revert of the merge that
/// just landed. Deriving the recovery is the hard part, so we spell it out
/// rather than leaving the user to work it out from a wall of `D ` lines.
///
/// Goes to stdout, deliberately: this used to be a `tracing::warn!` that was
/// invisible without `THEGN_LOG`, and a `Skipped` outcome was dropped entirely.
pub(crate) fn report_resyncs(branch: &str, resyncs: &[util::CheckoutResync]) {
    for r in resyncs {
        let why = match &r.outcome {
            // Healed is the happy path and needs no warning.
            util::ResyncOutcome::Healed => continue,
            util::ResyncOutcome::Skipped(why) => *why,
            util::ResyncOutcome::Failed => "the fast-forward could not be applied",
        };
        outln!(
            "! {} is on {branch} and was NOT resynced ({why}).",
            r.path.display()
        );
        outln!("  Its working tree still holds the pre-fold content, so `git status`");
        outln!("  there mixes this fold in with your own changes — don't commit it");
        outln!("  blindly. Reconcile your changes, then sync it with:");
        outln!("    {}", r.manual_fix());
    }
}

/// What the driver's single-branch land attempt decided.
#[derive(Debug, Clone)]
pub(crate) enum AttemptOutcome {
    /// Merged clean, gated green, and CAS-advanced the target. `commit` is the
    /// fold tip now at the target ref. `resyncs` reports what happened to each
    /// live checkout of the target branch (see `util::resync_branch_checkouts`).
    Landed {
        commit: String,
        resyncs: Vec<util::CheckoutResync>,
    },
    /// Merged clean and gated green, but `auto_land` is off — held for a manual
    /// land. `tip` is the (unreferenced) fold commit in the object DB.
    Ready { tip: String },
    /// A textual (or unresolved regenerable) conflict against the current target.
    Conflict { paths: Vec<String> },
    /// Merged clean but the gate went red. `log` is the tail of the gate output.
    /// A verdict about the *branch* — this is the one a fixing agent can act on.
    GateFailed { log: String },
    /// Merged clean, but the gate could not RUN (missing binary, unprovisioned
    /// gate worktree, killed). A fact about the *environment*, so the branch is
    /// not blamed and the fixing agent is never dispatched — it cannot help.
    GateError { reason: String, log: String },
    /// The branch tip is already an ancestor of the target — nothing to do.
    UpToDate,
    /// The branch lives on another host and its tip could not be fetched into
    /// the target store (host unreachable / bundle or fetch failed). `detail`
    /// is the reason. Held (deferred) rather than dropped, so a transient
    /// network blip is retryable on the next drain.
    Unreachable { detail: String },
}

/// Attempt to land a *single* branch onto the repo's current target tip, the way
/// the queue driver drains one at a time. Mirrors [`run_fold`]'s fold→gate→CAS
/// path (re-reading the tip and re-folding on a lost CAS race), but for one branch
/// and with a richer per-outcome result the driver can route to an agent. Never
/// touches a working tree except the throwaway gate worktree and — on a successful
/// advance — the guarded main-checkout fast-forward.
pub(crate) fn attempt_land(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    branch_name: &str,
    branch_loc: &GitLoc,
) -> Result<AttemptOutcome> {
    let loc = GitLoc::for_worktree(repo_root);
    let target_branch = resolve_target(cfg, repo_root);
    let target_ref = format!("refs/heads/{target_branch}");
    // Cross-host: if the branch's worktree lives on another machine, fetch its
    // tip into the target store first and fold that synthetic ref — a bare
    // `refs/heads/<branch>` only exists in the branch host's own object store.
    // A same-store branch just yields `refs/heads/<branch>` (no I/O).
    let branch_ref = match crate::merge_remote::ensure_tip_in_target(&loc, branch_name, branch_loc)
    {
        Ok(r) => r,
        Err(e) => {
            return Ok(AttemptOutcome::Unreachable {
                detail: format!("{e:#}"),
            });
        }
    };
    let adapter = PlumbingAdapter {
        loc: loc.clone(),
        repo_root: repo_root.to_path_buf(),
        regenerate_paths: cfg.regenerate_paths.clone(),
        regenerate_command: cfg.regenerate_command.clone(),
    };
    let gate_on = cfg.gate_on && !cfg.gate_command.is_empty();
    let mut cas_attempts = 0u32;
    loop {
        let base = CliGit.rev_parse(&loc, &target_ref)?;
        let branch_tip = CliGit.rev_parse(&loc, &branch_ref)?;
        if util::git_ok(
            repo_root,
            &["merge-base", "--is-ancestor", &branch_tip, &base],
        ) {
            return Ok(AttemptOutcome::UpToDate);
        }
        let branch = Branch {
            name: branch_name.to_string(),
            tip: branch_tip,
        };
        let plan = fold::fold(&adapter, &base, vec![branch], &cfg.regenerate_paths)?;
        if !plan.advanced() {
            // One branch that didn't advance the tip ⇒ it was deferred (conflict).
            let paths = plan
                .deferred
                .first()
                .map(|d| d.paths.clone())
                .unwrap_or_default();
            return Ok(AttemptOutcome::Conflict { paths });
        }
        let folded_tip = plan.final_tip.clone();
        if gate_on {
            match gate_tip(repo_root, &folded_tip, cfg)? {
                GateVerdict::Passed => {}
                GateVerdict::Failed { log } => {
                    return Ok(AttemptOutcome::GateFailed { log });
                }
                GateVerdict::Error { reason, log } => {
                    return Ok(AttemptOutcome::GateError { reason, log });
                }
            }
        }
        if !cfg.auto_land {
            return Ok(AttemptOutcome::Ready { tip: folded_tip });
        }
        cas_attempts += 1;
        if CliGit.update_ref_cas(&loc, &target_ref, &folded_tip, &base)? {
            // Every live checkout of the target, not just the main one — and the
            // outcomes ride out on the result so the caller can report the ones
            // left stale. Dropping `Skipped` here is what made the desync silent.
            let resyncs =
                util::resync_branch_checkouts(repo_root, &target_branch, &base, &folded_tip);
            return Ok(AttemptOutcome::Landed {
                commit: folded_tip,
                resyncs,
            });
        }
        if cas_attempts >= 5 {
            anyhow::bail!("merge queue: {target_branch} kept moving under the fold");
        }
        // Lost the CAS race — loop, re-read, re-fold onto the moved tip.
    }
}

/// The opt-in guard. Kept apart from the git-fixture tests below because it is
/// pure: the whole point of `hold_unenqueued` taking a set is that the rule that
/// decides whether someone's branch gets landed is testable without a repo.
#[cfg(test)]
mod enqueue_guard_tests {
    use super::*;

    fn cands(pairs: &[(&str, &str)]) -> Candidates {
        Candidates {
            branches: pairs
                .iter()
                .map(|(b, _)| Branch {
                    name: (*b).to_string(),
                    tip: format!("{b}-tip"),
                })
                .collect(),
            skipped_dirty: Vec::new(),
            worktrees: pairs
                .iter()
                .map(|(b, w)| ((*b).to_string(), (*w).to_string()))
                .collect(),
        }
    }

    #[test]
    fn only_enqueued_branches_survive() {
        let mut c = cands(&[("feat/a", "/wt/a"), ("feat/b", "/wt/b")]);
        let held = hold_unenqueued(&mut c, &HashSet::from(["/wt/a".to_string()]));
        assert_eq!(
            c.branches.iter().map(|b| &b.name).collect::<Vec<_>>(),
            ["feat/a"]
        );
        assert_eq!(held, ["feat/b"]);
    }

    /// The regression this whole guard exists for: an empty queue must fold
    /// NOTHING. The old behavior folded every clean worktree branch in the repo,
    /// so a branch nobody nominated was landed and (with `on_landed = "remove"`)
    /// its worktree deleted.
    #[test]
    fn an_empty_queue_folds_nothing() {
        let mut c = cands(&[("wip/mine", "/wt/mine"), ("wip/yours", "/wt/yours")]);
        let held = hold_unenqueued(&mut c, &HashSet::new());
        assert!(c.branches.is_empty(), "an empty queue must fold nothing");
        assert_eq!(held.len(), 2, "and must name what it held back");
    }

    /// Membership is keyed by worktree path, not branch name — a rename must not
    /// silently drop a branch out of the queue it is sitting in.
    #[test]
    fn membership_follows_the_worktree_not_the_branch_name() {
        let mut c = cands(&[("renamed/later", "/wt/a")]);
        let held = hold_unenqueued(&mut c, &HashSet::from(["/wt/a".to_string()]));
        assert_eq!(c.branches.len(), 1);
        assert!(held.is_empty());
    }

    /// A candidate with no worktree mapping cannot be proven enqueued, so it is
    /// held rather than folded: unknown provenance fails closed.
    #[test]
    fn a_candidate_without_a_worktree_is_held() {
        let mut c = cands(&[("feat/a", "/wt/a")]);
        c.branches.push(Branch {
            name: "orphan".into(),
            tip: "orphan-tip".into(),
        });
        let held = hold_unenqueued(&mut c, &HashSet::from(["/wt/a".to_string()]));
        assert_eq!(held, ["orphan"]);
        assert_eq!(c.branches.len(), 1);
    }

    #[test]
    fn holding_preserves_candidate_order() {
        let mut c = cands(&[("a", "/1"), ("b", "/2"), ("c", "/3"), ("d", "/4")]);
        let held = hold_unenqueued(&mut c, &HashSet::from(["/1".to_string(), "/3".to_string()]));
        assert_eq!(
            c.branches.iter().map(|b| &b.name).collect::<Vec<_>>(),
            ["a", "c"]
        );
        assert_eq!(held, ["b", "d"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Minimal real-git fixture: a repo with `main` plus N branches each adding
    /// one file, created via the worktree-less `git branch` + index plumbing so
    /// we exercise `run_fold` against actual object-DB merges.
    struct Repo {
        dir: PathBuf,
    }
    impl Repo {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tg-integ-{tag}-{}-{}",
                std::process::id(),
                util::now()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q", "-b", "main"]);
            git(&dir, &["config", "user.name", "t"]);
            git(&dir, &["config", "user.email", "t@e"]);
            git(&dir, &["config", "commit.gpgsign", "false"]);
            let r = Repo { dir };
            r.commit("base.txt", "base\n", "c0");
            r
        }
        fn commit(&self, file: &str, body: &str, msg: &str) {
            std::fs::write(self.dir.join(file), body).unwrap();
            git(&self.dir, &["add", file]);
            git(&self.dir, &["commit", "-q", "-m", msg]);
        }
        /// Create `branch` off main with one extra commit touching `file`.
        fn feature(&self, branch: &str, file: &str, body: &str) {
            git(&self.dir, &["checkout", "-q", "-b", branch]);
            self.commit(file, body, &format!("{branch} work"));
            git(&self.dir, &["checkout", "-q", "main"]);
        }
        // test code: fixture plumbing, never on the event loop.
        #[expect(clippy::disallowed_methods)]
        fn out(&self, args: &[&str]) -> String {
            String::from_utf8_lossy(&util::git_cmd(&self.dir).args(args).output().unwrap().stdout)
                .trim()
                .to_string()
        }
        fn branch_set(&self) -> Vec<Branch> {
            // All local branches except main, as (name, tip).
            self.out(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
                .lines()
                .filter(|b| *b != "main")
                .map(|b| Branch {
                    name: b.to_string(),
                    tip: self.out(&["rev-parse", &format!("refs/heads/{b}")]),
                })
                .collect()
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    // test code: fixture plumbing, never on the event loop.
    #[expect(clippy::disallowed_methods)]
    fn git(dir: &Path, args: &[&str]) {
        let ok = util::git_cmd(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }
    fn cfg(gate: &str) -> MergeQueueConfig {
        MergeQueueConfig {
            enabled: true,
            target_branch: "main".into(),
            gate_command: gate.into(),
            gate_on: !gate.is_empty(),
            bisect_on_red: true,
            snapshot_dirty: false,
            regenerate_paths: vec!["Cargo.lock".into()],
            regenerate_command: String::new(),
            conflict_handoff: Default::default(),
            agent_command: String::new(),
            auto_land: true,
            agent_max_attempts: 2,
            agent_timeout_secs: 0,
            // Throwaway gate worktree (a unique /tmp path) rather than the
            // reused one, which lives under `$XDG_STATE_HOME`. Rust runs these
            // tests as threads in ONE process, and `testenv::EnvVarGuard`
            // repoints `XDG_STATE_HOME` process-globally — its lock only
            // excludes other ENV_LOCK-respecting tests, which these are not. So
            // a reused gate would be built inside a *different* test's temp dir
            // and vanish under it when that test's guard dropped. Depending on
            // ambient env here is the bug; not depending on it is the fix.
            gate_reuse_worktree: false,
            ..MergeQueueConfig::default()
        }
    }

    #[test]
    fn clean_disjoint_branches_all_land_and_advance_main() {
        let repo = Repo::new("clean");
        repo.feature("b1", "a.txt", "a\n");
        repo.feature("b2", "b.txt", "b\n");
        let before = repo.out(&["rev-parse", "main"]);

        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced);
        assert_eq!(report.landed.len(), 2);
        assert!(report.deferred.is_empty());
        // main moved and now contains both files.
        assert_ne!(repo.out(&["rev-parse", "main"]), before);
        let files = repo.out(&["ls-tree", "-r", "--name-only", "main"]);
        assert!(
            files.contains("a.txt") && files.contains("b.txt"),
            "{files}"
        );
    }

    #[test]
    fn conflicting_branch_is_deferred_clean_one_still_lands() {
        let repo = Repo::new("conflict");
        repo.feature("clean", "ok.txt", "ok\n");
        // Both edit base.txt → conflicts against main once nothing else, but
        // here main is unchanged so the conflict is branch-vs-base.
        repo.feature("bad", "base.txt", "changed\n");
        // Advance main's base.txt so `bad` truly conflicts.
        repo.commit("base.txt", "mainline\n", "main edits base");

        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced, "the clean branch should land");
        assert_eq!(
            report
                .landed
                .iter()
                .map(|l| l.branch.as_str())
                .collect::<Vec<_>>(),
            ["clean"]
        );
        assert_eq!(report.deferred.len(), 1);
        assert_eq!(report.deferred[0].branch, "bad");
        assert!(!report.deferred[0].gate_failed);
    }

    #[test]
    fn green_gate_advances_red_gate_holds_back() {
        let repo = Repo::new("gate");
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);

        // Green gate → advances.
        let report = run_fold(&cfg("true"), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced);
        assert_eq!(report.gate, GateOutcome::Passed);
        assert_ne!(repo.out(&["rev-parse", "main"]), before);

        // Red gate on a fresh branch → main is NOT advanced; branch deferred as
        // a gate offender (bisect isolates the single landed branch).
        let mid = repo.out(&["rev-parse", "main"]);
        repo.feature("b2", "b.txt", "b\n");
        let report = run_fold(&cfg("false"), &repo.dir, repo.branch_set()).unwrap();
        assert!(!report.advanced);
        assert_eq!(
            repo.out(&["rev-parse", "main"]),
            mid,
            "red gate must not move main"
        );
        assert!(matches!(report.gate, GateOutcome::Failed { .. }));
        assert!(
            report
                .deferred
                .iter()
                .any(|d| d.branch == "b2" && d.gate_failed)
        );
    }

    /// Build a repo where branch `b1` and `main` both bump `Cargo.lock` (so the
    /// fold conflicts ONLY on the lockfile), plus a disjoint file on `b1`.
    fn regen_repo(tag: &str) -> Repo {
        let repo = Repo::new(tag);
        repo.commit("Cargo.lock", "base\n", "c0 lock");
        git(&repo.dir, &["checkout", "-q", "-b", "b1"]);
        repo.commit("a.txt", "a\n", "b1 add");
        repo.commit("Cargo.lock", "b1\n", "b1 lock");
        git(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit("Cargo.lock", "mainline\n", "main lock"); // diverge the lockfile
        repo
    }

    #[test]
    fn regenerable_lockfile_conflict_auto_lands_with_regenerate_command() {
        let repo = regen_repo("regen-land");
        let mut c = cfg("");
        c.regenerate_command = "printf 'regenerated\\n' > Cargo.lock".into();

        let report = run_fold(&c, &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced, "the regenerable branch should land");
        assert_eq!(
            report
                .landed
                .iter()
                .map(|l| l.branch.as_str())
                .collect::<Vec<_>>(),
            ["b1"]
        );
        assert!(report.deferred.is_empty());
        // main carries the regenerated lockfile and the disjoint file.
        assert_eq!(repo.out(&["show", "main:Cargo.lock"]), "regenerated");
        let files = repo.out(&["ls-tree", "-r", "--name-only", "main"]);
        assert!(files.contains("a.txt"), "{files}");
    }

    #[test]
    fn regenerable_conflict_defers_without_a_regenerate_command() {
        let repo = regen_repo("regen-defer");
        // cfg("") has regenerate_command = "" → no regeneration, just classify+defer.
        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(!report.advanced);
        assert_eq!(report.deferred.len(), 1);
        assert_eq!(report.deferred[0].branch, "b1");
        assert_eq!(report.deferred[0].kind, ConflictKind::Regenerable);
    }

    #[test]
    fn advancing_main_fast_forwards_the_main_checkout_working_tree() {
        let repo = Repo::new("resync-clean");
        repo.feature("b1", "a.txt", "a\n");
        repo.feature("b2", "b.txt", "b\n");
        // Before the fold the main checkout holds only base.txt on disk.
        assert!(!repo.dir.join("a.txt").exists());

        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced);
        // The resync fast-forwarded the working tree in place, so the folded
        // files now exist on disk and `git status` is clean (no pending diff).
        assert!(repo.dir.join("a.txt").exists(), "a.txt not materialized");
        assert!(repo.dir.join("b.txt").exists(), "b.txt not materialized");
        assert_eq!(repo.out(&["status", "--porcelain"]), "");
    }

    #[test]
    fn resync_never_clobbers_uncommitted_work_in_the_main_checkout() {
        let repo = Repo::new("resync-dirty");
        repo.feature("b1", "a.txt", "a\n");
        // Genuine uncommitted edit in the main checkout.
        std::fs::write(repo.dir.join("base.txt"), "MY LOCAL EDIT\n").unwrap();

        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced, "the ref still advances");
        // The dirty edit survived — resync detected real work and skipped rather
        // than reset --hard over it.
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("base.txt")).unwrap(),
            "MY LOCAL EDIT\n"
        );
    }

    // ── attempt_land (the single-branch primitive the queue driver uses) ──────

    #[test]
    fn attempt_land_lands_a_clean_branch() {
        let repo = Repo::new("al-clean");
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);

        match attempt_land(&cfg(""), &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap() {
            AttemptOutcome::Landed { commit, .. } => assert!(!commit.is_empty()),
            o => panic!("expected Landed, got {o:?}"),
        }
        assert_ne!(repo.out(&["rev-parse", "main"]), before, "main advanced");
        assert!(repo.dir.join("a.txt").exists());
    }

    #[test]
    fn attempt_land_reports_a_textual_conflict_without_moving_main() {
        let repo = Repo::new("al-conflict");
        repo.feature("bad", "base.txt", "changed\n");
        repo.commit("base.txt", "mainline\n", "main edits base");
        let before = repo.out(&["rev-parse", "main"]);

        match attempt_land(&cfg(""), &repo.dir, "bad", &GitLoc::Local(repo.dir.clone())).unwrap() {
            AttemptOutcome::Conflict { paths } => assert!(paths.iter().any(|p| p == "base.txt")),
            o => panic!("expected Conflict, got {o:?}"),
        }
        assert_eq!(
            repo.out(&["rev-parse", "main"]),
            before,
            "main must not move"
        );
    }

    #[test]
    fn attempt_land_reports_gate_failure_and_holds_main() {
        let repo = Repo::new("al-gate");
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);

        match attempt_land(
            &cfg("false"),
            &repo.dir,
            "b1",
            &GitLoc::Local(repo.dir.clone()),
        )
        .unwrap()
        {
            AttemptOutcome::GateFailed { .. } => {}
            o => panic!("expected GateFailed, got {o:?}"),
        }
        assert_eq!(
            repo.out(&["rev-parse", "main"]),
            before,
            "red gate holds main"
        );
    }

    #[test]
    fn attempt_land_holds_at_ready_when_auto_land_is_off() {
        let repo = Repo::new("al-ready");
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);
        let mut c = cfg("true"); // green gate
        c.auto_land = false;

        match attempt_land(&c, &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap() {
            AttemptOutcome::Ready { tip } => assert!(!tip.is_empty()),
            o => panic!("expected Ready, got {o:?}"),
        }
        assert_eq!(
            repo.out(&["rev-parse", "main"]),
            before,
            "ready does not land"
        );
    }

    #[test]
    fn attempt_land_is_uptodate_for_an_already_merged_branch() {
        let repo = Repo::new("al-uptodate");
        repo.feature("b1", "a.txt", "a\n");
        attempt_land(&cfg(""), &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap(); // land it
        // A second attempt sees b1's tip already an ancestor of main.
        assert!(matches!(
            attempt_land(&cfg(""), &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap(),
            AttemptOutcome::UpToDate
        ));
    }
}
