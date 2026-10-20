//! Low-level git plumbing for the local merge queue ("fold-actor"): object-DB
//! merges and atomic ref updates that touch no working tree. `merge-tree`
//! computes a real 3-way merge and writes the result tree directly to the object
//! database, so N worktree branches can be folded against a moving `main` tip
//! entirely in the object DB — no checkout, no shared index contention.
//!
//! These are the seams `thegn-core::fold` drives through a thin adapter; the
//! fold *algorithm* lives in core (pure, gated tests), the *I/O* lives here.

use super::{GitBackend, gpg_args, run, run_background_commit_stdin, run_w};
use anyhow::{Context, Result};
use thegn_core::fold::{Author, CommitMeta};
use thegn_core::git_operand as op;
use thegn_core::remote::GitLoc;

/// Outcome of `git merge-tree --write-tree`. Both arms carry the written tree
/// oid — git writes the (conflict-marked) tree even when conflicts occur, but
/// the fold engine only commits the `Clean` ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeTreeOutcome {
    Clean { tree: String },
    Conflict { tree: String, paths: Vec<String> },
}

/// Result of a CAS `update-ref` that exited non-zero, after re-reading the ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasOutcome {
    Advanced,
    /// The ref exists but no longer points at the expected old oid.
    Moved {
        actual: String,
    },
    /// A source ref in a fenced update no longer names its captured object.
    SourceMoved {
        name: String,
        actual: String,
    },
    /// A source ref in a fenced update disappeared.
    SourceMissing {
        name: String,
    },
    /// The ref does not exist.
    Missing,
    /// Unclassifiable (lock error, unreadable ref, still at `old`, ...). Callers
    /// must fail closed.
    Other(String),
}

/// What re-reading the ref after a failed CAS showed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RefRead {
    At(String),
    Missing,
    Unknown,
}

/// Pure classification of a failed CAS from the ref re-read. Only a ref that
/// provably points somewhere other than `old` is `Moved`; if it still points at
/// `old` the failure was something else (lock, permissions) and is `Other`.
fn classify_cas_failure(reread: &RefRead, old: &str, stderr: &str) -> CasOutcome {
    match reread {
        RefRead::At(actual) if is_object_id(actual) && actual != old => CasOutcome::Moved {
            actual: actual.clone(),
        },
        RefRead::Missing => CasOutcome::Missing,
        _ => CasOutcome::Other(stderr.to_string()),
    }
}

/// Host-automatic fold plumbing never runs repository hooks or fsmonitor;
/// local only, like the read fence. User-invoked commits keep their hooks.
fn fenced<'a>(loc: &GitLoc, args: &[&'a str]) -> Vec<&'a str> {
    super::read_argv(loc, args)
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
    let out = super::output_bounded(loc.git_command(&fenced(loc, args)), args)
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

/// Parse a command that must return exactly one object id, optionally followed
/// by Git's conventional final line feed.
fn parse_object_id_output(stdout: &str, command: &str) -> Result<String> {
    let oid = stdout.strip_suffix('\n').unwrap_or(stdout);
    if !is_object_id(oid) {
        anyhow::bail!("git {command} returned invalid object id output: {stdout:?}");
    }
    Ok(oid.to_string())
}

/// Parse `git log -z`'s six NUL-separated commit fields. The final NUL is the
/// record terminator; requiring it makes a truncated final record an error.
/// Repository-controlled strings can contain every other byte without moving
/// a field boundary.
fn parse_commit_metas(out: &str) -> Result<Vec<CommitMeta>> {
    if out.is_empty() {
        return Ok(Vec::new());
    }
    if !out.ends_with('\0') {
        anyhow::bail!("git log commit metadata is missing its final NUL record terminator");
    }
    let mut fields: Vec<&str> = out.split('\0').collect();
    fields.pop();
    if !fields.len().is_multiple_of(6) {
        anyhow::bail!(
            "git log commit metadata has {} fields, expected a multiple of 6",
            fields.len()
        );
    }

    let mut commits = Vec::with_capacity(fields.len() / 6);
    let mut seen = std::collections::HashSet::with_capacity(fields.len() / 6);
    for record in fields.chunks_exact(6) {
        let oid = record[0];
        if !is_object_id(oid) {
            anyhow::bail!("git log returned invalid commit object id: {oid:?}");
        }
        if !seen.insert(oid) {
            anyhow::bail!("git log returned duplicate commit object id: {oid}");
        }
        let parents = record[1];
        let parent_ids: Vec<&str> = parents.split_ascii_whitespace().collect();
        if parent_ids.iter().any(|p| !is_object_id(p)) {
            anyhow::bail!("git log returned invalid parent object ids for {oid}");
        }
        let parent = parent_ids.first().copied().unwrap_or("");
        let date = record[4];
        if date.is_empty() || date.bytes().any(|byte| byte.is_ascii_whitespace()) {
            anyhow::bail!("git log returned invalid author date for {oid}: {date:?}");
        }
        commits.push(CommitMeta {
            oid: oid.to_string(),
            parent: parent.to_string(),
            parents: parent_ids
                .iter()
                .map(|parent| (*parent).to_string())
                .collect(),
            // `-z` replaces the usual pretty-format record newline with NUL.
            // Restore it so this matches `git log -1 --format=%B <oid>`.
            message: format!("{}\n", record[5]),
            author: Author {
                name: record[2].to_string(),
                email: record[3].to_string(),
                date: date.to_string(),
            },
        });
    }
    Ok(commits)
}

