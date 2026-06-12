//! Reflog undo/redo: read the HEAD reflog, let superzej-core's planner pick
//! the inverse action, and apply it. The caller (host) records every reset
//! WE make in the DB (`undo_marks`) and shows the plan in a confirm dialog
//! before applying.

use super::{GitBackend, run_w};
use anyhow::{Result, bail};
use superzej_core::reflog::{OurMarks, UndoPlan, plan_redo, plan_undo};
use superzej_core::remote::GitLoc;

pub trait UndoOps: GitBackend {
    /// What `z` (undo) would do right now.
    fn undo_plan(&self, loc: &GitLoc, marks: &OurMarks) -> Result<UndoPlan> {
        Ok(plan_undo(&self.reflog(loc, 100)?, marks))
    }

    /// What `Z` (redo) would do right now.
    fn redo_plan(&self, loc: &GitLoc, marks: &OurMarks) -> Result<UndoPlan> {
        Ok(plan_redo(&self.reflog(loc, 100)?, marks))
    }

    /// Apply a plan. `autostash` wraps a hard reset of a dirty worktree in
    /// `stash push -u` / `stash pop` (lazygit's guard); the caller asks the
    /// user first. Returns the reset target sha (for `undo_marks`) when the
    /// plan was a reset.
    fn undo_apply(&self, loc: &GitLoc, plan: &UndoPlan, autostash: bool) -> Result<Option<String>> {
        match plan {
            UndoPlan::Nothing => bail!("nothing to undo"),
            UndoPlan::Checkout { branch, .. } => {
                run_w(loc, &[], &["checkout", branch])?;
                Ok(None)
            }
            UndoPlan::HardResetTo { sha, .. } => {
                let dirty = self.is_dirty(loc).unwrap_or(false);
                if dirty && autostash {
                    run_w(
                        loc,
                        &[],
                        &["stash", "push", "-u", "-m", "[superzej] undo autostash"],
                    )?;
                }
                let reset = run_w(loc, &[], &["reset", "--hard", sha]);
                if dirty && autostash {
                    // Pop even when the reset failed; a pop conflict surfaces
                    // through the normal conflict UX.
                    let _ = run_w(loc, &[], &["stash", "pop"]);
                }
                reset?;
                Ok(Some(sha.clone()))
            }
        }
    }
}

impl<T: GitBackend + ?Sized> UndoOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{BranchOps, CliGit, GitBackend};
    use super::UndoOps;
    use std::path::Path;
    use superzej_core::reflog::{OurMarks, UndoPlan};

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity (autostash commits objects)
    /// and gpg pinned off.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn undo_a_commit_then_redo_it_via_the_recorded_mark() {
        let repo = TestRepo::new("un-commit");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c1");
        repo.commit_file("f.txt", "two\n", "c2");
        let c1 = repo.sha_of("c1");
        let c2 = repo.sha_of("c2");
        let loc = repo.loc();

        // Undo: the plan targets the pre-c2 state (HEAD before the commit).
        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        match &plan {
            UndoPlan::HardResetTo { sha, undoing } => {
                assert_eq!(sha, &c1);
                assert!(undoing.contains("c2"), "{undoing:?}");
            }
            other => panic!("expected a hard reset, got {other:?}"),
        }
        let mark = CliGit
            .undo_apply(&loc, &plan, false)
            .unwrap()
            .expect("a reset reports its target for the marks DB");
        assert_eq!(mark, c1);
        assert_eq!(repo.head(), c1);
        assert_eq!(repo.subjects(), vec!["c1"]);

        // Feed the mark back: redo now offers the redone (c2) state.
        let marks = OurMarks::new([mark]);
        let redo = CliGit.redo_plan(&loc, &marks).unwrap();
        match &redo {
            UndoPlan::HardResetTo { sha, .. } => assert_eq!(sha, &c2),
            other => panic!("expected a hard reset, got {other:?}"),
        }
        CliGit.undo_apply(&loc, &redo, false).unwrap();
        assert_eq!(repo.head(), c2);
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "two\n"
        );
    }

    #[test]
    fn undo_a_checkout_returns_to_the_previous_branch() {
        let repo = TestRepo::new("un-checkout");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        let loc = repo.loc();

        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        assert_eq!(
            plan,
            UndoPlan::Checkout {
                branch: "main".into(),
                undoing: "checkout: moving from main to feat".into(),
            }
        );
        let mark = CliGit.undo_apply(&loc, &plan, false).unwrap();
        assert!(mark.is_none(), "checkout undos record no mark");
        assert_eq!(CliGit.current_branch(&loc).unwrap(), "main");
    }

    #[test]
    fn undo_apply_autostash_preserves_dirty_changes() {
        let repo = TestRepo::new("un-autostash");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c1");
        repo.commit_file("g.txt", "g\n", "c2");
        let c1 = repo.sha_of("c1");
        // Dirty a file whose content is identical in both commits, so the
        // post-reset stash pop applies cleanly.
        std::fs::write(repo.dir.join("f.txt"), "dirty\n").unwrap();
        let loc = repo.loc();

        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        let mark = CliGit.undo_apply(&loc, &plan, true).unwrap();
        assert_eq!(mark.as_deref(), Some(c1.as_str()));
        assert_eq!(repo.head(), c1, "the reset happened");
        assert!(!repo.dir.join("g.txt").exists(), "c2's file is gone");
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "dirty\n",
            "the dirty change survived the round trip"
        );
        assert!(
            CliGit.stash_list(&loc).unwrap().is_empty(),
            "the autostash was popped, not left behind"
        );
    }

    #[test]
    fn undo_plan_computes_during_a_merge_conflict() {
        let repo = TestRepo::new("un-midmerge");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("f.txt", "feat\n", "fx");
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "main\n", "mx");
        let loc = repo.loc();

        let _ = CliGit.merge(&loc, "feat"); // conflicts → Err, MERGE_HEAD set
        assert!(CliGit.merge_state(&loc).unwrap().is_some());
        // Whether to allow applying mid-merge is the host's call; planning
        // alone must stay total.
        let plan = CliGit.undo_plan(&loc, &OurMarks::default());
        assert!(plan.is_ok(), "{plan:?}");
    }
}
