//! Construction of the API [`thegn_svc::provider::Provider`] from
//! an env's `[env.<name>.provider]` config — extracted from the pinned
//! `agent.rs` (kept flat); re-exported from `crate::agent` so call
//! sites are unchanged.

use anyhow::Context;
use thegn_svc::fly::{FlyProvider, FlySpec};
use thegn_svc::provider::{DaytonaProvider, Provider, SpritesProvider};

const UNREADABLE_BINDING_CONTEXT: &str = concat!(
    "the worktree's warm-pool sandbox binding is unreadable; ",
    "refusing to fall back to the derived sandbox id",
);

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
            K::Vercel => "VERCEL_TOKEN",
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
        && thegn_svc::vps::registry::read_checked(name)
            .with_context(|| {
                format!(
                    "the VPS ledger record for {name} is unreadable; refusing to pick its SSH key"
                )
            })?
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

pub(crate) fn provider_for_named_owned(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    owner: Option<&str>,
) -> Option<Provider> {
    let provider = provider_for_named(pc, name)?;
    Some(match (provider, owner) {
        (Provider::Vps(provider), Some(owner)) => {
            Provider::Vps(provider.with_owner(owner.to_owned()))
        }
        (Provider::Fly(provider), Some(owner)) => {
            Provider::Fly(provider.with_owner(owner.to_owned()))
        }
        (Provider::Vercel(provider), Some(owner)) => {
            Provider::Vercel(provider.with_owner(owner.to_owned()))
        }
        (provider, _) => provider,
    })
}

/// Build an account-level provider for fleet INVENTORY reads only: no
/// per-sandbox name, no managed ssh key (empty key path / pubkey placeholders)
/// and no iroh injection. Never use the result to exec or create: those paths
/// need [`provider_for_named`]. Unlike it, the error names the real cause and
/// nothing is warned.
pub(crate) fn provider_for_inventory(
    pc: &thegn_core::config::EnvProviderConfig,
) -> Result<Provider, String> {
    use thegn_core::config::EnvProviderKind as K;
    let no_key = std::path::PathBuf::new();
    match K::of(&pc.provider) {
        K::Sprites => {
            let key = if pc.api_key_env.trim().is_empty() {
                "SPRITES_TOKEN"
            } else {
                pc.api_key_env.trim()
            };
            let token = crate::secret::resolve_for(key, "provider:sprites")
                .ok_or_else(|| format!("API token {key} could not be resolved"))?;
            Ok(Provider::Sprites(sprites_policy(
                SpritesProvider::new(&pc.api_base, &token, "")
                    .with_custody_accounts(managed_custody_accounts(pc)),
                pc,
                key,
            )))
        }
        K::Daytona => {
            let key = pc.api_key_env.trim();
            let token = crate::secret::resolve_for(key, "provider:daytona")
                .ok_or_else(|| format!("API token {key} could not be resolved"))?;
            Ok(Provider::Daytona(DaytonaProvider::new(
                &pc.api_base,
                &token,
                &pc.template,
            )))
        }
        K::Hetzner | K::DigitalOcean => {
            let (kind, key_env, token) = vps_token(pc)?;
            Ok(Provider::Vps(
                thegn_svc::vps::VpsProvider::new(thegn_svc::vps::VpsSpec {
                    kind,
                    api_base: pc.api_base.clone(),
                    token,
                    name: String::new(),
                    region: pc.region.clone(),
                    size: pc.size.clone(),
                    image: pc.template.clone(),
                    max_instances: pc.max_instances,
                    max_lifetime_secs: pc.max_lifetime_secs,
                    key_path: no_key,
                    pubkey: String::new(),
                    skip_ready_wait: false,
                })
                .with_credential_ref(&key_env),
            ))
        }
        K::Fly => {
            let (key, token) = fly_token(pc)?;
            Ok(Provider::Fly(
                FlyProvider::new(FlySpec {
                    api_base: pc.api_base.clone(),
                    graphql_url: String::new(),
                    token,
                    org_slug: String::new(),
                    name: String::new(),
                    region: pc.region.clone(),
                    size: pc.size.clone(),
                    image: pc.template.clone(),
                    max_instances: pc.max_instances,
                    max_lifetime_secs: pc.max_lifetime_secs,
                    key_path: no_key,
                    pubkey: String::new(),
                    iroh: None,
                    skip_ready_wait: false,
                })
                .with_credential_ref(&key),
            ))
        }
        K::Machine0 => {
            let (_, api_key) = machine0_token(pc)?;
            Ok(Provider::Machine0(
                thegn_svc::machine0::Machine0Provider::new(thegn_svc::machine0::Machine0Spec {
                    endpoint: pc.api_base.clone(),
                    api_key,
                    name: String::new(),
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
                    key_path: no_key,
                    pubkey: String::new(),
                    max_instances: pc.max_instances,
                    max_lifetime_secs: pc.max_lifetime_secs,
                    skip_ready_wait: false,
                }),
            ))
        }
        K::Vercel => {
            let (key, token) = vercel_token(pc)?;
            vercel_provider_from(pc, "", &key, token).map(Provider::Vercel)
        }
        K::Custom => Err("custom provider has no inventory API".to_string()),
    }
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
            Some(Provider::Sprites(sprites_policy(
                SpritesProvider::new(&pc.api_base, &token, name)
                    .with_custody_accounts(managed_custody_accounts(pc)),
                pc,
                key,
            )))
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
        // No ssh: a managed keypair has no meaning for vercel.
        K::Vercel => vercel_provider_for(pc, name).map(Provider::Vercel),
        // exec_command-driven: no lifecycle API to build a provider for.
        K::Custom => None,
    }
}

