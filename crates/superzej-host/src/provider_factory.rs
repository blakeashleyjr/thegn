//! Construction of the API [`superzej_svc::provider::Provider`] from
//! an env's `[env.<name>.provider]` config — extracted from the pinned
//! `agent.rs` (file-size ratchet); re-exported from `crate::agent` so call
//! sites are unchanged.

use superzej_svc::provider::{DaytonaProvider, Provider, SpritesProvider};

/// Build the API provider for an env's provider config (best-effort: `None` if
/// unconfigured or the token env var is unset). Mirrors `cmd::env::api_provider`
/// but infallible for the launch path.
pub(crate) fn provider_for(pc: &superzej_core::config::EnvProviderConfig) -> Option<Provider> {
    provider_for_named(pc, &pc.id)
}

/// Like [`provider_for`] but bakes an explicit sandbox **name** into the provider
/// instead of the raw configured `pc.id`. This matters for `create()`/
/// `ensure_exists()`, which name the new sandbox from the provider's own baked
/// name (not a call argument): the raw `pc.id` may be a per-worktree template
/// (`{worktree}`) or empty, so the caller must pass the resolved
/// [`effective_provider_id`](superzej_core::envbuild::effective_provider_id) to
/// create the correctly-named sandbox. Exec/read/write/destroy take the id as an
/// argument, so for those `provider_for` is equivalent.
pub(crate) fn provider_for_named(
    pc: &superzej_core::config::EnvProviderConfig,
    name: &str,
) -> Option<Provider> {
    match pc.provider.as_str() {
        "sprites" => {
            let key = if pc.api_key_env.trim().is_empty() {
                "SPRITES_TOKEN"
            } else {
                pc.api_key_env.trim()
            };
            let token = std::env::var(key).ok()?;
            Some(Provider::Sprites(SpritesProvider::new(
                &pc.api_base,
                &token,
                name,
            )))
        }
        "daytona" => {
            let token = std::env::var(pc.api_key_env.trim()).ok()?;
            Some(Provider::Daytona(DaytonaProvider::new(
                &pc.api_base,
                &token,
                &pc.template,
            )))
        }
        vps if superzej_core::config::vps_provider_kind(vps) => {
            vps_provider_for(pc, name).map(Provider::Vps)
        }
        _ => None,
    }
}

/// Build the [`VpsProvider`](superzej_svc::vps::VpsProvider) for a VPS-kind
/// provider config. `None` when the token env var is unset or the managed
/// keypair can't be produced (both warned once at provision time by callers
/// that surface errors; the launch path stays best-effort).
pub(crate) fn vps_provider_for(
    pc: &superzej_core::config::EnvProviderConfig,
    name: &str,
) -> Option<superzej_svc::vps::VpsProvider> {
    let kind = superzej_svc::vps::VpsKind::parse(&pc.provider)?;
    let key_env = if pc.api_key_env.trim().is_empty() {
        kind.token_env_default()
    } else {
        pc.api_key_env.trim()
    };
    let token = std::env::var(key_env).ok()?;
    // The same managed keypair the sprite ssh transport uses — one key for all
    // superzej-managed remotes.
    let (key_path, pubkey) = match crate::agent::sprite_ssh_keypair() {
        Ok(k) => k,
        Err(e) => {
            superzej_core::msg::warn(&format!(
                "vps: managed ssh key generation failed ({e}); cannot drive {}",
                pc.provider
            ));
            return None;
        }
    };
    Some(superzej_svc::vps::VpsProvider::new(
        superzej_svc::vps::VpsSpec {
            kind,
            api_base: pc.api_base.clone(),
            token,
            name: name.to_string(),
            region: pc.region.clone(),
            size: pc.size.clone(),
            image: pc.template.clone(),
            max_instances: pc.max_instances,
            max_lifetime_secs: pc.max_lifetime_secs,
            key_path,
            pubkey,
            skip_ready_wait: false,
        },
    ))
}

/// The resolved provider sandbox NAME for a worktree's env — the single source of
/// truth. Resolves the env exactly as the pane path does (`resolve_env` →
/// `ProviderPlacement.id`) so provisioning, attach (`native_shell_exec`),
/// checkpoint, and teardown all compute the SAME name (the id embeds a stable
/// path-hash; deriving it inconsistently would orphan/leak sandboxes). `None` for
/// a non-provider env. Mirrors how the other launch paths resolve `repo_root`.
pub(crate) fn provider_sandbox_name(
    cfg: &superzej_core::config::Config,
    worktree: &str,
    env_name: &str,
) -> Option<String> {
    use std::path::{Path, PathBuf};
    use superzej_core::store::{PoolStore, WorkspaceStore};
    let loc = superzej_core::remote::GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = superzej_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| superzej_core::repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), Some(env_name));
    match env.placement {
        superzej_core::placement::Placement::Provider(p) => {
            // If this worktree CLAIMED a warm-pool spare, its sandbox is that
            // spare's name (a DB binding), which overrides the derived id — so all
            // lifecycle/exec calls target the handed-over sandbox. Else the derived
            // `effective_provider_id`.
            let bound = superzej_core::db::Db::open()
                .ok()
                .and_then(|db| db.worktree_provider_sandbox(worktree).ok().flatten());
            Some(bound.unwrap_or(p.id))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_provider_and_missing_token_yield_none() {
        let pc = superzej_core::config::EnvProviderConfig {
            provider: "nope".into(),
            ..Default::default()
        };
        assert!(provider_for_named(&pc, "x").is_none());
        // A VPS kind without its token env set is None (best-effort launch path).
        let pc = superzej_core::config::EnvProviderConfig {
            provider: "hetzner".into(),
            api_key_env: "SZ_TEST_NO_SUCH_HCLOUD_TOKEN".into(),
            ..Default::default()
        };
        assert!(provider_for_named(&pc, "x").is_none());
    }
}
