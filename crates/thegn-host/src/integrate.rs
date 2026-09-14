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
//! [`run_selected_fold`] is the synchronous, side-effecting batch entrypoint.
//! It retains pre-fold observations through snapshots, folding and publication;
//! callers must run it off the interactive event loop.

use crate::canonical_history::CanonicalHistory;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::fold::{
    self, Author, Branch, CommitMeta, ConflictKind, FoldGit, FoldPlan, LandOpts, MergeOutcome,
};
use thegn_core::outln;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;
use thegn_svc::git::{CliGit, GitBackend, MergeTreeOutcome, PlumbingOps};

#[path = "integrate_candidates.rs"]
mod candidates;
#[path = "integrate_gate.rs"]
mod gate_runner;
#[path = "integrate_persistence.rs"]
mod persistence;
pub(crate) use persistence::run_selected_fold;
#[cfg(test)]
use persistence::{observe_outcomes, persist};

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

/// A fold commit failed to be created while `sign_commits` was on. Carried as a
/// distinct error so the drain can classify it as an infrastructure fault (stop
/// with a reason, never blame the branch, never wake the fixing agent) rather
/// than a merge/gate verdict — a plumbing commit failure is never the branch's
/// fault, and under signing the overwhelmingly likely cause is a
/// gpg/ssh-agent that is locked, missing a key, or would prompt.
#[derive(Debug)]
pub(crate) struct SigningFailed(pub(crate) String);

impl std::fmt::Display for SigningFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "commit signing failed (non-interactive): {}", self.0)
    }
}

impl std::error::Error for SigningFailed {}

/// Drives the pure fold engine over real git plumbing at one repo root.
struct PlumbingAdapter {
    history: CanonicalHistory,
    loc: GitLoc,
    repo_root: PathBuf,
    regenerate_paths: Vec<String>,
    /// Empty disables lockfile regeneration (regenerable conflicts just defer).
    regenerate_command: String,
    /// Sign fold/land commits (`[merge_queue] sign_commits`).
    sign: bool,
    /// Enable git rerere in the driver-merge worktree (`[merge_queue] rerere`).
    rerere: bool,
}

impl PlumbingAdapter {
    /// Wrap a commit failure as [`SigningFailed`] when signing is on, so the
    /// drain routes it as infrastructure rather than blaming the branch.
    fn commit_err(&self, e: anyhow::Error) -> anyhow::Error {
        if self.sign {
            anyhow::Error::new(SigningFailed(format!("{e:#}")))
        } else {
            e
        }
    }
}

impl FoldGit for PlumbingAdapter {
    fn merge_tree(&self, ours: &str, theirs: &str) -> Result<MergeOutcome> {
        self.history
            .checked(|| match CliGit.merge_tree(&self.loc, ours, theirs)? {
                MergeTreeOutcome::Clean { tree } => Ok(MergeOutcome::Clean { tree }),
                MergeTreeOutcome::Conflict { paths, .. } => {
                    let base = CliGit.merge_base(&self.loc, ours, theirs)?;
                    Ok(self.resolve_conflict(base.as_deref(), ours, theirs, paths))
                }
            })
    }
    fn commit_tree(&self, tree: &str, parents: &[&str], msg: &str) -> Result<String> {
        self.history.checked(|| {
            CliGit
                .commit_tree_opts(&self.loc, tree, parents, msg, self.sign, None)
                .map_err(|e| self.commit_err(e))
        })
    }
    fn merge_tree_base(&self, base: &str, ours: &str, theirs: &str) -> Result<MergeOutcome> {
        self.history.checked(
            || match CliGit.merge_tree_base(&self.loc, base, ours, theirs)? {
                MergeTreeOutcome::Clean { tree } => Ok(MergeOutcome::Clean { tree }),
                // A per-commit replay conflict is never a lockfile-regeneration
                // case (that only makes sense for a whole-branch merge), so defer.
                MergeTreeOutcome::Conflict { paths, .. } => {
                    match self.submodule_conflicts(Some(base), ours, theirs, &paths) {
                        Ok(conflicts) if !conflicts.is_empty() => {
                            Ok(MergeOutcome::SubmoduleConflict { paths, conflicts })
                        }
                        // Conflict metadata is safety-critical. If the object DB
                        // cannot be read, retain the generic conflict so no later
                        // auto-resolution path can pick a pointer implicitly.
                        Ok(_) | Err(_) => Ok(MergeOutcome::Conflict { paths }),
                    }
                }
            },
        )
    }
    fn commit_tree_author(
        &self,
        tree: &str,
        parents: &[&str],
        msg: &str,
        author: &Author,
    ) -> Result<String> {
        self.history.checked(|| {
            CliGit
                .commit_tree_opts(&self.loc, tree, parents, msg, self.sign, Some(author))
                .map_err(|e| self.commit_err(e))
        })
    }
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        self.history.checked(|| CliGit.merge_base(&self.loc, a, b))
    }
    fn commits(&self, base_excl: &str, tip: &str) -> Result<Vec<CommitMeta>> {
        self.history
            .checked(|| CliGit.commits(&self.loc, base_excl, tip))
    }
}

impl PlumbingAdapter {
    fn submodule_conflicts(
        &self,
        base: Option<&str>,
        ours: &str,
        theirs: &str,
        paths: &[String],
    ) -> Result<Vec<thegn_core::submodule::SubmoduleConflict>> {
        CliGit.submodule_conflicts_for_paths(&self.loc, paths, base, ours, theirs)
    }

    /// Try to salvage a conflicting merge before deferring it. In order:
    ///
    /// 1. A conflict confined to regenerable artifacts (e.g. `Cargo.lock`) is
    ///    rebuilt via the throwaway-worktree path and landed.
    /// 2. When a conflicted path is governed by a custom `.gitattributes
    ///    merge=<driver>` — which the object-DB `merge-tree` may not honor — or
    ///    when `rerere` is on, the branch is merged in a throwaway worktree where
    ///    a real `git merge` runs the driver and rerere can auto-apply a recorded
    ///    resolution. A clean result feeds the fold; anything else defers.
    ///
    /// Clean folds never reach here, so they pay none of this cost.
    fn resolve_conflict(
        &self,
        base: Option<&str>,
        ours: &str,
        theirs: &str,
        paths: Vec<String>,
    ) -> MergeOutcome {
        // A gitlink is an atomic pointer owned by the superproject. Partition it
        // before regenerate/custom-driver/rerere handling so it can never enter
        // a throwaway real merge or the `git add -A` auto-resolution path.
        match self.submodule_conflicts(base, ours, theirs, &paths) {
            Ok(conflicts) if !conflicts.is_empty() => {
                return MergeOutcome::SubmoduleConflict { paths, conflicts };
            }
            Ok(_) => {}
            // Do not allow an unavailable object-db lookup to fall through to
            // regeneration, custom drivers, rerere, or blanket staging. A
            // generic conflict is the fail-closed result.
            Err(_) => return MergeOutcome::Conflict { paths },
        }
        if !self.regenerate_command.is_empty()
            && fold::classify(&paths, &self.regenerate_paths) == fold::ConflictKind::Regenerable
            && let Some(tree) = regenerate_merge(
                &self.repo_root,
                ours,
                theirs,
                &self.regenerate_paths,
                &self.regenerate_command,
            )
        {
            return MergeOutcome::Clean { tree };
        }
        // A real merge in a throwaway worktree is worth attempting when a custom
        // driver governs a conflicted path (the driver may resolve it) or rerere
        // is enabled (a recorded resolution may auto-apply). The check-attr probe
        // is skipped when rerere alone already warrants the worktree.
        if (self.rerere || has_custom_driver(&self.repo_root, &paths))
            && let Some(tree) = driver_merge(&self.repo_root, ours, theirs, self.rerere)
        {
            return MergeOutcome::Clean { tree };
        }
        MergeOutcome::Conflict { paths }
    }
}

/// Whether any of `paths` is governed by a custom `.gitattributes merge=<driver>`
/// declaration (`git check-attr merge`, batched, `-z` so paths with spaces
/// parse). Built-in values (`unspecified`/`unset`/`set`) are not custom drivers.
// off-loop: runs inside the fold (CLI / spawn_blocking), never on the loop.
fn has_custom_driver(repo_root: &Path, paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let mut args: Vec<&str> = vec!["check-attr", "-z", "merge", "--"];
    args.extend(paths.iter().map(|s| s.as_str()));
    let Some(out) = util::git_out(repo_root, &args) else {
        return false;
    };
    // `-z` output is a flat NUL-separated stream of (path, attr, value) triples.
    let mut it = out.split('\0');
    while let (Some(_path), Some(_attr), Some(value)) = (it.next(), it.next(), it.next()) {
        if !matches!(value, "unspecified" | "unset" | "set" | "") {
            return true;
        }
    }
    false
}

