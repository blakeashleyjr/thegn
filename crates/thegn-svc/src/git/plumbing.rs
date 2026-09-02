//! Low-level git plumbing for the local merge queue ("fold-actor"): object-DB
//! merges and atomic ref updates that touch no working tree. `merge-tree`
//! computes a real 3-way merge and writes the result tree directly to the object
//! database, so N worktree branches can be folded against a moving `main` tip
//! entirely in the object DB — no checkout, no shared index contention.
//!
//! These are the seams `thegn-core::fold` drives through a thin adapter; the
//! fold *algorithm* lives in core (pure, gated tests), the *I/O* lives here.

use super::{GitBackend, gpg_args, run, run_stdin, run_w};
use anyhow::{Context, Result};
use thegn_core::fold::{Author, CommitMeta};
use thegn_core::remote::GitLoc;

/// Record/field separators for the `commits` log format — ASCII RS/US, which
/// never appear in real commit metadata, so they parse a multi-commit `git log`
/// with embedded newlines unambiguously.
const RS: char = '\u{1e}';
const US: char = '\u{1f}';

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
    // Bound the local path so a ref lock held by a crashed process or a hung
    // NFS mount can't hang the merge-queue fold actor forever (merge-tree /
    // update-ref are local object-DB/ref ops that should finish in ms). The
    // exit-code-as-data semantics are unaffected: `output_bounded` only errors on
    // spawn failure or timeout, never on a non-zero exit.
    let out = super::output_bounded(loc.git_command(args), args)
        .with_context(|| format!("git {}", args.join(" ")))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// A git object id is 40 (SHA-1) or 64 (SHA-256) hex chars. Validate a parsed
