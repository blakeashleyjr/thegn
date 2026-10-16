//! Branch operations: checkout/create/delete/merge/ff/push/pull/upstream,
//! plus the nuke-working-tree reset.

use super::{GitBackend, run_w, run_w_locked, run_w_network};
use anyhow::Result;
use thegn_core::git_operand as op;
use thegn_core::remote::GitLoc;

/// Push force level. Plain `--force` only after `--force-with-lease` was
/// rejected and the user confirmed a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForceMode {
    #[default]
    None,
    WithLease,
    Force,
}

pub trait BranchOps: GitBackend {
    fn checkout(&self, loc: &GitLoc, refname: &str) -> Result<()> {
        // Fully qualify so a leading-dash short name can never be an option;
        // `op` refuses those before spawn.
        let refname = op::branch_name(refname)?;
        run_w(loc, &[], &["checkout", refname]).map(|_| ())
    }

    /// Check out a remote branch as a local tracking branch.
    fn checkout_remote(&self, loc: &GitLoc, remote: &str, branch: &str) -> Result<()> {
        let (remote, branch) = (op::remote_name(remote)?, op::branch_name(branch)?);
        let track = format!("{remote}/{branch}");
        run_w(loc, &[], &["checkout", "-b", branch, "--track", &track])
            .map(|_| after_ref_mutation(loc))
    }

    fn create_branch(&self, loc: &GitLoc, name: &str, base: &str) -> Result<()> {
        let (name, base) = (op::branch_name(name)?, op::revision(base)?);
        run_w(loc, &[], &["branch", name, base]).map(|_| after_ref_mutation(loc))
    }

    fn delete_branch(&self, loc: &GitLoc, name: &str, force: bool) -> Result<()> {
        let name = op::branch_name(name)?;
        let flag = if force { "-D" } else { "-d" };
        run_w(loc, &[], &["branch", flag, name]).map(|_| after_ref_mutation(loc))
    }

    fn delete_remote_branch(&self, loc: &GitLoc, remote: &str, name: &str) -> Result<()> {
        let (remote, name) = (op::remote_name(remote)?, op::branch_name(name)?);
        run_w_network(loc, &[], &["push", remote, "--delete", name]).map(|_| ())
    }

    /// Merge `branch` into the current branch (`--no-edit`); a conflict
    /// surfaces as MERGE_HEAD via `merge_state`.
    fn merge(&self, loc: &GitLoc, branch: &str) -> Result<()> {
        let branch = op::revision(branch)?;
        run_w(loc, &[("GIT_EDITOR", ":")], &["merge", "--no-edit", branch]).map(|_| ())
    }

