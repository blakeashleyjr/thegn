//! Git backend seam. `gix` for reads (status/diff/refs/log — the hot panel-poll
//! path); the `git` CLI for porcelain writes gix can't yet cover (worktree add
//! with checkout, push, merge, rebase) and for `GitLoc::Remote` (gix is local
//! only). Native impl lands in Phase 2; the CLI fallback wraps superzej-core's
//! existing `worktree`/`repo`/`util::git_*` code.

use anyhow::{Context, Result};
use std::path::Path;
use superzej_core::remote::GitLoc;

/// A changed file in `git status` terms (porcelain XY + path).
#[derive(Debug, Clone)]
pub struct FileStatus {
    pub path: String,
    pub staged: char,
    pub unstaged: char,
}

/// One entry of a diff against a base ref (added/deleted line counts).
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub path: String,
    pub added: u32,
    pub deleted: u32,
}

#[derive(Debug, Clone)]
pub struct Branch {
    pub name: String,
    pub is_head: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub path: String,
    pub branch: Option<String>,
}

/// Reads go native (gix) for local locs; writes and remote stay CLI.
pub trait GitBackend: Send + Sync {
    fn status(&self, loc: &GitLoc) -> Result<Vec<FileStatus>>;
    fn diff_files(&self, loc: &GitLoc, base: &str) -> Result<Vec<DiffEntry>>;
    fn branches(&self, loc: &GitLoc) -> Result<Vec<Branch>>;
    fn current_branch(&self, loc: &GitLoc) -> Result<String>;
    /// Commits the current branch is `(ahead, behind)` its upstream tracking
    /// branch. `None` when the branch has no configured upstream (or HEAD is
    /// detached) — the sidebar simply omits the ↑/↓ glyphs in that case.
    fn ahead_behind(&self, loc: &GitLoc) -> Result<Option<(usize, usize)>>;
    fn worktrees(&self, root: &Path) -> Result<Vec<WorktreeInfo>>;
    fn add_worktree(&self, root: &Path, branch: &str, base: &str, path: &Path) -> Result<()>;
    fn remove_worktree(&self, root: &Path, path: &Path, delete_branch: bool) -> Result<()>;
}

/// The permanent fallback: every op via the `git` CLI (through `GitLoc`, so it
/// works for both local and remote locs). The native impl composes over this.
pub struct CliGit;

fn run(loc: &GitLoc, args: &[&str]) -> Result<String> {
    let out = loc
        .git_command(args)
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_root(root: &Path, args: &[&str]) -> Result<()> {
    if superzej_core::util::git_ok(root, args) {
        Ok(())
    } else {
        anyhow::bail!("git {} failed in {}", args.join(" "), root.display())
    }
}

impl GitBackend for CliGit {
    fn status(&self, loc: &GitLoc) -> Result<Vec<FileStatus>> {
        let out = run(loc, &["status", "--porcelain=v1", "-z"])?;
        let mut v = Vec::new();
        for entry in out.split('\0').filter(|s| s.len() >= 3) {
            let bytes = entry.as_bytes();
            v.push(FileStatus {
                staged: bytes[0] as char,
                unstaged: bytes[1] as char,
                path: entry[3..].to_string(),
            });
        }
        Ok(v)
    }

    fn diff_files(&self, loc: &GitLoc, base: &str) -> Result<Vec<DiffEntry>> {
        let out = run(loc, &["diff", "--numstat", base])?;
        let mut v = Vec::new();
        for line in out.lines() {
            let mut it = line.splitn(3, '\t');
            let (a, d, p) = (it.next(), it.next(), it.next());
            if let (Some(a), Some(d), Some(p)) = (a, d, p) {
                v.push(DiffEntry {
                    added: a.parse().unwrap_or(0),
                    deleted: d.parse().unwrap_or(0),
                    path: p.to_string(),
                });
            }
        }
        Ok(v)
    }

    fn branches(&self, loc: &GitLoc) -> Result<Vec<Branch>> {
        let out = run(loc, &["branch", "--format=%(HEAD)\t%(refname:short)"])?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (mark, name) = l.split_once('\t')?;
                Some(Branch {
                    name: name.to_string(),
                    is_head: mark.trim() == "*",
                })
            })
            .collect())
    }

    fn current_branch(&self, loc: &GitLoc) -> Result<String> {
        Ok(run(loc, &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string())
    }

    fn ahead_behind(&self, loc: &GitLoc) -> Result<Option<(usize, usize)>> {
        // `@{u}` resolves the upstream; the command fails when none is set, so a
        // non-zero exit is treated as "no upstream" rather than an error.
        let out = match loc
            .git_command(&["rev-list", "--left-right", "--count", "@{u}...HEAD"])
            .output()
        {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => return Ok(None),
        };
        // Output is "<behind>\t<ahead>": left side is @{u} (commits we lack),
        // right side is HEAD (commits ahead).
        let mut it = out.split_whitespace();
        let behind = it.next().and_then(|s| s.parse().ok());
        let ahead = it.next().and_then(|s| s.parse().ok());
        match (ahead, behind) {
            (Some(a), Some(b)) => Ok(Some((a, b))),
            _ => Ok(None),
        }
    }

    fn worktrees(&self, root: &Path) -> Result<Vec<WorktreeInfo>> {
        let loc = GitLoc::for_worktree(root);
        let out = run(&loc, &["worktree", "list", "--porcelain"])?;
        let mut v = Vec::new();
        let mut cur: Option<WorktreeInfo> = None;
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                if let Some(w) = cur.take() {
                    v.push(w);
                }
                cur = Some(WorktreeInfo {
                    path: p.to_string(),
                    branch: None,
                });
            } else if let Some(b) = line.strip_prefix("branch ")
                && let Some(w) = cur.as_mut()
            {
                w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
            }
        }
        if let Some(w) = cur.take() {
            v.push(w);
        }
        Ok(v)
    }

    fn add_worktree(&self, root: &Path, branch: &str, base: &str, path: &Path) -> Result<()> {
        let p = path.to_string_lossy();
        run_root(root, &["worktree", "add", "-b", branch, &p, base])
    }

    fn remove_worktree(&self, root: &Path, path: &Path, delete_branch: bool) -> Result<()> {
        let p = path.to_string_lossy();
        run_root(root, &["worktree", "remove", "--force", &p])?;
        if delete_branch {
            // Best-effort: the branch may still be checked out elsewhere.
            let _ = run_root(root, &["branch", "-D", &p]);
        }
        Ok(())
    }
}

