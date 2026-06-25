//! Branch operations: checkout/create/delete/merge/ff/push/pull/upstream,
//! plus the nuke-working-tree reset.

use super::{GitBackend, run_w};
use anyhow::Result;
use superzej_core::remote::GitLoc;

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
        run_w(loc, &[], &["checkout", refname]).map(|_| ())
    }

    /// Check out a remote branch as a local tracking branch.
    fn checkout_remote(&self, loc: &GitLoc, remote: &str, branch: &str) -> Result<()> {
        let track = format!("{remote}/{branch}");
        run_w(loc, &[], &["checkout", "-b", branch, "--track", &track]).map(|_| ())
    }

    fn create_branch(&self, loc: &GitLoc, name: &str, base: &str) -> Result<()> {
        run_w(loc, &[], &["branch", name, base]).map(|_| ())
    }

    fn delete_branch(&self, loc: &GitLoc, name: &str, force: bool) -> Result<()> {
        let flag = if force { "-D" } else { "-d" };
        run_w(loc, &[], &["branch", flag, name]).map(|_| ())
    }

    fn delete_remote_branch(&self, loc: &GitLoc, remote: &str, name: &str) -> Result<()> {
        run_w(loc, &[], &["push", remote, "--delete", name]).map(|_| ())
    }

    /// Merge `branch` into the current branch (`--no-edit`); a conflict
    /// surfaces as MERGE_HEAD via `merge_state`.
    fn merge(&self, loc: &GitLoc, branch: &str) -> Result<()> {
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
    /// needed, and git refuses non-ff updates by default.
    fn fast_forward(&self, loc: &GitLoc, branch: &str, current: bool, remote: &str) -> Result<()> {
        if current {
            run_w(loc, &[], &["merge", "--ff-only", "@{u}"]).map(|_| ())
        } else {
            let spec = format!("{branch}:{branch}");
            run_w(loc, &[], &["fetch", remote, &spec]).map(|_| ())
        }
    }

    fn push(&self, loc: &GitLoc, force: ForceMode) -> Result<()> {
        let mut args = vec!["push"];
        match force {
            ForceMode::None => {}
            ForceMode::WithLease => args.push("--force-with-lease"),
            ForceMode::Force => args.push("--force"),
        }
        run_w(loc, &[], &args).map(|_| ())
    }

    fn push_set_upstream(&self, loc: &GitLoc, remote: &str, branch: &str) -> Result<()> {
        run_w(loc, &[], &["push", "-u", remote, branch]).map(|_| ())
    }

    fn pull(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[("GIT_EDITOR", ":")], &["pull", "--no-edit"]).map(|_| ())
    }

    fn fetch(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["fetch", "--all", "--prune"]).map(|_| ())
    }

    fn set_upstream(&self, loc: &GitLoc, remote: &str, branch: &str) -> Result<()> {
        let up = format!("--set-upstream-to={remote}/{branch}");
        run_w(loc, &[], &["branch", &up, branch]).map(|_| ())
    }

    fn rename_branch(&self, loc: &GitLoc, old: &str, new: &str) -> Result<()> {
        run_w(loc, &[], &["branch", "-m", old, new]).map(|_| ())
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

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, commit_empty, git_in};
    use super::super::{CliGit, GitBackend, MergeKind};
    use super::{BranchOps, ForceMode};
    use std::path::{Path, PathBuf};
    use superzej_core::remote::GitLoc;

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
        let out = superzej_core::util::git_cmd(dir)
            .args(args)
            .output()
            .unwrap();
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
        let _ = CliGit.merge(&loc, "feat2");
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
}
