//! Cherry-pick: the copy buffer lives in the host; this applies it. Commits
//! are applied oldest-first in one invocation so a conflict pauses at the
//! right spot and `--continue` resumes the remainder.

use super::{GitBackend, gpg_args, run_w};
use anyhow::{Result, bail};
use superzej_core::remote::GitLoc;

pub trait CherryOps: GitBackend {
    /// Cherry-pick `shas` (the caller orders them oldest-first). Merge
    /// commits need `mainline` (`-m N`); a mixed selection is refused.
    fn cherry_pick(
        &self,
        loc: &GitLoc,
        shas: &[&str],
        mainline: Option<u32>,
        override_gpg: bool,
    ) -> Result<()> {
        if shas.is_empty() {
            bail!("nothing to cherry-pick");
        }
        let m;
        let mut args = gpg_args(override_gpg).to_vec();
        args.push("cherry-pick");
        if let Some(n) = mainline {
            m = n.to_string();
            args.extend(["-m", &m]);
        }
        args.extend_from_slice(shas);
        run_w(loc, &[("GIT_EDITOR", ":")], &args).map(|_| ())
    }

    fn cherry_continue(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[("GIT_EDITOR", ":")], &["cherry-pick", "--continue"]).map(|_| ())
    }

    fn cherry_skip(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[("GIT_EDITOR", ":")], &["cherry-pick", "--skip"]).map(|_| ())
    }

    fn cherry_abort(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["cherry-pick", "--abort"]).map(|_| ())
    }
}

impl<T: GitBackend + ?Sized> CherryOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend, MergeKind};
    use super::CherryOps;
    use std::path::Path;

    /// Ops run through `GitLoc` (the user's real git env, not the testutil
    /// env), so the repo itself needs an identity and gpg pinned off.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn cherry_pick_two_commits_oldest_first() {
        let repo = TestRepo::new("ch-two");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("a.txt", "a\n", "p1");
        repo.commit_file("b.txt", "b\n", "p2");
        let (p1, p2) = (repo.sha_of("p1"), repo.sha_of("p2"));
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("m.txt", "m\n", "m1");
        let loc = repo.loc();

        assert!(
            CliGit.cherry_pick(&loc, &[], None, false).is_err(),
            "empty selection refused"
        );
        CliGit.cherry_pick(&loc, &[&p1, &p2], None, false).unwrap();
        assert_eq!(repo.subjects(), vec!["p2", "p1", "m1", "c0"]);
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("a.txt")).unwrap(),
            "a\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("b.txt")).unwrap(),
            "b\n"
        );
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
    }

    #[test]
    fn cherry_conflict_abort_then_resolve_and_continue() {
        let repo = TestRepo::new("ch-conflict");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("f.txt", "feat\n", "fx");
        let fx = repo.sha_of("fx");
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "main\n", "mx");
        let head = repo.head();
        let loc = repo.loc();

        // Conflict pauses the pick.
        assert!(CliGit.cherry_pick(&loc, &[&fx], None, false).is_err());
        let st = CliGit.merge_state(&loc).unwrap().expect("pick in progress");
        assert_eq!(st.kind, MergeKind::CherryPick);

        CliGit.cherry_abort(&loc).unwrap();
        assert_eq!(repo.head(), head, "abort restored HEAD");
        assert!(CliGit.merge_state(&loc).unwrap().is_none());

        // Redo, resolve, continue → Done.
        assert!(CliGit.cherry_pick(&loc, &[&fx], None, false).is_err());
        std::fs::write(repo.dir.join("f.txt"), "resolved\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        CliGit.cherry_continue(&loc).unwrap();
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
        assert_eq!(repo.subjects()[0], "fx");
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("f.txt")).unwrap(),
            "resolved\n"
        );
    }

    #[test]
    fn cherry_pick_of_a_merge_commit_requires_mainline() {
        let repo = TestRepo::new("ch-merge");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let c0 = repo.sha_of("c0");
        git_in(&repo.dir, &["checkout", "-q", "-b", "side"]);
        repo.commit_file("s.txt", "side\n", "s1");
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("m.txt", "main\n", "m1");
        git_in(&repo.dir, &["merge", "-q", "--no-edit", "side"]);
        let merge_sha = repo.head();

        git_in(&repo.dir, &["checkout", "-q", "-b", "target", &c0]);
        let loc = repo.loc();
        assert!(
            CliGit
                .cherry_pick(&loc, &[&merge_sha], None, false)
                .is_err(),
            "a merge commit without -m must be refused"
        );
        assert!(
            CliGit.merge_state(&loc).unwrap().is_none(),
            "the refusal leaves no sequencer state"
        );

        // Mainline 1 = the main-side parent: picking applies side's change.
        CliGit
            .cherry_pick(&loc, &[&merge_sha], Some(1), false)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(repo.dir.join("s.txt")).unwrap(),
            "side\n"
        );
        assert!(!repo.dir.join("m.txt").exists(), "mainline side excluded");
        assert!(CliGit.merge_state(&loc).unwrap().is_none());
    }
}
