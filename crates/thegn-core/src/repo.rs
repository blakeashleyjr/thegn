//! Repo / main-worktree resolution from a directory's git context, and repo
//! discovery for the workspace picker. Self-healing across session
//! resurrection (git is the source of truth; the DB is only a cache).

use crate::config::Config;
use crate::git_memo;
use crate::identity::{ExactPath, IdentityError, RepositoryId};
use crate::store::WorkspaceStore;
use crate::util;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const MAX_GIT_IDENTITY_OUTPUT: usize = 16 * 1024;

/// Resolve Git's canonical common directory as an exact, bounded path.
///
/// Repository identity is intentionally tied to this directory rather than a
/// remote URL or basename. Moving the common directory therefore changes the
/// [`RepositoryId`] and requires an explicit trusted-overlay migration.
pub fn canonical_common_dir(root: &Path) -> Result<ExactPath, IdentityError> {
    let output = util::git_stdout_bounded(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        MAX_GIT_IDENTITY_OUTPUT,
    )
    .map_err(|_| IdentityError::GitProbeFailed {
        operation: "rev-parse --git-common-dir",
    })?;
    let raw = trim_git_line(&output).ok_or(IdentityError::GitProbeFailed {
        operation: "empty git common directory",
    })?;
    let path = ExactPath::from_git_bytes(raw)?;
    let canonical =
        std::fs::canonicalize(path.as_path()).map_err(|_| IdentityError::GitProbeFailed {
            operation: "canonicalize git common directory",
        })?;
    ExactPath::from_path(&canonical)
}

/// Construct the canonical repository identity without consulting SQLite.
pub fn repository_id(root: &Path) -> Result<RepositoryId, IdentityError> {
    RepositoryId::from_common_dir(&canonical_common_dir(root)?)
}

fn trim_git_line(output: &[u8]) -> Option<&[u8]> {
    let end = output
        .len()
        .checked_sub(1)
        .filter(|&end| output[end] == b'\n')?;
    (end > 0).then_some(&output[..end])
}