/// Merge `theirs` onto `ours` in a throwaway detached worktree with a real
/// `git merge`, so custom `.gitattributes` merge drivers run and (when `rerere`)
/// a recorded resolution auto-applies against the shared `rr-cache`. Returns the
/// written tree oid only if the merge ends with NO unmerged paths; otherwise
/// `None` (caller defers). Never leaves a worktree behind.
// off-loop: the fold runs from the CLI / spawn_blocking (see the module doc).
#[expect(clippy::disallowed_methods)]
fn driver_merge(repo_root: &Path, ours: &str, theirs: &str, rerere: bool) -> Option<String> {
    let tmp = tmp_path("tg-drivermerge");
    let tmp_s = tmp.to_string_lossy().to_string();
    if !util::git_ok(
        repo_root,
        &["worktree", "add", "--detach", "--force", &tmp_s, ours],
    ) {
        return None;
    }
    let tree = (|| -> Option<String> {
        // rerere on: apply recorded resolutions and stage them (autoUpdate).
        let mut args: Vec<&str> = Vec::new();
        if rerere {
            args.extend_from_slice(&["-c", "rerere.enabled=true", "-c", "rerere.autoUpdate=true"]);
        }
        // A background merge must never sign or prompt.
        args.extend_from_slice(&[
            "-c",
            "commit.gpgSign=false",
            "merge",
            "--no-commit",
            "--no-ff",
            theirs,
        ]);
        // Conflicts are expected (the driver / rerere may resolve them) → ignore
        // the exit status and inspect the index next.
        util::git_cmd(&tmp).args(&args).output().ok()?;
        // Stage whatever the driver / rerere resolved.
        let _ = util::git_cmd(&tmp).args(["add", "-A"]).output(); // best-effort: stage: the unmerged-path check below catches a failed stage
        // Any remaining unmerged path means we couldn't resolve it here → defer.
        let unmerged =
            util::git_out(&tmp, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        if !unmerged.trim().is_empty() {
            return None;
        }
        let tree = util::git_out(&tmp, &["write-tree"])?;
        let tree = tree.trim().to_string();
        (!tree.is_empty()).then_some(tree)
    })();
    let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &tmp_s]); // best-effort: cleanup: a leaked tmp worktree is reaped by `worktree prune`; it must not fail the fold
    if tree.is_some() {
        thegn_core::msg::info(
            "merge queue: resolved a conflict via a merge-driver/rerere worktree",
        );
    }
    tree
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
        util::git_cmd(&tmp)
            .args(["merge", "--no-commit", "--no-ff", theirs])
            .output()
            .ok()?;
        // Take the incoming version of each regenerate path so it's a valid file
        // (not conflict-marked), then the regen command reconciles it.
        for p in regenerate_paths {
            let _ = util::git_cmd(&tmp) // best-effort: regenerate paths are rewritten below; the unmerged check catches the rest
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
        let _ = util::git_cmd(&tmp).args(["add", "-A"]).output(); // best-effort: stage: the unmerged-path check below catches a failed stage
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
    let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &tmp_s]); // best-effort: cleanup: a leaked tmp worktree is reaped by `worktree prune`; it must not fail the fold
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
    /// Typed pointer conflicts retained separately from raw paths so reports
    /// and handoffs can name both object IDs without changing git targets.
    pub submodule_conflicts: Vec<thegn_core::submodule::SubmoduleConflict>,
    /// True when this branch was deferred by the test-gate (bisected offender),
    /// not by a textual merge conflict.
    pub gate_failed: bool,
}

/// The outcome of one batch fold.
#[derive(Debug, Clone)]
pub struct FoldReport {
    pub target_branch: String,
    pub original: String,
    pub final_tip: String,
    pub advanced: bool,
    pub landed: Vec<LandedReport>,
    /// Object-DB folds not committed to the target. Never authorizes cleanup.
    pub prepared: Vec<LandedReport>,
    /// Candidates held before a complete fold plan could be produced.
    pub unprepared: Vec<String>,
    pub deferred: Vec<DeferredReport>,
    pub gate: GateOutcome,
    /// Bounded union/base/prefix diagnostics, including failures before CAS.
    pub diagnostics: String,
    /// Git may have advanced even when final cache persistence was refused.
    pub bookkeeping_error: Option<String>,
    /// How many CAS attempts it took (main moving under the fold forces a re-fold).
    pub cas_attempts: u32,
    /// What happened to each live checkout of the target branch when the ref
    /// advanced. Advisory, like `Candidates::skipped_dirty`: the caller reports
    /// the ones we could not fast-forward, so a stale working tree is never a
    /// silent surprise. Empty when nothing advanced.
    pub resyncs: Vec<util::CheckoutResync>,
}

impl FoldReport {
    /// A no-op is successful; any requested branch still held is not.
    pub(crate) fn request_result(&self) -> Result<()> {
        match &self.gate {
            GateOutcome::Errored { reason } => anyhow::bail!("integration held: {reason}"),
            GateOutcome::Failed { .. } => anyhow::bail!("integration held: gate failed"),
            _ if !self.prepared.is_empty()
                || !self.unprepared.is_empty()
                || !self.deferred.is_empty()
                || (!self.advanced && !self.landed.is_empty()) =>
            {
                anyhow::bail!("integration incomplete: requested branches remain held")
            }
            _ if self.bookkeeping_error.is_some() => anyhow::bail!(
                "queue bookkeeping unavailable: {}",
                self.bookkeeping_error.as_deref().unwrap_or_default()
            ),
            _ => Ok(()),
        }
    }
}