/// The ownership scope, memory policy and instance cap every sprites provider
/// carries. The scope is the controller id plus the credential-free account
/// fingerprint of `credential_ref` (the Vercel derivation); when the controller
/// id cannot be loaded the scope stays unset, so create and destroy refuse
/// rather than act on an unrecorded sprite.
///
/// The legacy ids are left empty: only a ledger record this host wrote before
/// the create proves a sprite ours (a worktree location row proves nothing).
pub(crate) fn sprites_policy(
    provider: SpritesProvider,
    pc: &thegn_core::config::EnvProviderConfig,
    credential_ref: &str,
) -> SpritesProvider {
    let provider = provider
        .with_memory_limit_mb(pc.min_ram_gb.saturating_mul(1024))
        .with_max_instances(pc.max_instances)
        .with_legacy(Vec::new());
    let owner = match thegn_svc::vps::controller::controller_id() {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(target: "thegn::sprites", %error, "sprites: no controller id; create and destroy will refuse");
            return provider;
        }
    };
    match provider.account(credential_ref) {
        Ok(account) => {
            provider.with_scope(thegn_svc::sprites::ledger::OwnerScope { owner, account })
        }
        Err(error) => {
            tracing::warn!(target: "thegn::sprites", %error, "sprites: no account fingerprint; create and destroy will refuse");
            provider
        }
    }
}

/// The exact token ref string [`provider_for_named_with_key`] resolves for
/// `pc`: the trimmed `api_key_env`, else the kind's default. Daytona has no
/// default there, so an empty `api_key_env` yields `Some("")`. `None` for kinds
/// that need no token (custom, or an unknown kind).
pub(crate) fn token_ref(pc: &thegn_core::config::EnvProviderConfig) -> Option<String> {
    use thegn_core::config::EnvProviderKind as K;
    let configured = pc.api_key_env.trim();
    let default = match K::of(&pc.provider) {
        K::Sprites => "SPRITES_TOKEN",
        K::Daytona => "",
        K::Hetzner | K::DigitalOcean => {
            thegn_svc::vps::VpsKind::parse(&pc.provider)?.token_env_default()
        }
        K::Fly => "FLY_API_TOKEN",
        K::Machine0 => "MACHINE0_API_KEY",
        K::Vercel => "VERCEL_TOKEN",
        K::Custom => return None,
    };
    Some(
        if configured.is_empty() {
            default
        } else {
            configured
        }
        .to_string(),
    )
}

