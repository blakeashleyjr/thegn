//! Native (`gix`) line-count diffs — the `--numstat` reads without a subprocess.
//!
//! These are the CLI holdouts that sat *inside* an otherwise-native path:
//! [`super::GixGit`] already served `is_dirty` / `current_branch` / `branches` /
//! `ahead_behind` from gix, but [`super::local_glyph_reads`] still shelled out
//! twice per sidebar glyph scan, and the diff panel shelled out again per
//! rebuild. On a large repo `git diff --numstat <base>...HEAD` is a merge-base
//! computation plus a whole-tree diff: it measured at ~88% of a core per
//! invocation, ~0.65 spawns/second, and — being subprocess CPU — was invisible
//! to `THEGN_PERF`'s in-process accounting.
//!
//! No new dependency buys this. `gix`'s default features include
//! `basic = ["blob-diff", "revision", "index"]`, so `merge_base()` and the blob
//! line-counter were already compiled into the binary.
//!
//! **Scope.** gix is a read engine here, exactly as it is for the rest of
//! [`super::GixGit`]. Every function returns `Result`, and every caller falls
//! back to [`super::CliGit`] on `Err` — a remote `GitLoc`, a bare repo, a
//! revspec shape we don't model, or any gix error degrades to the previous
//! behavior rather than to a wrong number on screen.

use anyhow::{Context, Result, bail};

use super::DiffEntry;

/// What a caller's revspec string means for diffing.
///
/// `diff_files` is polymorphic over this: the panel passes `"HEAD"`, the merge
/// banner passes `"HEAD...<ref>"`, and the commit drill passes `"<from>..<to>"`.
/// git's own semantics differ per shape, and conflating them silently reports
/// the wrong lines, so the shape is parsed explicitly and unit-tested.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Spec<'a> {
    /// `a...b` — the *symmetric* form: `merge_base(a, b)` vs `b`. This is the
    /// "what does my branch add" diff, and the expensive one.
    Symmetric { base: &'a str, head: &'a str },
    /// `a..b` — tree(a) vs tree(b).
    Range { from: &'a str, to: &'a str },
    /// A bare rev — tree(rev) vs the **worktree** (staged + unstaged, never
    /// untracked). `git diff <rev>` with no range.
    Worktree { rev: &'a str },
}

/// Parse a revspec into the diff shape it denotes.
///
/// Order matters: `"a...b".split_once("..")` yields `("a", ".b")`, so the
/// three-dot form must be tested first. An empty side means `HEAD`, matching
/// git (`git diff ..b` == `git diff HEAD..b`).
pub(crate) fn parse_spec(s: &str) -> Spec<'_> {
    fn or_head(v: &str) -> &str {
        if v.is_empty() { "HEAD" } else { v }
    }
    if let Some((a, b)) = s.split_once("...") {
        return Spec::Symmetric {
            base: or_head(a),
            head: or_head(b),
        };
    }
    if let Some((a, b)) = s.split_once("..") {
        return Spec::Range {
            from: or_head(a),
            to: or_head(b),
        };
    }
    Spec::Worktree { rev: s }
}

/// Resolve a revspec to its commit's tree.
fn tree_of<'r>(repo: &'r gix::Repository, rev: &str) -> Result<gix::Tree<'r>> {
    let id = repo
        .rev_parse_single(rev)
        .with_context(|| format!("gix rev-parse {rev}"))?;
    id.object()
        .context("gix find object")?
        .peel_to_commit()
        .context("gix peel to commit")?
        .tree()
        .context("gix commit tree")
}

/// Open the repo at `path` with an object cache sized for repeated commit
/// lookups — gix's `merge_base` docs call this out specifically, and the glyph
/// scan calls it on every refresh.
fn open(path: impl AsRef<std::path::Path>) -> Result<gix::Repository> {
    let mut repo = gix::discover(path).context("gix discover")?;
    repo.object_cache_size_if_unset(4 * 1024 * 1024);
    Ok(repo)
}

