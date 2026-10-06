//! Reflog undo/redo: read the HEAD reflog, let thegn-core's planner pick
//! the inverse action, and apply it. The caller (host) records every reset
//! WE make in the DB (`undo_marks`) and shows the plan in a confirm dialog
//! before applying.

use super::{GitBackend, run_w};
use anyhow::{Context, Result, bail};
use thegn_core::git_operand as op;
use thegn_core::reflog::{OurMarks, UndoPlan, plan_redo, plan_undo};
use thegn_core::remote::GitLoc;

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
                let branch = op::branch_name(branch)?;
                run_w(loc, &[], &["checkout", branch])?;
                Ok(None)
            }
            UndoPlan::HardResetTo { sha, .. } => {
                // Validate before the autostash so a refusal changes nothing.
                let target = op::revision(sha)?;
                let unknown = || {
                    format!(
                        "worktree state unknown \u{2014} nothing was changed; run `git status` in {} to diagnose",
                        loc.path()
                    )
                };
                // With autostash the dirty probe is racy (edits can land after
                // it), so always attempt the push; `stash push` is a no-op
                // when there is nothing to save, and the ref comparison below
                // is what tells us whether it saved anything. Without
                // autostash, unknown dirtiness must never read as clean.
                let pushed = if autostash {
                    let before = stash_top(loc).with_context(unknown)?;
                    run_w(
                        loc,
                        &[],
                        &["stash", "push", "-u", "-m", "[thegn] undo autostash"],
                    )
                    .with_context(unknown)?;
                    let after = stash_top(loc).with_context(unknown)?;
                    // exit 0 with "No local changes to save" leaves the ref
                    // alone; popping then would apply an unrelated older stash.
                    if after != before { after } else { None }
                } else {
                    self.is_dirty(loc).with_context(unknown)?;
                    None
                };
                let reset = run_w(loc, &[], &["reset", "--hard", target]);
                if let Some(entry) = pushed {
                    // Pop even when the reset failed; a pop conflict surfaces
                    // through the normal conflict UX. Pop exactly our entry.
                    let pop = pop_entry(loc, &entry);
                    if let Err(pop) = pop {
                        // The stash is kept; say so rather than losing track of it.
                        return match reset {
                            Err(e) => Err(e.context(format!(
                                "stash pop also failed ({pop}); your changes remain in the stash"
                            ))),
                            Ok(_) => Err(pop.context(
                                "reset succeeded but stash pop failed; your changes remain in the stash",
                            )),
                        };
                    }
                }
                reset?;
                Ok(Some(sha.clone()))
            }
        }
    }
}

/// Object id at `refs/stash`, `None` when there is no stash. A read failure is
/// an error (not "no stash").
fn stash_top(loc: &GitLoc) -> Result<Option<String>> {
    let out = run_w(
        loc,
        &[],
        &["for-each-ref", "--format=%(objectname)", "refs/stash"],
    )?;
    Ok(out
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty()))
}

/// Pop the stash entry whose commit is `entry`, wherever it sits in the list.
fn pop_entry(loc: &GitLoc, entry: &str) -> Result<()> {
    let list = run_w(loc, &[], &["stash", "list", "--format=%H"])?;
    let idx = list
        .lines()
        .position(|l| l.trim() == entry)
        .ok_or_else(|| anyhow::anyhow!("autostash entry {entry} not found in the stash list"))?;
    run_w(loc, &[], &["stash", "pop", &format!("stash@{{{idx}}}")]).map(|_| ())
}

