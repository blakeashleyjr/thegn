//! The **warm-pool** seam (schema v26): pre-provisioned spare sandboxes
//! (`pool_spares`), per-`(repo, env)` provider base snapshots
//! (`env_base_snapshots`), and the runtime pool-target override (`pool_targets`).
//!
//! Backs the spare-pool lifecycle (create → checkpoint → claim → recycle). A
//! server backend managing a shared pool across users would implement this
//! against Postgres; the local shell implements it over the embedded SQLite `Db`
//! (`db_pool.rs`). `PoolSpare` is defined in [`crate::db`].

use anyhow::Result;

use crate::db::PoolSpare;

/// Persisted warm-pool state. Object-safe (`&self` + concrete args), so
/// `&dyn PoolStore` works for backend-agnostic consumers.
pub trait PoolStore {
    /// Record a per-`(repo, env)` provider base snapshot + the `flake.lock` hash
    /// it was built against. Replaces any prior base for the pair.
    fn set_base_snapshot(
        &self,
        repo_path: &str,
        env_name: &str,
        snapshot_id: &str,
        lock_hash: &str,
    ) -> Result<()>;

    /// The recorded base snapshot for `(repo, env)` as `(snapshot_id, lock_hash)`.
    fn base_snapshot(&self, repo_path: &str, env_name: &str) -> Result<Option<(String, String)>>;

    /// Insert a freshly-minted spare (state `"provisioning"`) for `(repo, env)`.
    fn insert_pool_spare(&self, name: &str, repo: &str, env: &str) -> Result<()>;

    /// Mark a spare `ready` with its checkpoint id + the `flake.lock` hash it was
    /// built against (for staleness checks).
    fn set_pool_spare_ready(
        &self,
        name: &str,
        checkpoint_id: Option<&str>,
        lock_hash: &str,
    ) -> Result<()>;

    /// Drop a spare row (destroyed or claimed-and-finalized).
    fn delete_pool_spare(&self, name: &str) -> Result<()>;

    /// All spares for `(repo, env)`, any state, newest first.
    fn pool_spares_for(&self, repo: &str, env: &str) -> Result<Vec<PoolSpare>>;

    /// Atomically claim a `ready` spare for `(repo, env)` and bind it to
    /// `worktree`. Returns the claimed `(sandbox_name, checkpoint_id)` or `None`.
    fn claim_pool_spare(
        &self,
        repo: &str,
        env: &str,
        worktree: &str,
    ) -> Result<Option<(String, Option<String>)>>;

    /// The pool-spare row for one sandbox name (any state), or `None`.
    fn pool_spare_by_name(&self, name: &str) -> Result<Option<PoolSpare>>;

    /// The provider sandbox name a worktree is bound to (a claimed pool spare),
    /// or `None` to use the derived `effective_provider_id`.
    fn worktree_provider_sandbox(&self, worktree: &str) -> Result<Option<String>>;

    /// The runtime pool-target override for `(repo, env)`, or `None` to fall back
    /// to the configured `[lifecycle.pool] size`.
    fn pool_target(&self, repo: &str, env: &str) -> Result<Option<i64>>;

    /// Set the runtime pool-target override for `(repo, env)`.
    fn set_pool_target(&self, repo: &str, env: &str, target: i64) -> Result<()>;
}