/// Preserve readable text without executable terminal controls or bidi formatting.
fn diagnostic_char(c: char) -> bool {
    (!c.is_control() || matches!(c, '\n' | '\t'))
        && !matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// Preserve early union/base evidence even if a long bisect exhausts the budget.
fn record_gate(diagnostics: &mut String, phase: &str, verdict: &GateVerdict) {
    const MAX_DIAGNOSTICS: usize = 32 * 1024;
    const TRUNCATED: &str = "\n[additional gate diagnostics omitted]\n";
    if diagnostics.ends_with(TRUNCATED) {
        return;
    }
    let (headline, log) = match verdict {
        GateVerdict::Passed => ("passed", ""),
        GateVerdict::Failed { log } => ("failed", log.as_str()),
        GateVerdict::Error { reason, log } => (reason.as_str(), log.as_str()),
    };
    let entry = format!("[{phase}] {headline}\n{}\n", tail(log, 4000));
    let mut clean = String::new();
    for c in entry.chars().take(5000) {
        if diagnostic_char(c) {
            clean.push(c);
        }
    }
    if diagnostics.len() + clean.len() + TRUNCATED.len() > MAX_DIAGNOSTICS {
        diagnostics.push_str(TRUNCATED);
    } else {
        diagnostics.push_str(&clean);
    }
}

/// Render raw conflict paths while replacing typed gitlink paths with their
/// pointer-specific detail. Mixed text/gitlink conflicts therefore retain the
/// ordinary paths instead of losing them when typed metadata is available.
pub(crate) fn conflict_details(
    paths: &[String],
    conflicts: &[thegn_core::submodule::SubmoduleConflict],
) -> Vec<String> {
    let typed: std::collections::HashSet<&str> = conflicts
        .iter()
        .map(|conflict| conflict.path.as_str())
        .collect();
    paths
        .iter()
        .filter(|path| !typed.contains(path.as_str()))
        .cloned()
        .chain(
            conflicts
                .iter()
                .map(thegn_core::submodule::format_submodule_conflict),
        )
        .collect()
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
    /// Local Git identities captured during read-only discovery, never DB-routed.
    pub(crate) identities: HashMap<String, candidates::LocalIdentity>,
    /// Dirty candidates whose snapshot is pending explicit fold admission.
    pub(crate) pending_snapshots: HashSet<String>,
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
    let override_gpg = cfg.repo_git(&repo_root).override_gpg;
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
    run_selected_fold(mq, &repo_root, &cands, override_gpg)
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
/// main checkout, not the target branch itself). This discovery never snapshots.
/// Dirty worktrees are included for later admission when `snapshot_dirty`, else skipped.
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
    let (_, history) = CanonicalHistory::worktree_loc(repo_root)?;
    let porc = util::git_out(repo_root, &["worktree", "list", "--porcelain"])
        .context("git worktree list")?;
    let main = repo_root.to_string_lossy().to_string();
    let mut branches = Vec::new();
    let mut skipped_dirty = Vec::new();
    let mut worktrees = HashMap::new();
    let mut identities = HashMap::new();
    let mut pending_snapshots = HashSet::new();
    let mut wt_path = String::new();
    for line in porc.lines().chain(std::iter::once("")) {
        if let Some(p) = line.strip_prefix("worktree ") {
            wt_path = p.to_string();
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            let branch = b.to_string();
            if wt_path != main && branch != target_branch {
                let loc = GitLoc::Local(PathBuf::from(&wt_path));
                let dirty = CliGit
                    .is_dirty(&loc)
                    .context("candidate dirty-state lookup failed")?;
                if dirty && !cfg.snapshot_dirty {
                    skipped_dirty.push(branch.clone());
                    continue;
                }
                if dirty {
                    pending_snapshots.insert(branch.clone());
                }
                let tip = if cfg.snapshot_dirty {
                    let identity =
                        candidates::LocalIdentity::read(repo_root, Path::new(&wt_path), &branch)?;
                    let tip = identity.tip.clone();
                    identities.insert(branch.clone(), identity);
                    tip
                } else {
                    // Existing snapshot-disabled discovery stays a read-only
                    // local tip lookup; it never needs snapshot-only authority.
                    CliGit.rev_parse(&loc, "HEAD")?
                };
                worktrees.insert(branch.clone(), wt_path.clone());
                branches.push(Branch { name: branch, tip });
            }
        }
    }
    history.revalidate()?;
    Ok(Candidates {
        branches,
        skipped_dirty,
        worktrees,
        identities,
        pending_snapshots,
    })
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

/// Gate the exact commit in a verified, exclusively owned local checkout.
/// Admission/setup/identity failures are infrastructure holds, never branch blame.
pub(crate) fn gate_tip(repo_root: &Path, oid: &str, cfg: &MergeQueueConfig) -> Result<GateVerdict> {
    if cfg.gate_command.is_empty() {
        return Ok(GateVerdict::Passed);
    }
    Ok(
        gate_runner::run(repo_root, oid, cfg).unwrap_or_else(|error| GateVerdict::Error {
            reason: "gate preparation or identity unavailable".into(),
            log: tail(
                &format!("{error:#}")
                    .chars()
                    .filter(|c| diagnostic_char(*c))
                    .collect::<String>(),
                4000,
            ),
        }),
    )
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
    landed: &[Branch],
    cfg: &MergeQueueConfig,
    opts: &LandOpts,
    diagnostics: &mut String,
) -> Result<Option<String>> {
    // A red base says nothing about any candidate. Establish this before any
    // prefix can be blamed; retain its diagnostic alongside the union failure.
    let base_verdict = diagnosed_gate(repo_root, base, cfg, "base", diagnostics, &adapter.history);
    match base_verdict {
        GateVerdict::Passed => {}
        GateVerdict::Failed { .. } => return Ok(None),
        verdict @ GateVerdict::Error { .. } => return Err(BisectAborted(verdict).into()),
    }
    let mut prefix: Vec<Branch> = Vec::new();
    for branch in landed {
        // Re-fold the exact candidate object tested in the union. A branch ref
        // may have moved during the union/base gate; re-reading it would test
        // different code and attribute that verdict to the original snapshot.
        prefix.push(branch.clone());
        let plan = fold::fold(adapter, base, prefix.clone(), &cfg.regenerate_paths, opts)
            .map_err(|error| aborted_bisect(error, diagnostics))?;
        if plan.advanced() {
            let verdict = diagnosed_gate(
                repo_root,
                &plan.final_tip,
                cfg,
                &format!("prefix {}", prefix.len()),
                diagnostics,
                &adapter.history,
            );
            match verdict {
                GateVerdict::Passed => {}
                GateVerdict::Failed { .. } => return Ok(Some(branch.name.clone())),
                // Environmental: identical at every prefix, so stop rather than
                // blame this candidate for a missing binary.
                v @ GateVerdict::Error { .. } => return Err(BisectAborted(v).into()),
            }
        }
    }
    Ok(None)
}

fn aborted_bisect(error: anyhow::Error, diagnostics: &mut String) -> BisectAborted {
    let verdict = GateVerdict::Error {
        reason: "bisect prefix preparation unavailable".into(),
        log: tail(&format!("{error:#}"), 4000),
    };
    record_gate(diagnostics, "prefix preparation", &verdict);
    BisectAborted(verdict)
}

fn diagnosed_gate(
    repo_root: &Path,
    tip: &str,
    cfg: &MergeQueueConfig,
    phase: &str,
    diagnostics: &mut String,
    history: &CanonicalHistory,
) -> GateVerdict {
    let verdict = history
        .checked(|| gate_tip(repo_root, tip, cfg))
        .unwrap_or_else(|error| GateVerdict::Error {
            reason: "gate preparation unavailable".into(),
            log: tail(&format!("{error:#}"), 4000),
        });
    record_gate(diagnostics, phase, &verdict);
    verdict
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

/// Compose a [`FoldReport`] from a plan plus any gate offenders. `advanced` is
/// left false; callers set it after a successful CAS.
fn build_report(
    target_branch: &str,
    original: &str,
    plan: &FoldPlan,
    gate_offenders: &[String],
    gate: GateOutcome,
    cas_attempts: u32,
    diagnostics: &str,
) -> FoldReport {
    let mut deferred: Vec<DeferredReport> = plan
        .deferred
        .iter()
        .map(|d| DeferredReport {
            branch: d.branch.name.clone(),
            paths: d.paths.clone(),
            kind: d.kind,
            submodule_conflicts: d.submodule_conflicts.clone(),
            gate_failed: false,
        })
        .collect();
    for off in gate_offenders {
        deferred.push(DeferredReport {
            branch: off.clone(),
            paths: Vec::new(),
            kind: ConflictKind::Textual,
            submodule_conflicts: Vec::new(),
            gate_failed: true,
        });
    }
    let prepared: Vec<LandedReport> = plan
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
        landed: Vec::new(),
        prepared,
        unprepared: Vec::new(),
        deferred,
        gate,
        diagnostics: diagnostics.to_string(),
        bookkeeping_error: None,
        cas_attempts,
    }
}

/// The [`LandOpts`] for a fold: the configured land strategy + message template,
/// with `target` bound for `{target}` rendering. One place so every fold call
/// site lands identically.
fn land_opts<'a>(cfg: &'a MergeQueueConfig, target: &'a str) -> LandOpts<'a> {
    LandOpts {
        strategy: cfg.land_strategy,
        message_template: &cfg.land_message,
        target,
    }
}

/// A no-op plan (nothing landed/deferred) at `tip` — for reports produced when a
/// fold aborts before any branch is processed (e.g. a signing failure).
fn empty_plan(tip: &str) -> FoldPlan {
    FoldPlan {
        original: tip.to_string(),
        final_tip: tip.to_string(),
        landed: Vec::new(),
        deferred: Vec::new(),
    }
}

fn history_failure_report(
    target: &str,
    original: &str,
    candidates: &[Branch],
    attempts: u32,
    error: anyhow::Error,
    earlier_diagnostics: &str,
) -> FoldReport {
    let mut diagnostics = earlier_diagnostics.to_owned();
    let reason = "canonical Git history unavailable".to_string();
    record_gate(
        &mut diagnostics,
        "history admission",
        &GateVerdict::Error {
            reason: reason.clone(),
            log: tail(&format!("{error:#}"), 4000),
        },
    );
    let mut report = build_report(
        target,
        original,
        &empty_plan(original),
        &[],
        GateOutcome::Errored { reason },
        attempts,
        &diagnostics,
    );
    report.unprepared = candidates
        .iter()
        .map(|branch| branch.name.clone())
        .collect();
    report
}

/// Fold `candidates` onto the repo's target branch: merge clean branches in the
/// object DB, gate the union, and CAS-advance the target ref. Clean branches
/// land; conflicts and gate-offenders are deferred. No working tree is touched
/// except the throwaway gate worktree and — after a successful advance — a
/// guarded fast-forward of the repo's own main checkout (see
/// [`util::resync_ff_checkout`]) so `git status` there stays coherent.
#[cfg(test)]
pub fn run_fold(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    candidates: Vec<Branch>,
) -> Result<FoldReport> {
    run_fold_with_cas(cfg, repo_root, candidates, |loc, target, tip, base| {
        CliGit.update_ref_cas(loc, target, tip, base)
    })
}

/// The mutation seam is injectable only through this private function, so
/// exhaustion can be tested without racing or modifying a real target branch.
#[cfg(test)]
fn run_fold_with_cas(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    candidates: Vec<Branch>,
    advance: impl FnMut(&GitLoc, &str, &str, &str) -> Result<bool>,
) -> Result<FoldReport> {
    let (loc, history) = CanonicalHistory::worktree_loc(repo_root)?;
    run_fold_admitted(cfg, repo_root, candidates, loc, history, advance)
}

fn run_fold_observed(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    candidates: Vec<Branch>,
    history: CanonicalHistory,
) -> Result<FoldReport> {
    anyhow::ensure!(
        history.worktree_path() == repo_root,
        "observed fold repository identity mismatch"
    );
    run_fold_admitted(
        cfg,
        repo_root,
        candidates,
        GitLoc::Local(repo_root.to_path_buf()),
        history,
        |loc, target, tip, base| CliGit.update_ref_cas(loc, target, tip, base),
    )
}

fn run_fold_admitted(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    candidates: Vec<Branch>,
    loc: GitLoc,
    history: CanonicalHistory,
    mut advance: impl FnMut(&GitLoc, &str, &str, &str) -> Result<bool>,
) -> Result<FoldReport> {
    history.revalidate()?;
    let adapter = PlumbingAdapter {
        history,
        loc: loc.clone(),
        repo_root: repo_root.to_path_buf(),
        regenerate_paths: cfg.regenerate_paths.clone(),
        regenerate_command: cfg.regenerate_command.clone(),
        sign: cfg.sign_commits,
        rerere: cfg.rerere,
    };
    let target_branch = resolve_target(cfg, repo_root);
    let target_ref = format!("refs/heads/{target_branch}");
    let original = CliGit.rev_parse(&loc, &target_ref)?;
    let opts = land_opts(cfg, &target_branch);

    let gate_on = cfg.gate_on && !cfg.gate_command.is_empty();
    let mut excluded: HashSet<String> = HashSet::new();
    let mut gate_offenders: Vec<String> = Vec::new();
    let mut cas_attempts = 0u32;
    let mut diagnostics = String::new();

    loop {
        if let Err(error) = adapter.history.revalidate() {
            return Ok(history_failure_report(
                &target_branch,
                &original,
                &candidates,
                cas_attempts,
                error,
                &diagnostics,
            ));
        }
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
        let attempted: Vec<String> = to_fold.iter().map(|branch| branch.name.clone()).collect();
        let plan = match fold::fold(&adapter, &base, to_fold, &cfg.regenerate_paths, &opts) {
            Ok(p) => p,
            // Any preparation failure (including signing) holds the requested
            // candidates without inventing prepared commits or branch blame.
            Err(e) => {
                let reason = "fold preparation unavailable".to_string();
                record_gate(
                    &mut diagnostics,
                    "fold preparation",
                    &GateVerdict::Error {
                        reason: reason.clone(),
                        log: tail(&format!("{e:#}"), 4000),
                    },
                );
                let empty = empty_plan(&original);
                let mut report = build_report(
                    &target_branch,
                    &original,
                    &empty,
                    &gate_offenders,
                    GateOutcome::Errored { reason },
                    cas_attempts,
                    &diagnostics,
                );
                report.unprepared = attempted;
                return Ok(report);
            }
        };

        if let Err(error) = adapter.history.revalidate() {
            return Ok(history_failure_report(
                &target_branch,
                &original,
                &candidates,
                cas_attempts,
                error,
                &diagnostics,
            ));
        }

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
                &diagnostics,
            ));
        }

        // Test-gate the union before blessing it.
        let gate = if gate_on {
            let verdict = diagnosed_gate(
                repo_root,
                &plan.final_tip,
                cfg,
                "union",
                &mut diagnostics,
                &adapter.history,
            );
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
                    &diagnostics,
                ));
            }
            if verdict.passed() {
                GateOutcome::Passed
            } else if cfg.bisect_on_red {
                let landed: Vec<Branch> = plan
                    .landed
                    .iter()
                    .map(|landed| landed.branch.clone())
                    .collect();
                let bisected = match bisect_offender(
                    repo_root,
                    &adapter,
                    &base,
                    &landed,
                    cfg,
                    &opts,
                    &mut diagnostics,
                ) {
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
                            &diagnostics,
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
                    &diagnostics,
                ));
            } else {
                return Ok(build_report(
                    &target_branch,
                    &original,
                    &plan,
                    &gate_offenders,
                    GateOutcome::Failed { offender: None },
                    cas_attempts,
                    &diagnostics,
                ));
            }
        } else {
            GateOutcome::Skipped
        };

        // Green (or no gate) → atomically advance the target ref.
        if let Err(error) = adapter.history.revalidate() {
            return Ok(history_failure_report(
                &target_branch,
                &original,
                &candidates,
                cas_attempts,
                error,
                &diagnostics,
            ));
        }
        cas_attempts += 1;
        let advanced = match advance(&loc, &target_ref, &plan.final_tip, &base) {
            Ok(advanced) => advanced,
            Err(error) => {
                let reason = "target CAS operation failed".to_string();
                record_gate(
                    &mut diagnostics,
                    "target CAS",
                    &GateVerdict::Error {
                        reason: reason.clone(),
                        log: tail(&format!("{error:#}"), 4000),
                    },
                );
                return Ok(build_report(
                    &target_branch,
                    &original,
                    &plan,
                    &gate_offenders,
                    GateOutcome::Errored { reason },
                    cas_attempts,
                    &diagnostics,
                ));
            }
        };
        if advanced {
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
                &diagnostics,
            );
            report.advanced = true;
            report.landed = std::mem::take(&mut report.prepared);
            report.resyncs = resyncs;
            return Ok(report);
        }
        if cas_attempts >= 5 {
            return Ok(build_report(
                &target_branch,
                &original,
                &plan,
                &gate_offenders,
                GateOutcome::Errored {
                    reason: format!(
                        "{target_branch} kept moving under the fold; CAS retry budget exhausted"
                    ),
                },
                cas_attempts,
                &diagnostics,
            ));
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
    Conflict {
        paths: Vec<String>,
        submodule_conflicts: Vec<thegn_core::submodule::SubmoduleConflict>,
    },
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
/// the queue driver drains one at a time. Mirrors [`run_fold_admitted`]'s fold→gate→CAS
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
    attempt_land_admitted(cfg, repo_root, branch_name, branch_loc).or_else(|error| {
        Ok(AttemptOutcome::GateError {
            reason: "merge preparation or canonical history unavailable".into(),
            log: tail(&format!("{error:#}"), 4000),
        })
    })
}

