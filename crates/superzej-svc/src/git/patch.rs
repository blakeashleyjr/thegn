//! Custom-patch operations (lazygit's "rebase magic"): a patch built
//! interactively from an old commit's diff (the host holds the selection;
//! superzej-core's `patch::transform_all` renders it) is removed from the
//! commit, split into a new commit, or moved to the index — each a composite
//! sequence around an `edit` rebase stop. The host records the pre-op HEAD
//! in `undo_marks` so one `z` undoes the whole composite.

use super::rebase::{RebaseOps, RebaseOpts, RebaseOutcome};
use super::{GitBackend, gpg_args, run_stdin, run_w};
use anyhow::{Result, bail};
use superzej_core::rebase_todo::TodoAction;
use superzej_core::remote::GitLoc;

/// Stop a rebase at `sha` with `edit`, run `at_stop`, then continue.
fn with_edit_stop(
    loc: &GitLoc,
    backend: &(impl GitBackend + ?Sized),
    sha: &str,
    opts: &RebaseOpts,
    at_stop: impl FnOnce(&GitLoc) -> Result<()>,
) -> Result<RebaseOutcome> {
    match backend.rebase_retag(loc, sha, &[sha], TodoAction::Edit, opts)? {
        RebaseOutcome::Paused => {}
        RebaseOutcome::Conflict => bail!("rebase hit a conflict before reaching the commit"),
        RebaseOutcome::Done => bail!("rebase did not stop at the commit"),
    }
    if let Err(e) = at_stop(loc) {
        // Leave nothing half-applied: a failed stop action aborts the rebase.
        let _ = backend.rebase_abort(loc);
        return Err(e);
    }
    backend.rebase_continue(loc)
}

pub trait PatchOps: GitBackend {
    /// Apply a rendered patch to the working tree (`reverse` undoes it).
    /// `three_way` falls back to 3-way merge when context drifted.
    fn apply_patch(&self, loc: &GitLoc, patch: &str, reverse: bool, three_way: bool) -> Result<()> {
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if reverse {
            args.push("--reverse");
        }
        if three_way {
            args.push("--3way");
        }
        args.push("-");
        run_stdin(loc, &[], &args, patch.as_bytes()).map(|_| ())
    }

    /// Remove the patch's lines from `sha` (later commits replay on top; a
    /// conflict there pauses as a normal rebase conflict). The reverse apply
    /// uses `--index` (index AND worktree): a `--cached`-only apply would
    /// leave the removed lines dangling in the worktree at the edit stop,
    /// and `git rebase --continue` refuses to proceed over unstaged changes.
    fn remove_patch_from_commit(
        &self,
        loc: &GitLoc,
        sha: &str,
        patch: &str,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        with_edit_stop(loc, self, sha, opts, |loc| {
            run_stdin(
                loc,
                &[],
                &["apply", "--index", "--reverse", "--whitespace=nowarn", "-"],
                patch.as_bytes(),
            )?;
            let mut args = gpg_args(opts.override_gpg).to_vec();
            args.extend(["commit", "--amend", "--no-edit", "--no-verify"]);
            run_w(loc, &[], &args).map(|_| ())
        })
    }

    /// Split the patch out of `sha` into a NEW commit directly after it.
    /// Both applies use `--index` so the worktree tracks the index through
    /// the stop — `git rebase --continue` refuses over unstaged changes.
    fn split_patch_into_commit(
        &self,
        loc: &GitLoc,
        sha: &str,
        patch: &str,
        message: &str,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        with_edit_stop(loc, self, sha, opts, |loc| {
            run_stdin(
                loc,
                &[],
                &["apply", "--index", "--reverse", "--whitespace=nowarn", "-"],
                patch.as_bytes(),
            )?;
            let mut amend = gpg_args(opts.override_gpg).to_vec();
            amend.extend(["commit", "--amend", "--no-edit", "--no-verify"]);
            run_w(loc, &[], &amend)?;
            run_stdin(
                loc,
                &[],
                &["apply", "--index", "--whitespace=nowarn", "-"],
                patch.as_bytes(),
            )?;
            let mut commit = gpg_args(opts.override_gpg).to_vec();
            commit.extend(["commit", "--no-verify", "-m", message]);
            run_w(loc, &[], &commit).map(|_| ())
        })
    }

    /// Remove the patch from `sha` and land it in the index instead — the
    /// staged change re-applies to both index and worktree (`--index`), so
    /// the file's on-disk content ends up exactly where it started. Context
    /// may have shifted after the rewrite, so the final apply uses `--3way`;
    /// if even that fails the rewrite stands and the caller offers reflog
    /// undo.
    fn move_patch_to_index(
        &self,
        loc: &GitLoc,
        sha: &str,
        patch: &str,
        opts: &RebaseOpts,
    ) -> Result<RebaseOutcome> {
        let outcome = self.remove_patch_from_commit(loc, sha, patch, opts)?;
        if outcome != RebaseOutcome::Done {
            return Ok(outcome);
        }
        run_stdin(
            loc,
            &[],
            &["apply", "--index", "--3way", "--whitespace=nowarn", "-"],
            patch.as_bytes(),
        )?;
        Ok(RebaseOutcome::Done)
    }
}

impl<T: GitBackend + ?Sized> PatchOps for T {}

#[cfg(test)]
mod tests {
    use super::super::CliGit;
    use super::super::testutil::{TestRepo, git_in};
    use super::*;
    use superzej_core::patch::{LineKind, Selection, parse_patch, transform};

    /// a.txt content after c2 — the worktree end-state every op must
    /// preserve (the line moves between commits/index, never off disk).
    const AFTER_C2: &str = "one\ntwo\n# redundant\nreal\nthree\nfour\nfive\n";

