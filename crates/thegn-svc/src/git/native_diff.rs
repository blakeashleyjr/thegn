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

/// A diff resource cache that **reaps** its long-running filter processes when
/// it goes out of scope, on every path including `?` and panic.
///
/// # Why this is RAII and not a call at the end of the function
///
/// A worktree diff runs in `Mode::ToGit`, i.e. it applies the *clean* filter so
/// worktree content can be compared against the stored blob. When a repository
/// — or the user's global config — sets `filter.lfs.process`, gix spawns
/// `git-lfs filter-process` as a long-running driver, a direct child of this
/// process.
///
/// `gix_filter::driver::State`'s own docs are explicit: *"shutdown() must be
/// called to finalize long-running processes. Failing to do so will naturally
/// shut them down by terminating their pipes, but finishing explicitly allows to
/// wait for processes as well."* Without that wait the child is never reaped,
/// and `std::process::Child` does not wait on drop — so each diff leaves a
/// zombie.
///
/// Measured before this existed: **4,408** zombie `git-lfs` processes parented
/// to a single thegn instance after three days, taking the machine's process
/// table to 5,274 entries and making a plain `pgrep` cost 20% of a core.
///
/// Both diff functions below have `?` early-returns between building the cache
/// and finishing, and a failed diff spawns the filter just the same — so a
/// cleanup call at the happy-path exit would leak on precisely the paths that
/// are hardest to notice. Hence `Drop`.
///
/// **The filter is deliberately NOT disabled instead.** That would be cheaper
/// and it would be wrong: without the clean filter, an LFS worktree file's full
/// content is compared against its stored pointer, producing a spurious
/// whole-file diff on every LFS-tracked path.
///
/// `Drop` is the second line of defence, not the first — see
/// [`prefer_oneshot_filters`], which stops the long-running process being
/// spawned at all for the driver shape git-lfs actually installs.
struct ReapingCache(gix::diff::blob::Platform);

/// Route a driver that offers **both** shapes through its one-shot program
/// instead of its long-running `process`, because gix owns the one-shot child's
/// lifetime and leaks the persistent one.
///
/// git-lfs installs all three (`filter.lfs.clean`, `.smudge`, `.process`, with
/// `required = true`), and gix prefers `process` whenever it is set. That
/// preference is what leaks, in two separate places inside gix-filter 0.31:
///
/// * on the happy path the `Client` lives in `driver::State::running` until the
///   state is dropped, and gix's own comment there asserts that *"nothing else
///   needs to be done to clean them up after drop"* — which is false, because
///   the pipes closing makes the child **exit**, not get **waited on**;
/// * on an I/O error `driver::apply::handle_io_err` does
///   `running.remove(process)` and drops the `Client` on the floor, so it never
///   reaches `shutdown()` at all. That path is unreachable from outside gix,
///   which is why `ReapingCache`'s `Drop` alone cannot close this.
///
/// The one-shot path has no such gap: gix keeps the child as
/// `driver.required.then_some((child, command))` and `wait()`s it when the
/// reader hits EOF. So the reroute is only safe for a `required` driver — for a
/// non-required one gix discards the one-shot child unwaited too, which would
/// trade one leak per *diff* for one leak per *blob*.
///
/// Correctness is unchanged: this is exactly what git itself does when a driver
/// configures `clean` but no `process`, and `git-lfs clean` computes the pointer
/// from local content with no network round-trip. What is lost is the protocol's
/// `delay` batching, which matters when filtering a whole checkout and not when
/// diffing the handful of paths that differ from HEAD.
fn prefer_oneshot_filters(cache: &mut gix::diff::blob::Platform) {
    for driver in &mut cache.filter.worktree_filter.options_mut().drivers {
        // Both caches below convert `Mode::ToGit`, so `clean` is the one-shot
        // program that has to exist for the reroute to preserve behaviour.
        if driver.required && driver.clean.is_some() {
            driver.process = None;
        }
    }
}

