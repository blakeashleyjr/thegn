//! `superzej env <action>` — inspect and select named execution environments
//! (`[env.<name>]`). An environment bundles a *placement* (where a worktree's
//! processes run — local / ssh / k8s / provider), a sandbox *isolation* overlay,
//! and a *data* mode. Selection layers worktree → workspace → repo `.superzej.*`
//! → global `[sandbox] default_env` → the implicit `default`.

use anyhow::Result;
use std::path::{Path, PathBuf};
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::remote::GitLoc;
use superzej_core::store::WorkspaceStore;
use superzej_core::{msg, outln, repo};
use superzej_svc::projection::ProjectionBackend;

#[derive(clap::Subcommand, Clone)]
// `Create` carries many inline clap flags (the setup surface), dwarfing the small
// variants; clap needs the fields inline, so boxing isn't an option here.
#[allow(clippy::large_enum_variant)]
pub enum Action {
    /// List the defined `[env.<name>]` environments and their placement.
    List {
        /// Emit one JSON array instead of the human list.
        #[arg(long)]
        json: bool,
    },
    /// Show the environment that resolves for a worktree (defaults to cwd).
    Show { worktree: Option<String> },
    /// Select an environment for a worktree (persists to the DB).
    Set {
        /// The `[env.<name>]` to use.
        name: String,
        /// Worktree path (defaults to the current directory).
        worktree: Option<String>,
        /// Apply to the whole workspace (repo) instead of just this worktree.
        #[arg(long)]
        workspace: bool,
    },
    /// Clear a worktree's (or workspace's) env selection (inherit the default).
    Clear {
        worktree: Option<String>,
        #[arg(long)]
        workspace: bool,
    },
    /// Bring a worktree's environment up (k8s: spawn the pod and wait for Ready).
    Up { worktree: Option<String> },
    /// Tear a worktree's environment down (k8s: delete the pod/manifest).
    Down { worktree: Option<String> },
    /// Forward a port from the environment to localhost (k8s `port-forward`).
    /// Runs in the foreground until interrupted. `spec` is `local:remote` or `port`.
    Forward {
        spec: String,
        worktree: Option<String>,
    },
    /// Create a new managed-sandbox via the env's API provider (Daytona/Sprites)
    /// and print its id. Requires `[env.<name>.provider]` (+ `api_key_env`); on
    /// create it also applies the env's network allow/block as the sandbox's
    /// egress policy when the provider supports it.
    Provision { worktree: Option<String> },
    /// Destroy a managed-sandbox by id via the env's API provider.
    Deprovision {
        id: String,
        worktree: Option<String>,
    },
    /// Create a checkpoint/snapshot of the env's sandbox (providers that support it).
    Snapshot {
        worktree: Option<String>,
        /// Optional label/comment for the checkpoint.
        #[arg(long)]
        label: Option<String>,
    },
    /// List the env sandbox's checkpoints.
    Snapshots { worktree: Option<String> },
    /// Restore the env's sandbox to a checkpoint id.
    Restore {
        id: String,
        worktree: Option<String>,
    },
    /// Bake a reusable VPS base image (nix + direnv + docker preinstalled) and
    /// print the `template = "snapshot:<id>"` to use it — the VPS stand-in for
    /// checkpoints (~3-6 min cold provisions drop to ~30-90 s).
    ImageBake { worktree: Option<String> },
    /// Create (or update — upsert) a `[env.<name>]` in the global config. The
    /// authoring path behind the TUI setup wizard; also scriptable on its own.
    Create {
        /// The env name (e.g. `fly-dev`).
        name: String,
        /// Kind: local | ssh | fly | digitalocean | hetzner | daytona | sprites.
        #[arg(long)]
        provider: String,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        size: Option<String>,
        /// Image/snapshot template (e.g. `image:<ref>` for Fly, `snapshot:<id>` for VPS).
        #[arg(long)]
        template: Option<String>,
        #[arg(long)]
        max_instances: Option<i64>,
        #[arg(long)]
        max_lifetime: Option<i64>,
        #[arg(long)]
        auto_provision: bool,
        /// SSH target (`user@host:port`) when `--provider ssh`.
        #[arg(long)]
        ssh_host: Option<String>,
        /// Local sandbox backend when `--provider local`.
        #[arg(long)]
        sandbox: Option<String>,
        /// Token, entered directly — stored in the OS keyring (else a 0600 file);
        /// the config records only a `keyring:`/`file:` ref, never the token.
        #[arg(long)]
        token: Option<String>,
        /// Reference an existing env var holding the token (no storage).
        #[arg(long)]
        token_env: Option<String>,
        /// Reference a file holding the token (no copy; a `file:` ref).
        #[arg(long)]
        token_file: Option<String>,
    },
    /// Remove a `[env.<name>]` from the global config (and forget its stored token).
    Rm { name: String },
    /// Verify a provider env's token works by making a cheap `list()` API call.
    Test { name: String },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(cfg, json),
        Action::Show { worktree } => show(cfg, worktree),
        Action::Set {
            name,
            worktree,
            workspace,
        } => set(cfg, &name, worktree, workspace),
        Action::Clear {
            worktree,
            workspace,
        } => set(cfg, "", worktree, workspace),
        Action::Up { worktree } => lifecycle(cfg, worktree, Lifecycle::Up),
        Action::Down { worktree } => lifecycle(cfg, worktree, Lifecycle::Down),
        Action::Forward { spec, worktree } => forward(cfg, worktree, &spec),
        Action::Provision { worktree } => provision(cfg, worktree, None),
        Action::Deprovision { id, worktree } => provision(cfg, worktree, Some(id)),
        Action::Snapshot { worktree, label } => snapshot(cfg, worktree, label),
        Action::Snapshots { worktree } => snapshots(cfg, worktree),
        Action::Restore { id, worktree } => restore(cfg, worktree, &id),
        Action::ImageBake { worktree } => super::env_image::run(cfg, worktree),
        Action::Create {
            name,
            provider,
            region,
            size,
            template,
            max_instances,
            max_lifetime,
            auto_provision,
            ssh_host,
            sandbox,
            token,
            token_env,
            token_file,
        } => create_env(CreateArgs {
            name,
            provider,
            region,
            size,
            template,
            max_instances,
            max_lifetime,
            auto_provision,
            ssh_host,
            sandbox,
            token,
            token_env,
            token_file,
        }),
        Action::Rm { name } => remove(&name),
        Action::Test { name } => test(cfg, &name),
    }
}

