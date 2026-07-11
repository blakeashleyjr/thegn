//! Bisect: marks drive `git bisect` and each step's stdout is scanned for
//! the culprit line. State reads come from the gitdir (`BISECT_LOG`,
//! `BISECT_TERMS`) and `refs/bisect`.

use super::{GitBackend, run_w};
use anyhow::Result;
use thegn_core::gitrefs::{BisectState, find_culprit, parse_bisect};
use thegn_core::remote::GitLoc;

pub trait BisectOps: GitBackend {
    /// Whether a bisect session is in progress.
    fn bisecting(&self, loc: &GitLoc) -> bool {
        loc.read_git_path("BISECT_LOG").is_some()
    }

    /// The live bisect state, or `None` outside a session.
    fn bisect_state(&self, loc: &GitLoc) -> Result<Option<BisectState>> {
        if !self.bisecting(loc) {
            return Ok(None);
        }
        let refs = run_w(
            loc,
            &[],
            &[
                "for-each-ref",
                "refs/bisect",
                "--format=%(refname:short)\u{1f}%(objectname)",
            ],
        )?;
        let terms = loc
            .read_git_path("BISECT_TERMS")
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        let head = loc.git_out(&["rev-parse", "HEAD"]).unwrap_or_default();
        Ok(Some(parse_bisect(&refs, terms.as_deref(), &head)))
    }

    /// Start a session, optionally seeding bad/good (each mark checks out
    /// the next candidate, so the caller re-hydrates after every step).
    /// Returns the culprit sha if git already narrowed it to one commit.
    fn bisect_start(
        &self,
        loc: &GitLoc,
        bad: Option<&str>,
        good: Option<&str>,
    ) -> Result<Option<String>> {
        let mut args = vec!["bisect", "start"];
        if let Some(b) = bad {
            args.push(b);
            if let Some(g) = good {
                args.push(g);
            }
        }
        let out = run_w(loc, &[], &args)?;
        Ok(find_culprit(&out))
    }

    /// Mark a commit with a bisect term (`good`/`bad` or custom). Empty
    /// `sha` marks the current candidate. Returns the culprit when found.
    fn bisect_mark(&self, loc: &GitLoc, term: &str, sha: Option<&str>) -> Result<Option<String>> {
        let mut args = vec!["bisect", term];
        if let Some(s) = sha {
            args.push(s);
        }
        let out = run_w(loc, &[], &args)?;
        Ok(find_culprit(&out))
    }

    fn bisect_skip(&self, loc: &GitLoc) -> Result<Option<String>> {
        let out = run_w(loc, &[], &["bisect", "skip"])?;
        Ok(find_culprit(&out))
    }

    /// End the session and return to the pre-bisect HEAD.
    fn bisect_reset(&self, loc: &GitLoc) -> Result<()> {
        run_w(loc, &[], &["bisect", "reset"]).map(|_| ())
    }
}

impl<T: GitBackend + ?Sized> BisectOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::TestRepo;
    use super::super::{CliGit, GitBackend};
    use super::BisectOps;

    #[test]
    fn full_bisect_drive_finds_the_culprit() {
        let repo = TestRepo::new("bi-drive");
        // 8 linear commits; commit 5 introduces the BUG marker.
        for i in 1..=8 {
            let content = if i >= 5 {
                format!("v{i}\nBUG\n")
            } else {
                format!("v{i}\n")
            };
            repo.commit_file("data.txt", &content, &format!("c{i}"));
        }
        let first = repo.sha_of("c1");
        let culprit_sha = repo.sha_of("c5");
        let bad_head = repo.head();
        let loc = repo.loc();

        assert!(!CliGit.bisecting(&loc));
        assert!(CliGit.bisect_state(&loc).unwrap().is_none());

        let mut culprit = CliGit
            .bisect_start(&loc, Some(&bad_head), Some(&first))
            .unwrap();

        // Mid-session: state is live and seeded with our endpoints.
        assert!(CliGit.bisecting(&loc));
        let st = CliGit.bisect_state(&loc).unwrap().expect("live state");
        assert_eq!(st.bad_term, "bad");
        assert_eq!(st.good_term, "good");
        assert_eq!(st.bad.as_deref(), Some(bad_head.as_str()));
        assert!(st.good.contains(&first), "good list: {:?}", st.good);
        assert!(st.culprit.is_none());

        // Drive: test the checked-out tree, mark, repeat. log2(6) ≈ 3 steps.
        let mut steps = 0;
        while culprit.is_none() {
            steps += 1;
            assert!(steps <= 8, "bisect did not converge");
            let data = std::fs::read_to_string(repo.dir.join("data.txt")).unwrap();
            let term = if data.contains("BUG") { "bad" } else { "good" };
            culprit = CliGit.bisect_mark(&loc, term, None).unwrap();
        }
        assert_eq!(culprit.unwrap(), culprit_sha);
        assert!(CliGit.bisecting(&loc), "still in session until reset");

        CliGit.bisect_reset(&loc).unwrap();
        assert!(!CliGit.bisecting(&loc));
        assert!(CliGit.bisect_state(&loc).unwrap().is_none());
        assert_eq!(CliGit.current_branch(&loc).unwrap(), "main");
        assert_eq!(
            repo.head(),
            bad_head,
            "reset returns to the pre-bisect HEAD"
        );
    }
}
