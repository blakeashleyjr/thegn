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
}
