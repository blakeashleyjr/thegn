//! The local merge queue ("fold-actor") runner.
//!
//! Folds queued worktree branches onto a repo's `target_branch` entirely in the
//! git object database (no checkout), test-gates the folded tip, and advances the
//! branch with an atomic compare-and-swap. Clean branches land automatically;
//! genuine conflicts are deferred. The pure sequencing lives in
//! [`superzej_core::fold`]; this module is the I/O around it — merge plumbing
//! ([`superzej_svc::git::PlumbingOps`]), the throwaway-worktree gate, and the CAS
//! retry loop.
//!
//! [`run_fold`] is synchronous and side-effecting on the repo; the CLI calls it
//! directly and the host daemon calls it from `spawn_blocking`.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use superzej_core::config::MergeQueueConfig;
use superzej_core::db::Db;
use superzej_core::fold::{self, Branch, ConflictKind, FoldGit, FoldPlan, MergeOutcome};
use superzej_core::remote::GitLoc;
use superzej_core::util;
use superzej_svc::git::{CliGit, GitBackend, MergeTreeOutcome, PlumbingOps};

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
// off-loop: the fold runs from the CLI (`szhost integrate`) or from
// spawn_fold's spawn_blocking (see the module doc) — never on the loop.
#[expect(clippy::disallowed_methods)]
fn regenerate_merge(
    repo_root: &Path,
    ours: &str,
    theirs: &str,
    regenerate_paths: &[String],
    regenerate_command: &str,
) -> Option<String> {
    let tmp = std::env::temp_dir().join(format!(
        "sz-foldregen-{}-{}",
        std::process::id(),
        util::now()
    ));
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
        superzej_core::msg::info(&format!(
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
}

/// Resolve the branch the fold advances. `"auto"` (or empty) → the repo's
/// default branch; otherwise the configured name verbatim.
pub fn resolve_target(cfg: &MergeQueueConfig, repo_root: &Path) -> String {
    if cfg.target_branch.is_empty() || cfg.target_branch == "auto" {
        superzej_core::worktree::default_branch(repo_root)
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
pub fn fold_active_repo(mq: &MergeQueueConfig, any_path: &Path) -> Result<FoldReport> {
    let repo_root = main_checkout(any_path).context("not inside a git repository")?;
    let target = resolve_target(mq, &repo_root);
    let cands = candidate_branches(mq, &repo_root, &target)?;
    let report = run_fold(mq, &repo_root, cands.branches.clone())?;
    if let Ok(db) = Db::open() {
        let _ = persist(&db, &cands, &report);
    }
    Ok(report)
}

/// Collect a repo's foldable worktree branches: every linked worktree (not the
/// main checkout, not the target branch itself). Dirty worktrees are snapshotted
/// into a commit when `snapshot_dirty`, else skipped.
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
pub fn persist(db: &Db, cands: &Candidates, report: &FoldReport) -> Result<()> {
    for b in &cands.branches {
        if let Some(wt) = cands.worktrees.get(&b.name) {
            db.enqueue_merge(wt, &b.name, &report.target_branch)?;
        }
    }
    for l in &report.landed {
        if let Some(wt) = cands.worktrees.get(&l.branch) {
            db.update_merge_status(wt, "landed", Some(&l.commit), None, None)?;
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
        }
    }
    Ok(())
}

/// Build/test the folded tip in a throwaway detached worktree. Returns whether
/// `gate_command` exited zero. The worktree is always removed afterward.
// off-loop: the fold runs from the CLI (`szhost integrate`) or from
// spawn_fold's spawn_blocking (see the module doc) — never on the loop.
#[expect(clippy::disallowed_methods)]
fn gate_tip(repo_root: &Path, oid: &str, gate_command: &str) -> Result<bool> {
    let tmp = std::env::temp_dir().join(format!(
        "sz-foldgate-{}-{}",
        std::process::id(),
        util::now()
    ));
    let tmp_s = tmp.to_string_lossy().to_string();
    if !util::git_ok(
        repo_root,
        &["worktree", "add", "--detach", "--force", &tmp_s, oid],
    ) {
        anyhow::bail!("merge queue: could not create gate worktree at {tmp_s}");
    }
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(gate_command)
        .current_dir(&tmp)
        .status();
    // Best-effort teardown — never leak the gate worktree.
    let _ = util::git_ok(repo_root, &["worktree", "remove", "--force", &tmp_s]);
    Ok(status.map(|s| s.success()).unwrap_or(false))
}

/// On a red gate, re-fold growing prefixes of the landed branches; the first
/// prefix whose gate goes red names its last branch as the offender. Returns
/// `None` when it can't localize one (e.g. a flaky gate), in which case the
/// whole batch is held back.
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
        if plan.advanced() && !gate_tip(repo_root, &plan.final_tip, &cfg.gate_command)? {
            return Ok(Some(l.branch.clone()));
        }
    }
    Ok(None)
}

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
            if gate_tip(repo_root, &plan.final_tip, &cfg.gate_command)? {
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
                if let Some(off) = bisect_offender(repo_root, &adapter, &base, &landed, cfg)? {
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
            match util::resync_ff_checkout(repo_root, &target_branch, &base, &plan.final_tip) {
                util::ResyncOutcome::Healed => superzej_core::msg::info(&format!(
                    "merge queue: synced {target_branch} checkout to {}",
                    &plan.final_tip[..plan.final_tip.len().min(9)]
                )),
                util::ResyncOutcome::Skipped(why) => tracing::debug!(
                    target: "szhost::integrate",
                    why,
                    "left main checkout working tree as-is"
                ),
                util::ResyncOutcome::Failed => tracing::warn!(
                    target: "szhost::integrate",
                    "could not fast-forward the main checkout; run `git -C <repo> reset --hard {target_branch}` to sync it"
                ),
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
            return Ok(report);
        }
        if cas_attempts >= 5 {
            anyhow::bail!("merge queue: {target_branch} kept moving under the fold");
        }
        // Lost the race — loop, re-read, re-fold.
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
                "sz-integ-{tag}-{}-{}",
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
            auto_drain: false,
            snapshot_dirty: false,
            regenerate_paths: vec!["Cargo.lock".into()],
            regenerate_command: String::new(),
            conflict_handoff: Default::default(),
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
}
