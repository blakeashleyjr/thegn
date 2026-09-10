//! Construction of the API [`thegn_svc::provider::Provider`] from
//! an env's `[env.<name>.provider]` config — extracted from the pinned
//! `agent.rs` (kept flat); re-exported from `crate::agent` so call
//! sites are unchanged.

use thegn_svc::fly::{FlyProvider, FlySpec};
use thegn_svc::provider::{DaytonaProvider, Provider, SpritesProvider};

static MANAGED_KEY_SCOPE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Publish the configured managed-key scope for provider factories. Factories
/// are deliberately passed only an `EnvProviderConfig`; this process setting is
/// refreshed from the authoritative top-level config at startup.
pub(crate) fn install_managed_key_scope(scope: thegn_core::config::ManagedKeyScope) {
    MANAGED_KEY_SCOPE.store(
        u8::from(scope == thegn_core::config::ManagedKeyScope::PerAccount),
        std::sync::atomic::Ordering::Relaxed,
    );
}

fn managed_key_scope() -> thegn_core::config::ManagedKeyScope {
    if MANAGED_KEY_SCOPE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        thegn_core::config::ManagedKeyScope::Shared
    } else {
        thegn_core::config::ManagedKeyScope::PerAccount
    }
}

/// Stable, value-free identity of the provider account: the configured secret
/// ref name (or the provider's default token env ref). Two accounts using
/// different refs therefore cannot accidentally share a managed private key.
pub(crate) fn managed_key_account(pc: &thegn_core::config::EnvProviderConfig) -> String {
    use thegn_core::config::EnvProviderKind as K;
    let raw = if !pc.api_key_env.trim().is_empty() {
        pc.api_key_env.trim()
    } else {
        match K::of(&pc.provider) {
            K::Sprites => "SPRITES_TOKEN",
            K::Daytona => "DAYTONA_API_KEY",
            K::Hetzner => "HCLOUD_TOKEN",
            K::DigitalOcean => "DIGITALOCEAN_TOKEN",
            K::Fly => "FLY_API_TOKEN",
            K::Machine0 => "MACHINE0_API_KEY",
            K::Custom => "default",
        }
    };
    thegn_core::secretref::SecretRef::parse(raw, thegn_core::secretref::BareAs::EnvName)
        .audit_name()
}

/// Exact value-free custody namespace written by the service layer. This is
/// derived from the managed private-key basename so host lookups and provider
/// teardown always agree even when the configured account ref contains path
/// separators or other characters normalized for filesystem custody.
pub(crate) fn managed_custody_account(pc: &thegn_core::config::EnvProviderConfig) -> String {
    let basename =
        managed_key_scope().managed_key_basename(pc.provider.trim(), &managed_key_account(pc));
    thegn_core::managed_ssh::account_label(std::path::Path::new(&basename))
}

/// Candidate custody namespaces for an existing instance. The configured scope
/// controls new provisioning, but changing it must not strand a ledger-recorded
/// instance on the other namespace.
pub(crate) fn managed_custody_accounts(pc: &thegn_core::config::EnvProviderConfig) -> Vec<String> {
    let raw = managed_key_account(pc);
    let candidates = [
        managed_custody_account(pc),
        thegn_core::managed_ssh::account_label(std::path::Path::new(
            &thegn_core::config::ManagedKeyScope::PerAccount
                .managed_key_basename(pc.provider.trim(), &raw),
        )),
        "shared".to_string(),
    ];
    let mut accounts = Vec::new();
    for account in candidates {
        if !accounts.contains(&account) {
            accounts.push(account);
        }
    }
    accounts
}

