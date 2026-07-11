//! Low-level git plumbing for the local merge queue ("fold-actor"): object-DB
//! merges and atomic ref updates that touch no working tree. `merge-tree`
//! computes a real 3-way merge and writes the result tree directly to the object
//! database, so N worktree branches can be folded against a moving `main` tip
//! entirely in the object DB — no checkout, no shared index contention.
//!
//! These are the seams `thegn-core::fold` drives through a thin adapter; the
//! fold *algorithm* lives in core (pure, gated tests), the *I/O* lives here.

use super::{GitBackend, run, run_stdin, run_w};
use anyhow::{Context, Result};
use thegn_core::remote::GitLoc;

/// Outcome of `git merge-tree --write-tree`. Both arms carry the written tree
/// oid — git writes the (conflict-marked) tree even when conflicts occur, but
/// the fold engine only commits the `Clean` ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTreeOutcome {
    Clean { tree: String },
    Conflict { tree: String, paths: Vec<String> },
}

/// Run git capturing the raw exit code plus both streams. `merge-tree` exits 1
/// on conflicts and `update-ref` exits 1 on a CAS mismatch — both are normal
/// outcomes, not errors, so the shared [`run`]/[`run_w`] helpers (which bail on
/// any non-zero status) can't express them.
fn run_status(loc: &GitLoc, args: &[&str]) -> Result<(i32, String, String)> {
    if let Some(b) = crate::bridge::for_loc(loc) {
        let mut argv: Vec<String> = vec!["git".into(), "-C".into(), loc.path()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let r = b.exec(&refs, None, &[])?;
        return Ok((r.exit, r.stdout, r.stderr));
    }
    let out = loc
        .git_command(args)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

pub trait PlumbingOps: GitBackend {
    /// Resolve a rev to a full object id (`git rev-parse <rev>`).
    fn rev_parse(&self, loc: &GitLoc, rev: &str) -> Result<String> {
        Ok(run(loc, &["rev-parse", rev])?.trim().to_string())
    }

    /// Fold `theirs` onto `ours` in the object DB. `--write-tree` (git ≥ 2.38)
    /// finds the merge base itself, performs a real 3-way merge, and writes the
    /// result tree — nothing else is touched. `--name-only -z` makes the
    /// conflicted-file section a NUL-delimited path list (robust to spaces).
    /// Exit 0 = clean, 1 = conflicts, >1 = genuine failure.
    fn merge_tree(&self, loc: &GitLoc, ours: &str, theirs: &str) -> Result<MergeTreeOutcome> {
        let (code, stdout, stderr) = run_status(
            loc,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                ours,
                theirs,
            ],
        )?;
        // Output sections are NUL-separated: <tree-oid> then (on conflict) the
        // conflicted filenames, then an empty record before informational
        // messages. Take the oid, then paths up to that separator.
        let mut parts = stdout.split('\0');
        let tree = parts.next().unwrap_or("").trim().to_string();
        match code {
            0 => {
                if tree.is_empty() {
                    anyhow::bail!("merge-tree: empty tree oid (stderr: {})", stderr.trim());
                }
                Ok(MergeTreeOutcome::Clean { tree })
            }
            1 => {
                let mut paths = Vec::new();
                for p in parts {
                    if p.is_empty() {
                        break; // section separator → informational messages follow
                    }
                    paths.push(p.to_string());
                }
                Ok(MergeTreeOutcome::Conflict { tree, paths })
            }
            _ => anyhow::bail!("git merge-tree failed: {}", stderr.trim()),
        }
    }

    /// Create a commit object from an existing tree (`git commit-tree`). The
    /// message rides stdin to dodge arg-length/quoting limits. `commit-tree`
    /// does not gpg-sign unless `-S` is passed, so a daemon fold never stalls on
    /// a passphrase prompt.
    fn commit_tree(&self, loc: &GitLoc, tree: &str, parents: &[&str], msg: &str) -> Result<String> {
        let mut args: Vec<&str> = vec!["commit-tree", tree];
        for p in parents {
            args.push("-p");
            args.push(p);
        }
        Ok(run_stdin(loc, &[], &args, msg.as_bytes())?
            .trim()
            .to_string())
    }

    /// Atomically advance a (fully-qualified) ref only if it still points at
    /// `old` — `git update-ref <ref> <new> <old>`. A mismatch means `main` moved
    /// under the fold; that's a normal "re-fold" signal returned as `Ok(false)`,
    /// distinct from a genuine lock/ref error which is `Err`.
    fn update_ref_cas(&self, loc: &GitLoc, name: &str, new: &str, old: &str) -> Result<bool> {
        let (code, _out, stderr) = run_status(loc, &["update-ref", name, new, old])?;
        if code == 0 {
            return Ok(true);
        }
        // `update-ref` reports the CAS mismatch as e.g.
        //   "fatal: cannot lock ref 'refs/heads/main': is at X but expected Y"
        if stderr.contains("but expected") {
            return Ok(false);
        }
        anyhow::bail!("git update-ref {name} failed: {}", stderr.trim());
    }

    /// Snapshot uncommitted worktree work into a commit so `merge-tree` (which
    /// needs committed trees) can fold it. Returns the new branch tip, or `None`
    /// when the worktree is clean (caller folds the existing branch tip).
    /// `--no-verify` skips hooks — this is an automated snapshot, not a user
    /// commit.
    fn snapshot_worktree(&self, loc: &GitLoc, msg: &str) -> Result<Option<String>> {
        if !self.is_dirty(loc)? {
            return Ok(None);
        }
        run_w(loc, &[], &["add", "-A"])?;
        run_stdin(
            loc,
            &[("GIT_EDITOR", ":")],
            &["commit", "--no-verify", "-F", "-"],
            msg.as_bytes(),
        )?;
        Ok(Some(self.rev_parse(loc, "HEAD")?))
    }
}

impl<T: GitBackend + ?Sized> PlumbingOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend};
    use super::{MergeTreeOutcome, PlumbingOps};
    use std::path::Path;

    /// Ops run through `GitLoc` (the user's real git env), so the repo needs an
    /// identity for `commit-tree`/`commit` to succeed deterministically.
    fn ident(dir: &Path) {
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "user.email", "t@e"]);
        git_in(dir, &["config", "commit.gpgsign", "false"]);
    }

    #[test]
    fn rev_parse_matches_head() {
        let repo = TestRepo::new("plumb-revparse");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        assert_eq!(CliGit.rev_parse(&loc, "HEAD").unwrap(), repo.head());
    }

    #[test]
    fn merge_tree_clean_folds_disjoint_branches() {
        let repo = TestRepo::new("plumb-clean");
        ident(&repo.dir);
        repo.commit_file("base.txt", "base\n", "c0");
        let loc = repo.loc();
        let base = repo.head();

        // Two diverged branches touching disjoint files.
        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("feat.txt", "feat\n", "feat add");
        let feat = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("main.txt", "main\n", "main add");
        let main = repo.head();

        let outcome = CliGit.merge_tree(&loc, &main, &feat).unwrap();
        let tree = match outcome {
            MergeTreeOutcome::Clean { tree } => tree,
            other => panic!("expected clean, got {other:?}"),
        };

        // The folded commit carries both parents and the union of both trees,
        // without ever checking anything out.
        let merge = CliGit
            .commit_tree(&loc, &tree, &[&main, &feat], "fold feat")
            .unwrap();
        let parents = repo.out(&["rev-list", "--parents", "-n", "1", &merge]);
        assert!(
            parents.contains(&main) && parents.contains(&feat),
            "{parents}"
        );
        let files = repo.out(&["ls-tree", "-r", "--name-only", &merge]);
        assert!(
            files.contains("feat.txt") && files.contains("main.txt"),
            "{files}"
        );
        // Worktree/HEAD untouched by the object-DB fold.
        assert_eq!(repo.head(), main);
        assert_ne!(base, main);
    }

    #[test]
    fn merge_tree_reports_conflicted_paths() {
        let repo = TestRepo::new("plumb-conflict");
        ident(&repo.dir);
        repo.commit_file("f.txt", "base\n", "c0");
        let loc = repo.loc();

        git_in(&repo.dir, &["checkout", "-q", "-b", "feat"]);
        repo.commit_file("f.txt", "feat\n", "feat edit");
        let feat = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("f.txt", "main\n", "main edit");
        let main = repo.head();

        match CliGit.merge_tree(&loc, &main, &feat).unwrap() {
            MergeTreeOutcome::Conflict { paths, .. } => {
                assert_eq!(paths, vec!["f.txt".to_string()]);
            }
            other => panic!("expected conflict, got {other:?}"),
        }
    }

    #[test]
    fn update_ref_cas_advances_then_refuses_stale() {
        let repo = TestRepo::new("plumb-cas");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let main0 = repo.head();
        repo.commit_file("g.txt", "two\n", "c1");
        let main1 = repo.head();

        // Fresh old → advances (rewind main to main0).
        assert!(
            CliGit
                .update_ref_cas(&loc, "refs/heads/main", &main0, &main1)
                .unwrap()
        );
        assert_eq!(repo.out(&["rev-parse", "refs/heads/main"]), main0);

        // Stale old (ref already moved) → Ok(false), ref untouched.
        assert!(
            !CliGit
                .update_ref_cas(&loc, "refs/heads/main", &main1, &main1)
                .unwrap()
        );
        assert_eq!(repo.out(&["rev-parse", "refs/heads/main"]), main0);
    }

    #[test]
    fn snapshot_worktree_commits_dirty_and_noops_clean() {
        let repo = TestRepo::new("plumb-snap");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();

        // Clean → None.
        assert!(CliGit.snapshot_worktree(&loc, "snap").unwrap().is_none());

        // Dirty (tracked edit + new file) → a real commit folding both.
        std::fs::write(repo.dir.join("f.txt"), "edited\n").unwrap();
        std::fs::write(repo.dir.join("new.txt"), "n\n").unwrap();
        let tip = CliGit
            .snapshot_worktree(&loc, "snap dirty")
            .unwrap()
            .expect("dirty snapshot");
        assert_eq!(tip, repo.head());
        assert!(!CliGit.is_dirty(&loc).unwrap());
        let files = repo.out(&["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(files.contains("new.txt"), "{files}");
    }
}