impl std::ops::Deref for ReapingCache {
    type Target = gix::diff::blob::Platform;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ReapingCache {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ReapingCache {
    fn drop(&mut self) {
        // `gix::filter::plumbing` is gix's re-export of the `gix-filter` crate
        // (`pub use gix_filter as plumbing`), so this needs no new dependency.
        use gix::filter::plumbing::driver::shutdown::Mode;
        // `shutdown` consumes the state and `State: Default`, so take it and
        // leave a fresh one behind; we are being dropped anyway.
        let state = std::mem::take(self.0.filter.worktree_filter.driver_state_mut());
        if let Err(error) = state.shutdown(Mode::WaitForProcesses) {
            // best-effort: the diff itself already succeeded; a filter we could
            // not wait for is worth a line, not a failure.
            tracing::debug!(
                target: "thegn::git",
                %error,
                "could not wait for a diff filter process"
            );
        }
    }
}

/// The only way a diff cache is built here: wrapped so it reaps, and rerouted so
/// there is normally nothing left to reap.
fn new_cache(mut platform: gix::diff::blob::Platform) -> ReapingCache {
    prefer_oneshot_filters(&mut platform);
    ReapingCache(platform)
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
/// [`super::sum_numstat`], which omits those rows.
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
    Ok(super::sum_entries(&entries))
}

/// The tree-to-tree case: one gix walk, line-counting each changed blob.
///
/// `track_rewrites(None)` is required, not merely an optimisation — gix's own
/// `Tree::stats()` documents that rename tracking costs time and does not
/// affect line statistics. It also keeps us matching `git diff --numstat`, which
/// reports renames as a delete plus an add unless `-M` is passed, and the CLI
/// path here never passed it.
fn tree_to_tree(old: &gix::Tree<'_>, new: &gix::Tree<'_>) -> Result<Vec<DiffEntry>> {
    let mut cache = new_cache(
        old.repo
            .diff_resource_cache_for_tree_diff()
            .context("gix diff resource cache")?,
    );
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

/// First-occurrence-order dedup in expected O(N) (hash set, byte-exact keys).
/// The set holds a clone of each key (duplicate storage is accepted: hashing
/// the bytes to a `u64` would need a collision fallback, which is overkill).
fn dedup_in_order<K: std::hash::Hash + Eq + Clone>(items: impl IntoIterator<Item = K>) -> Vec<K> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for k in items {
        if seen.insert(k.clone()) {
            out.push(k);
        }
    }
    out
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

    let mut cache = new_cache(
        repo.diff_resource_cache(
            gix::diff::blob::pipeline::Mode::ToGit,
            gix::diff::blob::pipeline::WorktreeRoots {
                old_root: None,
                new_root: Some(workdir),
            },
        )
        .context("gix worktree diff resource cache")?,
    );

    // Enumerate the paths that differ from HEAD — staged and unstaged both,
    // which together are exactly what `git diff HEAD` reports.
    // A path appears once per staged and once per unstaged change; dedup is
    // order-preserving and O(1) per item (a linear `contains` was O(N^2)).
    let iter = repo
        .status(gix::progress::Discard)
        .context("gix status")?
        .untracked_files(gix::status::UntrackedFiles::None)
        .into_iter(None)
        .context("gix status iter")?;
    let mut status_paths: Vec<gix::bstr::BString> = Vec::new();
    for item in iter {
        let item = item.context("gix status item")?;
        if let Some(p) = item.location().to_owned().into() {
            let p: gix::bstr::BString = p;
            status_paths.push(p);
        }
    }
    let paths = dedup_in_order(status_paths);

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
    #[derive(Clone, Hash)]
    struct Counted(Vec<u8>);
    thread_local!(static EQS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });
    impl PartialEq for Counted {
        fn eq(&self, o: &Self) -> bool {
            EQS.with(|c| c.set(c.get() + 1));
            self.0 == o.0
        }
    }
    impl Eq for Counted {}

    #[test]
    fn dedup_is_linear_and_order_preserving() {
        let n = 40_000usize;
        let mut items: Vec<Counted> = Vec::new();
        for i in 0..n {
            // Raw non-UTF-8 bytes, long shared prefix.
            let mut b = vec![b'd'; 200];
            b.extend_from_slice(&(i as u32).to_be_bytes());
            b.push(0xff);
            items.push(Counted(b));
        }
        let mut input = items.clone();
        input.extend(items.iter().rev().cloned()); // every path twice
        EQS.with(|c| c.set(0));
        let out = dedup_in_order(input);
        let eqs = EQS.with(|c| c.get());
        assert_eq!(out.len(), n);
        assert!(out.iter().zip(&items).all(|(a, b)| a.0 == b.0));
        // Quadratic would be ~n^2/2 = 800M comparisons.
        assert!(eqs < 4 * n, "too many comparisons: {eqs}");
    }

    use super::*;
    use crate::git::testutil::TestRepo;

    /// Our own child processes as `(state, comm)` — naming them, because
    /// "5 children leaked" does not say which spawn site to fix.
    ///
    /// A runtime probe rather than a compile-time platform gate: this crate's
    /// platform ratchet keeps per-OS compilation out of service logic, and where
    /// `/proc` is absent there is simply nothing to count.
    fn own_children() -> Vec<(String, String)> {
        let me = std::process::id();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.filter_map(Result::ok) {
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                continue;
            };
            // comm can contain spaces and parens, so split on the LAST ')'.
            let Some((head, rest)) = stat.rsplit_once(") ") else {
                continue;
            };
            let comm = head.split_once(" (").map(|(_, c)| c).unwrap_or("?");
            let mut f = rest.split_whitespace();
            let state = f.next().unwrap_or("?");
            if f.next().and_then(|p| p.parse::<u32>().ok()) == Some(me) {
                out.push((state.to_string(), comm.to_string()));
            }
        }
        out
    }