fn attempt_land_admitted(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    branch_name: &str,
    branch_loc: &GitLoc,
) -> Result<AttemptOutcome> {
    let (loc, history) = CanonicalHistory::worktree_loc(repo_root)?;
    // A target-store observation is not proof about a remote/provider source.
    let source_history = history.local_child(branch_loc)?;
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
        history,
        loc: loc.clone(),
        repo_root: repo_root.to_path_buf(),
        regenerate_paths: cfg.regenerate_paths.clone(),
        regenerate_command: cfg.regenerate_command.clone(),
        sign: cfg.sign_commits,
        rerere: cfg.rerere,
    };
    let opts = land_opts(cfg, &target_branch);
    let gate_on = cfg.gate_on && !cfg.gate_command.is_empty();
    let mut cas_attempts = 0u32;
    loop {
        adapter.history.revalidate()?;
        source_history.revalidate()?;
        let base = CliGit.rev_parse(&loc, &target_ref)?;
        let branch_tip = CliGit.rev_parse(&loc, &branch_ref)?;
        if util::git_ok(
            repo_root,
            &["merge-base", "--is-ancestor", &branch_tip, &base],
        ) {
            adapter.history.revalidate()?;
            source_history.revalidate()?;
            return Ok(AttemptOutcome::UpToDate);
        }
        let branch = Branch {
            name: branch_name.to_string(),
            tip: branch_tip,
        };
        adapter.history.revalidate()?;
        source_history.revalidate()?;
        let plan = match fold::fold(&adapter, &base, vec![branch], &cfg.regenerate_paths, &opts) {
            Ok(p) => p,
            // Signing failure ⇒ infrastructure error: stop with a reason, keep
            // the branch's status, never dispatch the fixing agent (GateError's
            // routing does exactly this).
            Err(e) => {
                let sf = e.downcast::<SigningFailed>()?;
                return Ok(AttemptOutcome::GateError {
                    reason: sf.to_string(),
                    log: String::new(),
                });
            }
        };
        adapter.history.revalidate()?;
        source_history.revalidate()?;
        if !plan.advanced() {
            // One branch that didn't advance the tip ⇒ it was deferred (conflict).
            let deferred = plan.deferred.first();
            let paths = deferred.map(|d| d.paths.clone()).unwrap_or_default();
            let submodule_conflicts = deferred
                .map(|d| d.submodule_conflicts.clone())
                .unwrap_or_default();
            return Ok(AttemptOutcome::Conflict {
                paths,
                submodule_conflicts,
            });
        }
        let folded_tip = plan.final_tip.clone();
        if gate_on {
            let verdict = adapter
                .history
                .checked(|| gate_tip(repo_root, &folded_tip, cfg))?;
            source_history.revalidate()?;
            match verdict {
                GateVerdict::Passed => {}
                GateVerdict::Failed { log } => {
                    return Ok(AttemptOutcome::GateFailed { log });
                }
                GateVerdict::Error { reason, log } => {
                    return Ok(AttemptOutcome::GateError { reason, log });
                }
            }
        }
        adapter.history.revalidate()?;
        source_history.revalidate()?;
        if !cfg.auto_land {
            return Ok(AttemptOutcome::Ready { tip: folded_tip });
        }
        cas_attempts += 1;
        adapter.history.revalidate()?;
        source_history.revalidate()?;
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
            identities: HashMap::new(),
            pending_snapshots: HashSet::new(),
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
        _worktree: tempfile::TempDir,
        _env: thegn_core::testenv::EnvGuard,
        _state: tempfile::TempDir,
    }
    impl Repo {
        fn new(tag: &str) -> Self {
            // GitLoc::for_worktree opens the process state DB even for local
            // fixtures. Keep every test's implicit reads/migrations off live
            // state, and do not execute the operator's Git hooks/configuration.
            let state = tempfile::Builder::new()
                .prefix("thegn-integ-state-")
                .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
                .unwrap();
            let state_path = state.path().to_str().unwrap();
            let empty_git_config = state.path().join("absent.gitconfig");
            let env = thegn_core::testenv::EnvGuard::set(&[
                ("XDG_STATE_HOME", state_path),
                ("XDG_CONFIG_HOME", state_path),
                ("LOCALAPPDATA", state_path),
                ("THEGN_DIR", state_path),
                ("THEGN_PROFILE", ""),
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_CONFIG_GLOBAL", empty_git_config.to_str().unwrap()),
            ]);
            let worktree = tempfile::Builder::new()
                .prefix(&format!("tg-integ-{tag}-"))
                .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
                .unwrap();
            let dir = worktree.path().to_path_buf();
            git(&dir, &["init", "-q", "-b", "main"]);
            git(&dir, &["config", "user.name", "t"]);
            git(&dir, &["config", "user.email", "t@e"]);
            git(&dir, &["config", "commit.gpgsign", "false"]);
            let r = Repo {
                dir,
                _worktree: worktree,
                _env: env,
                _state: state,
            };
            r.commit("base.txt", "base\n", "c0");
            Db::open().unwrap(); // owned private registry; no lossy location fallback
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
        /// Create `branch` off main with one commit per `(file, body, msg)`.
        fn feature_multi(&self, branch: &str, commits: &[(&str, &str, &str)]) {
            git(&self.dir, &["checkout", "-q", "-b", branch]);
            for (file, body, msg) in commits {
                self.commit(file, body, msg);
            }
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

        /// Keep real nonempty-gate tests truthful on platforms where verified
        /// local admission is unavailable. This is an exercised refusal, not an
        /// ignored test or a successful substitute gate.
        fn gate_supported_or_refused(&self, config: &MergeQueueConfig) -> bool {
            if thegn_core::sandbox_backend::host_os()
                != thegn_core::sandbox_backend::HostOs::Windows
            {
                return true;
            }
            assert!(!config.gate_command.is_empty());
            let refs = self.out(&["for-each-ref", "--format=%(refname) %(objectname)"]);
            let index = std::fs::read(self.dir.join(".git/index")).unwrap();
            let tracked: Vec<_> = self
                .out(&["ls-files"])
                .lines()
                .map(|name| {
                    let path = self.dir.join(name);
                    let bytes = std::fs::read(&path).unwrap();
                    (path, bytes)
                })
                .collect();
            let sentinel = self._state.path().join("gate-refusal-sentinel");
            std::fs::write(&sentinel, "must remain").unwrap();
            match gate_tip(&self.dir, &self.out(&["rev-parse", "HEAD"]), config).unwrap() {
                GateVerdict::Error { log, .. } => assert!(log.contains("unsupported")),
                other => panic!("unsupported gate must refuse: {other:?}"),
            }
            assert!(run_fold(config, &self.dir, self.branch_set()).is_err());
            let disabled = cfg("");
            assert!(run_fold(&disabled, &self.dir, self.branch_set()).is_err());
            assert!(matches!(
                attempt_land(
                    &disabled,
                    &self.dir,
                    "main",
                    &GitLoc::Local(self.dir.clone())
                )
                .unwrap(),
                AttemptOutcome::GateError { .. }
            ));
            assert_eq!(
                self.out(&["for-each-ref", "--format=%(refname) %(objectname)"]),
                refs
            );
            assert_eq!(std::fs::read(self.dir.join(".git/index")).unwrap(), index);
            for (path, bytes) in tracked {
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "must remain");
            false
        }

        fn history_supported_or_refused(&self) -> bool {
            self.gate_supported_or_refused(&cfg("true"))
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
        if !repo.history_supported_or_refused() {
            return;
        }
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
    fn fold_report_keeps_typed_submodule_conflict_and_formats_both_shas() {
        let conflict = thegn_core::submodule::SubmoduleConflict {
            path: "vendor/lib".into(),
            ours_sha: "abc1234".into(),
            theirs_sha: "def5678".into(),
        };
        let plan = FoldPlan {
            original: "base".into(),
            final_tip: "base".into(),
            landed: Vec::new(),
            deferred: vec![thegn_core::fold::Deferred {
                branch: Branch {
                    name: "feature".into(),
                    tip: "tip".into(),
                },
                paths: vec![conflict.path.clone()],
                kind: ConflictKind::Textual,
                submodule_conflicts: vec![conflict.clone()],
            }],
        };
        let report = build_report("main", "base", &plan, &[], GateOutcome::Skipped, 0, "");
        assert_eq!(report.deferred[0].paths, ["vendor/lib"]);
        assert_eq!(
            report.deferred[0].submodule_conflicts,
            std::slice::from_ref(&conflict)
        );
        assert_eq!(
            conflict_details(
                &["src/lib.rs".into(), "vendor/lib".into()],
                &report.deferred[0].submodule_conflicts,
            ),
            [
                "src/lib.rs",
                "submodule pointer conflict: vendor/lib (abc1234 vs def5678)",
            ]
        );
        assert_eq!(
            thegn_core::submodule::format_submodule_conflicts(
                &report.deferred[0].submodule_conflicts
            ),
            "submodule pointer conflict: vendor/lib (abc1234 vs def5678)"
        );
    }

    #[test]
    fn squash_strategy_lands_one_single_parent_commit() {
        let repo = Repo::new("squash");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature_multi(
            "feat",
            &[
                ("a.txt", "a\n", "add a"),
                ("b.txt", "b\n", "add b"),
                ("c.txt", "c\n", "add c"),
            ],
        );
        let before = repo.out(&["rev-parse", "main"]);
        let mut c = cfg("");
        c.land_strategy = thegn_core::config::LandStrategy::Squash;

        let report = run_fold(&c, &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced);
        assert_eq!(report.landed.len(), 1);
        let head = repo.out(&["rev-parse", "main"]);
        // Exactly one commit ahead of the previous tip, and its SOLE parent is
        // that tip (single-parent squash, no 2-parent merge).
        assert_eq!(
            repo.out(&["rev-list", "--count", &format!("{before}..{head}")]),
            "1"
        );
        assert_eq!(
            repo.out(&["rev-list", "--parents", "-n", "1", "main"])
                .split_whitespace()
                .count(),
            2
        );
        // The squashed tree carries all three files.
        let files = repo.out(&["ls-tree", "-r", "--name-only", "main"]);
        assert!(
            files.contains("a.txt") && files.contains("b.txt") && files.contains("c.txt"),
            "{files}"
        );
        // Default squash message lists the folded subjects.
        let msg = repo.out(&["log", "-1", "--format=%B", "main"]);
        assert!(
            msg.contains("Squash branch 'feat'") && msg.contains("- add a"),
            "{msg}"
        );
    }

    #[test]
    fn rebase_strategy_replays_commits_linearly_preserving_author() {
        let repo = Repo::new("rebase");
        if !repo.history_supported_or_refused() {
            return;
        }
        // A commit by a different author, to prove authorship is preserved.
        git(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(repo.dir.join("a.txt"), "a\n").unwrap();
        git(&repo.dir, &["add", "a.txt"]);
        git(
            &repo.dir,
            &[
                "-c",
                "user.name=Ada",
                "-c",
                "user.email=ada@x",
                "commit",
                "-q",
                "-m",
                "add a",
            ],
        );
        repo.commit("b.txt", "b\n", "add b");
        git(&repo.dir, &["checkout", "-q", "main"]);
        // Advance main so the replay is non-trivial (rebase onto a moved tip).
        repo.commit("main.txt", "m\n", "main moves");

        let before = repo.out(&["rev-parse", "main"]);
        let mut c = cfg("");
        c.land_strategy = thegn_core::config::LandStrategy::Rebase;
        let report = run_fold(&c, &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced, "{report:?}");

        let head = repo.out(&["rev-parse", "main"]);
        // Two replayed commits, linear (every commit single-parent — no merge).
        assert_eq!(
            repo.out(&["rev-list", "--count", &format!("{before}..{head}")]),
            "2"
        );
        let merges = repo.out(&["rev-list", "--merges", &format!("{before}..{head}")]);
        assert!(
            merges.is_empty(),
            "rebase must not create merge commits: {merges}"
        );
        // The first replayed commit keeps Ada's authorship.
        let authors = repo.out(&["log", &format!("{before}..{head}"), "--format=%an"]);
        assert!(authors.contains("Ada"), "author not preserved: {authors}");
        let files = repo.out(&["ls-tree", "-r", "--name-only", "main"]);
        assert!(files.contains("a.txt") && files.contains("b.txt") && files.contains("main.txt"));
    }

    #[test]
    fn signing_failure_is_infrastructure_not_a_branch_verdict() {
        let repo = Repo::new("signfail");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);
        // Force signing on, with a signer that fails fast (stands in for a locked
        // agent / a pinentry that would hang a headless fold).
        git(&repo.dir, &["config", "gpg.program", "false"]);
        let mut c = cfg("");
        c.sign_commits = true;

        let out = attempt_land(&c, &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap();
        match out {
            // Routed as an infrastructure error: the branch is never blamed and
            // the fixing agent is never dispatched (GateError's semantics).
            AttemptOutcome::GateError { reason, .. } => {
                assert!(reason.contains("signing"), "{reason}");
            }
            other => panic!("expected GateError for a signing failure, got {other:?}"),
        }
        // main did not move — nothing landed.
        assert_eq!(repo.out(&["rev-parse", "main"]), before);
    }

    /// Real signing proof, not a fake signer: generate an isolated throwaway
    /// OpenPGP key with loopback pinentry, fold one branch, and have gpg verify
    /// the resulting commit. A capable host must run this; only a genuinely
    /// absent `gpg` binary skips it.
    #[test]
    #[expect(clippy::disallowed_methods)]
    fn signed_fold_has_gpgsig_and_verifies_with_loopback_pinentry() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;

        let availability = std::process::Command::new("gpg")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match availability {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("gpg exists but could not be probed: {error}"),
            Ok(status) if !status.success() => panic!("gpg --version failed: {status}"),
            Ok(_) => {}
        }

        let repo = Repo::new("signed-fold");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "signed.txt", "signed\n");
        let gpg_home = repo.dir.join("gnupg-fixture");
        std::fs::create_dir(&gpg_home).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&gpg_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let identity = "Thegn Fixture <thegn-fixture@example.invalid>";
        let generated = std::process::Command::new("gpg")
            .args([
                "--homedir",
                gpg_home.to_str().unwrap(),
                "--batch",
                "--pinentry-mode",
                "loopback",
                "--passphrase",
                "",
                "--quick-generate-key",
                identity,
                "ed25519",
                "sign",
                "0",
            ])
            .output()
            .expect("gpg was already proved present");
        assert!(
            generated.status.success(),
            "throwaway key generation failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let listed = std::process::Command::new("gpg")
            .args([
                "--homedir",
                gpg_home.to_str().unwrap(),
                "--batch",
                "--with-colons",
                "--list-secret-keys",
                identity,
            ])
            .output()
            .unwrap();
        assert!(listed.status.success());
        let listing = String::from_utf8_lossy(&listed.stdout);
        let fingerprint = listing
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split(':').collect();
                (fields.first() == Some(&"fpr"))
                    .then(|| fields.get(9).copied())
                    .flatten()
            })
            .expect("generated key has a fingerprint");
        #[cfg(unix)]
        let (wrapper, wrapper_script) = (
            repo.dir.join("gpg-loopback.sh"),
            format!(
                "#!/bin/sh\nexec gpg --homedir {} --batch --pinentry-mode loopback --passphrase '' \"$@\"\n",
                thegn_core::util::sh_quote(&gpg_home.to_string_lossy())
            ),
        );
        #[cfg(windows)]
        let (wrapper, wrapper_script) = (
            repo.dir.join("gpg-loopback.cmd"),
            format!(
                "@echo off\r\ngpg --homedir \"{}\" --batch --pinentry-mode loopback --passphrase \"\" %*\r\n",
                gpg_home.display()
            ),
        );
        std::fs::write(&wrapper, wrapper_script).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            &repo.dir,
            &["config", "gpg.program", wrapper.to_str().unwrap()],
        );
        git(&repo.dir, &["config", "user.signingkey", fingerprint]);

        let before = repo.out(&["rev-parse", "main"]);
        let mut config = cfg("");
        config.sign_commits = true;
        let outcome =
            attempt_land(&config, &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap();
        assert!(
            matches!(outcome, AttemptOutcome::Landed { .. }),
            "{outcome:?}"
        );
        assert_ne!(repo.out(&["rev-parse", "main"]), before);
        let raw = repo.out(&["cat-file", "-p", "main"]);
        assert!(raw.contains("gpgsig"), "signed commit lacks gpgsig: {raw}");

        let verified = util::git_cmd(&repo.dir)
            .args([
                "-c",
                &format!("gpg.program={}", wrapper.display()),
                "verify-commit",
                "main",
            ])
            .output()
            .unwrap();
        assert!(
            verified.status.success(),
            "git verify-commit failed: {}{}",
            String::from_utf8_lossy(&verified.stdout),
            String::from_utf8_lossy(&verified.stderr)
        );
    }

    #[test]
    fn custom_merge_driver_resolves_a_conflict_through_a_worktree_merge() {
        let repo = Repo::new("driver");
        if !repo.history_supported_or_refused() {
            return;
        }
        // A custom driver that always resolves to "ours" (exits 0 leaving %A).
        git(&repo.dir, &["config", "merge.takeours.driver", "true"]);
        repo.commit(".gitattributes", "data.txt merge=takeours\n", "attrs");
        repo.commit("data.txt", "base\n", "seed");
        // Two textually-conflicting edits to the driver-governed file.
        git(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit("data.txt", "feat\n", "feat edit");
        git(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit("data.txt", "main\n", "main edit");

        // merge-tree conflicts (it does not run a custom *command* driver), so the
        // fold routes the branch through a throwaway-worktree real merge where the
        // driver runs → the branch LANDS instead of deferring.
        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced, "{report:?}");
        assert_eq!(report.landed.len(), 1, "driver-resolved branch should land");
        assert!(report.deferred.is_empty(), "{report:?}");
        // The `true` driver kept "ours" (main's content).
        assert_eq!(repo.out(&["show", "main:data.txt"]), "main");
    }

    #[test]
    fn conflicting_branch_is_deferred_clean_one_still_lands() {
        let repo = Repo::new("conflict");
        if !repo.history_supported_or_refused() {
            return;
        }
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
        assert!(
            report.request_result().is_err(),
            "partial success reports held branches"
        );
    }

    #[test]
    fn green_gate_advances_red_gate_holds_back() {
        let repo = Repo::new("gate");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);

        if !repo.gate_supported_or_refused(&cfg("true")) {
            assert!(!repo.gate_supported_or_refused(&cfg("false")));
            return;
        }

        // Green gate → advances.
        let report = run_fold(&cfg("true"), &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced);
        assert_eq!(report.gate, GateOutcome::Passed);
        assert_ne!(repo.out(&["rev-parse", "main"]), before);

        // A universally red gate also fails at base: no branch can be blamed.
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
        assert!(report.landed.is_empty());
        assert_eq!(report.prepared[0].branch, "b2");
        assert!(report.deferred.is_empty());
        assert!(report.diagnostics.contains("[base] failed"));
    }

    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::disallowed_methods,
        reason = "owned actual Git integration fixture"
    )]
    fn configured_cleanup_runs_only_after_real_target_advance_and_committed_outcome() {
        // One environment lock for the entire fold/persist/cleanup chain. Repo::new
        // also owns that non-reentrant lock, so it must not be nested here.
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let private = tempfile::tempdir().unwrap();
        let base = private.path().canonicalize().unwrap();
        let root = base.join("selected-repository");
        let foreign = base.join("foreign-repository");
        let checkout = base.join("selected-checkout");
        let foreign_checkout = base.join("foreign-checkout");
        let git = |cwd: &Path, args: &[&str]| {
            let output = isolation.git(cwd).args(args).output().unwrap();
            assert!(
                output.status.success(),
                "private git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout)
                .unwrap()
                .trim_end_matches('\n')
                .to_owned()
        };
        for (repository, worktree, branch) in [
            (&root, &checkout, "candidate"),
            (&foreign, &foreign_checkout, "foreign-candidate"),
        ] {
            std::fs::create_dir(repository).unwrap();
            git(repository, &["init", "-q", "-b", "main"]);
            git(repository, &["config", "user.name", "private-fixture"]);
            git(
                repository,
                &["config", "user.email", "private@example.invalid"],
            );
            git(repository, &["config", "commit.gpgsign", "false"]);
            std::fs::write(repository.join("base.txt"), "base\n").unwrap();
            git(repository, &["add", "base.txt"]);
            git(repository, &["commit", "-q", "-m", "base"]);
            git(
                repository,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    worktree.to_str().unwrap(),
                ],
            );
            std::fs::write(worktree.join("owned.txt"), format!("{branch} payload\n")).unwrap();
            git(worktree, &["add", "owned.txt"]);
            git(worktree, &["commit", "-q", "-m", "distinct candidate"]);
        }
        // Implicit GitLoc registry reads and explicit persistence use this same
        // fresh private state database. No final outcome is seeded for candidate.
        let db = Db::open().unwrap();
        let wt = checkout.to_str().unwrap();
        let foreign_wt = foreign_checkout.to_str().unwrap();
        db.enqueue_merge(wt, "candidate", "main").unwrap();
        db.set_merge_agent_attempts(wt, 2).unwrap();
        db.enqueue_merge(foreign_wt, "foreign-candidate", "main")
            .unwrap();
        let foreign_tip = git(&foreign, &["rev-parse", "foreign-candidate"]);
        db.update_merge_status(
            foreign_wt,
            "landed",
            Some(&foreign_tip),
            None,
            Some("foreign evidence"),
        )
        .unwrap();
        let queued = db.list_merge_queue().unwrap();
        let selected_before = queued
            .iter()
            .find(|row| row.worktree == wt)
            .unwrap()
            .clone();
        let foreign_before = queued
            .iter()
            .find(|row| row.worktree == foreign_wt)
            .unwrap()
            .clone();
        let foreign_refs = git(
            &foreign,
            &["for-each-ref", "--format=%(refname) %(objectname)"],
        );
        let foreign_registrations = git(&foreign, &["worktree", "list", "--porcelain"]);
        let foreign_bytes = std::fs::read(foreign_checkout.join("owned.txt")).unwrap();
        let target_before = git(&root, &["rev-parse", "main"]);
        let source_tip = git(&root, &["rev-parse", "candidate"]);
        assert_ne!(
            target_before, source_tip,
            "must land a distinct unmerged commit"
        );
        let candidates = Candidates {
            branches: vec![Branch {
                name: "candidate".into(),
                tip: source_tip.clone(),
            }],
            skipped_dirty: Vec::new(),
            identities: HashMap::new(),
            pending_snapshots: HashSet::new(),
            worktrees: HashMap::from([("candidate".into(), wt.into())]),
        };
        let observations = observe_outcomes(&db, &candidates).unwrap();
        let mut config = cfg("printf 'private-positive-cleanup-gate\\n'");
        config.land_strategy = thegn_core::config::LandStrategy::Merge;
        config.organize_folders = true;
        config.on_landed = thegn_core::config::OnLanded::Remove;
        let report = run_fold(&config, &root, candidates.branches.clone()).unwrap();
        assert!(report.advanced && report.gate == GateOutcome::Passed);
        assert_eq!(report.landed.len(), 1);
        assert_eq!(report.landed[0].branch, "candidate");
        let landed = &report.landed[0].commit;
        assert_ne!(landed, &target_before);
        assert_eq!(git(&root, &["rev-parse", "main"]), *landed);
        git(&root, &["merge-base", "--is-ancestor", &source_tip, "main"]);
        assert!(
            checkout.join("owned.txt").is_file(),
            "fold alone must not perform lifecycle cleanup"
        );
        assert_eq!(
            db.list_merge_queue()
                .unwrap()
                .iter()
                .find(|row| row.worktree == wt)
                .unwrap(),
            &selected_before
        );
        assert!(report.request_result().is_ok());

        // The actual production persistence function commits Landed before
        // invoking apply_landed with the configured automatic Remove policy.
        persist(&config, &root, &db, &candidates, &report, &observations).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(&checkout).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let registrations = git(&root, &["worktree", "list", "--porcelain"]);
        assert!(
            !registrations
                .lines()
                .any(|line| line == format!("worktree {wt}"))
        );
        assert_eq!(git(&root, &["rev-parse", "main"]), *landed);
        assert_eq!(
            git(&root, &["rev-parse", "candidate"]),
            source_tip,
            "THE596 keeps the branch ref"
        );
        let rows = db.list_merge_queue().unwrap();
        let held = rows.iter().find(|row| row.worktree == wt).unwrap();
        assert_eq!(held.status, "landed");
        assert_eq!(held.result_oid.as_deref(), Some(landed.as_str()));
        assert_eq!(
            held.error_detail.as_deref(),
            Some(thegn_core::merge_sweep::CleanupHold::BranchRetained.marker())
        );
        assert!(held.conflict_paths.is_none());
        assert_eq!(held.agent_attempts, selected_before.agent_attempts);
        assert_eq!(held.queued_at, selected_before.queued_at);
        assert_eq!(
            rows.iter().find(|row| row.worktree == foreign_wt).unwrap(),
            &foreign_before
        );
        assert_eq!(
            git(
                &foreign,
                &["for-each-ref", "--format=%(refname) %(objectname)"]
            ),
            foreign_refs
        );
        assert_eq!(
            git(&foreign, &["worktree", "list", "--porcelain"]),
            foreign_registrations
        );
        assert_eq!(
            std::fs::read(foreign_checkout.join("owned.txt")).unwrap(),
            foreign_bytes
        );
        drop(db);
        private.close().expect("private real-land fixture cleanup");
    }

    /// Every failed case is exercised with a real linked worktree and a private
    /// DB. Destructive on-land policy is intentionally armed: speculative
    /// persistence must never reach it. Implicit state access is isolated by Repo.
    fn assert_held_persistence(repo: &Repo, mut config: MergeQueueConfig, report: &FoldReport) {
        let private = tempfile::tempdir().unwrap();
        let worktree = private.path().join("candidate");
        git(
            &repo.dir,
            &["worktree", "add", worktree.to_str().unwrap(), "b1"],
        );
        let original_tip = repo.out(&["rev-parse", "b1"]);
        let wt = worktree.to_string_lossy().to_string();
        let db = Db::open_at(&private.path().join("test.db")).unwrap();
        db.enqueue_merge(&wt, "b1", "main").unwrap();
        // Prove stale optimistic result_oid is cleared, not retained by the
        // nullable update API's COALESCE behavior.
        db.update_merge_status(&wt, "queued", Some("stale-result"), None, None)
            .unwrap();
        config.organize_folders = true;
        config.on_landed = thegn_core::config::OnLanded::Remove;
        let branches = repo.branch_set();
        let mut worktrees = HashMap::from([("b1".into(), wt.clone())]);
        for (index, branch) in branches
            .iter()
            .filter(|branch| branch.name != "b1")
            .enumerate()
        {
            let other = private.path().join(format!("candidate-other-{index}"));
            git(
                &repo.dir,
                &["worktree", "add", other.to_str().unwrap(), &branch.name],
            );
            worktrees.insert(branch.name.clone(), other.to_string_lossy().into_owned());
        }
        let candidates = Candidates {
            branches,
            skipped_dirty: Vec::new(),
            identities: HashMap::new(),
            pending_snapshots: HashSet::new(),
            worktrees,
        };
        let observations = observe_outcomes(&db, &candidates).unwrap();
        persist(&config, &repo.dir, &db, &candidates, report, &observations).unwrap();
        let rows = db.list_merge_queue().unwrap();
        let row = rows.iter().find(|row| row.worktree == wt).unwrap();
        assert!(
            rows.iter().all(|row| row.branch != "already-in-target"),
            "bystanders are not published"
        );
        assert_eq!(row.status, "gate_error");
        assert!(row.result_oid.is_none(), "never persist speculative commit");
        assert!(row.error_detail.as_deref().unwrap().contains("not landed"));
        assert!(worktree.join("a.txt").exists());
        assert_eq!(repo.out(&["rev-parse", "b1"]), original_tip);
        assert!(report.request_result().is_err());
    }

    #[test]
    fn failed_union_cannot_persist_a_land_or_run_landed_lifecycle() {
        let repo = Repo::new("failed-persist");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let original = repo.out(&["rev-parse", "main"]);
        let mut config = cfg("printf 'union-failure-proof\\n'; exit 1");
        config.bisect_on_red = false;
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(!report.advanced);
        assert!(report.landed.is_empty());
        assert_eq!(report.prepared.len(), 1);
        assert!(
            report
                .diagnostics
                .contains("[union] failed\nunion-failure-proof")
        );
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn infrastructure_gate_holds_without_branch_blame_and_keeps_output() {
        let repo = Repo::new("infra-persist");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let original = repo.out(&["rev-parse", "main"]);
        let config = cfg("printf 'infrastructure-proof\\n'; exit 127");
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(matches!(report.gate, GateOutcome::Errored { .. }));
        assert!(report.deferred.is_empty());
        assert!(report.landed.is_empty());
        assert!(report.diagnostics.contains("infrastructure-proof"));
        assert!(!report.diagnostics.contains("[base]"));
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn red_base_is_not_a_candidate_failure_and_keeps_both_gate_phases() {
        let repo = Repo::new("base-persist");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let original = repo.out(&["rev-parse", "main"]);
        let config =
            cfg("if test -f a.txt; then printf union-red; else printf base-red; fi; exit 1");
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(report.deferred.is_empty());
        assert!(report.diagnostics.contains("[union] failed\nunion-red"));
        assert!(report.diagnostics.contains("[base] failed\nbase-red"));
        assert!(!report.diagnostics.contains("[prefix"));
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn prefix_infrastructure_error_keeps_union_base_and_prefix_diagnostics() {
        let repo = Repo::new("prefix-infra");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        repo.feature("b2", "b.txt", "candidate\n");
        let original = repo.out(&["rev-parse", "main"]);
        let config = cfg(
            "if test -f b.txt; then printf union-red; exit 1; fi; if test -f a.txt; then printf prefix-infra; exit 127; fi; exit 0",
        );
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(matches!(report.gate, GateOutcome::Errored { .. }));
        assert!(report.deferred.is_empty());
        assert!(report.landed.is_empty());
        assert!(report.diagnostics.contains("union-red"));
        assert!(report.diagnostics.contains("[base] passed"));
        assert!(report.diagnostics.contains("[prefix 1]"));
        assert!(report.diagnostics.contains("prefix-infra"));
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn bisect_tests_original_candidate_when_live_ref_moves_during_union_gate() {
        let repo = Repo::new("bisect-pinned-input");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "original red snapshot\n");
        let base = repo.out(&["rev-parse", "main"]);
        let original_candidate = repo.out(&["rev-parse", "b1"]);
        let candidates = repo.branch_set();
        // Every gate runs in a detached private worktree sharing only this
        // fixture's Git store. Move the candidate to green base while the red
        // union is being tested: a live-ref re-read would now test other code.
        let config = cfg(&format!(
            "if test -f a.txt; then git update-ref refs/heads/b1 {base} || exit 127; printf original-snapshot-red; exit 1; fi; exit 0"
        ));
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let report = run_fold(&config, &repo.dir, candidates).unwrap();
        assert_ne!(original_candidate, base);
        assert_eq!(
            repo.out(&["rev-parse", "b1"]),
            base,
            "gate actually moved live ref"
        );
        assert_eq!(repo.out(&["rev-parse", "main"]), base);
        assert!(!report.advanced);
        assert!(report.landed.is_empty());
        assert!(report.prepared.is_empty());
        assert_eq!(report.deferred.len(), 1);
        assert_eq!(report.deferred[0].branch, "b1");
        assert!(
            report.deferred[0].gate_failed,
            "verdict belongs to original pinned snapshot"
        );
        assert!(report.diagnostics.contains("[base] passed"));
        assert!(
            report
                .diagnostics
                .contains("[prefix 1] failed\noriginal-snapshot-red")
        );
        assert!(report.request_result().is_err());
    }

    #[test]
    fn cas_exhaustion_is_an_unadvanced_report_and_preserves_candidates() {
        let repo = Repo::new("cas-hold");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let original = repo.out(&["rev-parse", "main"]);
        let config = cfg("printf green; exit 0");
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let mut calls = 0;
        let report = run_fold_with_cas(&config, &repo.dir, repo.branch_set(), |_, _, _, _| {
            calls += 1;
            Ok(false)
        })
        .unwrap();
        assert_eq!(calls, 5);
        assert_eq!(report.cas_attempts, 5);
        assert!(!report.advanced);
        assert!(report.landed.is_empty());
        assert!(
            report
                .request_result()
                .unwrap_err()
                .to_string()
                .contains("CAS retry budget")
        );
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn cas_io_error_keeps_prepared_candidate_and_gate_diagnostics() {
        let repo = Repo::new("cas-error");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let original = repo.out(&["rev-parse", "main"]);
        let config = cfg("true");
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let report = run_fold_with_cas(&config, &repo.dir, repo.branch_set(), |_, _, _, _| {
            anyhow::bail!("private-cas-error-proof")
        })
        .unwrap();
        assert!(!report.advanced);
        assert_eq!(report.cas_attempts, 1);
        assert!(report.landed.is_empty());
        assert_eq!(report.prepared.len(), 1);
        assert!(report.diagnostics.contains("[union] passed"));
        assert!(report.diagnostics.contains("private-cas-error-proof"));
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn signing_failure_preserves_unprepared_candidates_without_inventing_commits() {
        let repo = Repo::new("signing-hold");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        git(&repo.dir, &["branch", "already-in-target", "main"]);
        let original = repo.out(&["rev-parse", "main"]);
        let missing_signer = repo.dir.join("absent-signing-program");
        git(
            &repo.dir,
            &["config", "gpg.program", missing_signer.to_str().unwrap()],
        );
        let mut config = cfg("");
        config.sign_commits = true;
        let report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(!report.advanced);
        assert!(report.landed.is_empty());
        assert!(report.prepared.is_empty());
        assert_eq!(report.unprepared, ["b1"]);
        assert!(matches!(report.gate, GateOutcome::Errored { .. }));
        assert!(report.diagnostics.contains("signing failed"));
        assert_eq!(repo.out(&["rev-parse", "main"]), original);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn prefix_signing_error_keeps_phase_diagnostics_instead_of_blame() {
        let repo = Repo::new("prefix-signing");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let missing_signer = repo.dir.join("absent-signing-program");
        git(
            &repo.dir,
            &["config", "gpg.program", missing_signer.to_str().unwrap()],
        );
        let config = cfg("true");
        if !repo.gate_supported_or_refused(&config) {
            return;
        }
        let adapter = PlumbingAdapter {
            history: CanonicalHistory::capture(&repo.dir).unwrap(),
            loc: GitLoc::Local(repo.dir.clone()),
            repo_root: repo.dir.clone(),
            regenerate_paths: Vec::new(),
            regenerate_command: String::new(),
            sign: true,
            rerere: false,
        };
        let mut diagnostics = String::new();
        let error = bisect_offender(
            &repo.dir,
            &adapter,
            &repo.out(&["rev-parse", "main"]),
            &repo.branch_set(),
            &config,
            &land_opts(&config, "main"),
            &mut diagnostics,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<BisectAborted>().is_some());
        assert!(diagnostics.contains("[base] passed"));
        assert!(diagnostics.contains("[prefix preparation]"));
        assert!(diagnostics.contains("signing failed"));
    }

    #[test]
    fn persist_defensively_refuses_legacy_speculative_landed_entries() {
        let repo = Repo::new("malformed-report");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let mut config = cfg("false");
        config.bisect_on_red = false;
        let mut report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        report.landed = std::mem::take(&mut report.prepared);
        assert!(!report.advanced);
        assert_held_persistence(&repo, config, &report);
    }

    #[test]
    fn diagnostics_are_aggregate_bounded_and_keep_early_failure_context() {
        let mut diagnostics = String::new();
        record_gate(
            &mut diagnostics,
            "union",
            &GateVerdict::Failed {
                log: "union-marker".into(),
            },
        );
        record_gate(
            &mut diagnostics,
            "base",
            &GateVerdict::Failed {
                log: "base-marker".into(),
            },
        );
        for _ in 0..1000 {
            record_gate(
                &mut diagnostics,
                "prefix",
                &GateVerdict::Failed {
                    log: "\x1b\0\u{9b}é".repeat(5000),
                },
            );
        }
        assert!(diagnostics.len() <= 32 * 1024);
        assert!(diagnostics.contains("union-marker"));
        assert!(diagnostics.contains("base-marker"));
        assert!(diagnostics.contains("omitted"));
        assert!(
            !diagnostics
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        );
    }

    #[test]
    fn diagnostic_marker_reservation_holds_at_exact_byte_boundaries() {
        // Exercise every UTF-8 alignment and near-boundary remaining capacity.
        for remaining in 0..100 {
            let marker = "\n[additional gate diagnostics omitted]\n";
            let target = 32 * 1024 - marker.len() - remaining;
            let mut diagnostics = "x".repeat(target);
            record_gate(
                &mut diagnostics,
                "union",
                &GateVerdict::Failed {
                    log: "é".repeat(60),
                },
            );
            record_gate(
                &mut diagnostics,
                "base",
                &GateVerdict::Failed { log: "tail".into() },
            );
            assert!(diagnostics.len() <= 32 * 1024, "remaining={remaining}");
            assert!(diagnostics.ends_with(marker));
        }
    }

    #[test]
    fn gate_diagnostics_drop_bidi_formatting_and_preserve_readable_unicode() {
        for c in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            let mut diagnostics = String::new();
            record_gate(
                &mut diagnostics,
                "union",
                &GateVerdict::Failed {
                    log: format!("before{c}after\n\t日本語 café"),
                },
            );
            assert!(diagnostics.contains("beforeafter"));
            assert!(!diagnostics.contains(c));
            assert!(diagnostics.contains("\n\t日本語 café"));
        }
    }

    #[test]
    fn explicit_manual_fold_can_land_with_auto_land_disabled_and_noop_succeeds() {
        let repo = Repo::new("manual-auto-off");
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "candidate\n");
        let mut config = cfg("");
        config.auto_land = false;
        let report = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(report.advanced);
        assert!(report.prepared.is_empty());
        assert_eq!(report.landed.len(), 1);
        assert!(report.request_result().is_ok());
        let private = tempfile::tempdir().unwrap();
        let worktree = private.path().join("candidate");
        git(
            &repo.dir,
            &["worktree", "add", worktree.to_str().unwrap(), "b1"],
        );
        let db = Db::open_at(&private.path().join("test.db")).unwrap();
        let candidates = Candidates {
            branches: repo.branch_set(),
            skipped_dirty: Vec::new(),
            identities: HashMap::new(),
            pending_snapshots: HashSet::new(),
            worktrees: HashMap::from([("b1".into(), worktree.to_string_lossy().into_owned())]),
        };
        config.organize_folders = false;
        let observations = observe_outcomes(&db, &candidates).unwrap();
        persist(&config, &repo.dir, &db, &candidates, &report, &observations).unwrap();
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "landed");
        assert_eq!(
            rows[0].result_oid.as_deref(),
            Some(report.final_tip.as_str())
        );
        assert_eq!(repo.out(&["rev-parse", "main"]), report.final_tip);
        let noop = run_fold(&config, &repo.dir, repo.branch_set()).unwrap();
        assert!(!noop.advanced);
        assert!(noop.landed.is_empty());
        assert!(noop.prepared.is_empty());
        assert!(noop.request_result().is_ok());
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
        if !repo.history_supported_or_refused() {
            return;
        }
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
        if !repo.history_supported_or_refused() {
            return;
        }
        // cfg("") has regenerate_command = "" → no regeneration, just classify+defer.
        let report = run_fold(&cfg(""), &repo.dir, repo.branch_set()).unwrap();
        assert!(!report.advanced);
        assert_eq!(report.deferred.len(), 1);
        assert_eq!(report.deferred[0].branch, "b1");
        assert_eq!(report.deferred[0].kind, ConflictKind::Regenerable);
        assert!(
            report.request_result().is_err(),
            "conflict-only requested fold fails"
        );
    }

    #[test]
    fn advancing_main_fast_forwards_the_main_checkout_working_tree() {
        let repo = Repo::new("resync-clean");
        if !repo.history_supported_or_refused() {
            return;
        }
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
        if !repo.history_supported_or_refused() {
            return;
        }
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
        if !repo.history_supported_or_refused() {
            return;
        }
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
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("bad", "base.txt", "changed\n");
        repo.commit("base.txt", "mainline\n", "main edits base");
        let before = repo.out(&["rev-parse", "main"]);

        match attempt_land(&cfg(""), &repo.dir, "bad", &GitLoc::Local(repo.dir.clone())).unwrap() {
            AttemptOutcome::Conflict { paths, .. } => {
                assert!(paths.iter().any(|p| p == "base.txt"))
            }
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
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);
        if !repo.gate_supported_or_refused(&cfg("false")) {
            return;
        }

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
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "a\n");
        let before = repo.out(&["rev-parse", "main"]);
        let mut c = cfg("true"); // green gate
        c.auto_land = false;
        if !repo.gate_supported_or_refused(&c) {
            return;
        }

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
        if !repo.history_supported_or_refused() {
            return;
        }
        repo.feature("b1", "a.txt", "a\n");
        attempt_land(&cfg(""), &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap(); // land it
        // A second attempt sees b1's tip already an ancestor of main.
        assert!(matches!(
            attempt_land(&cfg(""), &repo.dir, "b1", &GitLoc::Local(repo.dir.clone())).unwrap(),
            AttemptOutcome::UpToDate
        ));
    }
}