/// Args for [`create_env`] (grouped so the dispatch arm stays flat). Also the
/// payload the TUI "Add environment" wizard builds, so both paths share the write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct CreateArgs {
    pub name: String,
    pub provider: String,
    pub region: Option<String>,
    pub size: Option<String>,
    pub template: Option<String>,
    pub max_instances: Option<i64>,
    pub max_lifetime: Option<i64>,
    pub auto_provision: bool,
    pub ssh_host: Option<String>,
    pub sandbox: Option<String>,
    pub token: Option<String>,
    pub token_env: Option<String>,
    pub token_file: Option<String>,
}

/// Create/upsert `[env.<name>]` in the global config, storing any entered token
/// via the secret backend and recording only a SecretRef in config. Shared by the
/// `env create` CLI and the TUI wizard.
pub(crate) fn create_env(a: CreateArgs) -> Result<()> {
    use superzej_core::config_write::{EnvSpec, upsert_env};
    let kind = a.provider.trim().to_lowercase();
    let placement = match kind.as_str() {
        "local" => "local",
        "ssh" => "ssh",
        "fly" | "digitalocean" | "hetzner" | "vultr" | "daytona" | "sprites" => "provider",
        other => anyhow::bail!(
            "unknown --provider {other:?} (local|ssh|fly|digitalocean|hetzner|daytona|sprites)"
        ),
    };

    // Resolve the token source into a SecretRef (only for provider kinds).
    let api_key_env = if placement == "provider" {
        match (&a.token, &a.token_env, &a.token_file) {
            (Some(t), _, _) => Some(
                crate::secret::store(&a.name, t)
                    .map_err(|e| anyhow::anyhow!("store token: {e}"))?,
            ),
            (_, Some(v), _) => Some(format!("env:{}", v.trim())),
            (_, _, Some(p)) => Some(format!("file:{}", p.trim())),
            _ => None, // fall back to the provider's default env var at launch
        }
    } else {
        None
    };

    let spec = EnvSpec {
        name: a.name.clone(),
        placement: placement.to_string(),
        data: (placement == "provider").then(|| "in_env".to_string()),
        provider: (placement == "provider").then(|| kind.clone()),
        api_key_env,
        region: a.region,
        size: a.size,
        template: a.template,
        max_instances: a.max_instances,
        max_lifetime_secs: a.max_lifetime,
        auto_provision: a.auto_provision.then_some(true),
        ssh_host: a.ssh_host,
        sandbox_backend: a.sandbox,
    };
    let path = Config::path();
    upsert_env(&path, &spec)?;
    outln!("created env '{}' ({kind}) in {}", a.name, path.display());
    outln!("  bind it: superzej env set {} [worktree]", a.name);
    Ok(())
}