/// Parse `git merge-base`'s status-coded result. Exit 1 means no common
/// ancestor; successful but malformed output is protocol corruption.
fn parse_merge_base(code: i32, stdout: &str, stderr: &str) -> Result<Option<String>> {
    match code {
        0 => parse_object_id_output(stdout, "merge-base").map(Some),
        1 => Ok(None),
        _ => anyhow::bail!("git merge-base failed: {}", stderr.trim()),
    }
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
        let rev = op::revision(rev)?;
        parse_object_id_output(&run(loc, &["rev-parse", rev])?, "rev-parse")
    }

    /// Fold `theirs` onto `ours` in the object DB. `--write-tree` (git ≥ 2.38)
    /// finds the merge base itself, performs a real 3-way merge, and writes the
    /// result tree — nothing else is touched. `--name-only -z` makes the
    /// conflicted-file section a NUL-delimited path list (robust to spaces).
    /// Exit 0 = clean, 1 = conflicts, >1 = genuine failure.
    fn merge_tree(&self, loc: &GitLoc, ours: &str, theirs: &str) -> Result<MergeTreeOutcome> {
        self.merge_tree_with_config(loc, &[], ours, theirs)
    }

    /// Fold with explicit Git config overrides placed before the command.
    fn merge_tree_with_config(
        &self,
        loc: &GitLoc,
        config: &[String],
        ours: &str,
        theirs: &str,
    ) -> Result<MergeTreeOutcome> {
        let (ours, theirs) = (op::revision(ours)?, op::revision(theirs)?);
        let mut args = Vec::with_capacity(config.len() * 2 + 6);
        for item in config {
            args.push("-c".to_string());
            args.push(item.clone());
        }
        args.extend([
            "merge-tree".into(),
            "--write-tree".into(),
            "--name-only".into(),
            "-z".into(),
            ours.into(),
            theirs.into(),
        ]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, stdout, stderr) = run_status(loc, &refs)?;
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
    ///   `user.signingkey`). Signing is **non-interactive**: the dedicated
    ///   background-commit runner closes the message pipe, exposes no tty /
    ///   askpass/display surface, and bounds the operation. It reaps Git on all
    ///   platforms and kills its isolated process group on Unix. The caller
    ///   classifies failure as infrastructure, never a bad branch.
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
        let tree = op::revision(tree)?;
        let mut args: Vec<&str> = vec!["commit-tree", tree];
        for p in parents {
            op::revision(p)?;
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
        parse_object_id_output(
            &run_background_commit_stdin(loc, &env, &args, msg.as_bytes())?,
            "commit-tree",
        )
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
        self.merge_tree_base_with_config(loc, &[], base, ours, theirs)
    }

    fn merge_tree_base_with_config(
        &self,
        loc: &GitLoc,
        config: &[String],
        base: &str,
        ours: &str,
        theirs: &str,
    ) -> Result<MergeTreeOutcome> {
        let (base, ours, theirs) = (
            op::revision(base)?,
            op::revision(ours)?,
            op::revision(theirs)?,
        );
        let mut args = Vec::with_capacity(config.len() * 2 + 8);
        for item in config {
            args.push("-c".to_string());
            args.push(item.clone());
        }
        args.extend([
            "merge-tree".into(),
            "--write-tree".into(),
            "--merge-base".into(),
            base.into(),
            "--name-only".into(),
            "-z".into(),
            ours.into(),
            theirs.into(),
        ]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, stdout, stderr) = run_status(loc, &refs)?;
        parse_merge_tree(code, &stdout, &stderr)
    }

    /// Merge base of two commits (`git merge-base`). `Ok(None)` when they are
    /// unrelated (exit 1 with no output), distinct from a genuine failure.
    fn merge_base(&self, loc: &GitLoc, a: &str, b: &str) -> Result<Option<String>> {
        let (a, b) = (op::revision(a)?, op::revision(b)?);
        let (code, stdout, stderr) = run_status(loc, &["merge-base", a, b])?;
        parse_merge_base(code, &stdout, &stderr)
    }

    /// Commits in `base_excl..tip`, ancestor-first (`git log --reverse`), each
    /// with its first parent, full message, and author — the replay list for a
    /// `rebase` land and the `{subjects}` source for a `land_message`.
    fn commits(&self, loc: &GitLoc, base_excl: &str, tip: &str) -> Result<Vec<CommitMeta>> {
        let (base_excl, tip) = (op::revision(base_excl)?, op::revision(tip)?);
        let fmt = "%H%x00%P%x00%an%x00%ae%x00%aI%x00%B";
        let range = format!("{base_excl}..{tip}");
        let out = run(
            loc,
            &["log", "--reverse", "-z", &format!("--format={fmt}"), &range],
        )?;
        parse_commit_metas(&out)
    }

    /// Atomically advance a (fully-qualified) ref only if it still points at
    /// `old` — `git update-ref <ref> <new> <old>`. A mismatch means `main` moved
    /// under the fold; that's a normal "re-fold" signal returned as `Ok(false)`,
    /// distinct from a genuine lock/ref error which is `Err`. Failure is
    /// classified by re-reading the ref ([`PlumbingOps::update_ref_cas_outcome`]),
    /// never by matching git's locale/version-dependent stderr; anything that is
    /// not provably "moved" fails closed as `Err`.
    fn update_ref_cas(&self, loc: &GitLoc, name: &str, new: &str, old: &str) -> Result<bool> {
        match self.update_ref_cas_outcome(loc, name, new, old)? {
            CasOutcome::Advanced => Ok(true),
            CasOutcome::Moved { .. } => Ok(false),
            CasOutcome::SourceMoved { .. } => Ok(false),
            CasOutcome::SourceMissing { name } => {
                anyhow::bail!("git update-ref source {name} failed: ref no longer exists")
            }
            CasOutcome::Missing => {
                anyhow::bail!("git update-ref {name} failed: ref no longer exists")
            }
            CasOutcome::Other(msg) => anyhow::bail!("git update-ref {name} failed: {msg}"),
        }
    }

    /// Typed form of [`PlumbingOps::update_ref_cas`]. On a non-zero exit the ref
    /// is re-read (`rev-parse --verify --quiet`, exit-code based) to tell a lost
    /// race from a missing ref or an unrelated failure.
    fn update_ref_cas_outcome(
        &self,
        loc: &GitLoc,
        name: &str,
        new: &str,
        old: &str,
    ) -> Result<CasOutcome> {
        let (name, new, old) = (op::revision(name)?, op::revision(new)?, op::revision(old)?);
        let (code, _out, stderr) = run_status(loc, &["update-ref", name, new, old])?;
        if code == 0 {
            return Ok(CasOutcome::Advanced);
        }
        let reread = match run_status(loc, &["rev-parse", "--verify", "--quiet", name]) {
            Ok((0, out, _)) => RefRead::At(out.trim().to_string()),
            Ok((1, _, _)) => RefRead::Missing,
            _ => RefRead::Unknown,
        };
        Ok(classify_cas_failure(&reread, old, stderr.trim()))
    }

    /// Verify all captured source refs and advance the target in one Git ref
    /// transaction. Git commits none of the updates if any verify fails.
    fn update_ref_cas_with_sources(
        &self,
        loc: &GitLoc,
        target: &str,
        new: &str,
        old: &str,
        sources: &[(String, String)],
    ) -> Result<CasOutcome> {
        let target = op::revision(target)?;
        let new = op::revision(new)?;
        let old = op::revision(old)?;
        let mut transaction = String::from("start\n");
        for (name, oid) in sources {
            let name = op::revision(name)?;
            let oid = op::revision(oid)?;
            transaction.push_str(&format!("verify {name} {oid}\n"));
        }
        transaction.push_str(&format!("update {target} {new} {old}\nprepare\ncommit\n"));
        let args = fenced(loc, &["update-ref", "--stdin"]);
        match super::output_bounded_with_input(
            loc.git_command(&args),
            &["update-ref", "--stdin"],
            Some(transaction.as_bytes()),
            super::git_read_timeout(),
        )
        .map(|output| {
            if !output.status.success() {
                anyhow::bail!(
                    "git update-ref --stdin failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        })
        .and_then(|result| result)
        {
            Ok(_) => Ok(CasOutcome::Advanced),
            Err(error) => {
                // The transaction error itself is locale-dependent; classify
                // only by exact ref reads and fail closed otherwise.
                for (name, expected) in sources {
                    let name = op::revision(name)?;
                    let expected = op::revision(expected)?;
                    match run_status(loc, &["rev-parse", "--verify", "--quiet", name]) {
                        Ok((0, actual, _))
                            if is_object_id(actual.trim()) && actual.trim() != expected =>
                        {
                            return Ok(CasOutcome::SourceMoved {
                                name: name.to_owned(),
                                actual: actual.trim().to_owned(),
                            });
                        }
                        Ok((1, _, _)) => {
                            return Ok(CasOutcome::SourceMissing {
                                name: name.to_owned(),
                            });
                        }
                        _ => {}
                    }
                }
                match run_status(loc, &["rev-parse", "--verify", "--quiet", target]) {
                    Ok((0, actual, _)) if is_object_id(actual.trim()) && actual.trim() != old => {
                        Ok(CasOutcome::Moved {
                            actual: actual.trim().to_owned(),
                        })
                    }
                    Ok((1, _, _)) => Ok(CasOutcome::Missing),
                    _ => Ok(CasOutcome::Other(format!("{error:#}"))),
                }
            }
        }
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
        let index_tree = run_w(loc, &[], &fenced(loc, &["write-tree"])).context(
            "could not capture the index tree before snapshot; refusing to stage changes",
        )?;
        let index_tree = index_tree.trim();
        run_w(loc, &[], &fenced(loc, &["add", "-A"]))?;
        // `-c commit.gpgSign=false …` must precede the subcommand — prepend it.
        let mut args: Vec<&str> = gpg_args(override_gpg).to_vec();
        args.extend(fenced(loc, &["commit", "--no-verify", "-F", "-"]));
        if let Err(commit_error) =
            run_background_commit_stdin(loc, &[("GIT_EDITOR", ":")], &args, msg.as_bytes())
        {
            return match run_w(loc, &[], &fenced(loc, &["read-tree", index_tree])) {
                Ok(_) => Err(commit_error),
                Err(restore_error) => Err(commit_error.context(format!(
                    "restoring pre-snapshot index tree {index_tree} also failed ({restore_error:#}); the index now has every change staged"
                ))),
            };
        }
        Ok(Some(self.rev_parse(loc, "HEAD")?))
    }
}

impl<T: GitBackend + ?Sized> PlumbingOps for T {}

#[cfg(test)]
mod tests {
    use super::super::testutil::{TestRepo, git_in};
    use super::super::{CliGit, GitBackend};
    use super::{
        CasOutcome, MergeTreeOutcome, PlumbingOps, RefRead, classify_cas_failure, fenced,
        is_object_id, parse_commit_metas, parse_merge_base,
    };
    use std::path::Path;
    use std::process::Command;
    use thegn_core::fold::{Author, CommitMeta};
    use thegn_core::remote::GitLoc;

    #[test]
    fn plumbing_refuses_option_shaped_operands_before_spawn() {
        let repo = TestRepo::new("pl-dash");
        let loc = repo.loc();
        let bad = "--output=/tmp/thegn-injected";
        assert!(CliGit.rev_parse(&loc, bad).is_err());
        assert!(CliGit.merge_base(&loc, bad, "HEAD").is_err());
        assert!(CliGit.merge_tree(&loc, "HEAD", bad).is_err());
        assert!(CliGit.merge_tree_base(&loc, bad, "HEAD", "HEAD").is_err());
        assert!(CliGit.commits(&loc, "HEAD", bad).is_err());
        assert!(CliGit.update_ref_cas(&loc, bad, "HEAD", "HEAD").is_err());
        assert!(CliGit.commit_tree(&loc, bad, &[], "m").is_err());
        assert!(CliGit.commit_tree(&loc, "HEAD", &[bad], "m").is_err());
        assert!(!std::path::Path::new("/tmp/thegn-injected").exists());
    }

    #[test]
    fn merge_tree_with_config_passes_driver_refusal_overrides() {
        let repo = TestRepo::new("pl-merge-driver-refusal");
        ident(&repo.dir);
        repo.commit_file(".gitattributes", "data.txt merge=counter\n", "attrs");
        repo.commit_file("data.txt", "base\n", "seed");
        git_in(&repo.dir, &["checkout", "-q", "-b", "feature"]);
        repo.commit_file("data.txt", "feature\n", "feature edit");
        let theirs = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("data.txt", "main\n", "main edit");
        let ours = repo.head();
        let counter = repo.dir.join("driver-count");
        git_in(
            &repo.dir,
            &[
                "config",
                "merge.counter.driver",
                &format!(
                    "printf x >> {}; true %O %A %B %L %P",
                    thegn_core::util::sh_quote(&counter.to_string_lossy())
                ),
            ],
        );
        let result = CliGit
            .merge_tree_with_config(
                &repo.loc(),
                &["merge.counter.driver=false".into()],
                &ours,
                &theirs,
            )
            .unwrap();
        assert!(
            matches!(result, MergeTreeOutcome::Conflict { .. }),
            "{result:?}"
        );
        assert!(
            !counter.exists(),
            "configured custom driver ran despite override"
        );
    }

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

    fn install_script(path: &Path, marker: &Path) -> bool {
        let script = format!(
            "#!/bin/sh\nprintf x >> {}\nprintf 'token\\n'\n",
            thegn_core::util::sh_quote(&marker.to_string_lossy())
        );
        std::fs::write(path, script).unwrap();
        let status = match Command::new("chmod").arg("+x").arg(path).status() {
            Ok(status) => status,
            Err(_) => return false,
        };
        assert!(status.success(), "chmod failed for {}", path.display());
        true
    }

    fn remove_marker(path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove {}: {error}", path.display()),
        }
    }

    #[test]
    fn fenced_argv_is_local_only() {
        let local = GitLoc::Local(Path::new("/repo").to_path_buf());
        let args = ["update-ref", "--stdin"];
        assert_eq!(
            fenced(&local, &args),
            [
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "update-ref",
                "--stdin"
            ]
        );

        let provider = GitLoc::provider(vec!["sh".into()], "/remote/repo");
        assert_eq!(fenced(&provider, &args), args);
    }

    #[test]
    fn update_ref_cas_never_runs_reference_transaction_hook() {
        let repo = TestRepo::new("plumb-ref-hook");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let new = repo.head();
        repo.commit_file("g.txt", "two\n", "c1");
        let old = repo.head();
        let loc = repo.loc();
        let hook = repo.dir.join(".git/hooks/reference-transaction");
        let marker = repo.dir.join("rt-fired");
        if !install_script(&hook, &marker) {
            return;
        }

        git_in(&repo.dir, &["update-ref", "refs/heads/probe", "HEAD"]);
        assert!(marker.exists(), "plain update-ref is the positive control");
        remove_marker(&marker);

        assert!(
            CliGit
                .update_ref_cas(&loc, "refs/heads/main", &new, &old)
                .unwrap()
        );
        assert_eq!(repo.out(&["rev-parse", "refs/heads/main"]), new);
        assert!(!marker.exists(), "fenced update-ref ran the reference hook");
    }

    #[test]
    fn update_ref_cas_with_sources_never_runs_reference_transaction_hook() {
        let repo = TestRepo::new("plumb-ref-sources-hook");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let source_oid = repo.head();
        git_in(&repo.dir, &["branch", "side"]);
        repo.commit_file("g.txt", "two\n", "c1");
        let old = repo.head();
        let loc = repo.loc();
        let hook = repo.dir.join(".git/hooks/reference-transaction");
        let marker = repo.dir.join("rt-fired");
        if !install_script(&hook, &marker) {
            return;
        }

        git_in(&repo.dir, &["update-ref", "refs/heads/probe", "HEAD"]);
        assert!(marker.exists(), "plain update-ref is the positive control");
        remove_marker(&marker);

        let outcome = CliGit
            .update_ref_cas_with_sources(
                &loc,
                "refs/heads/main",
                &source_oid,
                &old,
                &[("refs/heads/side".into(), source_oid.clone())],
            )
            .unwrap();
        assert_eq!(outcome, CasOutcome::Advanced);
        assert_eq!(repo.out(&["rev-parse", "refs/heads/main"]), source_oid);
        assert!(
            !marker.exists(),
            "fenced ref transaction ran the reference hook"
        );
    }

    #[test]
    fn snapshot_worktree_never_runs_post_commit_or_fsmonitor() {
        let repo = TestRepo::new("plumb-snapshot-hook");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let hook_marker = repo.dir.join("pc-fired");
        let fsmonitor_marker = repo.dir.join("fsm-fired");
        let hook = repo.dir.join(".git/hooks/post-commit");
        if !install_script(&hook, &hook_marker) {
            return;
        }
        let fsmonitor_script = repo.dir.join("fsmonitor.sh");
        if !install_script(&fsmonitor_script, &fsmonitor_marker) {
            return;
        }

        git_in(
            &repo.dir,
            &[
                "config",
                "core.fsmonitor",
                &format!(
                    "sh {}",
                    thegn_core::util::sh_quote(&fsmonitor_script.to_string_lossy())
                ),
            ],
        );
        git_in(&repo.dir, &["status", "--short"]);
        assert!(
            fsmonitor_marker.exists(),
            "plain status is the fsmonitor positive control"
        );
        remove_marker(&fsmonitor_marker);
        git_in(
            &repo.dir,
            &["commit", "--allow-empty", "-q", "-m", "control"],
        );
        assert!(
            hook_marker.exists(),
            "plain commit is the post-commit positive control"
        );
        remove_marker(&hook_marker);
        remove_marker(&fsmonitor_marker);

        std::fs::write(repo.dir.join("f.txt"), "edited\n").unwrap();
        let snapshot = CliGit
            .snapshot_worktree(&loc, "snap", true)
            .unwrap()
            .expect("dirty snapshot commit");
        assert_eq!(repo.head(), snapshot);
        assert!(!hook_marker.exists(), "snapshot ran the post-commit hook");
        assert!(
            !fsmonitor_marker.exists(),
            "snapshot ran the fsmonitor command"
        );
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
    fn rev_parse_rejects_multiple_successful_records() {
        let repo = TestRepo::new("plumb-revparse-range");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        repo.commit_file("g.txt", "two\n", "c1");
        let loc = repo.loc();
        assert!(CliGit.rev_parse(&loc, "HEAD~1..HEAD").is_err());
    }

    #[test]
    fn commits_nul_framing_contains_hostile_metadata_to_its_record() {
        let repo = TestRepo::new("plumb-commits-nul");
        ident(&repo.dir);
        repo.commit_file("base.txt", "base\n", "base");
        let base = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "-b", "main-side"]);
        repo.commit_file("main.txt", "main\n", "unrelated main commit");
        let unrelated = repo.head();
        git_in(&repo.dir, &["checkout", "-q", &base]);
        git_in(&repo.dir, &["checkout", "-q", "-b", "feature"]);

        let name = "author\u{1f}name";
        git_in(&repo.dir, &["config", "user.name", name]);
        let message = format!(
            "subject\n\nx\u{1e}{unrelated}\u{1f}{base}\u{1f}forged\u{1f}forged@example.test\u{1f}2020-01-01T00:00:00+00:00\u{1f}forged-message"
        );
        std::fs::write(repo.dir.join("feature.txt"), "feature\n").unwrap();
        git_in(&repo.dir, &["add", "feature.txt"]);
        let output = thegn_core::util::git_cmd(&repo.dir)
            .args(["commit", "-q", "-m", &message])
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", "author@example.test")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tip = repo.head();
        let loc = repo.loc();
        let commits = CliGit.commits(&loc, &base, &tip).unwrap();

        assert_eq!(commits.len(), 1);
        let commit = &commits[0];
        assert_eq!(commit.oid, tip);
        assert_eq!(commit.parent, base);
        assert_eq!(commit.author.name, name);
        assert_eq!(commit.author.email, "author@example.test");
        assert_eq!(
            commit.author.date,
            repo.out(&["show", "-s", "--format=%aI", &tip])
        );
        assert!(!commits.iter().any(|commit| commit.oid == unrelated));

        let expected_message = thegn_core::util::git_cmd(&repo.dir)
            .args(["log", "-1", "--format=%B", &tip])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(expected_message.status.success());
        assert_eq!(
            commit.message,
            String::from_utf8_lossy(&expected_message.stdout).into_owned()
        );
    }

    #[test]
    fn commit_metadata_parser_is_all_or_error() {
        let repo = TestRepo::new("plumb-commit-parser");
        ident(&repo.dir);
        repo.commit_file("first.txt", "first\n", "first");
        let oid_a = repo.head();
        repo.commit_file("second.txt", "second\n", "second");
        let oid_b = repo.head();
        let date = repo.out(&["show", "-s", "--format=%aI", &oid_b]);
        let record = |oid: &str, parent: &str, name: &str, date: &str, message: &str| {
            [oid, parent, name, "author@example.test", date, message].join("\0") + "\0"
        };
        let valid_a = record(&oid_a, "", "first", &date, "one\u{1f}\u{1e}");
        let valid_b = record(&oid_b, &oid_a, "second", &date, "two");
        let parsed = parse_commit_metas(&(valid_a.clone() + &valid_b)).unwrap();
        assert_eq!(
            parsed,
            vec![
                CommitMeta {
                    oid: oid_a.clone(),
                    parent: String::new(),
                    parents: vec![],
                    message: "one\u{1f}\u{1e}\n".into(),
                    author: Author {
                        name: "first".into(),
                        email: "author@example.test".into(),
                        date: date.clone(),
                    },
                },
                CommitMeta {
                    oid: oid_b.clone(),
                    parent: oid_a.clone(),
                    parents: vec![oid_a.clone()],
                    message: "two\n".into(),
                    author: Author {
                        name: "second".into(),
                        email: "author@example.test".into(),
                        date: date.clone(),
                    },
                },
            ]
        );

        for malformed in [
            // A truncated final record has no NUL terminator.
            format!("{oid_a}\0\0name\0author@example.test\0{date}\0message"),
            // Wrong arity despite a terminator.
            format!("{oid_a}\0\0name\0author@example.test\0{date}\0"),
            record("bad-oid", "", "name", &date, "message"),
            record(&oid_a, "bad-parent", "name", &date, "message"),
            valid_a.clone() + &valid_a,
            record(&oid_a, "", "name", "", "message"),
        ] {
            assert!(parse_commit_metas(&malformed).is_err(), "{malformed:?}");
        }
    }

    #[test]
    fn commit_metas_carry_every_parent() {
        let repo = TestRepo::new("plumb-commit-parents");
        ident(&repo.dir);
        let commit_file = |path: &str, content: &str, message: &str| {
            std::fs::write(repo.dir.join(path), content).unwrap();
            git_in(&repo.dir, &["add", path]);
            git_in(
                &repo.dir,
                &["-c", "commit.gpgsign=false", "commit", "-q", "-m", message],
            );
        };
        commit_file("first.txt", "first\n", "root");
        let root = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "-b", "side"]);
        commit_file("side.txt", "side\n", "side");
        let side = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        commit_file("main.txt", "main\n", "main");
        let first_parent = repo.head();
        git_in(
            &repo.dir,
            &[
                "-c",
                "commit.gpgsign=false",
                "merge",
                "--no-ff",
                "-q",
                "-m",
                "merge side",
                "side",
            ],
        );
        let merge_oid = repo.head();
        let merge_parents = repo.out(&["rev-list", "--parents", "-n", "1", &merge_oid]);
        let mut merge_parent_iter = merge_parents.split_ascii_whitespace();
        assert_eq!(merge_parent_iter.next(), Some(merge_oid.as_str()));
        let parents = merge_parent_iter.collect::<Vec<_>>().join(" ");
        assert_eq!(
            parents.split_ascii_whitespace().collect::<Vec<_>>(),
            [first_parent.as_str(), side.as_str()]
        );
        let date = repo.out(&["show", "-s", "--format=%aI", &merge_oid]);
        let record = |oid: &str, parents: &str| {
            [
                oid,
                parents,
                "merge",
                "author@example.test",
                &date,
                "message",
            ]
            .join("\0")
                + "\0"
        };
        let parsed =
            parse_commit_metas(&(record(&root, "") + &record(&merge_oid, &parents))).unwrap();

        assert!(parsed[0].parents.is_empty(), "root commit has no parents");
        assert_eq!(parsed[1].parents, [first_parent.clone(), side]);
        assert_eq!(parsed[1].parent, first_parent);
    }

    #[test]
    fn merge_base_protocol_distinguishes_unrelated_from_corrupt_output() {
        assert!(parse_merge_base(0, "garbage\n", "").is_err());
        assert_eq!(parse_merge_base(1, "", "").unwrap(), None);
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
    fn classify_cas_failure_is_typed_and_fails_closed() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        // Moved only when the ref is provably elsewhere, regardless of stderr text.
        assert_eq!(
            classify_cas_failure(&RefRead::At(b.clone()), &a, "localized garbage"),
            CasOutcome::Moved { actual: b.clone() }
        );
        assert_eq!(
            classify_cas_failure(&RefRead::Missing, &a, ""),
            CasOutcome::Missing
        );
        // Still at old, unreadable, or garbled reread: Other (fail closed), even
        // if stderr happens to contain the old magic phrase.
        for r in [
            RefRead::At(a.clone()),
            RefRead::At("garbled".into()),
            RefRead::Unknown,
        ] {
            assert!(matches!(
                classify_cas_failure(&r, &a, "but expected"),
                CasOutcome::Other(_)
            ));
        }
    }

    #[test]
    fn update_ref_cas_missing_ref_is_error_not_moved() {
        let repo = TestRepo::new("plumb-cas-missing");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let main0 = repo.head();
        let out = CliGit
            .update_ref_cas_outcome(&loc, "refs/heads/nope", &main0, &main0)
            .unwrap();
        assert_eq!(out, CasOutcome::Missing);
        assert!(
            CliGit
                .update_ref_cas(&loc, "refs/heads/nope", &main0, &main0)
                .is_err()
        );
    }

    #[test]
    fn update_ref_cas_stale_is_moved_with_actual() {
        let repo = TestRepo::new("plumb-cas-moved");
        ident(&repo.dir);
        repo.commit_file("f.txt", "one\n", "c0");
        let loc = repo.loc();
        let main0 = repo.head();
        repo.commit_file("g.txt", "two\n", "c1");
        let main1 = repo.head();
        let out = CliGit
            .update_ref_cas_outcome(&loc, "refs/heads/main", &main0, &main0)
            .unwrap();
        assert_eq!(out, CasOutcome::Moved { actual: main1 });
    }

    #[test]
    fn fenced_update_ref_aborts_when_source_moves_and_keeps_target() {
        let repo = TestRepo::new("plumb-source-fence");
        ident(&repo.dir);
        repo.commit_file("base", "base\n", "base");
        let loc = repo.loc();
        let target_before = repo.head();
        git_in(&repo.dir, &["branch", "source"]);
        git_in(&repo.dir, &["checkout", "-q", "source"]);
        repo.commit_file("source", "one\n", "source one");
        let captured_source = repo.head();
        repo.commit_file("source", "two\n", "source two");
        let moved_source = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        repo.commit_file("target", "next\n", "target next");
        let target_new = repo.head();
        git_in(
            &repo.dir,
            &["update-ref", "refs/heads/main", &target_before],
        );

        let outcome = CliGit
            .update_ref_cas_with_sources(
                &loc,
                "refs/heads/main",
                &target_new,
                &target_before,
                &[("refs/heads/source".into(), captured_source)],
            )
            .unwrap();
        assert_eq!(
            outcome,
            CasOutcome::SourceMoved {
                name: "refs/heads/source".into(),
                actual: moved_source,
            }
        );
        assert_eq!(repo.out(&["rev-parse", "refs/heads/main"]), target_before);
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
        let before_failed_snapshot = repo.head();
        assert!(
            CliGit.snapshot_worktree(&loc, "snap gpg 2", false).is_err(),
            "without override_gpg, an ambient commit.gpgSign snapshots must not succeed silently"
        );
        assert_eq!(
            repo.head(),
            before_failed_snapshot,
            "a failed ambient signature must not advance the branch"
        );
    }

    #[test]
    fn snapshot_commit_failure_restores_the_partially_staged_index() {
        let repo = TestRepo::new("plumb-snap-restore-index");
        ident(&repo.dir);
        git_in(&repo.dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.dir.join("f.txt"), "before f\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "f seed"],
        );
        std::fs::write(repo.dir.join("g.txt"), "before g\n").unwrap();
        git_in(&repo.dir, &["add", "g.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "g seed"],
        );
        let loc = repo.loc();

        std::fs::write(repo.dir.join("f.txt"), "staged f\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        std::fs::write(repo.dir.join("g.txt"), "unstaged g\n").unwrap();
        git_in(&repo.dir, &["config", "commit.gpgsign", "true"]);
        git_in(&repo.dir, &["config", "gpg.program", "false"]);
        let head_before = repo.head();

        assert!(
            CliGit
                .snapshot_worktree(&loc, "snap failure", false)
                .is_err()
        );

        assert_eq!(repo.head(), head_before);
        assert_eq!(repo.out(&["status", "--porcelain"]), "M  f.txt\n M g.txt");
    }

    #[test]
    fn snapshot_refuses_before_add_when_the_index_cannot_be_captured() {
        let repo = TestRepo::new("plumb-snap-unmerged-index");
        ident(&repo.dir);
        std::fs::write(repo.dir.join("f.txt"), "base f\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        );
        std::fs::write(repo.dir.join("g.txt"), "base g\n").unwrap();
        git_in(&repo.dir, &["add", "g.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "g seed"],
        );
        let base = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "-b", "side"]);
        std::fs::write(repo.dir.join("f.txt"), "side f\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "side"],
        );
        let theirs = repo.head();
        git_in(&repo.dir, &["checkout", "-q", "main"]);
        std::fs::write(repo.dir.join("f.txt"), "main f\n").unwrap();
        git_in(&repo.dir, &["add", "f.txt"]);
        git_in(
            &repo.dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "main"],
        );
        let ours = repo.head();
        git_in(&repo.dir, &["read-tree", "-m", &base, &ours, &theirs]);
        assert!(!repo.out(&["ls-files", "-u"]).is_empty());
        std::fs::write(repo.dir.join("g.txt"), "unstaged g\n").unwrap();
        let loc = repo.loc();

        assert!(
            CliGit
                .snapshot_worktree(&loc, "must refuse", false)
                .is_err()
        );

        let status = repo.out(&["status", "--porcelain"]);
        assert!(status.lines().any(|line| line == " M g.txt"), "{status}");
        assert!(!status.lines().any(|line| line == "M  g.txt"), "{status}");
    }
}
