//! Stash operations, including the staged-only (git ≥ 2.35) and
//! unstaged-only (temp-commit dance) variants.

use super::{GitBackend, run_w};
use anyhow::{Result, bail};
use thegn_core::gitrefs::{parse_git_version, supports_stash_staged};
use thegn_core::remote::GitLoc;

fn at(index: usize) -> String {
    format!("stash@{{{index}}}")
}

pub trait StashOps: GitBackend {
    /// `git stash push [-u] -m <msg>`.
    fn stash_push(&self, loc: &GitLoc, message: &str, include_untracked: bool) -> Result<()> {
        let mut args = vec!["stash", "push"];
        if include_untracked {
            args.push("-u");
        }
        args.extend(["-m", message]);
        run_w(loc, &[], &args).map(|_| ())
    }

    /// Stash only the staged changes (`--staged`, git ≥ 2.35 — probed via
    /// `git version` so older gits get a clear error, not a flag complaint).
    fn stash_staged(&self, loc: &GitLoc, message: &str) -> Result<()> {
        let v = loc
            .git_out(&["version"])
            .and_then(|s| parse_git_version(&s));
        if !v.is_some_and(supports_stash_staged) {
            bail!("`git stash push --staged` needs git >= 2.35");
        }
        run_w(loc, &[], &["stash", "push", "--staged", "-m", message]).map(|_| ())
    }

    /// Stash only the UNSTAGED changes: park the index in a temp commit,
    /// stash what remains (incl. untracked), then soft-reset the temp commit
    /// away — the index comes back exactly as it was.
    fn stash_unstaged(&self, loc: &GitLoc, message: &str) -> Result<()> {
        run_w(
            loc,
            &[],
            &[
                "commit",
                "--no-verify",
                "--allow-empty",
                "-m",
                "[thegn] temp index",
            ],
        )?;
        let stashed = run_w(loc, &[], &["stash", "push", "-u", "-m", message]);
        // Always restore the index, even when the stash failed (e.g. nothing
        // unstaged to stash).
        let restored = run_w(loc, &[], &["reset", "--soft", "HEAD^"]);
        stashed?;
        restored.map(|_| ())
    }

    fn stash_pop(&self, loc: &GitLoc, index: usize) -> Result<()> {
        run_w(loc, &[], &["stash", "pop", &at(index)]).map(|_| ())
    }

    fn stash_apply(&self, loc: &GitLoc, index: usize) -> Result<()> {
        run_w(loc, &[], &["stash", "apply", &at(index)]).map(|_| ())
    }

    fn stash_drop(&self, loc: &GitLoc, index: usize) -> Result<()> {
        run_w(loc, &[], &["stash", "drop", &at(index)]).map(|_| ())
    }

    /// A stash's patch. `-u` includes the untracked third-parent tree that
    /// plain `stash show -p` silently omits.
    fn stash_show(&self, loc: &GitLoc, index: usize) -> Result<String> {
        run_w(
            loc,
            &[],
            &["stash", "show", "-p", "-u", "--no-color", &at(index)],
        )
    }
}