pub(crate) fn managed_custody_record(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> anyhow::Result<Option<thegn_core::managed_ssh::AuthorizedKeyRecord>> {
    let mut matches = Vec::new();
    for account in managed_custody_accounts(pc) {
        if let Some(record) = thegn_core::managed_ssh::read(pc.provider.trim(), &account, name)? {
            matches.push(record);
        }
    }
    anyhow::ensure!(
        matches.len() <= 1,
        "managed SSH custody for {}/{name} is ambiguous across configured scope namespaces",
        pc.provider.trim()
    );
    Ok(matches.pop())
}

fn managed_keypair(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> anyhow::Result<(std::path::PathBuf, String)> {
    let provider = pc.provider.trim();
    if let Some(record) = managed_custody_record(pc, name)? {
        return crate::agent_ssh::existing_managed_ssh_keypair(&record.key_path);
    }
    if managed_key_scope() == thegn_core::config::ManagedKeyScope::PerAccount
        && thegn_svc::vps::registry::read(name)
            .is_some_and(|record| record.provider.eq_ignore_ascii_case(provider))
    {
        // VPS/Fly instances created before the custody ledger were authorized
        // with the historical shared key. The lifecycle registry proves that
        // the instance predates a per-account custody record, so retain its
        // working identity until explicit rotation/reprovision.
        return crate::agent::sprite_ssh_keypair();
    }
    crate::agent::managed_ssh_keypair(managed_key_scope(), provider, &managed_key_account(pc))
}

/// Build the API provider for an env's provider config (best-effort: `None` if
/// unconfigured or the token env var is unset). Mirrors `cmd::env::api_provider`
/// but infallible for the launch path.
pub(crate) fn provider_for(pc: &thegn_core::config::EnvProviderConfig) -> Option<Provider> {
    provider_for_named(pc, &pc.id)
}

/// Like [`provider_for`] but bakes an explicit sandbox **name** into the provider
/// instead of the raw configured `pc.id`. This matters for `create()`/
/// `ensure_exists()`, which name the new sandbox from the provider's own baked
/// name (not a call argument): the raw `pc.id` may be a per-worktree template
/// (`{worktree}`) or empty, so the caller must pass the resolved
/// [`effective_provider_id`](thegn_core::envbuild::effective_provider_id) to
/// create the correctly-named sandbox. Exec/read/write/destroy take the id as an
/// argument, so for those `provider_for` is equivalent.
pub(crate) fn provider_for_named(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> Option<Provider> {
    provider_for_named_with_key(pc, name, None)
}

/// Build a provider using an explicit managed keypair. Rotation uses this only
/// after authorizing the replacement public key, to prove an actual connection
/// with the replacement private key before retiring the old one.
pub(crate) fn provider_for_named_with_key(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    managed_key: Option<(std::path::PathBuf, String)>,
) -> Option<Provider> {
    // Exhaustive over the kind vocabulary: a new `EnvProviderKind` without a
    // factory arm is a compile error, never a silent `None`.
    use thegn_core::config::EnvProviderKind as K;
    match K::of(&pc.provider) {
        K::Sprites => {
            let key = if pc.api_key_env.trim().is_empty() {
                "SPRITES_TOKEN"
            } else {
                pc.api_key_env.trim()
            };
            let token = crate::secret::resolve_for(key, "provider:sprites")?;
            Some(Provider::Sprites(
                SpritesProvider::new(&pc.api_base, &token, name)
                    .with_custody_accounts(managed_custody_accounts(pc)),
            ))
        }
        K::Daytona => {
            let token = crate::secret::resolve_for(pc.api_key_env.trim(), "provider:daytona")?;
            Some(Provider::Daytona(DaytonaProvider::new(
                &pc.api_base,
                &token,
                &pc.template,
            )))
        }
        K::Hetzner | K::DigitalOcean => {
            vps_provider_for_with_key(pc, name, managed_key).map(Provider::Vps)
        }
        K::Fly => fly_provider_for_with_key(pc, name, managed_key).map(Provider::Fly),
        K::Machine0 => {
            machine0_provider_for_with_key(pc, name, managed_key).map(Provider::Machine0)
        }
        // exec_command-driven: no lifecycle API to build a provider for.
        K::Custom => None,
    }
}

/// Build the [`Machine0Provider`](thegn_svc::machine0::Machine0Provider) for a
/// `provider = "machine0"` env config. `None` when the api key can't be resolved
/// or the managed keypair can't be produced. Shared by the launch path and the
/// (future) reaper. machine0 is driven over its MCP endpoint (no CLI binary); the
/// pane/exec/file plane rides ssh with thegn's managed key.
pub(crate) fn machine0_provider_for(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> Option<thegn_svc::machine0::Machine0Provider> {
    machine0_provider_for_with_key(pc, name, None)
}

fn machine0_provider_for_with_key(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    managed_key: Option<(std::path::PathBuf, String)>,
) -> Option<thegn_svc::machine0::Machine0Provider> {
    let key = if pc.api_key_env.trim().is_empty() {
        "MACHINE0_API_KEY"
    } else {
        pc.api_key_env.trim()
    };
    let Some(api_key) = crate::secret::resolve_for(key, "provider:machine0") else {
        thegn_core::msg::warn(&format!(
            "machine0: API key {key} could not be resolved; cannot drive machine0"
        ));
        return None;
    };
    let (key_path, pubkey) = match managed_key
        .map(Ok)
        .unwrap_or_else(|| managed_keypair(pc, name))
    {
        Ok(k) => k,
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "machine0: managed ssh key generation failed ({e}); cannot drive machine0"
            ));
            return None;
        }
    };
    Some(thegn_svc::machine0::Machine0Provider::new(
        thegn_svc::machine0::Machine0Spec {
            endpoint: pc.api_base.clone(),
            api_key,
            name: name.to_string(),
            image: pc.template.clone(),
            size: pc.size.clone(),
            size_req: thegn_svc::machine0::SizeReq {
                min_vcpu: pc.min_vcpu,
                min_ram_gb: pc.min_ram_gb,
                min_disk_gb: pc.min_disk_gb,
                gpu: pc.gpu,
                nvme: pc.nvme,
            },
            region: pc.region.clone(),
            provision_flake: pc.provision_flake.clone(),
            ssh_user: String::new(),
            key_path,
            pubkey,
            max_instances: pc.max_instances,
            max_lifetime_secs: pc.max_lifetime_secs,
            skip_ready_wait: false,
        },
    ))
}

/// Build a [`FlyProvider`] for a `provider = "fly"` env config. `None` when the
/// token can't be resolved or the managed keypair can't be produced. Shared by
/// the launch path and the Fly reaper.
pub(crate) fn fly_provider_for(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> Option<FlyProvider> {
    fly_provider_for_with_key(pc, name, None)
}

fn fly_provider_for_with_key(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    managed_key: Option<(std::path::PathBuf, String)>,
) -> Option<FlyProvider> {
    let key = if pc.api_key_env.trim().is_empty() {
        "FLY_API_TOKEN"
    } else {
        pc.api_key_env.trim()
    };
    let Some(token) = crate::secret::resolve_for(key, "provider:fly") else {
        thegn_core::msg::warn(&format!(
            "fly: API token {key} could not be resolved; cannot drive fly"
        ));
        return None;
    };
    let (key_path, pubkey) = match managed_key
        .map(Ok)
        .unwrap_or_else(|| managed_keypair(pc, name))
    {
        Ok(k) => k,
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "fly: managed ssh key generation failed ({e}); cannot drive fly"
            ));
            return None;
        }
    };
    // org defaults to "personal"; template ⇒ image. A stopped Fly machine is
    // near-free, so it's scale-to-zero.
    Some(FlyProvider::new(FlySpec {
        api_base: pc.api_base.clone(),
        graphql_url: String::new(),
        token,
        org_slug: String::new(),
        name: name.to_string(),
        region: pc.region.clone(),
        size: pc.size.clone(),
        image: pc.template.clone(),
        max_instances: pc.max_instances,
        max_lifetime_secs: pc.max_lifetime_secs,
        key_path,
        pubkey,
        // iroh call-home: when the reach is enabled, start the home endpoint, mint
        // this sandbox's auth token, and inject the three `THEGN_*` vars the
        // baked `tg-agent` reads on boot to dial home. `None` (disabled) keeps the
        // reaper/launch construction on today's ssh/IPv4-only path — additive, so
        // ssh readiness/provisioning is unchanged and iroh only carries panes.
        iroh: crate::iroh_home::injection_for(name),
        skip_ready_wait: false,
    }))
}