impl<T: GitBackend + ?Sized> UndoOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{BranchOps, CliGit, GitBackend};
    use super::UndoOps;
    use std::path::Path;
    use thegn_core::reflog::{OurMarks, UndoPlan};

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity (autostash commits objects)
    /// and gpg pinned off.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn undo_apply_refuses_option_shaped_targets() {
        let repo = TestRepo::new("undo-dash");
        let loc = repo.loc();
        let reset = UndoPlan::HardResetTo {
            sha: "--hard".into(),
            undoing: String::new(),
        };
        assert!(CliGit.undo_apply(&loc, &reset, true).is_err());
        let co = UndoPlan::Checkout {
            branch: "--orphan".into(),
            undoing: String::new(),
        };
        assert!(CliGit.undo_apply(&loc, &co, false).is_err());
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
    fn undo_apply_refuses_when_dirtiness_is_unknown() {
        let repo = TestRepo::new("un-unknown-dirty");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c1");
        repo.commit_file("f.txt", "two\n", "c2");
        std::fs::write(repo.dir.join("f.txt"), "precious\n").unwrap();
        let loc = repo.loc();
        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        let head = repo.head();
        // A corrupt index makes the status read fail; that must not read as
        // clean. Without autostash `reset --hard` would succeed here (it
        // rewrites the index), so only the refusal protects the file.
        std::fs::write(repo.dir.join(".git/index"), b"garbage").unwrap();
        assert!(CliGit.status(&loc).is_err(), "precondition: status fails");
        let err = CliGit.undo_apply(&loc, &plan, false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("worktree state unknown"), "{msg}");
        assert!(msg.contains("nothing was changed"), "{msg}");
        assert!(msg.contains("git status"), "{msg}");
        assert_eq!(repo.head(), head, "no reset ran");
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "precious\n"
        );
    }

    #[test]
    fn undo_apply_autostash_refuses_when_the_stash_cannot_be_made() {
        let repo = TestRepo::new("un-unknown-autostash");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c1");
        repo.commit_file("f.txt", "two\n", "c2");
        std::fs::write(repo.dir.join("f.txt"), "precious\n").unwrap();
        let loc = repo.loc();
        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        let head = repo.head();
        std::fs::write(repo.dir.join(".git/index"), b"garbage").unwrap();
        let err = CliGit.undo_apply(&loc, &plan, true).unwrap_err();
        assert!(
            format!("{err:#}").contains("worktree state unknown"),
            "{err:#}"
        );
        assert_eq!(repo.head(), head, "no reset ran");
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "precious\n"
        );
    }

    #[test]
    fn autostash_with_nothing_to_save_leaves_a_preexisting_stash_alone() {
        let repo = TestRepo::new("un-stash-survives");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c1");
        repo.commit_file("g.txt", "g\n", "c2");
        let loc = repo.loc();
        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        // A user stash from before the undo (planned first: stash writes the reflog).
        std::fs::write(repo.dir.join("f.txt"), "user stash\n").unwrap();
        repo.out(&["stash", "push", "-m", "mine"]);
        let before = repo.out(&["rev-parse", "refs/stash"]);
        // Clean tree: push saves nothing and exits 0.
        CliGit.undo_apply(&loc, &plan, true).unwrap();
        assert_eq!(repo.subjects(), vec!["c1"], "the reset happened");
        assert_eq!(repo.out(&["rev-parse", "refs/stash"]), before);
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "one\n",
            "the unrelated stash was not applied"
        );
    }

    #[test]
    fn autostash_pops_only_its_own_entry_above_an_older_stash() {
        let repo = TestRepo::new("un-stash-own");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c1");
        repo.commit_file("g.txt", "g\n", "c2");
        std::fs::write(repo.dir.join("f.txt"), "user stash\n").unwrap();
        let loc = repo.loc();
        let plan = CliGit.undo_plan(&loc, &OurMarks::default()).unwrap();
        repo.out(&["stash", "push", "-m", "mine"]);
        std::fs::write(repo.dir.join("f.txt"), "dirty\n").unwrap();
        let older = repo.out(&["rev-parse", "refs/stash"]);
        CliGit.undo_apply(&loc, &plan, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "dirty\n"
        );
        assert_eq!(repo.out(&["rev-parse", "refs/stash"]), older);
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

        let _ = CliGit.merge(&loc, "feat"); // best-effort: conflicts by design (conflicts → Err, MERGE_HEAD set)
        assert!(CliGit.merge_state(&loc).unwrap().is_some());
        // Whether to allow applying mid-merge is the host's call; planning
        // alone must stay total.
        let plan = CliGit.undo_plan(&loc, &OurMarks::default());
        assert!(plan.is_ok(), "{plan:?}");
    }
}