    /// A repo whose `tracked.txt` goes through a long-running `process` filter
    /// (`cat`, the stand-in for `git-lfs filter-process`) and whose worktree is
    /// dirty, so any content comparison must actually run that filter.
    fn filtered_dirty_repo(name: &str) -> TestRepo {
        let repo = TestRepo::new(name);
        repo.commit_file("tracked.txt", "one\ntwo\n", "seed");
        // git-lfs's own shape: all three programs, and `required`. `cat` is a
        // faithful stand-in — as a one-shot it is an identity clean filter, and
        // as a `process` it fails the long-running handshake, which is the
        // gix-filter path that drops the child without waiting.
        let ran = repo.dir.join("clean-ran");
        repo.out(&[
            "config",
            "filter.tgtest.clean",
            &format!("tee -a {}", ran.display()),
        ]);
        repo.out(&["config", "filter.tgtest.smudge", "cat"]);
        repo.out(&["config", "filter.tgtest.process", "cat"]);
        repo.out(&["config", "filter.tgtest.required", "true"]);
        std::fs::write(
            repo.dir.join(".gitattributes"),
            "tracked.txt filter=tgtest\n",
        )
        .unwrap();
        std::fs::write(repo.dir.join("tracked.txt"), "one\ntwo\nthree\n").unwrap();
        repo
    }

    /// Which of the two calls in `tree_to_worktree` spawns the filter: the diff
    /// resource cache we reap, or the `gix` status walk that finds the paths.
    /// Run alone, the status walk should leave nothing behind.
    #[test]
    fn a_status_walk_leaves_no_unreaped_filter_process() {
        // No procfs, no observation — see `own_children`.
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }

        let repo = filtered_dirty_repo("reap-filters-status");
        let before = own_children().len();
        for _ in 0..5 {
            let gr = gix::discover(&repo.dir).unwrap();
            let iter = gr
                .status(gix::progress::Discard)
                .unwrap()
                .untracked_files(gix::status::UntrackedFiles::None)
                .into_iter(None)
                .unwrap();
            for item in iter {
                // `unwrap()` already panics on an error, so nothing is being
                // swallowed here — a bare statement drops the item just the same.
                item.unwrap();
            }
        }
        let after = own_children();
        assert!(
            after.len() <= before,
            "a gix status walk left {} unreaped child process(es) behind \
             (before={before}, after={}): {after:?}",
            after.len().saturating_sub(before),
            after.len()
        );
    }

    /// A worktree diff against a repo configured with a long-running
    /// `filter.*.process` driver must leave **no** unreaped child behind.
    ///
    /// This is the 4,408-zombie regression. Counting real children rather than
    /// mocking: the defect was that `std::process::Child` is not waited on at
    /// drop, and only the process table can show that.
    #[test]
    fn a_worktree_diff_leaves_no_unreaped_filter_process() {
        // No procfs, no observation — see `own_children`.
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }

        let repo = filtered_dirty_repo("reap-filters");
        let before = own_children().len();
        // Several diffs: one leak per call is what produced thousands.
        for _ in 0..5 {
            let entries = diff_entries(&repo.dir, "HEAD").expect("worktree diff");
            // Guard against passing for the wrong reason: a diff that silently
            // stopped filtering would also leave no children behind.
            assert!(
                entries.iter().any(|e| e.path == "tracked.txt"),
                "the filtered path dropped out of the diff: {entries:?}"
            );
        }
        assert!(
            repo.dir.join("clean-ran").exists(),
            "the clean filter never ran, so this proves nothing about reaping it"
        );
        let after = own_children();
        assert!(
            after.len() <= before,
            "a worktree diff left {} unreaped child process(es) behind \
             (before={before}, after={}): {after:?}",
            after.len().saturating_sub(before),
            after.len()
        );
    }

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
        super::super::sum_numstat(&out).expect("numstat parses")
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
        // which `parse_numstat` omits.
        std::fs::write(repo.dir.join("blob.bin"), [0u8, 1, 2, 0, 3]).unwrap();
        repo.out(&["add", "blob.bin"]);
        repo.out(&["commit", "-q", "-m", "add binary"]);
        assert_parity(&repo, "main...HEAD");
    }

    #[test]
    fn typed_rows_match_strict_cli_parser_including_binary() {
        let repo = TestRepo::new("nd-typed");
        repo.commit_file("a.txt", "x\n", "base");
        repo.out(&["checkout", "-q", "-b", "feat"]);
        repo.commit_file("a.txt", "x\ny\n", "edit");
        std::fs::write(repo.dir.join("blob.bin"), [0u8, 1, 0, 2]).unwrap();
        repo.out(&["add", "blob.bin"]);
        repo.out(&["commit", "-q", "-m", "bin"]);
        let cli = repo.out(&[
            "-c",
            "core.quotePath=false",
            "diff",
            "--numstat",
            "main...HEAD",
        ]);
        let mut want = super::super::parse_numstat(&cli).unwrap();
        let mut got = diff_entries(&repo.dir, "main...HEAD").unwrap();
        want.sort_by(|x, y| x.path.cmp(&y.path));
        got.sort_by(|x, y| x.path.cmp(&y.path));
        assert_eq!(want.len(), got.len());
        for (w, g) in want.iter().zip(&got) {
            assert_eq!((&w.path, w.added, w.deleted), (&g.path, g.added, g.deleted));
        }
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