/// Build the [`VpsProvider`](thegn_svc::vps::VpsProvider) for a VPS-kind
/// provider config. `None` when the token env var is unset or the managed
/// keypair can't be produced (both warned once at provision time by callers
/// that surface errors; the launch path stays best-effort).
pub(crate) fn vps_provider_for(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> Option<thegn_svc::vps::VpsProvider> {
    vps_provider_for_with_key(pc, name, None)
}

fn vps_provider_for_with_key(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    managed_key: Option<(std::path::PathBuf, String)>,
) -> Option<thegn_svc::vps::VpsProvider> {
    let kind = thegn_svc::vps::VpsKind::parse(&pc.provider)?;
    let key_env = if pc.api_key_env.trim().is_empty() {
        kind.token_env_default()
    } else {
        pc.api_key_env.trim()
    };
    let consumer = format!("provider:{}", pc.provider.trim());
    let Some(token) = crate::secret::resolve_for(key_env, &consumer) else {
        thegn_core::msg::warn(&format!(
            "{}: API token {key_env} could not be resolved; cannot drive {}",
            pc.provider, pc.provider
        ));
        return None;
    };
    let (key_path, pubkey) = match managed_key
        .map(Ok)
        .unwrap_or_else(|| managed_keypair(pc, name))
    {
        Ok(k) => k,
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "vps: managed ssh key generation failed ({e}); cannot drive {}",
                pc.provider
            ));
            return None;
        }
    };
    Some(thegn_svc::vps::VpsProvider::new(thegn_svc::vps::VpsSpec {
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
    }))
}