/// Per-file added/deleted counts for `spec`, as `git diff --numstat` would emit.
///
/// Binary files contribute no line counts (gix returns `None` for them, and
/// `--numstat` prints `-`/`-`), so they are skipped — matching
/// [`super::sum_numstat`], which parses those as `0`.
pub(crate) fn diff_entries(
    path: impl AsRef<std::path::Path>,
    spec: &str,
) -> Result<Vec<DiffEntry>> {
    let repo = open(path)?;
    match parse_spec(spec) {
        Spec::Symmetric { base, head } => {
            let base_id = repo
                .rev_parse_single(base)
                .with_context(|| format!("gix rev-parse {base}"))?;
            let head_id = repo
                .rev_parse_single(head)
                .with_context(|| format!("gix rev-parse {head}"))?;
            let mb = repo
                .merge_base(base_id.detach(), head_id.detach())
                .context("gix merge-base")?;
            let old = repo
                .find_commit(mb.detach())
                .context("gix merge-base commit")?
                .tree()
                .context("gix merge-base tree")?;
            tree_to_tree(&old, &tree_of(&repo, head)?)
        }
        Spec::Range { from, to } => tree_to_tree(&tree_of(&repo, from)?, &tree_of(&repo, to)?),
        Spec::Worktree { rev } => tree_to_worktree(&repo, rev),
    }
}

/// `(added, deleted)` totals for `spec` — the [`super::sum_numstat`] of
/// [`diff_entries`], without materialising the rows.
pub(crate) fn totals(path: impl AsRef<std::path::Path>, spec: &str) -> Result<(u32, u32)> {
    let entries = diff_entries(path, spec)?;
    Ok(entries.iter().fold((0u32, 0u32), |(a, d), e| {
        (a.saturating_add(e.added), d.saturating_add(e.deleted))
    }))
}

/// The tree-to-tree case: one gix walk, line-counting each changed blob.
///
/// `track_rewrites(None)` is required, not merely an optimisation — gix's own
/// `Tree::stats()` documents that rename tracking costs time and does not
/// affect line statistics. It also keeps us matching `git diff --numstat`, which
/// reports renames as a delete plus an add unless `-M` is passed, and the CLI
/// path here never passed it.
fn tree_to_tree(old: &gix::Tree<'_>, new: &gix::Tree<'_>) -> Result<Vec<DiffEntry>> {
    let mut cache = old
        .repo
        .diff_resource_cache_for_tree_diff()
        .context("gix diff resource cache")?;
    let mut out: Vec<DiffEntry> = Vec::new();
    let mut changes = old.changes().context("gix tree changes")?;
    changes.options(|o| {
        o.track_rewrites(None);
    });
    changes
        .for_each_to_obtain_tree(new, |change| {
            let path = change.location().to_string();
            if let Some(counts) = change
                .diff(&mut cache)
                .ok()
                .and_then(|mut p| p.line_counts().ok())
                .flatten()
            {
                out.push(DiffEntry {
                    path,
                    added: counts.insertions,
                    deleted: counts.removals,
                });
            }
            cache.clear_resource_cache_keep_allocation();
            Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
        })
        .context("gix tree diff")?;
    Ok(out)
}