fn remove(name: &str) -> Result<()> {
    let path = Config::path();
    superzej_core::config_write::remove_env(&path, name)?;
    crate::secret::forget(name);
    outln!("removed env '{name}'");
    Ok(())
}

/// Verify a provider env's token by listing its sandboxes (a cheap control call).
fn test(cfg: &Config, name: &str) -> Result<()> {
    let envc = cfg
        .env
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no [env.{name}] defined"))?;
    let provider =
        crate::provider_factory::provider_for_named(&envc.provider, name).ok_or_else(|| {
            anyhow::anyhow!("env '{name}' has no API provider or its token could not be resolved")
        })?;
    let n = crate::agent::block_on_provider(|| async { provider.list().await })
        .map_err(|e| anyhow::anyhow!("provider check failed: {e}"))?
        .len();
    outln!("✓ env '{name}' reachable — {n} managed sandbox(es) visible");
    Ok(())
}

/// Build the generic API provider ([`superzej_svc::provider::Provider`]) for a
/// worktree's resolved env, or explain why not.
fn api_provider(
    cfg: &Config,
    worktree: Option<String>,
) -> Result<superzej_svc::provider::Provider> {
    use superzej_svc::provider::{DaytonaProvider, Provider, SpritesProvider};
    let env = resolve_for(cfg, worktree);
    let envc = cfg
        .env
        .get(&env.name)
        .ok_or_else(|| anyhow::anyhow!("the default env has no API provider configured"))?;
    let pc = &envc.provider;
    match pc.provider.as_str() {
        "daytona" => {
            if pc.api_base.trim().is_empty() {
                anyhow::bail!(
                    "[env.{}.provider] needs `api_base` (and `api_key_env`) for API provisioning",
                    env.name
                );
            }
            let token = crate::secret::resolve(pc.api_key_env.trim()).ok_or_else(|| {
                anyhow::anyhow!(
                    "the API token {:?} (api_key_env) could not be resolved",
                    pc.api_key_env
                )
            })?;
            Ok(Provider::Daytona(DaytonaProvider::new(
                &pc.api_base,
                &token,
                &pc.template,
            )))
        }
        "sprites" => {
            // api_base may be empty (the provider uses the documented default);
            // the token env var defaults to SPRITES_TOKEN when unset.
            let key_env = if pc.api_key_env.trim().is_empty() {
                "SPRITES_TOKEN"
            } else {
                pc.api_key_env.trim()
            };
            let token = crate::secret::resolve(key_env).ok_or_else(|| {
                anyhow::anyhow!("the Sprites API token {key_env:?} could not be resolved")
            })?;
            Ok(Provider::Sprites(SpritesProvider::new(
                &pc.api_base,
                &token,
                &pc.id,
            )))
        }
        vps if superzej_core::config::vps_provider_kind(vps) => {
            // The per-worktree resolved sandbox id (the placement bakes it) —
            // creates/destroys must name the same instance the panes attach to.
            let id = match &env.placement {
                superzej_core::placement::Placement::Provider(p) => p.id.clone(),
                _ => String::new(),
            };
            crate::provider_factory::vps_provider_for(pc, &id)
                .map(superzej_svc::provider::Provider::Vps)
                .ok_or_else(|| {
                    let key = if pc.api_key_env.trim().is_empty() {
                        superzej_svc::vps::VpsKind::parse(vps)
                            .map(|k| k.token_env_default())
                            .unwrap_or("<token env>")
                            .to_string()
                    } else {
                        pc.api_key_env.trim().to_string()
                    };
                    anyhow::anyhow!("the {vps} API token env var {key:?} is not set")
                })
        }
        "fly" => {
            // Same resolved-id contract as the VPS arm: create/destroy must name
            // the machine the panes attach to.
            let id = match &env.placement {
                superzej_core::placement::Placement::Provider(p) => p.id.clone(),
                _ => String::new(),
            };
            crate::provider_factory::provider_for_named(pc, &id).ok_or_else(|| {
                let key = if pc.api_key_env.trim().is_empty() {
                    "FLY_API_TOKEN"
                } else {
                    pc.api_key_env.trim()
                };
                anyhow::anyhow!("the Fly API token env var {key:?} is not set")
            })
        }
        other => anyhow::bail!(
            "API provisioning supports 'daytona', 'sprites', 'fly', and VPS kinds \
             (hetzner, digitalocean); env {} uses {:?}",
            env.name,
            other
        ),
    }
}