/// `merge-tree` tree oid before handing it to `commit-tree`: a truncated or
/// garbled output (lossy SSH/bridge transport) would otherwise flow downstream
/// as a bogus tree, failing deep inside `commit-tree` with an opaque "not a
/// valid object" and potentially leaving the fold wedged. Reject it loudly here.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parse a `git merge-tree --write-tree --name-only -z` result. Output sections
/// are NUL-separated: `<tree-oid>` then (on conflict) the conflicted filenames,
/// then an empty record before informational messages. Exit 0 = clean, 1 =
/// conflicts, >1 = genuine failure. Shared by [`PlumbingOps::merge_tree`] and
/// [`PlumbingOps::merge_tree_base`].
fn parse_merge_tree(code: i32, stdout: &str, stderr: &str) -> Result<MergeTreeOutcome> {
    let mut parts = stdout.split('\0');
    let tree = parts.next().unwrap_or("").trim().to_string();
    match code {
        0 => {
            if !is_object_id(&tree) {
                anyhow::bail!(
                    "merge-tree: invalid tree oid {tree:?} (stderr: {})",
                    stderr.trim()
                );
            }
            Ok(MergeTreeOutcome::Clean { tree })
        }
        1 => {
            if !is_object_id(&tree) {
                anyhow::bail!(
                    "merge-tree: invalid tree oid {tree:?} on conflict (stderr: {})",
                    stderr.trim()
                );
            }
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
        parse_merge_tree(code, &stdout, &stderr)
    }

    /// Resolve the gitlink-specific metadata for paths returned by a
    /// conflicted merge-tree result. Keeping this forwarding seam here lets
    /// the fold host enrich a conflict without teaching core how to run git.
    fn submodule_conflicts_for_paths(
        &self,
        loc: &GitLoc,
        paths: &[String],
        base: Option<&str>,
        ours: &str,
        theirs: &str,
    ) -> Result<Vec<thegn_core::submodule::SubmoduleConflict>> {
        GitBackend::submodule_conflicts(self, loc, paths, base, ours, theirs)
    }

    /// Create a commit object from an existing tree (`git commit-tree`). The
    /// message rides stdin to dodge arg-length/quoting limits. Unsigned, ambient
    /// author — the historical default; `merge`/`squash`/`rebase` land through
    /// [`commit_tree_opts`](Self::commit_tree_opts).
    fn commit_tree(&self, loc: &GitLoc, tree: &str, parents: &[&str], msg: &str) -> Result<String> {
        self.commit_tree_opts(loc, tree, parents, msg, false, None)
    }

    /// [`commit_tree`](Self::commit_tree) with two policy knobs:
    ///
    /// - `sign` ⇒ pass `-S` (honoring the active identity's `gpg.format` /
    ///   `user.signingkey`). Signing is **non-interactive**: `run_stdin` sets
    ///   `GIT_TERMINAL_PROMPT=0` and a null stdin, so a gpg/ssh-agent that would
    ///   prompt fails fast rather than hanging the daemon fold — the caller
    ///   classifies that failure as infrastructure, never a bad branch.
    /// - `author` ⇒ preserve the original author (name/email/date) while the
    ///   committer stays the ambient identity — how a `rebase` land keeps
    ///   authorship, exactly like `git rebase`.
    fn commit_tree_opts(
        &self,
        loc: &GitLoc,
        tree: &str,
        parents: &[&str],
        msg: &str,
        sign: bool,
        author: Option<&Author>,
    ) -> Result<String> {
        let mut args: Vec<&str> = vec!["commit-tree", tree];
        for p in parents {
            args.push("-p");
            args.push(p);
        }
        if sign {
            args.push("-S");
        }
        let mut env: Vec<(&str, &str)> = Vec::new();
        if let Some(a) = author {
            if !a.name.is_empty() {
                env.push(("GIT_AUTHOR_NAME", a.name.as_str()));
            }
            if !a.email.is_empty() {
                env.push(("GIT_AUTHOR_EMAIL", a.email.as_str()));
            }
            if !a.date.is_empty() {
                env.push(("GIT_AUTHOR_DATE", a.date.as_str()));
            }
        }
        Ok(run_stdin(loc, &env, &args, msg.as_bytes())?
            .trim()
            .to_string())
    }

    /// [`merge_tree`](Self::merge_tree) with an **explicit** merge base
    /// (`--merge-base <base>`): a plumbing cherry-pick that replays exactly
    /// `theirs`'s delta (`base..theirs`) onto `ours`. Powers the `rebase` land.
    fn merge_tree_base(
        &self,
        loc: &GitLoc,
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<MergeTreeOutcome> {
        let (code, stdout, stderr) = run_status(
            loc,
            &[
                "merge-tree",
                "--write-tree",
                "--merge-base",
                base,
                "--name-only",
                "-z",
                ours,
                theirs,
            ],
        )?;
        parse_merge_tree(code, &stdout, &stderr)
    }

    /// Merge base of two commits (`git merge-base`). `Ok(None)` when they are
    /// unrelated (exit 1 with no output), distinct from a genuine failure.
    fn merge_base(&self, loc: &GitLoc, a: &str, b: &str) -> Result<Option<String>> {
        let (code, stdout, stderr) = run_status(loc, &["merge-base", a, b])?;
        match code {
            0 => {
                let oid = stdout.trim().to_string();
                Ok(is_object_id(&oid).then_some(oid))
            }
            1 => Ok(None),
            _ => anyhow::bail!("git merge-base failed: {}", stderr.trim()),
        }
    }

    /// Commits in `base_excl..tip`, ancestor-first (`git log --reverse`), each
    /// with its first parent, full message, and author — the replay list for a
    /// `rebase` land and the `{subjects}` source for a `land_message`.
    fn commits(&self, loc: &GitLoc, base_excl: &str, tip: &str) -> Result<Vec<CommitMeta>> {
        let fmt = format!("{RS}%H{US}%P{US}%an{US}%ae{US}%aI{US}%B");
        let range = format!("{base_excl}..{tip}");
        let out = run(
            loc,
            &["log", "--reverse", &format!("--format={fmt}"), &range],
        )?;
        let mut commits = Vec::new();
        for rec in out.split(RS) {
            if rec.trim().is_empty() {
                continue;
            }
            let mut f = rec.splitn(6, US);
            let oid = f.next().unwrap_or("").trim().to_string();
            let parents = f.next().unwrap_or("");
            let name = f.next().unwrap_or("").to_string();
            let email = f.next().unwrap_or("").to_string();
            let date = f.next().unwrap_or("").trim().to_string();
            let message = f.next().unwrap_or("").trim_start_matches('\n').to_string();
            if oid.is_empty() {
                continue;
            }
            // First parent is the replay base for this commit's delta.
            let parent = parents.split_whitespace().next().unwrap_or("").to_string();
            commits.push(CommitMeta {
                oid,
                parent,
                message,
                author: Author { name, email, date },
            });
        }
        Ok(commits)
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
    ///
    /// `override_gpg` threads `[git] override_gpg` through: a background snapshot
    /// commit inheriting an ambient `commit.gpgSign = true` would otherwise stall
    /// on a pinentry prompt (the fold-actor runs headless). With it set, the
    /// commit is created with signing disabled via `-c commit.gpgSign=false`,
    /// exactly like every other background history operation.
    fn snapshot_worktree(
        &self,
        loc: &GitLoc,
        msg: &str,
        override_gpg: bool,
    ) -> Result<Option<String>> {
        if !self.is_dirty(loc)? {
            return Ok(None);
        }
        run_w(loc, &[], &["add", "-A"])?;
        // `-c commit.gpgSign=false …` must precede the subcommand — prepend it.
        let mut args: Vec<&str> = gpg_args(override_gpg).to_vec();
        args.extend_from_slice(&["commit", "--no-verify", "-F", "-"]);
        run_stdin(loc, &[("GIT_EDITOR", ":")], &args, msg.as_bytes())?;
        Ok(Some(self.rev_parse(loc, "HEAD")?))
    }
}

impl<T: GitBackend + ?Sized> PlumbingOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend};
    use super::{MergeTreeOutcome, PlumbingOps, is_object_id};
    use std::path::Path;

    #[test]
    fn is_object_id_accepts_sha1_and_sha256_only() {
        assert!(is_object_id(&"a".repeat(40))); // SHA-1
        assert!(is_object_id(&"0".repeat(64))); // SHA-256
        assert!(!is_object_id("")); // empty (truncated output)
        assert!(!is_object_id(&"a".repeat(39))); // one short
        assert!(!is_object_id(&"a".repeat(41))); // one long
        assert!(!is_object_id(&"g".repeat(40))); // non-hex
        assert!(!is_object_id("Already up to date.")); // stray informational line
    }

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
        assert!(
            CliGit
                .snapshot_worktree(&loc, "snap", false)
                .unwrap()
                .is_none()
        );

        // Dirty (tracked edit + new file) → a real commit folding both.
        std::fs::write(repo.dir.join("f.txt"), "edited\n").unwrap();
        std::fs::write(repo.dir.join("new.txt"), "n\n").unwrap();
        let tip = CliGit
            .snapshot_worktree(&loc, "snap dirty", false)
            .unwrap()
            .expect("dirty snapshot");
        assert_eq!(tip, repo.head());
        assert!(!CliGit.is_dirty(&loc).unwrap());
        let files = repo.out(&["ls-tree", "-r", "--name-only", "HEAD"]);
        assert!(files.contains("new.txt"), "{files}");
    }

    /// Regression: a snapshot commit under an ambient `commit.gpgSign = true` must
    /// not stall on a signer. With `[git] override_gpg = true` the snapshot skips
    /// signing entirely (`-c commit.gpgSign=false`) and completes unsigned; the
    /// deliberately-failing signer (`gpg.program = false`, which exits fast rather
    /// than prompting so the test can never hang) proves the signing path is not
    /// taken. Without the override the same setup fails — the exact hazard the fix
    /// prevents (a real gpg-agent would block on a pinentry instead of failing).
    #[test]
    fn snapshot_worktree_override_gpg_skips_signing_and_cannot_hang() {
        let repo = TestRepo::new("plumb-snap-gpg");
        git_in(&repo.dir, &["config", "user.name", "t"]);
        git_in(&repo.dir, &["config", "user.email", "t@e"]);
        git_in(&repo.dir, &["config", "commit.gpgsign", "false"]);
        let loc = repo.loc();
        repo.commit_file("f.txt", "one\n", "c0"); // base, unsigned

        // Turn on ambient signing, but point the signer at a program that fails
        // immediately — a stand-in for the pinentry that would hang a headless op.
        git_in(&repo.dir, &["config", "commit.gpgsign", "true"]);
        git_in(&repo.dir, &["config", "gpg.program", "false"]);

        // override_gpg = true ⇒ signing disabled ⇒ the commit succeeds, unsigned.
        std::fs::write(repo.dir.join("f.txt"), "edited\n").unwrap();
        let tip = CliGit
            .snapshot_worktree(&loc, "snap gpg", true)
            .unwrap()
            .expect("dirty snapshot");
        assert_eq!(tip, repo.head());
        let raw = repo.out(&["cat-file", "-p", "HEAD"]);
        assert!(
            !raw.contains("gpgsig"),
            "override_gpg must produce an unsigned snapshot: {raw}"
        );

        // Without the override, the ambient gpgsign makes the very same snapshot
        // fail (a real agent would hang instead) — this is what the fix guards.
        std::fs::write(repo.dir.join("f.txt"), "edited-again\n").unwrap();
        assert!(
            CliGit.snapshot_worktree(&loc, "snap gpg 2", false).is_err(),
            "without override_gpg, an ambient commit.gpgSign snapshots must not succeed silently"
        );
    }
}
