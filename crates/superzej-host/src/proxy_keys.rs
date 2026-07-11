//! Per-worktree virtual keys for the LLM proxy (V 287), split out of the
//! ratcheted `agent.rs`.
//!
//! A launched agent authenticates to `szproxy` with a minted virtual key whose
//! scope is `worktree:<path>` — the proxy resolves it to the worktree AND its
//! enclosing workspace/zone, so spend attribution and budget caps roll up the
//! whole chain (worktree → workspace → zone → global). `upstream`, when given
//! (the layered `[llm_proxy] upstream`, so a workspace overlay can bind its own
//! account), pins the key's traffic to that provider's lanes.

use std::hash::{Hash, Hasher};

use superzej_core::db::Db;
use superzej_core::store::ProxyStore;

/// Mint (or refresh) a per-worktree virtual key so the agent's model traffic
/// routes through `szproxy` scoped to `worktree:<path>`. Returns the bearer
/// token to hand the agent (best-effort; `None` if the DB is unavailable).
/// Revoke it with [`revoke_agent_proxy_key`] when the agent disconnects. Used
/// by the non-bouncer (TCP) path, which holds the minted token in scope for
/// revocation.
pub fn mint_agent_proxy_key(worktree: &str, upstream: Option<&str>) -> Option<String> {
    let slug = superzej_core::util::slugify(worktree);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    put_proxy_key(worktree, &format!("szk-{slug}-{nanos}"), upstream)
}

/// The **stable** virtual-key id for a worktree's bouncer agent. Deterministic
/// (slug-only, no timestamp) so the launch path (which injects it into the
/// sealed container's env before the agent connects) and the disconnect path
/// (which revokes it) derive the same token without threading it through.
pub fn agent_proxy_key_id(worktree: &str) -> String {
    format!("szk-{}", superzej_core::util::slugify(worktree))
}

/// Mint the [`agent_proxy_key_id`] for `worktree` (best-effort). Upserts the
/// row, so relaunching the same worktree's agent reuses the one key.
pub fn mint_stable_proxy_key(worktree: &str, upstream: Option<&str>) -> Option<String> {
    let key = agent_proxy_key_id(worktree);
    put_proxy_key(worktree, &key, upstream)
}

/// Persist a virtual key row for `worktree` and return the token. The proxy
/// looks up identity by the token itself; the hash column is stored for parity
/// with the schema (lookups don't verify it for a local daemon).
fn put_proxy_key(worktree: &str, key: &str, upstream: Option<&str>) -> Option<String> {
    let db = Db::open().ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let token_hash = format!("{:016x}", hasher.finish());
    let scope = format!("worktree:{worktree}");
    db.put_proxy_virtual_key(
        key,
        &token_hash,
        &format!("agent {worktree}"),
        &scope,
        upstream,
        superzej_core::util::now(),
    )
    .ok()?;
    Some(key.to_string())
}

/// Revoke a virtual key minted by [`mint_agent_proxy_key`] (best-effort).
pub fn revoke_agent_proxy_key(key: &str) {
    if let Ok(db) = Db::open() {
        let _ = db.revoke_proxy_virtual_key(key, superzej_core::util::now());
    }
}