/// Create (id=None) or destroy (id=Some) a managed sandbox via the API provider.
/// On create, when the provider can translate egress and the env declares a
/// network allow/block list, lower it to the provider's network policy so the
/// new sandbox comes up already governed.
fn provision(cfg: &Config, worktree: Option<String>, id: Option<String>) -> Result<()> {
    let env = resolve_for(cfg, worktree.clone());
    let provider = api_provider(cfg, worktree)?;
    let rt = tokio::runtime::Runtime::new()?;
    match id {
        None => {
            let handle = rt.block_on(provider.create())?;
            outln!("created sandbox: {}", handle.id);
            match handle.exec {
                superzej_svc::provider::ExecKind::Command(argv) => {
                    outln!("exec via: {}", argv.join(" "));
                }
                superzej_svc::provider::ExecKind::Ssh(t) => {
                    outln!("exec via ssh: {}:{}", t.host, t.port);
                }
            }
            // Egress translate: lower the env's allow/block lists onto the sandbox.
            let allow = &env.sandbox.network_allow;
            let block = &env.sandbox.network_block;
            if provider.caps().egress && (!allow.is_empty() || !block.is_empty()) {
                match rt.block_on(provider.set_network_policy(&handle.id, allow, block)) {
                    Ok(()) => outln!(
                        "applied network policy: {} allow, {} block",
                        allow.len(),
                        block.len()
                    ),
                    Err(e) => msg::warn(&format!("could not apply network policy: {e}")),
                }
            }
            outln!(
                "set `[env.<name>.provider] id = \"{}\"` to attach.",
                handle.id
            );
        }
        Some(id) => {
            rt.block_on(provider.destroy(&id))?;
            outln!("destroyed sandbox: {id}");
        }
    }
    Ok(())
}

/// Resolve `(provider, sandbox_id)` for checkpoint ops — the sandbox id is the
/// env's configured `[env.<name>.provider] id`.
fn provider_and_id(
    cfg: &Config,
    worktree: Option<String>,
) -> Result<(superzej_svc::provider::Provider, String)> {
    let env = resolve_for(cfg, worktree.clone());
    // The resolved placement carries the per-worktree sandbox id.
    let id = match &env.placement {
        superzej_core::placement::Placement::Provider(p) => p.id.clone(),
        _ => String::new(),
    };
    if id.is_empty() {
        anyhow::bail!(
            "set `[env.{}.provider] id` to the sandbox name first",
            env.name
        );
    }
    let provider = api_provider(cfg, worktree)?;
    if !provider.caps().checkpoints {
        anyhow::bail!("this env's provider does not support checkpoints");
    }
    Ok((provider, id))
}

/// Create a checkpoint of the env's sandbox.
fn snapshot(cfg: &Config, worktree: Option<String>, label: Option<String>) -> Result<()> {
    let (provider, id) = provider_and_id(cfg, worktree)?;
    let rt = tokio::runtime::Runtime::new()?;
    let cp = rt.block_on(provider.checkpoint(&id, label.as_deref()))?;
    outln!("created checkpoint: {cp}");
    Ok(())
}

/// List the env sandbox's checkpoints.
fn snapshots(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let (provider, id) = provider_and_id(cfg, worktree)?;
    let rt = tokio::runtime::Runtime::new()?;
    let list = rt.block_on(provider.list_checkpoints(&id))?;
    if list.is_empty() {
        outln!("no checkpoints for {id}");
    }
    for c in list {
        match c.label {
            Some(l) => outln!("{}  {l}", c.id),
            None => outln!("{}", c.id),
        }
    }
    Ok(())
}

/// Restore the env's sandbox to a checkpoint.
fn restore(cfg: &Config, worktree: Option<String>, cp: &str) -> Result<()> {
    let (provider, id) = provider_and_id(cfg, worktree)?;
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(provider.restore(&id, cp))?;
    outln!("restored {id} to checkpoint {cp}");
    Ok(())
}

enum Lifecycle {
    Up,
    Down,
}

