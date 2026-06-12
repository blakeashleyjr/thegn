//! Commit-level writes: commit/amend (message via stdin — multiline and
//! remote safe), revert, tags, and reset-to-commit.

use super::{GitBackend, gpg_args, run_stdin, run_w};
use anyhow::Result;
use superzej_core::remote::GitLoc;

/// `git reset` flavor for reset-to-commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    pub fn flag(self) -> &'static str {
        match self {
            ResetMode::Soft => "--soft",
            ResetMode::Mixed => "--mixed",
            ResetMode::Hard => "--hard",
        }
    }
}

pub trait CommitOps: GitBackend {
    /// Commit the staged changes (`git commit -F -`).
    fn commit(&self, loc: &GitLoc, message: &str, no_verify: bool) -> Result<()> {
        let mut args = vec!["commit"];
        if no_verify {
            args.push("--no-verify");
        }
        args.extend(["-F", "-"]);
        run_stdin(loc, &[], &args, message.as_bytes()).map(|_| ())
    }

    /// Amend HEAD with the staged changes, keeping its message.
    fn commit_amend(&self, loc: &GitLoc, no_verify: bool, override_gpg: bool) -> Result<()> {
        let mut args = gpg_args(override_gpg).to_vec();
        args.extend(["commit", "--amend", "--no-edit"]);
        if no_verify {
            args.push("--no-verify");
        }
        run_w(loc, &[], &args).map(|_| ())
    }

    /// Revert a commit (`--no-edit`); merge commits need a mainline parent
    /// (`-m N`, 1-based). A conflict surfaces as REVERT_HEAD via
    /// `merge_state`.
    fn revert(&self, loc: &GitLoc, sha: &str, mainline: Option<u32>) -> Result<()> {
        let m;
        let mut args = vec!["revert", "--no-edit"];
        if let Some(n) = mainline {
            m = n.to_string();
            args.extend(["-m", &m]);
        }
        args.push(sha);
        run_w(loc, &[("GIT_EDITOR", ":")], &args).map(|_| ())
    }

    fn revert_continue(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[("GIT_EDITOR", ":")], &["revert", "--continue"]).map(|_| ())
    }

    fn revert_abort(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["revert", "--abort"]).map(|_| ())
    }

    /// Tag `sha` — lightweight, or annotated when `annotate` carries a
    /// message.
    fn tag(&self, loc: &GitLoc, name: &str, sha: &str, annotate: Option<&str>) -> Result<()> {
        match annotate {
            Some(msg) => run_w(loc, &[], &["tag", "-a", "-m", msg, name, sha]).map(|_| ()),
            None => run_w(loc, &[], &["tag", name, sha]).map(|_| ()),
        }
    }

    fn delete_tag(&self, loc: &GitLoc, name: &str) -> Result<()> {
        run_w(loc, &[], &["tag", "-d", name]).map(|_| ())
    }

    fn push_tag(&self, loc: &GitLoc, remote: &str, name: &str) -> Result<()> {
        run_w(loc, &[], &["push", remote, name]).map(|_| ())
    }

    /// `git reset --soft|--mixed|--hard <sha>`. Hard is destructive —
    /// confirm at the call site.
    fn reset_to(&self, loc: &GitLoc, sha: &str, mode: ResetMode) -> Result<()> {
        run_w(loc, &[], &["reset", mode.flag(), sha]).map(|_| ())
    }
}

