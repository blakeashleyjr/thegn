pub mod protocol;
pub mod router;
pub mod transport;

#[cfg(test)]
mod router_test;

/// Host-provided git + semantic capability that the MCP router exposes to the
/// agent as house tools. The router lives in `superzej-core` and can't depend on
/// `superzej-svc` where `GitBackend` lives, so this trait inverts the dependency:
/// `superzej-svc` implements it over the real `GitBackend` + `semantic`, and the
/// host injects it via [`router::McpRouter::with_git`]. Methods take the agent's
/// worktree path and return MCP-ready text.
pub trait HouseGit: Send + Sync {
    /// Working-tree status (staged/unstaged/untracked).
    fn status(&self, worktree: &str) -> Result<String, String>;
    /// Changed files vs HEAD with +/- line counts.
    fn diff(&self, worktree: &str) -> Result<String, String>;
    /// Local branches (current marked).
    fn branches(&self, worktree: &str) -> Result<String, String>;
    /// Entity-level (semantic) summary of the diff vs HEAD + a suggested commit
    /// message — superzej's structural-diff intelligence.
    fn semantic_diff(&self, worktree: &str) -> Result<String, String>;
}

/// Host-provided forge (PR/CI) + git-write capability exposed as house tools.
/// Like [`HouseGit`], implemented in `superzej-svc` (over `gh`/`glab` + the git
/// backend) and injected via [`router::McpRouter::with_forge`]. Reads shell the
/// forge CLI synchronously; writes use the git backend. (In additive mode the
/// agent already has native shell, so these add structure, not new authority.)
pub trait HouseForge: Send + Sync {
    /// PR state for the current branch (`gh pr status`).
    fn pr_status(&self, worktree: &str) -> Result<String, String>;
    /// Open PRs in the repo (`gh pr list`).
    fn pr_list(&self, worktree: &str) -> Result<String, String>;
    /// Recent CI runs (`gh run list`).
    fn ci_runs(&self, worktree: &str) -> Result<String, String>;
    /// Create a branch off `base` in the worktree.
    fn create_branch(&self, worktree: &str, name: &str, base: &str) -> Result<String, String>;
    /// Commit staged changes with `message`.
    fn commit(&self, worktree: &str, message: &str) -> Result<String, String>;
}