/// Why [`provider_for_named`] returned `None` for env `name`, for a CLI error.
/// When the token ref does not resolve, the message names the ref and the
/// paths/variables tried (THE-2016); never a secret value.
pub(crate) fn provider_unavailable_reason(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> String {
    match token_ref(pc) {
        Some(r) if r.is_empty() => format!(
            "env '{name}': no API token reference configured (set api_key_env in [env.{name}.provider])"
        ),
        Some(r) if crate::secret::resolve_for(&r, "explain").is_none() => format!(
            "env '{name}': API token {r} could not be resolved: {}",
            crate::secret::explain_unresolved(&r)
        ),
        _ => format!("env '{name}' has no API provider or its token could not be resolved"),
    }
}

/// Build the [`Machine0Provider`](thegn_svc::machine0::Machine0Provider) for a
/// `provider = "machine0"` env config. `None` when the api key can't be resolved
/// or the managed keypair can't be produced. Shared by the launch path.
/// machine0 is driven over its MCP endpoint (no CLI binary); the
/// pane/exec/file plane rides ssh with thegn's managed key.
pub(crate) fn machine0_provider_for(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> Option<thegn_svc::machine0::Machine0Provider> {
    machine0_provider_for_with_key(pc, name, None)
}

/// Resolve the machine0 API key: `(env ref, secret)`; the error names the ref.
fn machine0_token(pc: &thegn_core::config::EnvProviderConfig) -> Result<(String, String), String> {
    let key = if pc.api_key_env.trim().is_empty() {
        "MACHINE0_API_KEY"
    } else {
        pc.api_key_env.trim()
    };
    crate::secret::resolve_for(key, "provider:machine0")
        .map(|token| (key.to_string(), token))
        .ok_or_else(|| format!("API token {key} could not be resolved"))
}

/// Resolve the Fly API token: `(env ref, secret)`; the error names the ref.
fn fly_token(pc: &thegn_core::config::EnvProviderConfig) -> Result<(String, String), String> {
    let key = if pc.api_key_env.trim().is_empty() {
        "FLY_API_TOKEN"
    } else {
        pc.api_key_env.trim()
    };
    crate::secret::resolve_for(key, "provider:fly")
        .map(|token| (key.to_string(), token))
        .ok_or_else(|| format!("API token {key} could not be resolved"))
}

/// Resolve the Vercel API token (or OIDC token) at call time through the one
/// audited chokepoint: `(env ref, secret)`; the error names the ref only.
fn vercel_token(pc: &thegn_core::config::EnvProviderConfig) -> Result<(String, String), String> {
    let key = if pc.api_key_env.trim().is_empty() {
        "VERCEL_TOKEN"
    } else {
        pc.api_key_env.trim()
    };
    crate::secret::resolve_for(key, "provider:vercel")
        .map(|token| (key.to_string(), token))
        .ok_or_else(|| format!("API token {key} could not be resolved"))
}

/// Build the [`VercelProvider`](thegn_svc::vercel::VercelProvider) for a
/// `provider = "vercel"` env config, resolving the token at call time. `None`
/// (with a value-free warning) when the token can't be resolved or the spec is
/// refused. The token moves straight into the provider's client.
pub(crate) fn vercel_provider_for(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
) -> Option<thegn_svc::vercel::VercelProvider> {
    let built =
        vercel_token(pc).and_then(|(key, token)| vercel_provider_from(pc, name, &key, token));
    match built {
        Ok(provider) => Some(provider),
        Err(why) => {
            thegn_core::msg::warn(&format!("vercel: {why}; cannot drive vercel"));
            None
        }
    }
}

/// The `VercelSpec` for `pc` with an already-resolved token. Errors name a
/// config field or the ref, never the token.
fn vercel_provider_from(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    key: &str,
    token: String,
) -> Result<thegn_svc::vercel::VercelProvider, String> {
    use thegn_core::vercel::policy;
    let vcpus = policy::vcpus_for(pc.size.trim(), pc.min_vcpu)
        .map_err(|e| format!("provider.{}: {}", e.field, e.reason))?;
    let non_empty = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    thegn_svc::vercel::VercelProvider::new(thegn_svc::vercel::VercelSpec {
        api_base: pc.api_base.trim().to_string(),
        token: thegn_svc::vercel::VercelToken::new(token),
        credential_ref: key.to_string(),
        scope: thegn_core::vercel::api::Scope {
            team_id: pc.team_id.trim().to_string(),
            project_id: pc.project_id.trim().to_string(),
        },
        name: name.to_string(),
        vcpus,
        image: non_empty(pc.template.as_str()),
        region: non_empty(pc.region.as_str()),
        max_instances: policy::max_instances(pc.max_instances),
        max_lifetime_secs: policy::max_lifetime_secs(pc.max_lifetime_secs),
        interactive_host_suffixes: pc.interactive_host_suffixes.clone(),
        model_credentials: pc.model_credentials,
        egress: thegn_core::vercel::egress::resolve(pc.egress, &pc.egress_allow)
            .map_err(|e| format!("provider.{}: {}", e.field, e.reason))?,
    })
    .map_err(|_| "the vercel provider could not be built (check api_base)".to_string())
}

/// Resolve a VPS-kind API token: `(kind, env ref, secret)`.
fn vps_token(
    pc: &thegn_core::config::EnvProviderConfig,
) -> Result<(thegn_svc::vps::VpsKind, String, String), String> {
    let kind = thegn_svc::vps::VpsKind::parse(&pc.provider)
        .ok_or_else(|| format!("unknown provider kind {:?}", pc.provider))?;
    let key_env = if pc.api_key_env.trim().is_empty() {
        kind.token_env_default()
    } else {
        pc.api_key_env.trim()
    };
    let consumer = format!("provider:{}", pc.provider.trim());
    crate::secret::resolve_for(key_env, &consumer)
        .map(|token| (kind, key_env.to_string(), token))
        .ok_or_else(|| format!("API token {key_env} could not be resolved"))
}

fn machine0_provider_for_with_key(
    pc: &thegn_core::config::EnvProviderConfig,
    name: &str,
    managed_key: Option<(std::path::PathBuf, String)>,
) -> Option<thegn_svc::machine0::Machine0Provider> {
    let api_key = match machine0_token(pc) {
        Ok((_, api_key)) => api_key,
        Err(why) => {
            thegn_core::msg::warn(&format!("machine0: {why}; cannot drive machine0"));
            return None;
        }
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
    let (key, token) = match fly_token(pc) {
        Ok(t) => t,
        Err(why) => {
            thegn_core::msg::warn(&format!("fly: {why}; cannot drive fly"));
            return None;
        }
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
    Some(
        FlyProvider::new(FlySpec {
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
        })
        .with_credential_ref(&key),
    )
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
    let (kind, key_env, token) = match vps_token(pc) {
        Ok(t) => t,
        Err(why) => {
            thegn_core::msg::warn(&format!(
                "{}: {why}; cannot drive {}",
                pc.provider, pc.provider
            ));
            return None;
        }
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
    Some(
        thegn_svc::vps::VpsProvider::new(thegn_svc::vps::VpsSpec {
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
        })
        .with_credential_ref(&key_env),
    )
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
    match provider_sandbox_name_checked(cfg, worktree, env_name) {
        Ok(name) => name,
        Err(error) => {
            tracing::warn!(
                target: "thegn::provider",
                error = %format!("{error:#}"),
                worktree,
                "provider sandbox binding unreadable; no sandbox target"
            );
            None
        }
    }
}

pub(crate) fn provider_sandbox_name_checked(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    env_name: &str,
) -> anyhow::Result<Option<String>> {
    let Some((env, bound)) = resolve_provider_env(cfg, worktree, env_name)? else {
        return Ok(None);
    };
    match env.placement {
        thegn_core::placement::Placement::Provider(p) => {
            // If this worktree CLAIMED a warm-pool spare, its sandbox is that
            // spare's name (a DB binding), which overrides the derived id — so all
            // lifecycle/exec calls target the handed-over sandbox. Else the derived
            // `effective_provider_id`.
            Ok(Some(bound.unwrap_or(p.id)))
        }
        _ => Ok(None),
    }
}

pub(crate) fn provider_sandbox_owner(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    env_name: &str,
) -> Option<String> {
    let (env, bound) = match resolve_provider_env(cfg, worktree, env_name) {
        Ok(Some(resolved)) => resolved,
        Ok(None) => return None,
        Err(error) => {
            tracing::debug!(target: "thegn::provider", error = %format!("{error:#}"), worktree, "provider sandbox owner binding unreadable");
            return None;
        }
    };
    let configured = configured_provider_id(cfg, &env.name);
    derived_owner(configured, bound.is_some(), worktree)
}

fn configured_provider_id<'a>(cfg: &'a thegn_core::config::Config, env_name: &str) -> &'a str {
    cfg.env
        .get(env_name)
        .map(|e| e.provider.id.as_str())
        .unwrap_or("")
}

fn resolve_provider_env(
    cfg: &thegn_core::config::Config,
    worktree: &str,
    env_name: &str,
) -> anyhow::Result<Option<(thegn_core::env::Environment, Option<String>)>> {
    use std::path::{Path, PathBuf};
    use thegn_core::store::{PoolStore, WorkspaceStore};
    let loc = match thegn_core::remote::GitLoc::for_worktree_op(
        Path::new(worktree),
        "provider sandbox resolution",
    ) {
        Ok(loc) => loc,
        Err(_)
            if !cfg
                .env
                .get(env_name)
                .is_some_and(|e| e.placement == thegn_core::config::PlacementMode::Provider) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(anyhow::anyhow!(error)),
    };
    let db = thegn_core::db::Db::open();
    let repo_root = match db.as_ref().ok() {
        Some(db) => match db.repo_root_for(worktree) {
            Ok(Some(root)) if !root.is_empty() => PathBuf::from(root),
            Ok(_) => thegn_core::repo::main_worktree(Path::new(worktree))
                .unwrap_or_else(|| PathBuf::from(worktree)),
            Err(error) => {
                tracing::debug!(target: "thegn::provider", %error, "provider sandbox repository lookup failed");
                thegn_core::repo::main_worktree(Path::new(worktree))
                    .unwrap_or_else(|| PathBuf::from(worktree))
            }
        },
        None => thegn_core::repo::main_worktree(Path::new(worktree))
            .unwrap_or_else(|| PathBuf::from(worktree)),
    };
    let env = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        &thegn_core::sandbox_pin::EnvSelection::named(env_name),
    );
    if !matches!(
        &env.placement,
        thegn_core::placement::Placement::Provider(_)
    ) {
        return Ok(None);
    }
    let db = db.context("open the state database to read the warm-pool sandbox binding")?;
    let bound = db
        .worktree_provider_sandbox(worktree)
        .context(UNREADABLE_BINDING_CONTEXT)?;
    Ok(Some((env, bound)))
}

pub(crate) fn bound_provider_sandbox(worktree: &str) -> anyhow::Result<Option<String>> {
    use thegn_core::store::PoolStore;
    let db = thegn_core::db::Db::open()
        .context("open the state database to read the warm-pool sandbox binding")?;
    db.worktree_provider_sandbox(worktree)
}

fn sandbox_id_from(
    binding: anyhow::Result<Option<String>>,
    derived: &str,
) -> anyhow::Result<String> {
    match binding {
        Ok(Some(bound)) => Ok(bound),
        Ok(None) => Ok(derived.to_string()),
        Err(error) => Err(error.context(UNREADABLE_BINDING_CONTEXT)),
    }
}

pub(crate) fn sandbox_id_for(worktree: &str, derived: &str) -> anyhow::Result<String> {
    sandbox_id_from(bound_provider_sandbox(worktree), derived)
}

fn derived_owner(configured_id: &str, claimed: bool, worktree: &str) -> Option<String> {
    if claimed || (!configured_id.trim().is_empty() && !configured_id.contains('{')) {
        None
    } else {
        Some(thegn_svc::vps::registry::worktree_owner(
            worktree.as_bytes(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_provider_needs_only_a_token() {
        let _state = crate::testenv::IsolatedState::with_env(&[("TG_TEST_INV_TOKEN", "tok")]);
        for kind in ["fly", "hetzner", "machine0"] {
            let pc = thegn_core::config::EnvProviderConfig {
                provider: kind.into(),
                api_key_env: "TG_TEST_INV_TOKEN".into(),
                ..Default::default()
            };
            assert!(provider_for_inventory(&pc).is_ok(), "{kind}");
        }
        // Vercel is the exception: every Vercel Sandbox API call is scoped by
        // `teamId` and `projectId`, so listing sandboxes needs both ids as
        // well as the token, and a token alone is refused.
        let vercel = |team_id: &str, project_id: &str| thegn_core::config::EnvProviderConfig {
            provider: "vercel".into(),
            api_key_env: "TG_TEST_INV_TOKEN".into(),
            team_id: team_id.into(),
            project_id: project_id.into(),
            ..Default::default()
        };
        assert!(provider_for_inventory(&vercel("team_A1", "prj_B2")).is_ok());
        assert!(provider_for_inventory(&vercel("", "")).is_err());
    }

    #[test]
    fn sprites_factory_scopes_and_sizes_the_provider() {
        let _state =
            crate::testenv::IsolatedState::with_env(&[("TG_TEST_SPRITES_SCOPE_TOKEN", "tok")]);
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "sprites".into(),
            api_key_env: "TG_TEST_SPRITES_SCOPE_TOKEN".into(),
            min_ram_gb: 2,
            ..Default::default()
        };
        let Some(Provider::Sprites(named)) = provider_for_named(&pc, "tg-sprite-a") else {
            panic!("a sprites provider");
        };
        let scope = named
            .scope()
            .expect("the factory scopes every sprites provider");
        assert_eq!(
            scope.owner,
            thegn_svc::vps::controller::controller_id().unwrap()
        );
        assert_eq!(
            scope.account,
            named.account("TG_TEST_SPRITES_SCOPE_TOKEN").unwrap()
        );
        assert!(scope.account.starts_with("acct1-"), "{}", scope.account);
        assert_eq!(named.memory_limit_mb(), 2048, "GiB x 1024");
        let Ok(Provider::Sprites(inventory)) = provider_for_inventory(&pc) else {
            panic!("a sprites inventory provider");
        };
        assert_eq!(
            inventory.scope(),
            Some(scope),
            "one scope for both factory paths"
        );
    }

    #[test]
    fn inventory_provider_error_names_the_cause() {
        let _state = crate::testenv::IsolatedState::new();
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "fly".into(),
            api_key_env: "TG_TEST_INV_NO_SUCH_TOKEN".into(),
            ..Default::default()
        };
        let err = provider_for_inventory(&pc).err().expect("no token");
        assert!(err.contains("TG_TEST_INV_NO_SUCH_TOKEN"), "{err}");
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "nope".into(),
            ..Default::default()
        };
        let err = provider_for_inventory(&pc).err().expect("unknown");
        assert!(
            err.contains("unknown provider kind") || err.contains("custom"),
            "{err}"
        );
    }

    #[test]
    fn token_ref_matches_each_factory_arm() {
        let pc = |provider: &str, key: &str| thegn_core::config::EnvProviderConfig {
            provider: provider.into(),
            api_key_env: key.into(),
            ..Default::default()
        };
        assert_eq!(
            token_ref(&pc("sprites", "")).as_deref(),
            Some("SPRITES_TOKEN")
        );
        assert_eq!(
            token_ref(&pc("sprites", "  file:~/.secrets/sprites  ")).as_deref(),
            Some("file:~/.secrets/sprites")
        );
        assert_eq!(token_ref(&pc("daytona", "")).as_deref(), Some(""));
        assert_eq!(
            token_ref(&pc("daytona", "DT_KEY")).as_deref(),
            Some("DT_KEY")
        );
        assert_eq!(token_ref(&pc("fly", "")).as_deref(), Some("FLY_API_TOKEN"));
        assert_eq!(
            token_ref(&pc("machine0", "")).as_deref(),
            Some("MACHINE0_API_KEY")
        );
        assert_eq!(
            token_ref(&pc("hetzner", "")).as_deref(),
            Some(thegn_svc::vps::VpsKind::Hetzner.token_env_default())
        );
        assert_eq!(
            token_ref(&pc("digitalocean", "")).as_deref(),
            Some(thegn_svc::vps::VpsKind::DigitalOcean.token_env_default())
        );
        assert_eq!(token_ref(&pc("custom", "")), None);
        assert_eq!(token_ref(&pc("custom", "IGNORED")), None);
    }

    #[test]
    fn vercel_token_ref_and_key_account_default_to_vercel_token() {
        let pc = |key: &str| thegn_core::config::EnvProviderConfig {
            provider: "vercel".into(),
            api_key_env: key.into(),
            ..Default::default()
        };
        assert_eq!(token_ref(&pc("")).as_deref(), Some("VERCEL_TOKEN"));
        assert_eq!(
            token_ref(&pc(" keyring:vercel ")).as_deref(),
            Some("keyring:vercel")
        );
        assert_eq!(managed_key_account(&pc("")), "env:VERCEL_TOKEN");
    }

    #[test]
    fn vercel_unresolved_token_names_the_ref_only() {
        let _state = crate::testenv::IsolatedState::new();
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "vercel".into(),
            api_key_env: "TG_TEST_VERCEL_NO_SUCH_TOKEN".into(),
            ..Default::default()
        };
        let err = provider_for_inventory(&pc).err().expect("no token");
        assert!(err.contains("TG_TEST_VERCEL_NO_SUCH_TOKEN"), "{err}");
        assert!(vercel_provider_for(&pc, "tg-a").is_none());
    }

    #[test]
    fn provider_unavailable_reason_names_the_ref_and_paths() {
        let state = crate::testenv::IsolatedState::new();
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "sprites".into(),
            api_key_env: "file:~/.secrets/tg-b145-none".into(),
            ..Default::default()
        };
        let reason = provider_unavailable_reason(&pc, "sprites");
        let expanded = state.root().join(".secrets/tg-b145-none");
        assert!(
            reason.starts_with(
                "env 'sprites': API token file:~/.secrets/tg-b145-none could not be resolved: token file not found (tried "
            ),
            "{reason}"
        );
        assert!(reason.contains(&*expanded.to_string_lossy()), "{reason}");

        let env = thegn_core::config::EnvProviderConfig {
            provider: "fly".into(),
            api_key_env: "TG_B145_NO_SUCH_FLY_TOKEN".into(),
            ..Default::default()
        };
        assert_eq!(
            provider_unavailable_reason(&env, "f"),
            "env 'f': API token TG_B145_NO_SUCH_FLY_TOKEN could not be resolved: environment variable TG_B145_NO_SUCH_FLY_TOKEN is not set in this process"
        );
        let daytona = thegn_core::config::EnvProviderConfig {
            provider: "daytona".into(),
            ..Default::default()
        };
        assert!(
            provider_unavailable_reason(&daytona, "d")
                .contains("no API token reference configured"),
        );
        let custom = thegn_core::config::EnvProviderConfig {
            provider: "custom".into(),
            ..Default::default()
        };
        assert_eq!(
            provider_unavailable_reason(&custom, "c"),
            "env 'c' has no API provider or its token could not be resolved"
        );
    }

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

    #[test]
    fn derived_owner_exempts_literals_and_claimed_spares() {
        assert!(derived_owner("", false, "/w/a").is_some());
        assert!(derived_owner("{repo}-{worktree}", false, "/w/a").is_some());
        assert!(derived_owner("shared-box", false, "/w/a").is_none());
        assert!(derived_owner("", true, "/w/a").is_none());
    }

    #[test]
    fn derived_owner_uses_unexpanded_configured_provider_id() {
        let cfg = thegn_core::config::Config {
            env: std::collections::BTreeMap::from([(
                "remote".into(),
                thegn_core::config::EnvConfig {
                    provider: thegn_core::config::EnvProviderConfig {
                        id: "{repo}-{worktree}-{hash}".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let configured = configured_provider_id(&cfg, "remote");
        assert_eq!(configured, "{repo}-{worktree}-{hash}");
        assert!(derived_owner(configured, false, "/w/a").is_some());
    }

    #[test]
    fn sandbox_id_prefers_the_bound_spare_and_refuses_an_unreadable_binding() {
        assert_eq!(
            sandbox_id_from(Ok(Some("spare-1".into())), "derived").unwrap(),
            "spare-1"
        );
        assert_eq!(sandbox_id_from(Ok(None), "derived").unwrap(), "derived");
        let error = sandbox_id_from(Err(anyhow::anyhow!("disk I/O error")), "derived").unwrap_err();
        assert!(format!("{error:#}").contains("refusing to fall back"));
    }

    #[test]
    fn bound_provider_sandbox_reports_an_unopenable_database() {
        let state = thegn_core::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn/thegn.db")).unwrap();
        assert!(bound_provider_sandbox("/w/x").is_err());
    }
}