/// Resolve a worktree's environment and bring it up / tear it down.
fn lifecycle(cfg: &Config, worktree: Option<String>, action: Lifecycle) -> Result<()> {
    let env = resolve_for(cfg, worktree.clone());
    match action {
        Lifecycle::Up => {
            outln!("bringing up env {} ({})…", env.name, env.placement.label());
            env.placement
                .ensure()
                .map_err(|e| anyhow::anyhow!("env up failed: {e}"))?;
            sshfs(&env, true)?;
            outln!("env {} is up", env.name);
        }
        Lifecycle::Down => {
            outln!("tearing down env {} ({})…", env.name, env.placement.label());
            sshfs(&env, false)?;
            env.placement
                .teardown()
                .map_err(|e| anyhow::anyhow!("env down failed: {e}"))?;
            outln!("env {} is down", env.name);
        }
    }
    Ok(())
}

/// Project (or unproject) the worktree for the env's data mode. Delegates to the
/// shared projection layer (`core::projection` plan → `svc::projection` action),
/// the same code path the pane lifecycle auto-runs. A no-op for non-projecting
/// data modes (`in_env`/`local_exec`); a misconfigured `sshfs` env reports why.
fn sshfs(env: &superzej_core::env::Environment, mount: bool) -> Result<()> {
    let Some(spec) = superzej_core::projection::for_environment(env) else {
        if env.data == superzej_core::config::DataMode::Sshfs {
            anyhow::bail!(
                "data = \"sshfs\" needs an ssh placement and [sandbox.remote] remote_dir (env {})",
                env.name
            );
        }
        return Ok(());
    };
    let backend = superzej_svc::projection::for_data_mode(&spec);
    if mount {
        outln!("mounting {} -> {}", spec.remote_dir, spec.mountpoint);
        backend.mount(&spec).map(|_| ())
    } else {
        outln!("unmounting {}", spec.mountpoint);
        backend.unmount(&spec)
    }
}

/// Forward a port from the resolved environment to localhost (foreground).
fn forward(cfg: &Config, worktree: Option<String>, spec: &str) -> Result<()> {
    let env = resolve_for(cfg, worktree);
    let Some(argv) = env.placement.port_forward_argv(spec) else {
        anyhow::bail!(
            "env {} ({}) does not support port forwarding",
            env.name,
            env.placement.label()
        );
    };
    outln!("forwarding {spec} via: {}", argv.join(" "));
    // CLI path: `szhost env forward` runs the tunnel in the foreground, no event loop.
    #[expect(clippy::disallowed_methods)]
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {}: {e}", argv[0]))?;
    if !status.success() {
        anyhow::bail!("port-forward exited with {status}");
    }
    Ok(())
}

/// Resolve the [`Environment`](superzej_core::env::Environment) for a worktree
/// (cwd default), honouring the DB worktree/workspace selection.
pub(crate) fn resolve_for(
    cfg: &Config,
    worktree: Option<String>,
) -> superzej_core::env::Environment {
    let wt = resolve_worktree(worktree);
    let loc = GitLoc::for_worktree(Path::new(&wt));
    let repo_root = repo_root_for(&wt);
    let selected = Db::open()
        .ok()
        .and_then(|db| db.effective_env(&wt, &repo_root.to_string_lossy()));
    cfg.resolve_env(&repo_root, &loc, Path::new(&wt), selected.as_deref())
}

fn list(cfg: &Config, json: bool) -> Result<()> {
    let default = if cfg.sandbox.default_env.trim().is_empty() {
        "default"
    } else {
        cfg.sandbox.default_env.trim()
    };
    if json {
        #[derive(serde::Serialize)]
        struct EnvJson<'a> {
            name: &'a str,
            placement: &'a str,
            data: &'a str,
            backend: Option<String>,
            default: bool,
        }
        let rows: Vec<EnvJson> = cfg
            .env
            .iter()
            .map(|(name, e)| EnvJson {
                name,
                placement: e.placement.as_str(),
                data: e.data.as_str(),
                backend: e.sandbox.backend.map(|b| b.as_str().to_string()),
                default: name == default,
            })
            .collect();
        return super::emit_json(&rows);
    }
    outln!("default env: {default}");
    if cfg.env.is_empty() {
        outln!("(no [env.<name>] defined — the implicit 'default' env is the [sandbox] block)");
        return Ok(());
    }
    for (name, e) in &cfg.env {
        let placement = e.placement.as_str();
        let detail = match e.placement {
            superzej_core::config::PlacementMode::Local => String::new(),
            superzej_core::config::PlacementMode::Ssh => {
                format!(" ({})", placement_ssh_target(e, cfg))
            }
            superzej_core::config::PlacementMode::K8s => {
                let ns = if e.k8s.namespace.is_empty() {
                    "-".into()
                } else {
                    e.k8s.namespace.clone()
                };
                format!(" ({ns}/{})", e.k8s.pod)
            }
            superzej_core::config::PlacementMode::Provider => {
                format!(" ({}:{})", e.provider.provider, e.provider.id)
            }
        };
        let mark = if name == default { " *" } else { "" };
        outln!(
            "  {name}{mark} — placement={placement}{detail} data={} backend={}",
            e.data.as_str(),
            e.sandbox
                .backend
                .map(|b| b.as_str().to_string())
                .unwrap_or_else(|| "(inherit)".into()),
        );
    }
    Ok(())
}

