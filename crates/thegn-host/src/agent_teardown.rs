//! Worktree-close/delete teardown for managed-provider sandboxes (extracted
//! from the ratchet-pinned `agent.rs`; re-exported from `crate::agent` so call
//! sites are unchanged). Both are best-effort and run off the event loop.

use crate::agent::{block_on_provider, provider_sandbox_name};
use crate::provider_factory::provider_for_named;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;

/// Drop a destroyed spare's ledger row only after the provider destroy
/// succeeded; a failure keeps the row (ownership).
fn after_destroy(
    outcome: Result<(), String>,
    disposition: crate::lifecycle::DeleteRecycle,
    forget: impl FnOnce(),
) -> Result<(), String> {
    outcome?;
    if disposition == (crate::lifecycle::DeleteRecycle::Destroy { pool_spare: true }) {
        forget();
    }
    Ok(())
}

/// Suspend-on-close (8-E): for a provider env with `auto_checkpoint`, snapshot the
/// sandbox when the worktree closes (fast resume next open). Called from the
/// fire-and-forget close thread, which has only the path — so it loads config +
/// resolves the env itself. Best-effort + off-loop; checkpoints-capable only.
pub fn checkpoint_on_close(worktree: &str) {
    let cfg = crate::config_startup::cleanup_config();
    let Ok(db) = Db::open() else {
        return;
    };
    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .unwrap_or_default();
    let Some(env_name) = db.effective_env(worktree, &repo_root) else {
        return;
    };
    let Some(env) = cfg.env.get(&env_name) else {
        return;
    };
    let pc = &env.provider;
    if !pc.auto_checkpoint {
        return;
    }
    // Per-worktree id from the single source of truth (resolved placement).
    let Some(name) = provider_sandbox_name(&cfg, worktree, &env_name).filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(provider) = provider_for_named(pc, &name) else {
        return;
    };
    if !provider.caps().checkpoints {
        return;
    }
    match block_on_provider(|| async { provider.checkpoint(&name, Some("auto-close")).await }) {
        Ok(id) => thegn_core::msg::info(&format!("checkpointed {name} on close: {id}")),
        Err(e) => thegn_core::msg::warn(&format!("auto-checkpoint on close failed: {e}")),
    }
}

/// Destroy a provider sandbox using an already-resolved configuration. The
/// lifecycle transaction calls this before deleting the worktree row, so the
/// selected environment and provider identity cannot be lost to cache pruning.
pub fn destroy_provider_sandbox_with(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
) -> Result<(), String> {
    let Some(env) = cfg.env.get(env_name) else {
        return Ok(());
    };
    let pc = &env.provider;
    let name = match crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env_name)
    {
        Ok(name) => name,
        Err(error) => {
            return Err(format!(
                "resolve the worktree's provider sandbox: {error:#}"
            ));
        }
    };
    let Some(name) = name.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    // Revoke before recycle/destroy. If SQLite is unavailable, fail closed and
    // leave the sandbox intact so the lifecycle can retry rather than orphaning
    // an unrevoked return credential with no remaining owner metadata.
    crate::remote_enqueue_auth::revoke_for_sandbox(pc, &name, Some(worktree))
        .map_err(|error| format!("revoke route-to-host credential: {error:#}"))?;
    // A CLAIMED pool spare with a fresh provisioned-base checkpoint is RECYCLED
    // back into the pool (restore-in-place, row → `ready`) instead of destroyed.
    let disposition = crate::lifecycle::recycle_claimed_on_delete(cfg, env_name, &name);
    if disposition == crate::lifecycle::DeleteRecycle::Recycled {
        thegn_core::msg::info(&format!("recycled spare {name} into the pool on delete"));
        return Ok(());
    }
    let Some(provider) = provider_for_named(pc, &name) else {
        return Ok(());
    };
    let outcome = match block_on_provider(|| async { provider.destroy(&name).await }) {
        Ok(()) => {
            thegn_core::msg::info(&format!("destroyed sandbox {name} on worktree delete"));
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    };
    after_destroy(outcome, disposition, || {
        crate::lifecycle::forget_destroyed_spare(&name)
    })?;
    // Drop the local provisioned marker so a later sandbox reusing this id isn't
    // treated as provisioned (the attach gate would skip re-provisioning).
    crate::provider_workdir::clear_provisioned(&name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::DeleteRecycle;

    #[test]
    fn pool_delete_destroy_failure_keeps_the_row() {
        let mut called = false;
        let result = after_destroy(
            Err("boom".into()),
            DeleteRecycle::Destroy { pool_spare: true },
            || called = true,
        );

        assert_eq!(result, Err("boom".into()));
        assert!(!called);
    }

    #[test]
    fn pool_delete_destroy_success_drops_the_row_once() {
        let mut calls = 0;
        let result = after_destroy(Ok(()), DeleteRecycle::Destroy { pool_spare: true }, || {
            calls += 1
        });

        assert_eq!(result, Ok(()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn pool_delete_success_without_spare_row_skips_forget() {
        let mut called = false;
        let result = after_destroy(Ok(()), DeleteRecycle::Destroy { pool_spare: false }, || {
            called = true
        });

        assert_eq!(result, Ok(()));
        assert!(!called);
    }
}