/// The native backend: gix for the clean read wins (current branch, branch list)
/// on local locs; everything else — status/diff (gix status semantics can
/// diverge), worktree enumeration, all writes, and every remote loc — delegates
/// to the CLI fallback. This is the plan's "gix is a read engine, not a write
/// engine" stance made concrete.
pub struct GixGit {
    fallback: CliGit,
}

impl Default for GixGit {
    fn default() -> Self {
        Self { fallback: CliGit }
    }
}

impl GixGit {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GitBackend for GixGit {
    fn current_branch(&self, loc: &GitLoc) -> Result<String> {
        if loc.is_remote() {
            return self.fallback.current_branch(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        match repo.head_name().context("gix head_name")? {
            Some(name) => Ok(name.shorten().to_string()),
            None => Ok("HEAD".to_string()), // detached
        }
    }

    fn branches(&self, loc: &GitLoc) -> Result<Vec<Branch>> {
        if loc.is_remote() {
            return self.fallback.branches(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        let head = repo
            .head_name()
            .context("gix head_name")?
            .map(|n| n.shorten().to_string());
        let platform = repo.references().context("gix references")?;
        let mut v = Vec::new();
        for r in platform.local_branches().context("gix local_branches")? {
            let r = r.map_err(|e| anyhow::anyhow!("gix branch ref: {e}"))?;
            let name = r.name().shorten().to_string();
            let is_head = head.as_deref() == Some(name.as_str());
            v.push(Branch { name, is_head });
        }
        Ok(v)
    }

    fn ahead_behind(&self, loc: &GitLoc) -> Result<Option<(usize, usize)>> {
        if loc.is_remote() {
            return self.fallback.ahead_behind(loc);
        }
        let repo = gix::discover(loc.path()).context("gix discover")?;
        // The current branch reference; bail to None when detached / unborn.
        let Some(head_ref) = repo.head_ref().context("gix head_ref")? else {
            return Ok(None);
        };
        // Its upstream tracking ref (e.g. refs/remotes/origin/main). Absent =>
        // no upstream configured => no counts.
        let Some(upstream_name) = head_ref.remote_tracking_ref_name(gix::remote::Direction::Fetch)
        else {
            return Ok(None);
        };
        let upstream_name = match upstream_name {
            Ok(n) => n,
            Err(_) => return Ok(None),
        };
        let Ok(mut upstream) = repo.find_reference(upstream_name.as_ref()) else {
            return Ok(None);
        };

        let head_id = head_ref.id();
        let upstream_id = upstream.peel_to_id().context("peel upstream ref")?;

        // ahead = commits reachable from HEAD but not from upstream; behind is
        // the symmetric reverse. `with_hidden` paints the opposite tip's
        // ancestry as unwanted so the walk yields exactly the difference.
        let count_excluding = |tip: gix::ObjectId, hidden: gix::ObjectId| -> Result<usize> {
            let walk = repo
                .rev_walk([tip])
                .with_hidden([hidden])
                .all()
                .context("gix rev_walk")?;
            let mut n = 0usize;
            for info in walk {
                info.map_err(|e| anyhow::anyhow!("gix rev_walk item: {e}"))?;
                n += 1;
            }
            Ok(n)
        };

        let ahead = count_excluding(head_id.detach(), upstream_id.detach())?;
        let behind = count_excluding(upstream_id.detach(), head_id.detach())?;
        Ok(Some((ahead, behind)))
    }

    // --- delegated to the CLI fallback ---
    fn status(&self, loc: &GitLoc) -> Result<Vec<FileStatus>> {
        self.fallback.status(loc)
    }
    fn diff_files(&self, loc: &GitLoc, base: &str) -> Result<Vec<DiffEntry>> {
        self.fallback.diff_files(loc, base)
    }
    fn worktrees(&self, root: &Path) -> Result<Vec<WorktreeInfo>> {
        self.fallback.worktrees(root)
    }
    fn add_worktree(&self, root: &Path, branch: &str, base: &str, path: &Path) -> Result<()> {
        self.fallback.add_worktree(root, branch, base, path)
    }
    fn remove_worktree(&self, root: &Path, path: &Path, delete_branch: bool) -> Result<()> {
        self.fallback.remove_worktree(root, path, delete_branch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> std::path::PathBuf {
        // The svc crate dir is <root>/crates/superzej-svc; the repo root is two up.
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn cli_and_gix_agree_on_current_branch_and_branches() {
        let loc = GitLoc::for_worktree(&repo_root());
        let cli = CliGit;
        let gix = GixGit::new();

        let cb_cli = cli.current_branch(&loc).unwrap();
        let cb_gix = gix.current_branch(&loc).unwrap();
        assert_eq!(cb_cli, cb_gix, "gix and git disagree on current branch");
        assert!(!cb_cli.is_empty());

        let mut b_cli: Vec<String> = cli
            .branches(&loc)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        let mut b_gix: Vec<String> = gix
            .branches(&loc)
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        b_cli.sort();
        b_gix.sort();
        assert_eq!(b_cli, b_gix, "gix and git disagree on the branch set");
        // exactly one head in each
        assert_eq!(
            gix.branches(&loc)
                .unwrap()
                .iter()
                .filter(|b| b.is_head)
                .count(),
            1
        );
    }

    #[test]
    fn cli_worktree_enumeration_includes_the_root() {
        let root = repo_root();
        let wts = CliGit.worktrees(&root).unwrap();
        assert!(!wts.is_empty());
        // The canonical root path should appear among the worktrees.
        let canon = std::fs::canonicalize(&root).unwrap();
        assert!(wts.iter().any(|w| {
            std::fs::canonicalize(&w.path)
                .map(|p| p == canon)
                .unwrap_or(false)
        }));
    }

    #[test]
    fn status_and_diff_run_without_error() {
        let loc = GitLoc::for_worktree(&repo_root());
        // Should not error on a normal repo (content is environment-dependent).
        let _ = CliGit.status(&loc).unwrap();
        let _ = CliGit.diff_files(&loc, "HEAD").unwrap();
    }

    /// Run `git` in `dir`, panicking on failure (test setup helper).
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    fn commit_empty(dir: &std::path::Path, msg: &str) {
        git_in(dir, &["commit", "--allow-empty", "-q", "-m", msg]);
    }

    #[test]
    fn ahead_behind_counts_divergence_and_is_none_without_upstream() {
        let base = std::env::temp_dir().join(format!("sz-ab-{}-{:p}", std::process::id(), &0u8));
        let _ = std::fs::remove_dir_all(&base);
        let remote = base.join("remote.git");
        let clone = base.join("clone");
        std::fs::create_dir_all(&base).unwrap();

        // A bare "remote" with one commit, cloned locally so the clone's branch
        // has a tracking upstream.
        git_in(&base, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git_in(&seed, &["init", "-q", "-b", "main"]);
        commit_empty(&seed, "c0");
        git_in(
            &seed,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git_in(&seed, &["push", "-q", "origin", "main"]);
        git_in(
            &base,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );

        let loc = GitLoc::for_worktree(&clone);
        let gix = GixGit::new();
        let cli = CliGit;

        // Freshly cloned, in sync with upstream.
        assert_eq!(gix.ahead_behind(&loc).unwrap(), Some((0, 0)));
        assert_eq!(cli.ahead_behind(&loc).unwrap(), Some((0, 0)));

        // Two local commits ahead.
        commit_empty(&clone, "a1");
        commit_empty(&clone, "a2");
        assert_eq!(gix.ahead_behind(&loc).unwrap(), Some((2, 0)));
        assert_eq!(cli.ahead_behind(&loc).unwrap(), Some((2, 0)));

        // Advance the remote by one and refetch: now also 1 behind.
        commit_empty(&seed, "r1");
        git_in(&seed, &["push", "-q", "origin", "main"]);
        git_in(&clone, &["fetch", "-q", "origin"]);
        assert_eq!(gix.ahead_behind(&loc).unwrap(), Some((2, 1)));
        assert_eq!(cli.ahead_behind(&loc).unwrap(), Some((2, 1)));

        // A repo with no upstream reports None.
        let solo = base.join("solo");
        std::fs::create_dir_all(&solo).unwrap();
        git_in(&solo, &["init", "-q", "-b", "main"]);
        commit_empty(&solo, "s0");
        let solo_loc = GitLoc::for_worktree(&solo);
        assert_eq!(gix.ahead_behind(&solo_loc).unwrap(), None);
        assert_eq!(cli.ahead_behind(&solo_loc).unwrap(), None);

        let _ = std::fs::remove_dir_all(&base);
    }
}