    /// c1 creates a.txt; c2 adds a redundant comment AMONG another change
    /// (so a partial selection is meaningful); c3 adds an unrelated file
    /// that must replay cleanly after the rewrite.
    fn fixture(tag: &str) -> TestRepo {
        let r = TestRepo::new(tag);
        for (k, v) in [
            ("user.name", "t"),
            ("user.email", "t@e"),
            ("commit.gpgsign", "false"),
        ] {
            git_in(&r.dir, &["config", k, v]);
        }
        r.commit_file("a.txt", "one\ntwo\nthree\nfour\nfive\n", "c1");
        r.commit_file("a.txt", AFTER_C2, "c2");
        r.commit_file("b.txt", "other\n", "c3");
        r
    }

    /// Render the one added line `needle` of `sha`'s diff as a custom patch.
    /// Built with `reverse=true` — the removal flows feed
    /// `git apply --reverse`, whose rendering keeps UNSELECTED added lines
    /// as context (they exist in the commit being reverse-applied to).
    fn line_patch(r: &TestRepo, sha: &str, needle: &str) -> String {
        let diff = CliGit.commit_diff(&r.loc(), sha, None).unwrap();
        for f in &parse_patch(&diff) {
            for (hi, h) in f.hunks.iter().enumerate() {
                for (li, l) in h.lines.iter().enumerate() {
                    if l.kind == LineKind::Add && l.text == needle {
                        let mut sel = Selection::default();
                        sel.insert(hi, li);
                        return transform(f, &sel, true).expect("non-empty patch");
                    }
                }
            }
        }
        panic!("no added line {needle:?} in {sha}'s diff");
    }

    fn opts() -> RebaseOpts {
        RebaseOpts::default()
    }

    #[test]
    fn remove_patch_from_commit_deletes_the_line_everywhere() {
        let r = fixture("rmpatch");
        let c2 = r.sha_of("c2");
        let patch = line_patch(&r, &c2, "# redundant");
        let out = CliGit
            .remove_patch_from_commit(&r.loc(), &c2, &patch, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c3", "c2", "c1"]);
        // c2 lost exactly the selected line; the rest of its change stands.
        let c2_diff = CliGit.commit_diff(&r.loc(), &r.sha_of("c2"), None).unwrap();
        assert!(!c2_diff.contains("# redundant"), "{c2_diff}");
        assert!(c2_diff.contains("+real"), "{c2_diff}");
        // The line is gone from the tree; c3 replayed; worktree clean.
        let a = std::fs::read_to_string(r.dir.join("a.txt")).unwrap();
        assert!(!a.contains("# redundant"), "{a}");
        assert!(a.contains("real\n"), "{a}");
        assert!(r.out(&["ls-tree", "--name-only", "HEAD"]).contains("b.txt"));
        assert_eq!(r.out(&["status", "--porcelain"]), "");
    }

    #[test]
    fn split_patch_into_commit_moves_the_line_to_a_new_commit() {
        let r = fixture("split");
        let old_head = r.head();
        let c2 = r.sha_of("c2");
        let patch = line_patch(&r, &c2, "# redundant");
        let out = CliGit
            .split_patch_into_commit(&r.loc(), &c2, &patch, "split out", &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        assert_eq!(r.subjects(), ["c3", "split out", "c2", "c1"]);
        // The new commit carries exactly the selected line, nothing else.
        let split = CliGit
            .commit_diff(&r.loc(), &r.sha_of("split out"), None)
            .unwrap();
        assert!(split.contains("+# redundant"), "{split}");
        assert!(!split.contains("+real"), "{split}");
        let c2_diff = CliGit.commit_diff(&r.loc(), &r.sha_of("c2"), None).unwrap();
        assert!(!c2_diff.contains("# redundant"), "{c2_diff}");
        // End-state tree identical to before the op; worktree clean.
        assert_eq!(r.out(&["diff", "--stat", &old_head, "HEAD"]), "");
        assert_eq!(r.out(&["status", "--porcelain"]), "");
    }

    #[test]
    fn move_patch_to_index_stages_the_line_without_touching_the_file() {
        let r = fixture("toindex");
        let c2 = r.sha_of("c2");
        let patch = line_patch(&r, &c2, "# redundant");
        let out = CliGit
            .move_patch_to_index(&r.loc(), &c2, &patch, &opts())
            .unwrap();
        assert_eq!(out, RebaseOutcome::Done);
        // The line now lives in the index …
        let staged = r.out(&["diff", "--cached"]);
        assert!(staged.contains("+# redundant"), "{staged}");
        // … not in the rewritten commit …
        let c2_diff = CliGit.commit_diff(&r.loc(), &r.sha_of("c2"), None).unwrap();
        assert!(!c2_diff.contains("# redundant"), "{c2_diff}");
        // … and the on-disk file content is unchanged overall (index and
        // worktree agree: nothing unstaged).
        assert_eq!(
            std::fs::read_to_string(r.dir.join("a.txt")).unwrap(),
            AFTER_C2
        );
        assert_eq!(r.out(&["diff"]), "");
    }

    #[test]
    fn failed_stop_action_aborts_the_rebase_cleanly() {
        let r = fixture("badpatch");
        let old_head = r.head();
        let c2 = r.sha_of("c2");
        let err = CliGit
            .remove_patch_from_commit(&r.loc(), &c2, "this is not a patch\n", &opts())
            .unwrap_err();
        assert!(err.to_string().contains("apply"), "{err}");
        // Nothing half-applied: no rebase in progress, HEAD untouched.
        assert!(CliGit.rebase_status(&r.loc()).unwrap().is_none());
        assert_eq!(r.head(), old_head);
        assert_eq!(r.out(&["status", "--porcelain"]), "");
    }
}
