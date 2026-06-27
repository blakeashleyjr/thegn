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