    fn merge_continue(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[("GIT_EDITOR", ":")], &["merge", "--continue"]).map(|_| ())
    }

    fn merge_abort(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["merge", "--abort"]).map(|_| ())
    }

    /// Fast-forward a branch to its upstream. The CURRENT branch
    /// fast-forwards via `merge --ff-only @{u}`; any other branch via the
    /// fetch-refspec trick (`git fetch <remote> <b>:<b>`) — no checkout
    /// needed, and git refuses non-ff updates by default. Local repositories
    /// fetch to a private ref first, then update the branch under the lock.
    fn fast_forward(&self, loc: &GitLoc, branch: &str, current: bool, remote: &str) -> Result<()> {
        let branch = op::branch_name(branch)?;
        if current {
            run_w(loc, &[], &["merge", "--ff-only", "@{u}"]).map(|_| ())
        } else {
            let remote = op::remote_name(remote)?;
            if !loc.is_remote() && crate::bridge::for_loc(loc).is_none() {
                fast_forward_local(loc, remote, branch, super::MUTATION_LOCK_WAIT)
            } else {
                let spec = format!("{branch}:{branch}");
                run_w(loc, &[], &["fetch", remote, &spec]).map(|_| after_ref_mutation(loc))
            }
        }
    }

    fn push(&self, loc: &GitLoc, force: ForceMode) -> Result<()> {
        let mut args = vec!["push"];
        match force {
            ForceMode::None => {}
            ForceMode::WithLease => args.push("--force-with-lease"),
            ForceMode::Force => args.push("--force"),
        }
        run_w_network(loc, &[], &args).map(|_| ())
    }

    fn push_set_upstream(&self, loc: &GitLoc, remote: &str, branch: &str) -> Result<()> {
        let (remote, branch) = (op::remote_name(remote)?, op::branch_name(branch)?);
        run_w_network(loc, &[], &["push", "-u", remote, branch]).map(|_| ())
    }

    fn pull(&self, loc: &GitLoc) -> Result<()> {
        // Pull combines a network fetch with a merge/rebase, so keep its
        // existing single locked command until those phases can be split.
        run_w(loc, &[("GIT_EDITOR", ":")], &["pull", "--no-edit"]).map(|_| ())
    }

    fn fetch(&self, loc: &GitLoc) -> Result<()> {
        run_w_network(loc, &[], &["fetch", "--all", "--prune"]).map(|_| ())
    }

    fn set_upstream(&self, loc: &GitLoc, remote: &str, branch: &str) -> Result<()> {
        let (remote, branch) = (op::remote_name(remote)?, op::branch_name(branch)?);
        let up = format!("--set-upstream-to={remote}/{branch}");
        run_w(loc, &[], &["branch", &up, branch]).map(|_| ())
    }

    fn rename_branch(&self, loc: &GitLoc, old: &str, new: &str) -> Result<()> {
        let (old, new) = (op::branch_name(old)?, op::branch_name(new)?);
        run_w(loc, &[], &["branch", "-m", old, new]).map(|_| after_ref_mutation(loc))
    }

    /// Nuke the working tree: hard reset + clean everything (untracked,
    /// ignored, dirs), recursing into submodules. Kidpix style — confirm
    /// hard at the call site; this is NOT undoable via reflog.
    fn nuke_working_tree(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["reset", "--hard", "HEAD"])?;
        run_w(loc, &[], &["clean", "-fdx"])?;
        // Dirty submodules survive a top-level reset/clean; recurse when any
        // are configured (.gitmodules at the worktree root).
        let has_submodules = loc
            .git_out(&["ls-files", "--error-unmatch", ".gitmodules"])
            .is_some();
        if has_submodules {
            run_w(
                loc,
                &[],
                &[
                    "submodule",
                    "foreach",
                    "--recursive",
                    "git reset --hard && git clean -fdx",
                ],
            )?;
        }
        Ok(())
    }
}

impl<T: GitBackend + ?Sized> BranchOps for T {}

fn after_ref_mutation(loc: &GitLoc) {
    super::invalidate_glyph_base(loc);
}

fn fast_forward_local(
    loc: &GitLoc,
    remote: &str,
    branch: &str,
    lock_wait: std::time::Duration,
) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FAST_FORWARD_SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

    let scratch = format!(
        "refs/thegn/ff/{}-{}",
        std::process::id(),
        FAST_FORWARD_SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fetch_spec = format!("+refs/heads/{branch}:{scratch}");
    let update_spec = format!("{scratch}:refs/heads/{branch}");
    let lock_args = [
        "fetch",
        "--no-tags",
        "--no-recurse-submodules",
        ".",
        &update_spec,
    ];

    let result = fast_forward_local_phases(loc, remote, &fetch_spec, &lock_args, lock_wait);

    if let Err(error) = run_w_network(loc, &[], &["update-ref", "-d", &scratch]) {
        tracing::debug!(target: "thegn::git", %error, scratch_ref = %scratch, "could not remove fast-forward scratch ref");
    }
    result.map(|()| after_ref_mutation(loc))
}