impl<T: GitBackend + ?Sized> CommitOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend, MergeKind};
    use super::{CommitOps, ResetMode};
    use std::path::Path;

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity and gpg pinned off.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
        git_in(dir, &["config", "tag.gpgsign", "false"]);
    }

    fn read(repo: &TestRepo, path: &str) -> String {
        std::fs::read_to_string(repo.dir.join(path)).unwrap()
    }

    #[test]
    fn commit_roundtrips_a_multiline_message_with_special_chars() {
        let repo = TestRepo::new("co-msg");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        std::fs::write(repo.dir.join("f.txt"), "two\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        let loc = repo.loc();

        let msg = "subject with 'single' \"double\" $VARS\n\nbody `backticks` and $HOME\nsecond body line";
        CliGit.commit(&loc, msg, false).unwrap();
        // %B is the raw body; stdin piping must round-trip it verbatim
        // (modulo git's trailing-newline cleanup, matched by out()'s trim).
        assert_eq!(repo.out(&["log", "-1", "--format=%B"]), msg);
        assert!(CliGit.status(&loc).unwrap().is_empty());
    }

    #[test]
    fn commit_amend_folds_staged_change_keeping_message() {
        let repo = TestRepo::new("co-amend");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        repo.commit_file("f.txt", "two\n", "the subject");
        std::fs::write(repo.dir.join("f.txt"), "three\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        let loc = repo.loc();

        CliGit.commit_amend(&loc, false, false).unwrap();
        assert_eq!(repo.subjects(), vec!["the subject", "c0"], "no new commit");
        assert_eq!(
            repo.out(&["show", "HEAD:f.txt"]),
            "three",
            "change folded in"
        );
        assert!(CliGit.status(&loc).unwrap().is_empty());
    }

    #[test]
    fn revert_creates_inverse_commit_and_conflict_surfaces_as_revert_state() {
        let repo = TestRepo::new("co-revert");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        repo.commit_file("f.txt", "two\n", "c1");
        let c1 = repo.sha_of("c1");
        let loc = repo.loc();

        CliGit.revert(&loc, &c1, None).unwrap();
        assert_eq!(read(&repo, "f.txt"), "one\n", "tree restored");
        assert!(
            repo.subjects()[0].starts_with("Revert"),
            "{:?}",
            repo.subjects()
        );
        assert!(CliGit.merge_state(&loc).unwrap().is_none());

        // c1's inverse (two→one) no longer applies on top of "three".
        repo.commit_file("f.txt", "three\n", "c2");
        assert!(CliGit.revert(&loc, &c1, None).is_err());
        let st = CliGit
            .merge_state(&loc)
            .unwrap()
            .expect("revert in progress");
        assert_eq!(st.kind, MergeKind::Revert);

        CliGit.revert_abort(&loc).unwrap();
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
        assert_eq!(
            read(&repo, "f.txt"),
            "three\n",
            "abort restored the worktree"
        );
    }

    #[test]
    fn tag_lightweight_annotated_and_delete() {
        let repo = TestRepo::new("co-tag");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let head = repo.head();
        let loc = repo.loc();

        CliGit.tag(&loc, "lw", &head, None).unwrap();
        CliGit
            .tag(&loc, "ann", &head, Some("annotated message"))
            .unwrap();
        let types = repo.out(&[
            "for-each-ref",
            "refs/tags",
            "--format=%(refname:short) %(objecttype)",
        ]);
        assert!(types.lines().any(|l| l == "lw commit"), "{types}");
        assert!(types.lines().any(|l| l == "ann tag"), "{types}");

        CliGit.delete_tag(&loc, "lw").unwrap();
        let types = repo.out(&[
            "for-each-ref",
            "refs/tags",
            "--format=%(refname:short) %(objecttype)",
        ]);
        assert!(!types.contains("lw"), "{types}");
        assert!(types.lines().any(|l| l == "ann tag"), "{types}");
    }

    #[test]
    fn reset_to_soft_keeps_index_mixed_unstages_hard_cleans() {
        let repo = TestRepo::new("co-reset");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        repo.commit_file("f.txt", "two\n", "c1");
        let c0 = repo.sha_of("c0");
        let c1 = repo.sha_of("c1");
        let loc = repo.loc();
        let dirty_index = || {
            std::fs::write(repo.dir.join("f.txt"), "three\n").unwrap();
            git_in(&repo.dir, &["add", "f.txt"]);
        };

        dirty_index();
        CliGit.reset_to(&loc, &c0, ResetMode::Soft).unwrap();
        assert_eq!(repo.head(), c0);
        assert_eq!(
            repo.out(&["show", ":f.txt"]),
            "three",
            "soft keeps the index"
        );
        assert_eq!(read(&repo, "f.txt"), "three\n");

        git_in(&repo.dir, &["reset", "-q", "--hard", &c1]);
        dirty_index();
        CliGit.reset_to(&loc, &c0, ResetMode::Mixed).unwrap();
        assert_eq!(repo.head(), c0);
        assert_eq!(
            repo.out(&["show", ":f.txt"]),
            "one",
            "mixed resets the index"
        );
        assert_eq!(read(&repo, "f.txt"), "three\n", "mixed keeps the worktree");

        git_in(&repo.dir, &["reset", "-q", "--hard", &c1]);
        dirty_index();
        CliGit.reset_to(&loc, &c0, ResetMode::Hard).unwrap();
        assert_eq!(repo.head(), c0);
        assert_eq!(read(&repo, "f.txt"), "one\n", "hard cleans the worktree");
        assert!(CliGit.status(&loc).unwrap().is_empty());
    }
}
