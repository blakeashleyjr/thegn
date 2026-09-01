//! Worktree-close/delete teardown for managed-provider sandboxes (extracted
//! from the ratchet-pinned `agent.rs`; re-exported from `crate::agent` so call
//! sites are unchanged). Both are best-effort and run off the event loop.

use crate::agent::{block_on_provider, provider_sandbox_name};
use crate::provider_factory::provider_for_named;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;

/// Suspend-on-close (8-E): for a provider env with `auto_checkpoint`, snapshot the
/// sandbox when the worktree closes (fast resume next open). Called from the
/// fire-and-forget close thread, which has only the path — so it loads config +
/// resolves the env itself. Best-effort + off-loop; checkpoints-capable only.
pub fn checkpoint_on_close(worktree: &str) {
    let cfg = Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
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
    let Some(name) = provider_sandbox_name(cfg, worktree, env_name).filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    // A CLAIMED pool spare with a fresh provisioned-base checkpoint is RECYCLED
    // back into the pool (restore-in-place, row → `ready`) instead of destroyed
    // — see `lifecycle::recycle_claimed_on_delete`. `false` ⇒ destroy as usual.
    if crate::lifecycle::recycle_claimed_on_delete(cfg, env_name, &name) {
        thegn_core::msg::info(&format!("recycled spare {name} into the pool on delete"));
        return Ok(());
    }
    let Some(provider) = provider_for_named(pc, &name) else {
        return Ok(());
    };
    match block_on_provider(|| async { provider.destroy(&name).await }) {
        Ok(()) => {
            thegn_core::msg::info(&format!("destroyed sandbox {name} on worktree delete"));
        }
        Err(e) => return Err(e.to_string()),
    }
    // Drop the local provisioned marker so a later sandbox reusing this id isn't
    // treated as provisioned (the attach gate would skip re-provisioning).
    crate::provider_workdir::clear_provisioned(&name);
    Ok(())
}