/// The resolved provider sandbox NAME for a worktree's env — the single source of
/// truth. Resolves the env exactly as the pane path does (`resolve_env` →
/// `ProviderPlacement.id`) so provisioning, attach (`native_shell_exec`),
/// checkpoint, and teardown all compute the SAME name (the id embeds a stable
/// path-hash; deriving it inconsistently would orphan/leak sandboxes). `None` for
/// a non-provider env. Mirrors how the other launch paths resolve `repo_root`.
pub(crate) fn provider_sandbox_name(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    env_name: &str,
) -> Option<String> {
    use std::path::{Path, PathBuf};
    use thegn_core::store::{PoolStore, WorkspaceStore};
    let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = thegn_core::db::Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| thegn_core::repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), Some(env_name));
    match env.placement {
        thegn_core::placement::Placement::Provider(p) => {
            // If this worktree CLAIMED a warm-pool spare, its sandbox is that
            // spare's name (a DB binding), which overrides the derived id — so all
            // lifecycle/exec calls target the handed-over sandbox. Else the derived
            // `effective_provider_id`.
            let bound = thegn_core::db::Db::open()
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
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "nope".into(),
            ..Default::default()
        };
        assert!(provider_for_named(&pc, "x").is_none());
        // A VPS kind without its token env set is None (best-effort launch path).
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "hetzner".into(),
            api_key_env: "TG_TEST_NO_SUCH_HCLOUD_TOKEN".into(),
            ..Default::default()
        };
        assert!(provider_for_named(&pc, "x").is_none());
        // machine0 without its api key env set is None (best-effort launch path).
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "machine0".into(),
            api_key_env: "TG_TEST_NO_SUCH_MACHINE0_KEY".into(),
            ..Default::default()
        };
        assert!(provider_for_named(&pc, "x").is_none());
    }
}