impl<T: GitBackend + ?Sized> StashOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend};
    use super::StashOps;
    use std::path::Path;
    use thegn_core::gitrefs::{parse_git_version, supports_stash_staged};

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity and gpg pinned off.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn read(repo: &TestRepo, path: &str) -> String {
        std::fs::read_to_string(repo.dir.join(path)).unwrap()
    }

    #[test]
    fn stash_push_list_pop_roundtrip_with_untracked() {
        let repo = TestRepo::new("st-roundtrip");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        std::fs::write(repo.dir.join("u.txt"), "new\n").unwrap();
        let loc = repo.loc();

        CliGit.stash_push(&loc, "wip stuff", true).unwrap();
        assert!(
            CliGit.status(&loc).unwrap().is_empty(),
            "-u swept everything"
        );
        assert_eq!(read(&repo, "f.txt"), "one\n");
        assert!(!repo.dir.join("u.txt").exists());
        let list = CliGit.stash_list(&loc).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].index, 0);
        assert!(
            list[0].message.contains("wip stuff"),
            "{:?}",
            list[0].message
        );

        CliGit.stash_pop(&loc, 0).unwrap();
        assert!(
            CliGit.stash_list(&loc).unwrap().is_empty(),
            "pop drops the entry"
        );
        assert_eq!(read(&repo, "f.txt"), "two\n");
        assert_eq!(read(&repo, "u.txt"), "new\n");
    }

    #[test]
    fn stash_apply_keeps_entry_and_drop_removes_it() {
        let repo = TestRepo::new("st-applydrop");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "keepme", false).unwrap();

        CliGit.stash_apply(&loc, 0).unwrap();
        assert_eq!(read(&repo, "f.txt"), "two\n");
        assert_eq!(CliGit.stash_list(&loc).unwrap().len(), 1, "apply keeps it");

        CliGit.stash_drop(&loc, 0).unwrap();
        assert!(CliGit.stash_list(&loc).unwrap().is_empty());
        assert_eq!(
            read(&repo, "f.txt"),
            "two\n",
            "drop leaves the worktree alone"
        );
    }

    #[test]
    fn stash_staged_takes_only_the_index() {
        let repo = TestRepo::new("st-staged");
        ident(&repo.dir);
        repo.commit_file("a.txt", "a1\n", "c0");
        repo.commit_file("b.txt", "b1\n", "c1");
        let loc = repo.loc();

        let v = parse_git_version(&repo.out(&["version"]));
        if !v.is_some_and(supports_stash_staged) {
            // Old git: the op must refuse with a clear error, not run.
            assert!(CliGit.stash_staged(&loc, "staged only").is_err());
            return;
        }

        std::fs::write(repo.dir.join("a.txt"), "a2\n").unwrap();
        git_in(&repo.dir, &["add", "a.txt"]);
        std::fs::write(repo.dir.join("b.txt"), "b2\n").unwrap();

        CliGit.stash_staged(&loc, "staged only").unwrap();
        assert_eq!(read(&repo, "b.txt"), "b2\n", "unstaged change stays put");
        assert_eq!(
            read(&repo, "a.txt"),
            "a1\n",
            "staged change left with the stash"
        );
        assert!(repo.out(&["diff", "--cached", "--name-only"]).is_empty());
        let patch = repo.out(&["stash", "show", "-p", "stash@{0}"]);
        assert!(patch.contains("a.txt"), "{patch}");
        assert!(!patch.contains("b.txt"), "{patch}");
    }

    #[test]
    fn stash_unstaged_takes_worktree_changes_and_preserves_the_index() {
        let repo = TestRepo::new("st-unstaged");
        ident(&repo.dir);
        repo.commit_file("a.txt", "a1\n", "c0");
        repo.commit_file("b.txt", "b1\n", "c1");
        let head = repo.head();
        let loc = repo.loc();

        std::fs::write(repo.dir.join("a.txt"), "a2\n").unwrap();
        git_in(&repo.dir, &["add", "a.txt"]);
        std::fs::write(repo.dir.join("b.txt"), "b2\n").unwrap();
        let staged_before = repo.out(&["diff", "--cached"]);

        CliGit.stash_unstaged(&loc, "unstaged only").unwrap();

        assert_eq!(repo.head(), head, "temp commit was soft-reset away");
        assert_eq!(
            repo.out(&["diff", "--cached"]),
            staged_before,
            "index restored exactly"
        );
        assert_eq!(read(&repo, "b.txt"), "b1\n", "unstaged change stashed");
        assert_eq!(
            read(&repo, "a.txt"),
            "a2\n",
            "staged content stays in the worktree"
        );
        let list = CliGit.stash_list(&loc).unwrap();
        assert_eq!(list.len(), 1);
        let patch = repo.out(&["stash", "show", "-p", "stash@{0}"]);
        assert!(patch.contains("b.txt"), "{patch}");
        assert!(!patch.contains("a.txt"), "{patch}");
    }

    #[test]
    fn stash_show_includes_untracked_files() {
        let repo = TestRepo::new("st-show");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        std::fs::write(repo.dir.join("brand-new.txt"), "fresh\n").unwrap();
        let loc = repo.loc();
        CliGit.stash_push(&loc, "with untracked", true).unwrap();

        let patch = CliGit.stash_show(&loc, 0).unwrap();
        assert!(patch.contains("f.txt"), "{patch}");
        assert!(
            patch.contains("brand-new.txt") && patch.contains("+fresh"),
            "-u must surface the untracked third parent: {patch}"
        );
    }
}
