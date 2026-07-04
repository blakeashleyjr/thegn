//! The **cache** seam: TTL'd read-through caches that feed the panel's instant
//! paint — PR status, CI runs, per-repo open-PRs-by-branch, issue-tracker
//! payloads, the unified "My Work" feed, and per-worktree diff/commit/test/LOC
//! snapshots.
//!
//! These are pure caches (best-effort; git / the live API is the source of
//! truth). [`crate::db::Db`] is the embedded-SQLite implementation
//! (`db_cache.rs`); a server backend would implement this against Postgres for
//! shared multi-user cache state. Each getter returns `(payload_json,
//! fetched_at_secs)` so the caller can apply its own TTL.

use anyhow::Result;

/// Persisted TTL caches. Object-safe (`&self` + concrete args), so
/// `&dyn CacheStore` works for backend-agnostic consumers.
pub trait CacheStore {
    /// PR-status cache for a worktree: `(json, fetched_at)`.
    fn get_pr_cache(&self, worktree: &str) -> Result<Option<(String, i64)>>;
    /// Replace the PR-status cache for a worktree.
    fn put_pr_cache(&self, worktree: &str, branch: &str, json: &str) -> Result<()>;

    /// CI run-history cache for a worktree: `(json, fetched_at)`.
    fn get_ci_cache(&self, worktree: &str) -> Result<Option<(String, i64)>>;
    /// Replace the CI run-history cache for a worktree.
    fn put_ci_cache(&self, worktree: &str, branch: &str, json: &str) -> Result<()>;

    /// Per-repo open-PRs-by-branch cache: `(json, fetched_at)`.
    fn get_pr_branch_cache(&self, repo_root: &str) -> Result<Option<(String, i64)>>;
    /// Replace the per-repo open-PRs-by-branch cache.
    fn put_pr_branch_cache(&self, repo_root: &str, json: &str) -> Result<()>;
    /// Open PR counts grouped by branch (`head_ref`), parsed from the per-repo
    /// cache. Empty when the cache is absent or unparseable.
    fn get_open_pr_counts_by_branch(
        &self,
        repo_root: &str,
    ) -> Result<std::collections::BTreeMap<String, usize>>;

    /// Issue-tracker cache for `(repo, provider)`: `(json, fetched_at)`.
    fn get_issue_cache(&self, repo_root: &str, provider: &str) -> Result<Option<(String, i64)>>;
    /// All cached provider payloads for a repo, as `(provider, json)` pairs.
    fn get_all_issue_cache(&self, repo_root: &str) -> Result<Vec<(String, String)>>;
    /// Replace the issue-tracker cache for `(repo, provider)`.
    fn put_issue_cache(&self, repo_root: &str, provider: &str, json: &str) -> Result<()>;

    /// The cached "My Work" payload for a `scope`: `(json, fetched_at)`.
    fn get_my_work_cache(&self, scope: &str) -> Result<Option<(String, i64)>>;
    /// Replace the cached "My Work" payload for a `scope`.
    fn put_my_work_cache(&self, scope: &str, json: &str) -> Result<()>;

    /// Per-worktree diff cache: `(files, fetched_at)`.
    fn get_diff_cache(&self, worktree: &str) -> Result<Option<(String, i64)>>;
    /// Replace the per-worktree diff cache.
    fn put_diff_cache(&self, worktree: &str, files: &str) -> Result<()>;

    /// Per-worktree commit cache: `(json, fetched_at)`.
    fn get_commit_cache(&self, worktree: &str) -> Result<Option<(String, i64)>>;
    /// Replace the per-worktree commit cache.
    fn put_commit_cache(&self, worktree: &str, json: &str) -> Result<()>;

    /// Per-worktree latest-test cache: `(json, fetched_at)`.
    fn get_test_cache(&self, worktree: &str) -> Result<Option<(String, i64)>>;
    /// Replace the per-worktree latest-test cache.
    fn put_test_cache(&self, worktree: &str, json: &str) -> Result<()>;

    /// Per-worktree LOC cache: the tokei report JSON + fetch timestamp (for TTL
    /// refresh); `None` if absent or pre-`report_json`.
    fn get_loc_cache_entry(&self, worktree: &str) -> Result<Option<(String, i64)>>;
    /// Cache the LOC report: `total` (the chip number) + the report JSON.
    fn put_loc_cache(&self, worktree: &str, total: usize, report_json: &str) -> Result<()>;
}