/// The bare-rev case: tree(rev) vs the worktree.
///
/// Only handled natively when `rev` resolves to the same commit as `HEAD` —
/// which is what every caller actually passes. gix's status compares against
/// HEAD and the index, so for any other rev the enumeration would be against
/// the wrong baseline; that case errors out to the CLI fallback rather than
/// reporting confidently wrong numbers.
///
/// Untracked files are excluded (`UntrackedFiles::None`): `git diff <rev>`
/// never counts them, but gix's status finds them via its dirwalk, so leaving
/// the dirwalk on would inflate every count in a worktree with new files.
fn tree_to_worktree(repo: &gix::Repository, rev: &str) -> Result<Vec<DiffEntry>> {
    let head = repo.head_id().context("gix head id")?;
    let want = repo
        .rev_parse_single(rev)
        .with_context(|| format!("gix rev-parse {rev}"))?;
    if want.detach() != head.detach() {
        bail!("worktree diff against a non-HEAD rev is not handled natively");
    }
    let workdir = repo
        .workdir()
        .context("bare repo has no worktree to diff")?
        .to_owned();
    let old_tree = tree_of(repo, rev)?;

    let mut cache = repo
        .diff_resource_cache(
            gix::diff::blob::pipeline::Mode::ToGit,
            gix::diff::blob::pipeline::WorktreeRoots {
                old_root: None,
                new_root: Some(workdir),
            },
        )
        .context("gix worktree diff resource cache")?;

    // Enumerate the paths that differ from HEAD — staged and unstaged both,
    // which together are exactly what `git diff HEAD` reports.
    let mut paths: Vec<gix::bstr::BString> = Vec::new();
    let iter = repo
        .status(gix::progress::Discard)
        .context("gix status")?
        .untracked_files(gix::status::UntrackedFiles::None)
        .into_iter(None)
        .context("gix status iter")?;
    for item in iter {
        let item = item.context("gix status item")?;
        if let Some(p) = item.location().to_owned().into() {
            let p: gix::bstr::BString = p;
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
    }

    let mut out: Vec<DiffEntry> = Vec::new();
    for rela in paths {
        // The HEAD side: the blob recorded in the tree. A path missing from the
        // tree is an add, which `set_resource` models as a null id.
        // `BString` → `Path` explicitly: `as_ref()` is ambiguous between
        // `AsRef<BStr>` and `AsRef<[u8]>`, and git paths are bytes on unix.
        let rela_path = gix::path::from_bstr(rela.as_ref() as &gix::bstr::BStr);
        let (old_id, old_kind) = match old_tree.lookup_entry_by_path(rela_path.as_ref()) {
            Ok(Some(entry)) => (entry.object_id(), entry.mode().kind()),
            // Not in the tree: an addition. A null id is how `set_resource`
            // models a non-existing source.
            _ => (
                repo.object_hash().null(),
                gix::object::tree::EntryKind::Blob,
            ),
        };
        if cache
            .set_resource(
                old_id,
                old_kind,
                rela.as_ref(),
                gix::diff::blob::ResourceKind::OldOrSource,
                &repo.objects,
            )
            .is_err()
        {
            continue;
        }
        // The worktree side: a null id means "read it from the worktree root".
        if cache
            .set_resource(
                repo.object_hash().null(),
                gix::object::tree::EntryKind::Blob,
                rela.as_ref(),
                gix::diff::blob::ResourceKind::NewOrDestination,
                &repo.objects,
            )
            .is_err()
        {
            continue;
        }
        let Ok(prep) = cache.prepare_diff() else {
            cache.clear_resource_cache_keep_allocation();
            continue;
        };
        if let gix::diff::blob::platform::prepare_diff::Operation::InternalDiff { algorithm } =
            prep.operation
        {
            let input = prep.interned_input();
            let d = gix::diff::blob::Diff::compute(algorithm, &input);
            out.push(DiffEntry {
                path: rela.to_string(),
                added: d.count_additions(),
                deleted: d.count_removals(),
            });
        }
        cache.clear_resource_cache_keep_allocation();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::TestRepo;

    /// `commit_file` writes with `fs::write`, which does not create parents;
    /// these cases deliberately use nested paths, so make the directory first.
    fn commit_nested(repo: &TestRepo, path: &str, content: &str, msg: &str) {
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(repo.dir.join(parent)).unwrap();
        }
        repo.commit_file(path, content, msg);
    }

    /// `git diff --numstat <spec>` reduced to `(added, deleted)` — the exact
    /// output the native path must reproduce.
    fn cli_totals(repo: &TestRepo, spec: &str) -> (u32, u32) {
        let out = repo.out(&["-c", "core.quotePath=false", "diff", "--numstat", spec]);
        super::super::sum_numstat(&out)
    }

    /// Every native read must agree with the CLI it replaced. A performance
    /// change that silently reports different line counts is worse than the
    /// cost it removed, so parity is asserted against real `git`, not against
    /// hand-written expectations.
    fn assert_parity(repo: &TestRepo, spec: &str) {
        let want = cli_totals(repo, spec);
        let got = totals(&repo.dir, spec).unwrap_or_else(|e| panic!("native {spec}: {e}"));
        assert_eq!(got, want, "numstat totals diverged for spec {spec:?}");
    }

    #[test]
    fn symmetric_range_matches_the_cli() {
        let repo = TestRepo::new("nd-sym");
        repo.commit_file("a.txt", "one\ntwo\n", "base");
        repo.out(&["checkout", "-q", "-b", "feat"]);
        repo.commit_file("a.txt", "one\ntwo\nthree\n", "add line");
        repo.commit_file("b.txt", "new\nfile\n", "add file");
        // The three-dot form is the expensive read the audit found at ~88% of a
        // core per spawn, and the one whose merge-base semantics are easiest to
        // get wrong.
        assert_parity(&repo, "main...HEAD");
        assert_parity(&repo, "main..HEAD");
    }

    #[test]
    fn worktree_diff_matches_the_cli_including_staged_and_untracked() {
        let repo = TestRepo::new("nd-wt");
        repo.commit_file("a.txt", "one\ntwo\n", "base");
        // Unstaged edit.
        std::fs::write(repo.dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        assert_parity(&repo, "HEAD");
        // Staged edit — `git diff HEAD` counts it, so the native path must too.
        std::fs::write(repo.dir.join("c.txt"), "c\n").unwrap();
        repo.out(&["add", "c.txt"]);
        assert_parity(&repo, "HEAD");
        // An UNTRACKED file must NOT be counted: `git diff HEAD` ignores it,
        // but gix's status finds it via the dirwalk, so the dirwalk has to be
        // off or every count inflates.
        std::fs::write(repo.dir.join("untracked.txt"), "u\nu\nu\n").unwrap();
        assert_parity(&repo, "HEAD");
    }

    #[test]
    fn per_file_entries_match_the_cli_rows() {
        let repo = TestRepo::new("nd-rows");
        repo.commit_file("a.txt", "one\n", "base");
        repo.out(&["checkout", "-q", "-b", "feat"]);
        repo.commit_file("a.txt", "one\ntwo\n", "edit a");
        commit_nested(&repo, "dir/b.txt", "b\n", "add b");
        let mut got = diff_entries(&repo.dir, "main...HEAD").unwrap();
        got.sort_by(|x, y| x.path.cmp(&y.path));
        let cli = repo.out(&[
            "-c",
            "core.quotePath=false",
            "diff",
            "--numstat",
            "main...HEAD",
        ]);
        let mut want: Vec<(String, u32, u32)> = cli
            .lines()
            .filter_map(|l| {
                let mut it = l.splitn(3, '\t');
                Some((
                    it.next()?.parse().ok()?,
                    it.next()?.parse().ok()?,
                    it.next()?.to_string(),
                ))
            })
            .map(|(a, d, p): (u32, u32, String)| (p, a, d))
            .collect();
        want.sort();
        let got: Vec<(String, u32, u32)> = got
            .into_iter()
            .map(|e| (e.path, e.added, e.deleted))
            .collect();
        assert_eq!(
            got, want,
            "per-file rows diverged from `git diff --numstat`"
        );
    }

    #[test]
    fn non_ascii_paths_need_no_quote_path_workaround() {
        // The CLI path passes `-c core.quotePath=false` because octal-quoted
        // paths broke the panel's exact-string join against `status -z`. gix
        // returns raw bytes, so the native path must produce the unquoted form
        // directly.
        let repo = TestRepo::new("nd-utf8");
        repo.commit_file("a.txt", "x\n", "base");
        repo.out(&["checkout", "-q", "-b", "feat"]);
        commit_nested(&repo, "docs/café.md", "hello\n", "add cafe");
        let got = diff_entries(&repo.dir, "main...HEAD").unwrap();
        assert!(
            got.iter().any(|e| e.path == "docs/café.md"),
            "expected the raw (unquoted) path, got {:?}",
            got.iter().map(|e| &e.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn binary_files_contribute_no_lines_like_numstat() {
        let repo = TestRepo::new("nd-bin");
        repo.commit_file("a.txt", "x\n", "base");
        repo.out(&["checkout", "-q", "-b", "feat"]);
        // NUL bytes make git call it binary; `--numstat` then prints `-`/`-`,
        // which `sum_numstat` parses as 0.
        std::fs::write(repo.dir.join("blob.bin"), [0u8, 1, 2, 0, 3]).unwrap();
        repo.out(&["add", "blob.bin"]);
        repo.out(&["commit", "-q", "-m", "add binary"]);
        assert_parity(&repo, "main...HEAD");
    }

    #[test]
    fn revspec_shapes_are_distinguished() {
        // Three dots first: `"a...b".split_once("..")` would yield `("a", ".b")`
        // and silently diff against a nonexistent rev.
        assert_eq!(
            parse_spec("main...HEAD"),
            Spec::Symmetric {
                base: "main",
                head: "HEAD"
            }
        );
        assert_eq!(
            parse_spec("abc..def"),
            Spec::Range {
                from: "abc",
                to: "def"
            }
        );
        assert_eq!(parse_spec("HEAD"), Spec::Worktree { rev: "HEAD" });
        // An empty side means HEAD, as it does for git.
        assert_eq!(
            parse_spec("..b"),
            Spec::Range {
                from: "HEAD",
                to: "b"
            }
        );
        assert_eq!(
            parse_spec("a..."),
            Spec::Symmetric {
                base: "a",
                head: "HEAD"
            }
        );
    }
}