fn fast_forward_local_phases(
    loc: &GitLoc,
    remote: &str,
    fetch_spec: &str,
    lock_args: &[&str],
    lock_wait: std::time::Duration,
) -> Result<()> {
    run_w_network(loc, &[], &["fetch", remote, fetch_spec])?;
    let lock = super::require_mutation_lock_within(
        std::path::Path::new(&loc.path()),
        lock_args,
        lock_wait,
    )?;
    run_w_locked(loc, &[], lock_args, &lock)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, commit_empty, git_in};
    use super::super::{CliGit, GitBackend, MergeKind};
    use super::{BranchOps, ForceMode};
    use std::path::{Path, PathBuf};
    use thegn_core::remote::GitLoc;

    struct GlyphBaseSentinel {
        key: super::super::RepoExecutionIdentity,
    }

    impl GlyphBaseSentinel {
        fn seed(loc: &GitLoc) -> Self {
            let key = super::super::repo_execution_identity(loc);
            super::super::base_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(
                    key.clone(),
                    (Some("sentinel".into()), std::time::Instant::now(), None),
                );
            Self { key }
        }

        fn is_present(&self) -> bool {
            super::super::base_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&self.key)
        }
    }

    impl Drop for GlyphBaseSentinel {
        fn drop(&mut self) {
            super::super::base_cache()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.key);
        }
    }

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity and deterministic knobs.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
        git_in(dir, &["config", "tag.gpgsign", "false"]);
    }

    /// Trimmed stdout of `git` in an arbitrary dir (panics on failure) — for
    /// asserting on the clone/remote repos a `TestRepo` method can't reach.
    fn out_in(dir: &Path, args: &[&str]) -> String {
        // Scrubbed `git -C dir` so it reads the intended repo, not an outer one
        // leaked via GIT_DIR when the suite runs inside a commit hook.
        let out = thegn_core::util::git_cmd(dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A bare "remote" (`remote.git`) seeded from `repo`'s main, plus a clone
    /// of it — both nested inside `repo.dir` so the `TestRepo` drop cleans
    /// everything. Returns the clone's path; ops under test run in the clone.
    fn with_remote(repo: &TestRepo) -> PathBuf {
        let remote = repo.dir.join("remote.git");
        git_in(
            &repo.dir,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        git_in(
            &repo.dir,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_in(&repo.dir, &["push", "-q", "origin", "main"]);
        let clone = repo.dir.join("clone");
        git_in(
            &repo.dir,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        ident(&clone);
        git_in(&clone, &["config", "push.default", "simple"]);
        git_in(&clone, &["config", "pull.rebase", "false"]);
        clone
    }

    #[test]
    fn dash_named_operands_refused_before_spawn() {
        use super::super::CommitOps as _;
        let repo = TestRepo::new("br-dash");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = GitLoc::Local(repo.dir.clone());
        let sha = out_in(&repo.dir, &["rev-parse", "HEAD"]);
        // A real raw ref whose short name looks like an option.
        git_in(&repo.dir, &["update-ref", "refs/heads/--help", "HEAD"]);
        for n in ["--help", "--detach", "-f", "-D", "--abort"] {
            assert!(CliGit.checkout(&loc, n).is_err(), "{n}");
            assert!(CliGit.delete_branch(&loc, n, true).is_err(), "{n}");
            assert!(CliGit.create_branch(&loc, n, "HEAD").is_err(), "{n}");
            assert!(CliGit.merge(&loc, n).is_err(), "{n}");
            assert!(CliGit.tag(&loc, n, &sha, Some("m")).is_err(), "{n}");
            assert!(CliGit.delete_tag(&loc, n).is_err(), "{n}");
            assert!(CliGit.push_tag(&loc, "origin", n).is_err(), "{n}");
            assert!(CliGit.push_tag(&loc, n, "v1").is_err(), "{n}");
            assert!(
                CliGit
                    .reset_to(&loc, n, super::super::ResetMode::Soft)
                    .is_err()
            );
        }
        // `tag -a -m m -f <sha>` used to force-create a tag named after the sha.
        let tags = out_in(&repo.dir, &["tag", "--list"]);
        assert!(tags.is_empty(), "no tag created: {tags:?}");
        let heads = out_in(&repo.dir, &["for-each-ref", "refs/heads/"]);
        assert!(heads.contains("refs/heads/--help"), "ref untouched");
    }

    #[test]
    fn checkout_create_and_delete_branch() {
        let repo = TestRepo::new("br-crud");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();

        CliGit.create_branch(&loc, "feat", "main").unwrap();
        let names = |loc: &GitLoc| -> Vec<String> {
            CliGit
                .branches(loc)
                .unwrap()
                .into_iter()
                .map(|b| b.name)
                .collect()
        };
        assert!(names(&loc).contains(&"feat".to_string()));

        CliGit.checkout(&loc, "feat").unwrap();
        assert_eq!(CliGit.current_branch(&loc).unwrap(), "feat");
        repo.commit_file("g.txt", "x\n", "feat work");
        CliGit.checkout(&loc, "main").unwrap();
        assert_eq!(CliGit.current_branch(&loc).unwrap(), "main");

        // Non-merged branch: `-d` refuses, `-D` deletes.
        assert!(CliGit.delete_branch(&loc, "feat", false).is_err());
        assert!(names(&loc).contains(&"feat".to_string()), "refusal kept it");
        CliGit.delete_branch(&loc, "feat", true).unwrap();
        assert!(!names(&loc).contains(&"feat".to_string()));

        // Merged (pointing at HEAD): plain `-d` succeeds.
        CliGit.create_branch(&loc, "merged", "main").unwrap();
        CliGit.delete_branch(&loc, "merged", false).unwrap();
        assert!(!names(&loc).contains(&"merged".to_string()));
    }

    #[test]
    fn successful_branch_mutations_invalidate_glyph_base() {
        let repo = TestRepo::new("br-cache-create");
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = GitLoc::Local(repo.dir.clone());
        let cache = GlyphBaseSentinel::seed(&loc);
        CliGit.create_branch(&loc, "feature", "HEAD").unwrap();
        assert!(!cache.is_present(), "create_branch invalidates the cache");
        drop(cache);

        let cache = GlyphBaseSentinel::seed(&loc);
        CliGit.delete_branch(&loc, "feature", true).unwrap();
        assert!(!cache.is_present(), "delete_branch invalidates the cache");
        drop(cache);

        CliGit.create_branch(&loc, "old", "HEAD").unwrap();
        let cache = GlyphBaseSentinel::seed(&loc);
        CliGit.rename_branch(&loc, "old", "new").unwrap();
        assert!(!cache.is_present(), "rename_branch invalidates the cache");
        drop(cache);
    }

    #[test]
    fn checkout_remote_invalidates_glyph_base() {
        let repo = TestRepo::new("br-cache-checkout-remote");
        repo.commit_file("f.txt", "base\n", "c0");
        let clone = with_remote(&repo);
        git_in(&repo.dir, &["push", "-q", "origin", "main:feature"]);
        git_in(&clone, &["fetch", "-q", "origin"]);
        let loc = GitLoc::Local(clone.clone());
        let cache = GlyphBaseSentinel::seed(&loc);
        CliGit.checkout_remote(&loc, "origin", "feature").unwrap();
        assert!(!cache.is_present(), "checkout_remote invalidates the cache");
    }

    #[test]
    fn non_current_fast_forward_invalidates_glyph_base() {
        let repo = TestRepo::new("br-cache-fast-forward");
        repo.commit_file("f.txt", "base\n", "c0");
        let clone = with_remote(&repo);
        git_in(&repo.dir, &["push", "-q", "origin", "main:feature"]);
        let loc = GitLoc::Local(clone);
        let cache = GlyphBaseSentinel::seed(&loc);
        CliGit
            .fast_forward(&loc, "feature", false, "origin")
            .unwrap();
        assert!(!cache.is_present(), "fast_forward invalidates the cache");
    }

    #[test]
    fn refused_branch_mutation_keeps_glyph_base() {
        let repo = TestRepo::new("br-cache-refused");
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = GitLoc::Local(repo.dir.clone());
        let cache = GlyphBaseSentinel::seed(&loc);
        assert!(CliGit.delete_branch(&loc, "missing", true).is_err());
        assert!(cache.is_present(), "failed mutation preserves the cache");
    }

    #[test]
    fn merge_clean_then_conflicted_with_abort() {
        let repo = TestRepo::new("br-merge");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = repo.loc();

        // Clean merge: disjoint files on diverged branches.
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("feat.txt", "feat\n", "feat add");
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("main.txt", "main\n", "main add");
        CliGit.merge(&loc, "feat").unwrap();
        assert!(repo.dir.join("feat.txt").exists());
        assert!(CliGit.merge_state(&loc).unwrap().is_none());

        // Conflicting merge: both sides edit f.txt. merge() surfaces the
        // conflict exit as Err; the durable signal is merge_state.
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat2"]);
        repo.commit_file("f.txt", "feat2\n", "feat2 edit");
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "main2\n", "main edit");
        let head = repo.head();
        let _ = CliGit.merge(&loc, "feat2"); // best-effort: conflicting merge fails by design; state asserted below
        let st = CliGit
            .merge_state(&loc)
            .unwrap()
            .expect("merge in progress");
        assert_eq!(st.kind, MergeKind::Merge);

        CliGit.merge_abort(&loc).unwrap();
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
        assert_eq!(repo.head(), head, "abort restored HEAD");
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "main2\n",
            "abort restored the worktree"
        );
    }

    /// THE-718: `merge_state` answers from `stat`s of the per-worktree git dir
    /// (no subprocess). In a LINKED worktree — whose `.git` is a redirect file —
    /// it must still agree with what the CLI says, in both directions.
    #[test]
    fn merge_state_in_a_linked_worktree_matches_the_cli() {
        let repo = TestRepo::new("br-merge-linked");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("f.txt", "feat\n", "feat edit");
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "main\n", "main edit");
        let wt = repo.dir.with_file_name(format!(
            "{}-linked",
            repo.dir.file_name().unwrap().to_string_lossy()
        ));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&wt);
        git_in(
            &repo.dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        let loc = GitLoc::Local(wt.clone());
        let cli_has =
            |what: &str| thegn_core::util::git_ok(&wt, &["rev-parse", "-q", "--verify", what]);
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
        assert!(!cli_has("MERGE_HEAD"));
        let _ = CliGit.merge(&loc, "feat"); // best-effort: conflicting merge fails by design; state asserted below
        assert!(cli_has("MERGE_HEAD"));
        let st = CliGit
            .merge_state(&loc)
            .unwrap()
            .expect("merge in progress");
        assert_eq!(st.kind, MergeKind::Merge);
        CliGit.merge_abort(&loc).unwrap();
        assert!(!cli_has("MERGE_HEAD"));
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn fast_forward_non_current_branch_via_fetch_refspec() {
        let repo = TestRepo::new("br-ff");
        repo.commit_file("f.txt", "one\n", "c0");
        let clone = with_remote(&repo);
        let loc = GitLoc::for_worktree(&clone);

        // Sit on another branch in the clone while the remote advances.
        git_in(&clone, &["checkout", "-q", "-b", "work"]);
        commit_empty(&repo.dir, "r1");
        git_in(&repo.dir, &["push", "-q", "origin", "main"]);

        CliGit.fast_forward(&loc, "main", false, "origin").unwrap();
        assert_eq!(out_in(&clone, &["rev-parse", "main"]), repo.head());
        assert_eq!(
            CliGit.current_branch(&loc).unwrap(),
            "work",
            "ff must not switch branches"
        );

        // Diverge the clone's main: the fetch-refspec update is non-ff and
        // git refuses it.
        commit_empty(&clone, "local divergence");
        git_in(&clone, &["branch", "-f", "main", "HEAD"]);
        assert!(CliGit.fast_forward(&loc, "main", false, "origin").is_err());
    }

    #[test]
    fn push_pull_and_force_with_lease_against_bare_remote() {
        let repo = TestRepo::new("br-push");
        repo.commit_file("f.txt", "one\n", "c0");
        let clone = with_remote(&repo);
        let remote = repo.dir.join("remote.git");
        let loc = GitLoc::for_worktree(&clone);
        let in_sync = || {
            out_in(&remote, &["rev-parse", "refs/heads/feat"])
                == out_in(&clone, &["rev-parse", "feat"])
        };

        let commit_file = |name: &str, msg: &str| {
            std::fs::write(clone.join(name), "x\n").unwrap();
            git_in(&clone, &["add", name]);
            git_in(&clone, &["commit", "-q", "-m", msg]);
        };
        git_in(&clone, &["checkout", "-q", "-b", "feat"]);
        commit_file("p1.txt", "p1");
        CliGit.push_set_upstream(&loc, "origin", "feat").unwrap();
        assert!(in_sync());

        commit_file("p2.txt", "p2");
        CliGit.push(&loc, ForceMode::None).unwrap();
        assert!(in_sync());

        // History rewrite: plain push is refused, the lease push lands.
        git_in(&clone, &["commit", "-q", "--amend", "-m", "p2 rewritten"]);
        assert!(CliGit.push(&loc, ForceMode::None).is_err());
        CliGit.push(&loc, ForceMode::WithLease).unwrap();
        assert!(in_sync());

        // pull() fast-forwards a behind clone back to the remote tip.
        let tip = out_in(&clone, &["rev-parse", "HEAD"]);
        git_in(&clone, &["reset", "-q", "--hard", "HEAD~1"]);
        CliGit.pull(&loc).unwrap();
        assert_eq!(out_in(&clone, &["rev-parse", "HEAD"]), tip);
    }

    #[test]
    fn nuke_working_tree_clears_tracked_untracked_and_ignored() {
        let repo = TestRepo::new("br-nuke");
        ident(&repo.dir);
        repo.commit_file(".gitignore", "ignored.txt\n", "c0");
        repo.commit_file("tracked.txt", "clean\n", "c1");
        std::fs::write(repo.dir.join("tracked.txt"), "dirty\n").unwrap();
        std::fs::write(repo.dir.join("untracked.txt"), "u\n").unwrap();
        std::fs::write(repo.dir.join("ignored.txt"), "i\n").unwrap();
        let loc = repo.loc();
        assert!(CliGit.is_dirty(&loc).unwrap());

        CliGit.nuke_working_tree(&loc).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("tracked.txt")).unwrap(),
            "clean\n"
        );
        assert!(!repo.dir.join("untracked.txt").exists());
        assert!(
            !repo.dir.join("ignored.txt").exists(),
            "clean -fdx removes ignored"
        );
        assert!(CliGit.status(&loc).unwrap().is_empty());
    }

    #[test]
    fn rename_branch_and_set_upstream() {
        let repo = TestRepo::new("br-upstream");
        repo.commit_file("f.txt", "one\n", "c0");
        let clone = with_remote(&repo);
        let loc = GitLoc::for_worktree(&clone);

        git_in(&clone, &["branch", "-q", "old", "main"]);
        CliGit.rename_branch(&loc, "old", "new").unwrap();
        let names: Vec<String> = CliGit
            .branches(&loc)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert!(names.contains(&"new".to_string()));
        assert!(!names.contains(&"old".to_string()));

        // Publish it without -u, then wire the upstream explicitly.
        git_in(&clone, &["push", "-q", "origin", "new"]);
        CliGit.set_upstream(&loc, "origin", "new").unwrap();
        assert_eq!(
            out_in(&clone, &["rev-parse", "--abbrev-ref", "new@{upstream}"]),
            "origin/new"
        );
    }

    #[test]
    fn network_ops_run_without_the_mutation_lock() {
        use super::super::commit::CommitOps;
        let repo = TestRepo::new("br-network-unlocked");
        std::fs::write(repo.dir.join("f.txt"), "one\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c0"],
        );
        let clone = with_remote(&repo);
        ident(&clone);
        let remote = repo.dir.join("remote.git");
        let loc = GitLoc::for_worktree(&clone);

        git_in(&clone, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(clone.join("feat.txt"), "feature\n").unwrap();
        git_in(&clone, &["add", "feat.txt"]);
        git_in(
            &clone,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "feature",
            ],
        );

        let lock_path = thegn_core::util::git_common_dir(&clone).join("thegn-git.lock");
        std::fs::create_dir(lock_path).unwrap();

        CliGit.push_set_upstream(&loc, "origin", "feat").unwrap();
        assert_eq!(
            out_in(&remote, &["rev-parse", "refs/heads/feat"]),
            out_in(&clone, &["rev-parse", "feat"])
        );

        git_in(
            &clone,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "second",
            ],
        );
        CliGit.push(&loc, ForceMode::None).unwrap();
        assert_eq!(
            out_in(&remote, &["rev-parse", "refs/heads/feat"]),
            out_in(&clone, &["rev-parse", "feat"])
        );

        git_in(&clone, &["tag", "v-network", "feat"]);
        CliGit.push_tag(&loc, "origin", "v-network").unwrap();
        assert_eq!(
            out_in(&remote, &["rev-parse", "refs/tags/v-network"]),
            out_in(&clone, &["rev-parse", "refs/tags/v-network"])
        );

        CliGit.delete_remote_branch(&loc, "origin", "feat").unwrap();
        assert!(
            out_in(
                &remote,
                &["for-each-ref", "--format=%(refname)", "refs/heads/feat"]
            )
            .is_empty()
        );

        git_in(
            &repo.dir,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "remote advance",
            ],
        );
        let remote_tip = repo.head();
        git_in(&repo.dir, &["push", "-q", "origin", "main"]);
        CliGit.fetch(&loc).unwrap();
        git_in(
            &clone,
            &["cat-file", "-e", &format!("{remote_tip}^{{commit}}")],
        );
    }

    #[test]
    fn local_fast_forward_fetches_unlocked_and_updates_the_branch_only_under_the_lock() {
        let repo = TestRepo::new("br-ff-lock");
        std::fs::write(repo.dir.join("f.txt"), "one\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c0"],
        );
        let clone = with_remote(&repo);
        git_in(&clone, &["checkout", "-q", "-b", "work"]);
        let loc = GitLoc::for_worktree(&clone);
        git_in(
            &repo.dir,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "remote tip",
            ],
        );
        git_in(&repo.dir, &["push", "-q", "origin", "main"]);
        let remote_tip = repo.head();
        let original_main = out_in(&clone, &["rev-parse", "main"]);

        let held = thegn_core::util::try_lock_git_mutations(&clone).unwrap();
        let error = super::fast_forward_local(
            &loc,
            "origin",
            "main",
            std::time::Duration::from_millis(100),
        )
        .unwrap_err();
        assert!(error.to_string().contains("mutation lock"));
        git_in(
            &clone,
            &["cat-file", "-e", &format!("{remote_tip}^{{commit}}")],
        );
        assert_eq!(out_in(&clone, &["rev-parse", "main"]), original_main);
        assert!(
            out_in(
                &clone,
                &["for-each-ref", "--format=%(refname)", "refs/thegn/ff"]
            )
            .is_empty()
        );

        drop(held);
        super::fast_forward_local(&loc, "origin", "main", super::super::MUTATION_LOCK_WAIT)
            .unwrap();
        assert_eq!(out_in(&clone, &["rev-parse", "main"]), remote_tip);
        assert!(
            out_in(
                &clone,
                &["for-each-ref", "--format=%(refname)", "refs/thegn/ff"]
            )
            .is_empty()
        );
    }

    #[test]
    fn local_fast_forward_refuses_a_branch_checked_out_elsewhere() {
        let repo = TestRepo::new("br-ff-linked");
        std::fs::write(repo.dir.join("f.txt"), "one\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c0"],
        );
        let clone = with_remote(&repo);
        let loc = GitLoc::for_worktree(&clone);
        git_in(&clone, &["checkout", "-q", "-b", "work"]);
        let linked = repo.dir.join("linked");
        git_in(
            &clone,
            &["worktree", "add", "-q", linked.to_str().unwrap(), "main"],
        );
        git_in(
            &repo.dir,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "remote tip",
            ],
        );
        git_in(&repo.dir, &["push", "-q", "origin", "main"]);
        let original_main = out_in(&clone, &["rev-parse", "main"]);

        assert!(CliGit.fast_forward(&loc, "main", false, "origin").is_err());
        assert_eq!(out_in(&clone, &["rev-parse", "main"]), original_main);
        assert!(
            out_in(
                &clone,
                &["for-each-ref", "--format=%(refname)", "refs/thegn/ff"]
            )
            .is_empty()
        );
    }

    #[test]
    fn local_fast_forward_non_fast_forward_refusal_cleans_scratch_ref() {
        let repo = TestRepo::new("br-ff-non-ff");
        std::fs::write(repo.dir.join("f.txt"), "one\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "c0"],
        );
        let clone = with_remote(&repo);
        git_in(&clone, &["checkout", "-q", "-b", "work"]);
        let loc = GitLoc::for_worktree(&clone);
        git_in(
            &repo.dir,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "remote advance",
            ],
        );
        git_in(&repo.dir, &["push", "-q", "origin", "main"]);
        git_in(
            &clone,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "local divergence",
            ],
        );
        git_in(&clone, &["branch", "-f", "main", "HEAD"]);
        let old_main = out_in(&clone, &["rev-parse", "main"]);

        assert!(CliGit.fast_forward(&loc, "main", false, "origin").is_err());
        assert_eq!(out_in(&clone, &["rev-parse", "main"]), old_main);
        assert!(
            out_in(
                &clone,
                &["for-each-ref", "--format=%(refname)", "refs/thegn/ff"]
            )
            .is_empty()
        );
    }
}
