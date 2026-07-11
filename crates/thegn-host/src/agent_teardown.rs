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

/// Tear down a worktree's managed-provider sandbox when the worktree is deleted —
/// a deleted worktree should not leave a paid-for per-worktree sandbox running.
/// The worktree-delete path resolves the env name from the DB *before* it forgets
/// the worktree's rows (a later DB-based resolve would return nothing and leak the
/// sandbox). Best-effort + off-loop (a network DELETE); idempotent (the provider
/// treats a 404 as already-gone, so racing a TTL/manual delete is fine). No-op for
/// local/ssh/k8s envs or an unconfigured/tokenless provider.
pub fn destroy_provider_sandbox(worktree: &str, env_name: &str) {
    let cfg = Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
    let Some(env) = cfg.env.get(env_name) else {
        return;
    };
    let pc = &env.provider;
    let Some(name) = provider_sandbox_name(&cfg, worktree, env_name).filter(|s| !s.is_empty())
    else {
        return;
    };
    // A CLAIMED pool spare with a fresh provisioned-base checkpoint is RECYCLED
    // back into the pool (restore-in-place, row → `ready`) instead of destroyed
    // — see `lifecycle::recycle_claimed_on_delete`. `false` ⇒ destroy as usual.
    if crate::lifecycle::recycle_claimed_on_delete(&cfg, env_name, &name) {
        thegn_core::msg::info(&format!("recycled spare {name} into the pool on delete"));
        return;
    }
    let Some(provider) = provider_for_named(pc, &name) else {
        return;
    };
    match block_on_provider(|| async { provider.destroy(&name).await }) {
        Ok(()) => thegn_core::msg::info(&format!("destroyed sandbox {name} on worktree delete")),
        Err(e) => thegn_core::msg::warn(&format!("sandbox teardown on delete failed: {e}")),
    }
}