fn placement_ssh_target(e: &superzej_core::config::EnvConfig, cfg: &Config) -> String {
    if !e.ssh.host.trim().is_empty() {
        e.ssh.host.trim().to_string()
    } else if !cfg.sandbox.remote.host.trim().is_empty() {
        cfg.sandbox.remote.host.trim().to_string()
    } else {
        "<worktree target>".into()
    }
}

fn show(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    let loc = GitLoc::for_worktree(Path::new(&wt));
    let repo_root = repo_root_for(&wt);
    let selected = Db::open()
        .ok()
        .and_then(|db| db.effective_env(&wt, &repo_root.to_string_lossy()));
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(&wt), selected.as_deref());
    outln!("worktree: {wt}");
    outln!("env:      {}", env.name);
    outln!("placement: {}", env.placement.label());
    outln!("data:      {}", env.data.as_str());
    outln!(
        "backend:   {} (image: {})",
        env.sandbox.backend,
        if env.sandbox.image.is_empty() {
            "(host toolchain)"
        } else {
            &env.sandbox.image
        }
    );
    match env_isolation(&env) {
        Some(class) => outln!("isolation: {class} — {}", class.escape_note()),
        None => outln!("isolation: resolved at spawn from backend_chain"),
    }
    Ok(())
}

/// The honest boundary class the resolved environment provides, or `None` when
/// the backend is `auto` (resolved at spawn). Placement owns the boundary first
/// (provider/k8s), otherwise it follows the concrete backend.
fn env_isolation(
    env: &superzej_core::env::Environment,
) -> Option<superzej_core::capabilities::IsolationClass> {
    use superzej_core::placement::Placement;
    match env.placement {
        Placement::Provider(_) | Placement::K8s(_) => Some(
            superzej_core::capabilities::Capabilities::from_parts(
                superzej_core::sandbox::Backend::None,
                &env.placement,
                false,
            )
            .isolation,
        ),
        _ => {
            let backend = superzej_core::sandbox::Backend::from_config(env.sandbox.backend)?;
            Some(
                superzej_core::capabilities::Capabilities::from_parts(
                    backend,
                    &env.placement,
                    false,
                )
                .isolation,
            )
        }
    }
}

fn set(cfg: &Config, name: &str, worktree: Option<String>, workspace: bool) -> Result<()> {
    let name = name.trim();
    if !name.is_empty() && name != "default" && !cfg.env.contains_key(name) {
        msg::warn(&format!(
            "environment {name:?} is not defined under [env.{name}]; it will resolve to the default until you add it"
        ));
    }
    let wt = resolve_worktree(worktree);
    let db = Db::open()?;
    if workspace {
        let repo_root = repo_root_for(&wt);
        let repo_s = repo_root.to_string_lossy().into_owned();
        db.set_workspace_env(&repo_s, name)?;
        if name.is_empty() {
            outln!("cleared workspace env for {repo_s}");
        } else {
            outln!("workspace {repo_s} → env {name}");
        }
    } else {
        db.set_worktree_env(&wt, name)?;
        if name.is_empty() {
            outln!("cleared env for {wt}");
        } else {
            outln!("worktree {wt} → env {name}");
        }
    }
    Ok(())
}

fn resolve_worktree(worktree: Option<String>) -> String {
    worktree
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

fn repo_root_for(wt: &str) -> PathBuf {
    Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(wt).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(wt)))
        .unwrap_or_else(|| PathBuf::from(wt))
}