/// Toplevel of the working tree containing `dir`.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    util::git_out(dir, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// The worktree root owning `cwd` — the "connect to root" resolver: from any
/// nested directory (a shell pane deep in a subdir), find the git worktree it
/// belongs to (`rev-parse --show-toplevel` from that dir). The result is
/// canonicalized so it compares equal to registered workspace/worktree paths
/// even when `cwd` came from a `/proc/<pid>/cwd` readlink or a symlinked
/// checkout. `None` when `cwd` is not inside any git worktree.
pub fn worktree_root_for_cwd(cwd: &Path) -> Option<PathBuf> {
    if !cwd.is_dir() {
        return None;
    }
    let root = toplevel(cwd)?;
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// The MAIN worktree root for `dir`'s repo — climb out of any linked worktree
/// so we never create worktrees-of-worktrees. None if `dir` isn't in a repo.
///
/// Memoised per path under a stat fingerprint of `<dir>/.git` (and, for a
/// linked worktree, the `commondir` file it points through), so the idle
/// hydration tick does not fork `git rev-parse` for an answer that cannot have
/// changed (THE-718). A path with no `.git` entry (a subdirectory, a bare
/// repo) cannot be fingerprinted and still spawns every call, as before.
pub fn main_worktree(dir: &Path) -> Option<PathBuf> {
    static MEMO: Mutex<Option<git_memo::Memo<Option<PathBuf>>>> = Mutex::new(None);
    git_memo::memoised_checked(
        &MEMO,
        MAIN_WORKTREE_MEMO_CAP,
        dir,
        git_memo::fingerprint(dir),
        || main_worktree_checked(dir),
    )
}

const MAIN_WORKTREE_MEMO_CAP: usize = 512;

/// The subprocess answer plus whether it is memoisable: a git that could not be
/// spawned is transient (EAGAIN/EMFILE under load) and must not be remembered;
/// a non-zero exit is deterministic for the fingerprinted inputs and may be.
fn main_worktree_checked(dir: &Path) -> (Option<PathBuf>, bool) {
    let out = util::git_cmd(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output();
    let Ok(out) = out else {
        return (None, false);
    };
    if !out.status.success() {
        return (None, true);
    }
    let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if common.is_empty() {
        return (None, true);
    }
    let common = PathBuf::from(common);
    // Normal repo: ".../.git" -> parent is the main worktree.
    // Bare repo: the common dir is the repo itself.
    let root = if common.file_name().map(|n| n == ".git").unwrap_or(false) {
        common.parent().map(|p| p.to_path_buf())
    } else {
        Some(common)
    };
    (root, true)
}

#[cfg(test)]
fn main_worktree_uncached(dir: &Path) -> Option<PathBuf> {
    main_worktree_checked(dir).0
}

pub fn is_bare(dir: &Path) -> bool {
    util::git_out(dir, &["rev-parse", "--is-bare-repository"]).as_deref() == Some("true")
}

/// Short repo name for tab names and worktree grouping.
pub fn repo_name(root: &Path) -> String {
    let top = toplevel(root).unwrap_or_else(|| root.to_path_buf());
    let base = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".git")
        .map(|s| s.to_string())
        .unwrap_or(base)
}

/// Short repo name derived purely from a repo *root* path — no git, no DB.
/// Use this for bulk listings (e.g. the sidebar inventory) where the paths are
/// already repo roots and spawning `git` per repo would be the bottleneck.
pub fn repo_name_from_path(root: &Path) -> String {
    let base = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    base.strip_suffix(".git")
        .map(|s| s.to_string())
        .unwrap_or(base)
}

/// A stable, globally-unique slug for a repo — the prefix of every tab that
/// belongs to it (`"{slug}/…"`). Tabs are scoped by this prefix.
///
/// The slug is assigned once and persisted (DB), with `-2`/`-3` suffixing when
/// two repos share a basename (e.g. two different `WASHU` checkouts), so their
/// tabs never collide. Same repo path always yields the same slug, so every call
/// site (tab creation, rename, resolve, sidebar grouping) stays consistent.
pub fn repo_slug(root: &Path) -> String {
    let base = {
        let s = util::slugify(&repo_name(root));
        if s.is_empty() { "repo".to_string() } else { s }
    };
    crate::db::Db::open()
        .ok()
        .and_then(|db| db.slug_for_repo(&root.to_string_lossy(), &base).ok())
        .unwrap_or(base)
}

/// Resolve a repo slug using a specific state database. This is for workers
/// whose caller already selected an isolated database path (notably tests and
/// alternate state profiles); it must not silently consult the default DB.
pub fn repo_slug_at(root: &Path, db_path: &Path) -> String {
    crate::db::Db::open_at(db_path)
        .map(|db| repo_slug_with(&db, root))
        .unwrap_or_else(|_| slug_base(&repo_name_from_path(root)))
}

/// Canonical slug for a repo via an existing DB handle — same contract as
/// [`repo_slug`] but without opening a second connection or spawning git
/// (the name is path-derived, so `root` must already be a repo root).
pub fn repo_slug_with(db: &crate::db::Db, root: &Path) -> String {
    let base = {
        let s = util::slugify(&repo_name_from_path(root));
        if s.is_empty() { "repo".to_string() } else { s }
    };
    db.slug_for_repo(&root.to_string_lossy(), &base)
        .unwrap_or(base)
}

/// Tab name for a repo's main checkout (its "home" tab).
pub fn home_tab(slug: &str) -> String {
    format!("{slug}/home")
}

/// Tab name for a worktree of `slug` on `branch`: `"{slug}/{branch}"` with the
/// EXACT branch name (THE-516). It used to slugify the branch, so `feat/a`,
/// `feat-a` and `feat_a` all became `{slug}/feat-a` and shared one tab/DB key.
/// Branch names are already unique per repository, so the exact name is a
/// collision-free key; `split_tab` splits on the FIRST `/` (the slug never
/// contains one), so a branch containing `/` round-trips.
///
/// The one reserved name is `home`, which would alias [`home_tab`]. It is
/// escaped as `home~`: `~` can never appear in a Git ref name, so no real
/// branch produces that tab.
pub fn branch_tab(slug: &str, branch: &str) -> String {
    if branch == "home" {
        return format!("{slug}/home~");
    }
    format!("{slug}/{branch}")
}

/// [`repo_slug`] for authority-bearing callers (registration, routing). The
/// infallible variant falls back to the unsuffixed basename slug when the DB
/// is unavailable, which re-aliases same-basename repositories (`repo` vs
/// `repo-2`). This variant refuses instead, so nothing is registered or routed
/// under a fabricated identity.
pub fn repo_slug_checked(root: &Path) -> anyhow::Result<String> {
    let db = crate::db::Db::open()?;
    db.slug_for_repo(&root.to_string_lossy(), &slug_base(&repo_name(root)))
}

/// [`repo_slug_with`] that refuses instead of falling back (see
/// [`repo_slug_checked`]). Path-derived name, no Git, like [`repo_slug_with`].
pub fn repo_slug_with_checked(db: &crate::db::Db, root: &Path) -> anyhow::Result<String> {
    db.slug_for_repo(
        &root.to_string_lossy(),
        &slug_base(&repo_name_from_path(root)),
    )
}

fn slug_base(name: &str) -> String {
    let s = util::slugify(name);
    if s.is_empty() { "repo".to_string() } else { s }
}

/// Pure, git-free and DB-free prefill for a repository slug. This is the same
/// path-derived base that [`repo_slug_with_checked`] passes to `slug_for_repo`;
/// the DB may add a collision suffix when the repository is registered.
pub fn repo_slug_guess(root: &Path) -> String {
    slug_base(&repo_name_from_path(root))
}

/// Discover git repos under the configured roots (parent dirs of a `.git`
/// directory). Skips worktrees (whose `.git` is a file). Sorted + deduped.
pub fn discover_repos(cfg: &Config) -> Vec<String> {
    let cancel = AtomicBool::new(false);
    let mut found = Vec::new();
    let report = discover_repos_bounded(cfg, DiscoveryBudget::default(), &cancel, |batch| {
        found.extend(batch);
        true
    });
    if report.truncated {
        crate::msg::warn(&format!(
            "repo discovery was truncated after visiting {} entries and finding {} repositories",
            report.visited_entries, report.repositories
        ));
    }
    found.sort();
    found.dedup();
    found
}

/// Hard limits for a repo-root walk. The CLI and picker share these defaults;
/// callers may lower them for deterministic bounded scans and tests.
#[derive(Debug, Clone, Copy)]
pub struct DiscoveryBudget {
    pub max_entries: usize,
    pub max_repositories: usize,
    pub max_path_bytes: usize,
    pub max_single_path_bytes: usize,
    pub max_elapsed: Duration,
    pub max_errors: usize,
    pub batch_size: usize,
}

impl Default for DiscoveryBudget {
    fn default() -> Self {
        Self {
            max_entries: 200_000,
            max_repositories: 20_000,
            max_path_bytes: 8 * 1024 * 1024,
            max_single_path_bytes: 16 * 1024,
            max_elapsed: Duration::from_secs(30),
            max_errors: 64,
            batch_size: 64,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveryReport {
    pub visited_entries: usize,
    pub repositories: usize,
    pub path_bytes: usize,
    pub traversal_errors: usize,
    pub missing_roots: usize,
    pub truncated: bool,
    pub cancelled: bool,
}

/// Bounded, cancellable discovery. `emit` receives batches no larger than the
/// budget and returns false when its consumer has gone away. Repositories are
/// deduplicated by filesystem identity within one scan (first spelling wins).
pub fn discover_repos_bounded(
    cfg: &Config,
    budget: DiscoveryBudget,
    cancel: &AtomicBool,
    mut emit: impl FnMut(Vec<String>) -> bool,
) -> DiscoveryReport {
    fn prune(entry: &walkdir::DirEntry) -> bool {
        if entry.depth() == 0 {
            return false;
        }
        let name = entry.file_name().to_string_lossy();
        name.starts_with('.') || matches!(name.as_ref(), "node_modules" | "target")
    }
    let started = Instant::now();
    let mut report = DiscoveryReport::default();
    let mut batch = Vec::with_capacity(budget.batch_size.min(256));
    let mut roots = std::collections::HashSet::new();
    let mut seen_repos: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw_root in &cfg.repo_roots {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let raw = Path::new(raw_root);
        if !raw.is_dir() {
            crate::msg::warn(&format!(
                "repo_roots: {} does not exist — skipping it",
                raw.display()
            ));
            report.missing_roots += 1;
            continue;
        }
        let root = match std::fs::canonicalize(raw) {
            Ok(root) => root,
            Err(_) => {
                report.traversal_errors += 1;
                if report.traversal_errors >= budget.max_errors {
                    report.truncated = true;
                    break;
                }
                continue;
            }
        };
        if !roots.insert(root.clone()) {
            continue;
        }
        let mut walker = WalkDir::new(raw)
            .max_depth(cfg.repo_scan_depth)
            .same_file_system(true)
            .follow_links(false)
            .into_iter();
        while let Some(item) = walker.next() {
            if cancel.load(Ordering::Relaxed) {
                report.cancelled = true;
                break;
            }
            if started.elapsed() >= budget.max_elapsed
                || report.visited_entries >= budget.max_entries
            {
                report.truncated = true;
                break;
            }
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => {
                    report.traversal_errors += 1;
                    if report.traversal_errors >= budget.max_errors {
                        report.truncated = true;
                        break;
                    }
                    continue;
                }
            };
            report.visited_entries += 1;
            if entry.file_type().is_dir()
                && entry.file_name() == ".git"
                && let Some(parent) = entry.path().parent()
            {
                let key = util::file_identity(parent)
                    .unwrap_or_else(|| format!("path:{}", parent.to_string_lossy()));
                if !seen_repos.insert(key) {
                    walker.skip_current_dir();
                    continue;
                }
                let path = parent.to_string_lossy().into_owned();
                let len = path.len();
                if len > budget.max_single_path_bytes
                    || report.path_bytes.saturating_add(len) > budget.max_path_bytes
                    || report.repositories >= budget.max_repositories
                {
                    report.truncated = true;
                    break;
                }
                report.repositories += 1;
                report.path_bytes += len;
                batch.push(path);
                if batch.len() >= budget.batch_size.max(1) && !emit(std::mem::take(&mut batch)) {
                    report.cancelled = true;
                    break;
                }
                // The repository has been recognized: never visit its Git object tree.
                walker.skip_current_dir();
                continue;
            }
            if entry.file_type().is_dir() && prune(&entry) {
                walker.skip_current_dir();
            }
        }
        if report.truncated || report.cancelled {
            break;
        }
    }
    if !batch.is_empty() && !report.cancelled && !cancel.load(Ordering::Relaxed) && !emit(batch) {
        report.cancelled = true;
    }
    report
}

/// Discover git repos under a single `root` (e.g. `$HOME`), depth-limited and
/// pruning hidden + heavy dirs so a deep home directory stays fast. A repo is a
/// dir containing a `.git` (dir or file); we don't descend into `.git`. Used by
/// the sidebar's "+ new workspace" fzf picker. Sorted + deduped.
pub fn discover_repos_in(root: &Path, max_depth: usize) -> Vec<String> {
    fn prune(e: &walkdir::DirEntry) -> bool {
        if e.depth() == 0 {
            return false;
        }
        let n = e.file_name().to_string_lossy();
        // Don't descend into .git, hidden dirs, or notorious build/cache dirs.
        n == ".git" || n.starts_with('.') || matches!(n.as_ref(), "node_modules" | "target")
    }
    let mut found: Vec<String> = Vec::new();
    if !root.is_dir() {
        return found;
    }
    for entry in WalkDir::new(root)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| !prune(e))
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir() && entry.path().join(".git").exists() {
            found.push(entry.path().to_string_lossy().into_owned());
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tg-repo-{}-{}", std::process::id(), name))
    }

    fn git_ok(dir: &Path, args: &[&str]) {
        assert!(
            util::git_cmd(dir)
                .args([
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t"
                ])
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    }

    #[test]
    fn bounded_discovery_prunes_git_objects_and_reports_limits() {
        let root = tmp("bounded-discovery");
        std::fs::remove_dir_all(&root).unwrap_or(());
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git/objects/pack")).unwrap();
        std::fs::create_dir_all(root.join(".hidden/hidden-repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/vendor/.git")).unwrap();
        std::fs::create_dir_all(root.join("target/build/.git")).unwrap();
        let cfg = Config {
            repo_roots: vec![root.to_string_lossy().into_owned()],
            ..Config::default()
        };
        let mut batches = Vec::new();
        let report = discover_repos_bounded(
            &cfg,
            DiscoveryBudget {
                batch_size: 1,
                ..DiscoveryBudget::default()
            },
            &AtomicBool::new(false),
            |batch| {
                assert!(batch.len() <= 1, "batch exceeded requested bound");
                batches.extend(batch);
                true
            },
        );
        assert_eq!(batches, vec![repo.to_string_lossy().into_owned()]);
        assert!(
            report.visited_entries < 8,
            "the .git object descendants and pruned trees must not be visited: {report:?}"
        );
        assert!(!report.truncated);

        let mut with_missing_root = cfg.clone();
        with_missing_root.repo_roots.insert(
            0,
            root.with_extension("missing-root")
                .to_string_lossy()
                .into_owned(),
        );
        let errored = discover_repos_bounded(
            &with_missing_root,
            DiscoveryBudget::default(),
            &AtomicBool::new(false),
            |_| true,
        );
        assert_eq!(errored.traversal_errors, 0);
        assert_eq!(errored.missing_roots, 1);

        let alias = root.parent().unwrap().join("walk-alias");
        std::fs::create_dir_all(&alias).unwrap();
        let raw_root = alias.join("..").join(root.file_name().unwrap());
        let mut aliased_cfg = cfg.clone();
        aliased_cfg.repo_roots = vec![raw_root.to_string_lossy().into_owned()];
        let mut aliased = Vec::new();
        let alias_report = discover_repos_bounded(
            &aliased_cfg,
            DiscoveryBudget::default(),
            &AtomicBool::new(false),
            |batch| {
                aliased.extend(batch);
                true
            },
        );
        assert_eq!(
            aliased,
            vec![raw_root.join("repo").to_string_lossy().into_owned()],
            "canonical paths are dedupe keys only; results retain the configured root spelling"
        );
        assert!(!alias_report.truncated);
        std::fs::remove_dir_all(alias).unwrap();

        let limited = discover_repos_bounded(
            &cfg,
            DiscoveryBudget {
                max_entries: 1,
                ..DiscoveryBudget::default()
            },
            &AtomicBool::new(false),
            |_| true,
        );
        assert!(
            limited.truncated,
            "the walk must stop once the entry cap is reached"
        );

        let cancelled = AtomicBool::new(true);
        let stopped =
            discover_repos_bounded(&cfg, DiscoveryBudget::default(), &cancelled, |_| true);
        assert!(stopped.cancelled);
        assert_eq!(stopped.visited_entries, 0);

        for budget in [
            DiscoveryBudget {
                max_elapsed: Duration::ZERO,
                ..DiscoveryBudget::default()
            },
            DiscoveryBudget {
                max_repositories: 0,
                ..DiscoveryBudget::default()
            },
            DiscoveryBudget {
                max_path_bytes: 0,
                ..DiscoveryBudget::default()
            },
            DiscoveryBudget {
                max_single_path_bytes: 1,
                ..DiscoveryBudget::default()
            },
        ] {
            assert!(
                discover_repos_bounded(&cfg, budget, &AtomicBool::new(false), |_| true).truncated
            );
        }
        let consumer_gone = discover_repos_bounded(
            &cfg,
            DiscoveryBudget {
                batch_size: 1,
                ..DiscoveryBudget::default()
            },
            &AtomicBool::new(false),
            |_| false,
        );
        assert!(consumer_gone.cancelled);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn repo_slug_at_persists_to_the_supplied_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("same-name");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = dir.path().join("isolated/thegn.db");
        let db = crate::db::Db::open_at(&db_path).unwrap();
        db.slug_for_repo("/another/same-name", "same-name").unwrap();

        assert_eq!(repo_slug_at(&root, &db_path), "same-name-2");
        assert_eq!(
            db.slug_for_repo(&root.to_string_lossy(), "same-name")
                .unwrap(),
            "same-name-2"
        );
        let bad = dir.path().join("dir-not-db");
        std::fs::create_dir_all(&bad).unwrap();
        assert_eq!(repo_slug_at(&root, &bad), "same-name");
    }

    #[test]
    fn main_worktree_memo_hit_skips_the_subprocess() {
        let root = tmp("mw-hit");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        git_ok(&root, &["init", "-q"]);
        let memo = Mutex::new(None);
        let spawns = std::cell::Cell::new(0);
        let run = || {
            git_memo::memoised(&memo, 4, &root, || {
                spawns.set(spawns.get() + 1);
                main_worktree_uncached(&root)
            })
        };
        let first = run().expect("repo resolves");
        assert_eq!(run(), Some(first.clone()));
        assert_eq!(run(), Some(first));
        assert_eq!(spawns.get(), 1, "two hits must not spawn git");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn main_worktree_linked_worktree_and_reuse_for_another_repo() {
        let base = tmp("mw-linked");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&base);
        let a = base.join("a");
        let b = base.join("b");
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            git_ok(d, &["init", "-q"]);
            git_ok(d, &["commit", "-q", "--allow-empty", "-m", "i"]);
        }
        let wt = base.join("wt");
        git_ok(
            &a,
            &["worktree", "add", "-q", "-b", "x", wt.to_str().unwrap()],
        );
        let canon = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        assert_eq!(canon(main_worktree(&wt).unwrap()), canon(a.clone()));
        assert_eq!(canon(main_worktree(&wt).unwrap()), canon(a.clone()));
        // Remove the linked worktree and reuse its path for a different repo's
        // worktree: the answer must follow.
        git_ok(&a, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        assert_eq!(main_worktree(&wt), None, "gone path resolves to nothing");
        git_ok(
            &b,
            &["worktree", "add", "-q", "-b", "y", wt.to_str().unwrap()],
        );
        assert_eq!(canon(main_worktree(&wt).unwrap()), canon(b.clone()));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn main_worktree_missing_path_is_retried_when_it_appears() {
        let base = tmp("mw-missing");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&base);
        let p = base.join("later");
        assert_eq!(main_worktree(&p), None);
        assert_eq!(main_worktree(&p), None);
        std::fs::create_dir_all(&p).unwrap();
        git_ok(&p, &["init", "-q"]);
        assert!(main_worktree(&p).is_some(), "retried once the path exists");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn worktree_root_for_cwd_resolves_nested_dir_to_root() {
        let root = tmp("root-resolve");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("src/deep/inner");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(util::git_cmd(&root).arg("init").status().unwrap().success());
        let resolved = worktree_root_for_cwd(&nested).expect("nested cwd resolves");
        assert_eq!(resolved, std::fs::canonicalize(&root).unwrap());
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worktree_root_for_cwd_outside_any_worktree_is_none() {
        let dir = tmp("root-none");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // /tmp is not a git repo; a plain dir resolves to nothing.
        assert_eq!(worktree_root_for_cwd(&dir), None);
        // A missing path resolves to nothing (no panic).
        assert_eq!(worktree_root_for_cwd(&dir.join("missing")), None);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repository_id_uses_git_common_dir_not_basename_or_symlink_alias() {
        let root = tmp("identity-root");
        let other = tmp("identity-other");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&other);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        assert!(util::git_cmd(&root).arg("init").status().unwrap().success());
        assert!(
            util::git_cmd(&other)
                .arg("init")
                .status()
                .unwrap()
                .success()
        );

        let first = repository_id(&root).unwrap();
        let second = repository_id(&other).unwrap();
        assert_ne!(
            first, second,
            "same basename roots must not share a repository ID"
        );

        #[cfg(unix)]
        {
            let alias = tmp("identity-root-alias");
            let _ = std::fs::remove_file(&alias);
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            assert_eq!(
                first,
                repository_id(&alias).unwrap(),
                "a symlink alias must resolve to the same Git common-dir identity"
            );
            let _ = std::fs::remove_file(alias);
        }

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn branch_tabs_are_exact_and_never_alias() {
        let branches = [
            "feat/a", "feat-a", "feat_a", "FEAT-A", "修复", "home", "home~x",
        ];
        let tabs: Vec<String> = branches.iter().map(|b| branch_tab("app", b)).collect();
        for (i, t) in tabs.iter().enumerate() {
            assert!(tabs[i + 1..].iter().all(|u| u != t), "{t} aliased");
            assert_ne!(t, &home_tab("app"), "no branch aliases the home tab");
        }
        assert_eq!(branch_tab("app", "feat/a"), "app/feat/a");
        assert_eq!(branch_tab("app", "home"), "app/home~");
    }

    #[test]
    fn repo_slug_guess_is_the_pure_checked_slug_base() {
        assert_eq!(
            repo_slug_guess(Path::new("/x/My Repo.git")),
            slug_base("My Repo")
        );
        assert_eq!(repo_slug_guess(Path::new("/")), "repo");

        let db = crate::db::Db::open_memory().unwrap();
        let root = Path::new("/fresh/My Repo.git");
        assert_eq!(
            repo_slug_guess(root),
            repo_slug_with_checked(&db, root).unwrap()
        );
    }

    #[test]
    fn discovery_emits_a_repo_reached_through_a_symlinked_root_once() {
        #[cfg(unix)]
        {
            let base = tempfile::tempdir().unwrap();
            let repo = base.path().join("a/sub/r");
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            let link = base.path().join("link");
            std::os::unix::fs::symlink(base.path().join("a/sub"), &link).unwrap();
            let cfg = Config {
                repo_roots: vec![
                    base.path().join("a").to_string_lossy().into_owned(),
                    link.to_string_lossy().into_owned(),
                ],
                repo_scan_depth: 4,
                ..Config::default()
            };
            let mut found = Vec::new();
            let report = discover_repos_bounded(
                &cfg,
                DiscoveryBudget::default(),
                &AtomicBool::new(false),
                |batch| {
                    found.extend(batch);
                    true
                },
            );

            assert_eq!(found, vec![repo.to_string_lossy().into_owned()]);
            assert_eq!(report.repositories, 1);
        }
    }

    #[test]
    fn discovery_counts_a_repo_under_nested_roots_once() {
        let base = tempfile::tempdir().unwrap();
        let a = base.path().join("a");
        let sub = a.join("sub");
        let repo = sub.join("r");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let cfg = Config {
            repo_roots: vec![
                a.to_string_lossy().into_owned(),
                sub.to_string_lossy().into_owned(),
            ],
            repo_scan_depth: 4,
            ..Config::default()
        };
        let mut found = Vec::new();
        let report = discover_repos_bounded(
            &cfg,
            DiscoveryBudget::default(),
            &AtomicBool::new(false),
            |batch| {
                found.extend(batch);
                true
            },
        );

        assert_eq!(found, vec![repo.to_string_lossy().into_owned()]);
        assert_eq!(report.repositories, 1);
    }

    #[test]
    fn discovery_keeps_distinct_repos_with_one_basename() {
        // Positive control: equal basenames with different filesystem identities stay distinct.
        let base = tempfile::tempdir().unwrap();
        let first = base.path().join("x/r");
        let second = base.path().join("y/r");
        std::fs::create_dir_all(first.join(".git")).unwrap();
        std::fs::create_dir_all(second.join(".git")).unwrap();
        let cfg = Config {
            repo_roots: vec![base.path().to_string_lossy().into_owned()],
            repo_scan_depth: 4,
            ..Config::default()
        };
        let mut found = Vec::new();
        let report = discover_repos_bounded(
            &cfg,
            DiscoveryBudget::default(),
            &AtomicBool::new(false),
            |batch| {
                found.extend(batch);
                true
            },
        );
        found.sort();

        let mut expected = vec![
            first.to_string_lossy().into_owned(),
            second.to_string_lossy().into_owned(),
        ];
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(report.repositories, 2);
    }
}
