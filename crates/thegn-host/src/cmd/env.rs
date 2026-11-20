//! `thegn env <action>` — inspect and select named execution environments
//! (`[env.<name>]`). An environment bundles a *placement* (where a worktree's
//! processes run — local / ssh / k8s / provider), a sandbox *isolation* overlay,
//! and a *data* mode. Selection layers worktree → workspace → repo `.thegn.*`
//! → global `[sandbox] default_env` → the implicit `default`.

use crate::agent::{ProvisionState, ProvisionStepView};
use crate::egress_admit::EgressSteps;
use crate::egress_admit::admit_egress;
use crate::provider_git::is_provider_location;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::store::WorkspaceStore;
use thegn_core::{msg, outln, repo};
use thegn_svc::projection::ProjectionBackend;

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
    /// Show the environment that resolves for a worktree (defaults to the
    /// current one).
    Show {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Select an environment for a worktree (persists to the DB).
    Set {
        /// The `[env.<name>]` to use.
        name: String,
        /// Apply to the whole workspace (repo) instead of just this worktree.
        #[arg(long)]
        workspace: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Clear a worktree's (or workspace's) env selection (inherit the default).
    Clear {
        #[arg(long)]
        workspace: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Bring a worktree's environment up (k8s: spawn the pod and wait for Ready;
    /// provider: provision the sandbox headlessly, streaming each step).
    Up {
        /// Provider envs: return once a shell can open; the background steps
        /// (coding agents, agent logins, shell history, snapshot) finish on
        /// the next open or the next plain env up.
        #[arg(long)]
        shell_ready: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Tear a worktree's environment down (k8s: delete the pod/manifest;
    /// provider: pull the sandbox's commits back, then destroy the sandbox).
    Down {
        /// Provider envs: park the sandbox (fly stop, machine0 suspend) instead
        /// of destroying it; `thegn env up` resumes it.
        #[arg(long)]
        stop: bool,
        /// Provider envs: destroy even when the pull-back fails (commits made
        /// inside the sandbox that were not pulled back are lost).
        #[arg(long)]
        force: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Forward a port from the environment to localhost (k8s `port-forward`).
    /// Runs in the foreground until interrupted. `spec` is `local:remote` or `port`.
    Forward {
        spec: String,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Create a new managed-sandbox via the env's API provider (Daytona/Sprites)
    /// and print its id. Requires `[env.<name>.provider]` (+ `api_key_env`); on
    /// create it applies and verifies the env's network egress policy when the
    /// provider supports it, and destroys the sandbox if verification fails.
    Provision {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Destroy managed-sandbox(es) via the env's API provider. Pass an `id`, or
    /// `--all` to destroy every listed sandbox thegn can prove it created (the
    /// rest are skipped with a warning). `--env <name>`
    /// targets a named env's provider instead of the worktree's resolved one.
    Deprovision {
        id: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        env: Option<String>,
        /// Skip the confirmation prompt (required for `--all` when stdin is not
        /// a TTY).
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Create a checkpoint/snapshot of the env's sandbox (providers that support it).
    Snapshot {
        /// Optional label/comment for the checkpoint.
        #[arg(long)]
        label: Option<String>,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// List the env sandbox's checkpoints.
    Snapshots {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Restore the env's sandbox to a checkpoint id.
    Restore {
        id: String,
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Bake a reusable VPS base image (nix + direnv + docker preinstalled) and
    /// print the `template = "snapshot:<id>"` to use it — the VPS stand-in for
    /// checkpoints (~3-6 min cold provisions drop to ~30-90 s).
    ImageBake {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
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
        /// Vercel team id (`team_…`); vercel only.
        #[arg(long)]
        team_id: Option<String>,
        /// Vercel project id (`prj_…`); vercel only.
        #[arg(long)]
        project_id: Option<String>,
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
    /// Fetch commits made inside a provider sandbox back into the worktree's
    /// branch (fast-forward only). A diverged or locally-dirty result is kept in
    /// `refs/thegn/pull/<worktree>` for you to merge.
    Pull {
        #[command(flatten)]
        target: super::target::WorktreeTarget,
    },
    /// Find sandboxes this host's controller created but no longer records, or sprites past their lifetime (dry-run unless --apply).
    Reap {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
        /// Sprites only: list sprites this host has no record of (read-only).
        #[arg(long, conflicts_with = "apply")]
        unrecorded: bool,
    },
    /// Record an existing sprite as thegn's so thegn may delete it later (asks for confirmation)
    Adopt(super::env_adopt::AdoptArgs),
    /// Retry failed pull-backs of `data = "sync"` sandboxes (never destroys a sandbox).
    SyncRetry {
        #[arg(long)]
        json: bool,
    },
    /// Fill the warm pool for a worktree's project and env now, in the foreground (creates sandboxes; never destroys).
    PoolFill(super::env_pool::PoolFillArgs),
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List { json } => list(cfg, json),
        Action::Show { target } => show(cfg, target.get()),
        Action::Set {
            name,
            workspace,
            target,
        } => set(cfg, &name, target.get(), workspace),
        Action::Clear { workspace, target } => set(cfg, "", target.get(), workspace),
        Action::Up {
            target,
            shell_ready,
        } => lifecycle(cfg, target.get(), Lifecycle::Up { shell_ready }),
        Action::Down {
            stop,
            force,
            target,
        } => lifecycle(
            cfg,
            target.get(),
            Lifecycle::Down(DownOpts {
                mode: if stop {
                    DownMode::Stop
                } else {
                    DownMode::Destroy
                },
                force,
            }),
        ),
        Action::Forward { spec, target } => forward(cfg, target.get(), &spec),
        Action::Provision { target } => provision(cfg, target.get()),
        Action::Deprovision {
            id,
            all,
            env,
            yes,
            target,
        } => deprovision(cfg, target.get(), id, all, env, yes),
        Action::Snapshot { label, target } => snapshot(cfg, target.get(), label),
        Action::Snapshots { target } => snapshots(cfg, target.get()),
        Action::Restore { id, target } => restore(cfg, target.get(), &id),
        Action::ImageBake { target } => super::env_image::run(cfg, target.get()),
        Action::Create {
            name,
            provider,
            region,
            size,
            team_id,
            project_id,
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
            team_id,
            project_id,
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
        Action::Rm { name } => remove(cfg, &name),
        Action::Test { name } => test(cfg, &name),
        Action::Pull { target } => pull(cfg, target.get()),
        Action::Reap {
            provider,
            apply,
            yes,
            json,
            unrecorded,
        } => super::env_reap::run(cfg, &provider, apply, yes, json, unrecorded),
        Action::Adopt(args) => super::env_adopt::run(cfg, args),
        Action::SyncRetry { json } => super::env_sync_retry::run(json),
        Action::PoolFill(args) => super::env_pool::run(cfg, args),
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
    pub team_id: Option<String>,
    pub project_id: Option<String>,
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
    use thegn_core::config_write::{EnvSpec, upsert_env};
    let kind = a.provider.trim().to_lowercase();
    let placement = match kind.as_str() {
        "local" => "local",
        "ssh" => "ssh",
        "fly" | "digitalocean" | "hetzner" | "vultr" | "daytona" | "sprites" | "machine0"
        | "vercel" => "provider",
        other => anyhow::bail!(
            "unknown --provider {other:?} \
             (local|ssh|fly|digitalocean|hetzner|daytona|sprites|machine0|vercel)"
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
        team_id: a.team_id,
        project_id: a.project_id,
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
    outln!("  bind it: thegn env set {} [--worktree <path>]", a.name);
    Ok(())
}

fn remove(cfg: &Config, name: &str) -> Result<()> {
    let rows = Db::open()?.worktrees()?;
    let mut unresolved = Vec::new();
    let using: Vec<String> = rows
        .iter()
        .filter_map(|row| match resolve_for(cfg, Some(row.worktree.clone())) {
            Ok(env) if env.name == name && env.placement.is_provider() => {
                Some(row.worktree.clone())
            }
            Ok(_) => None,
            Err(_) => {
                unresolved.push(row.worktree.clone());
                None
            }
        })
        .collect();
    if !unresolved.is_empty() {
        let wts = unresolved.join(", ");
        anyhow::bail!(
            "cannot resolve the env of {wts}; nothing was removed — fix or `thegn wt rm` them first"
        );
    }
    if let Some(refusal) = rm_refusal(name, &using) {
        anyhow::bail!(refusal);
    }
    let path = Config::path();
    thegn_core::config_write::remove_env(&path, name)?;
    crate::secret::forget(name);
    outln!("removed env '{name}'");
    Ok(())
}

fn rm_refusal(name: &str, using: &[String]) -> Option<String> {
    if using.is_empty() {
        return None;
    }
    let first_five = using
        .iter()
        .take(5)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "env {name} is the environment of {} registered worktree(s) with a provider sandbox ({}); removing it would orphan those sandboxes and any commits not pulled from them. Tear them down first (`thegn env down --worktree <wt>` or `thegn wt rm <wt>`), or move them (`thegn env set <env> --worktree <wt>`)",
        using.len(),
        first_five
    ))
}

/// Destroy managed sandbox(es). With `--all`, list every sandbox the provider
/// can see and destroy each one thegn can prove it created (best-effort per id:
/// an unproven name is skipped with a warning, a failed destroy warns and the
/// rest continue). `--env <name>` targets that env's provider; otherwise the
/// worktree's resolved env. Without `--all`, an explicit `id` is required.
fn deprovision(
    cfg: &Config,
    worktree: Option<String>,
    id: Option<String>,
    all: bool,
    env: Option<String>,
    yes: bool,
) -> Result<()> {
    let provider_config = match env.as_deref() {
        Some(name) => cfg
            .env
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("no [env.{name}] defined"))?
            .provider
            .clone(),
        None => {
            let resolved = resolve_for(cfg, worktree.clone())?;
            cfg.env
                .get(&resolved.name)
                .ok_or_else(|| anyhow::anyhow!("the default env has no API provider configured"))?
                .provider
                .clone()
        }
    };
    let provider = match env.as_deref() {
        Some(name) => {
            let envc = cfg
                .env
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("no [env.{name}] defined"))?;
            crate::provider_factory::provider_for_named(&envc.provider, name).ok_or_else(|| {
                anyhow::anyhow!(crate::provider_factory::provider_unavailable_reason(
                    &envc.provider,
                    name
                ))
            })?
        }
        None => api_provider(cfg, worktree.clone())?,
    };
    let bindings = worktree_bindings(cfg)?;
    if all {
        if let Err(rows) = owner_of("", &bindings) {
            anyhow::bail!(
                "cannot tell whether registered worktrees use these sandboxes ({rows}); nothing was destroyed — remove a stale one with `thegn wt rm <wt>`, or move it with `thegn env set <env> --worktree <wt>`"
            );
        }
        let listed = crate::agent::block_on_provider(|| async { provider.list().await })
            .map_err(|e| anyhow::anyhow!("list sandboxes failed: {e}"))?;
        if listed.is_empty() {
            outln!("no managed sandboxes to destroy");
            return Ok(());
        }
        let mut ids = Vec::with_capacity(listed.len());
        let mut bound = 0;
        for id in listed {
            match owner_of(&id, &bindings) {
                Ok(Some(wt)) => {
                    msg::warn(&format!(
                        "kept {id}: it belongs to worktree {wt} (use `thegn env down --worktree {wt}`)"
                    ));
                    bound += 1;
                }
                Ok(None) => ids.push(id),
                Err(rows) => anyhow::bail!(
                    "cannot tell whether registered worktrees use these sandboxes ({rows}); nothing was destroyed — remove a stale one with `thegn wt rm <wt>`, or move it with `thegn env set <env> --worktree <wt>`"
                ),
            }
        }
        if ids.is_empty() {
            outln!(
                "{}",
                DestroyAllOutcome {
                    bound,
                    ..Default::default()
                }
                .summary()
            );
            return Ok(());
        }
        // `--all` lists every sandbox on the provider account (not just this
        // repo's); the provider destroys only the ones it can prove this host
        // created, which is still irreversible, so gate it on an explicit
        // confirmation. `--yes` bypasses; without a TTY and without `--yes`,
        // refuse rather than silently blocking on a stdin read a script can't
        // answer.
        if !yes {
            let prompt = destroy_all_prompt(ids.len());
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                anyhow::bail!("{prompt} refusing without a TTY — pass --yes to confirm");
            }
            if !super::confirm(&prompt) {
                outln!("aborted");
                return Ok(());
            }
        }
        let mut outcome = destroy_all(
            &ids,
            |id| {
                crate::remote_enqueue_auth::revoke_for_sandbox(&provider_config, id, None)
                    .map(|_| ())
            },
            |id| crate::agent::block_on_provider(|| async { provider.destroy(id).await }),
            |line| outln!("{line}"),
            msg::warn,
        );
        outcome.bound = bound;
        outln!("{}", outcome.summary());
        return Ok(());
    }
    let id = id.ok_or_else(|| anyhow::anyhow!("provide a sandbox id, or pass --all"))?;
    match owner_of(&id, &bindings) {
        Ok(Some(wt)) => anyhow::bail!(
            "sandbox {id} belongs to worktree {wt}; tear it down with `thegn env down --worktree {wt}` (pulls its commits first) or `thegn wt rm {wt}`; nothing was destroyed"
        ),
        Ok(None) => {}
        Err(rows) => anyhow::bail!(
            "cannot tell whether a registered worktree uses sandbox {id} ({rows}); nothing was destroyed — remove a stale one with `thegn wt rm <wt>`, or move it with `thegn env set <env> --worktree <wt>`"
        ),
    }
    crate::remote_enqueue_auth::revoke_for_sandbox(&provider_config, &id, worktree.as_deref())
        .context("route-to-host credential revocation failed; sandbox left intact")?;
    crate::agent::block_on_provider(|| async { provider.destroy(&id).await })
        .map_err(|e| anyhow::anyhow!("destroy {id} failed: {e}"))?;
    outln!("destroyed sandbox: {id}");
    Ok(())
}

fn worktree_bindings(cfg: &Config) -> Result<Vec<(String, crate::provider_git::OfflineBinding)>> {
    Ok(Db::open()?
        .worktrees()?
        .into_iter()
        .map(|row| {
            let wt = row.worktree;
            let binding = crate::provider_git::bound_offline(cfg, &wt)
                .unwrap_or(crate::provider_git::OfflineBinding::BoundUnknown);
            (wt, binding)
        })
        .collect())
}

fn owner_of<'a>(
    id: &str,
    rows: &'a [(String, crate::provider_git::OfflineBinding)],
) -> std::result::Result<Option<&'a str>, String> {
    let unknown = rows
        .iter()
        .filter_map(|(wt, binding)| {
            matches!(binding, crate::provider_git::OfflineBinding::BoundUnknown).then_some(wt)
        })
        .take(5)
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Err(unknown.join(", "));
    }
    Ok(rows.iter().find_map(|(wt, binding)| match binding {
        crate::provider_git::OfflineBinding::Bound { sandbox } if sandbox == id => {
            Some(wt.as_str())
        }
        _ => None,
    }))
}

/// The `deprovision --all` confirmation prompt.
fn destroy_all_prompt(listed: usize) -> String {
    format!("Destroy every sandbox thegn can prove it created ({listed} listed)?")
}

/// Whether a destroy error is the provider refusing a name this host cannot
/// prove it created (the sprites ledger, the Vercel controller tag), as opposed
/// to a failed delete. Both providers word the refusal "refusing to delete".
fn is_ownership_refusal(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains("refusing to delete")
}

/// The tally of a `deprovision --all` run.
#[derive(Debug, Default, PartialEq, Eq)]
struct DestroyAllOutcome {
    destroyed: usize,
    /// Names the provider refused because no ownership record proves them.
    skipped: usize,
    /// Revocation or delete failures (warned, not fatal).
    failed: usize,
    /// Listed sandboxes kept because they are bound to a registered worktree.
    bound: usize,
}

impl DestroyAllOutcome {
    fn summary(&self) -> String {
        let mut line = format!(
            "destroyed {}, skipped {} (no ownership record)",
            self.destroyed, self.skipped
        );
        if self.failed > 0 {
            line.push_str(&format!(", failed {}", self.failed));
        }
        if self.bound > 0 {
            line.push_str(&format!(", kept {} (bound to a worktree)", self.bound));
        }
        line
    }
}

/// Destroy each listed sandbox: revoke its route-to-host credential first (a
/// failed revocation leaves the sandbox intact), then delete. An unproven name
/// is a warning and a skip, never an error exit.
fn destroy_all(
    ids: &[String],
    mut revoke: impl FnMut(&str) -> Result<()>,
    mut destroy: impl FnMut(&str) -> Result<()>,
    mut out: impl FnMut(&str),
    mut warn: impl FnMut(&str),
) -> DestroyAllOutcome {
    let mut outcome = DestroyAllOutcome::default();
    for id in ids {
        if let Err(e) = revoke(id) {
            warn(&format!(
                "destroy {id} refused: route-to-host credential revocation failed: {e:#}"
            ));
            outcome.failed += 1;
            continue;
        }
        match destroy(id) {
            Ok(()) => {
                outcome.destroyed += 1;
                out(&format!("destroyed sandbox: {id}"));
            }
            Err(e) if is_ownership_refusal(&e) => {
                outcome.skipped += 1;
                warn(&format!(
                    "skipped {id}: thegn has no ownership record proving it created this sandbox"
                ));
            }
            Err(e) => {
                outcome.failed += 1;
                warn(&format!("destroy {id} failed: {e:#}"));
            }
        }
    }
    outcome
}

/// Verify a provider env's token by listing its sandboxes (a cheap control call).
fn test(cfg: &Config, name: &str) -> Result<()> {
    let envc = cfg
        .env
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("no [env.{name}] defined"))?;
    let provider =
        crate::provider_factory::provider_for_named(&envc.provider, name).ok_or_else(|| {
            anyhow::anyhow!(crate::provider_factory::provider_unavailable_reason(
                &envc.provider,
                name
            ))
        })?;
    let n = crate::agent::block_on_provider(|| async { provider.list().await })
        .map_err(|e| anyhow::anyhow!("provider check failed: {e}"))?
        .len();
    outln!("✓ env '{name}' reachable — {n} managed sandbox(es) visible");
    Ok(())
}

/// Build the generic API provider ([`thegn_svc::provider::Provider`]) for a
/// worktree's resolved env, or explain why not.
fn api_provider(cfg: &Config, worktree: Option<String>) -> Result<thegn_svc::provider::Provider> {
    use thegn_svc::provider::{DaytonaProvider, Provider, SpritesProvider};
    let env = resolve_for(cfg, worktree)?;
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
            Ok(Provider::Sprites(crate::provider_factory::sprites_policy(
                SpritesProvider::new(&pc.api_base, &token, &pc.id)
                    .with_custody_accounts(crate::provider_factory::managed_custody_accounts(pc)),
                pc,
                key_env,
            )))
        }
        vps if thegn_core::config::vps_provider_kind(vps) => {
            // The per-worktree resolved sandbox id (the placement bakes it) —
            // creates/destroys must name the same instance the panes attach to.
            let id = match &env.placement {
                thegn_core::placement::Placement::Provider(p) => p.id.clone(),
                _ => String::new(),
            };
            crate::provider_factory::vps_provider_for(pc, &id)
                .map(thegn_svc::provider::Provider::Vps)
                .ok_or_else(|| {
                    let key = if pc.api_key_env.trim().is_empty() {
                        thegn_svc::vps::VpsKind::parse(vps)
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
                thegn_core::placement::Placement::Provider(p) => p.id.clone(),
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
        "machine0" => {
            // Same resolved-id contract as the VPS/Fly arms: create/destroy must
            // name the VM the panes attach to.
            let id = match &env.placement {
                thegn_core::placement::Placement::Provider(p) => p.id.clone(),
                _ => String::new(),
            };
            crate::provider_factory::machine0_provider_for(pc, &id)
                .map(Provider::Machine0)
                .ok_or_else(|| {
                    let key = if pc.api_key_env.trim().is_empty() {
                        "MACHINE0_API_KEY"
                    } else {
                        pc.api_key_env.trim()
                    };
                    anyhow::anyhow!("the machine0 API key env var {key:?} is not set")
                })
        }
        "vercel" => {
            let id = match &env.placement {
                thegn_core::placement::Placement::Provider(p) => p.id.clone(),
                _ => String::new(),
            };
            crate::provider_factory::vercel_provider_for(pc, &id)
                .map(Provider::Vercel)
                .ok_or_else(|| vercel_token_error(pc))
        }
        other => anyhow::bail!(
            "API provisioning supports 'daytona', 'sprites', 'fly', 'machine0', 'vercel', and VPS kinds \
             (hetzner, digitalocean); env {} uses {:?}",
            env.name,
            other
        ),
    }
}

/// The error for an unbuildable Vercel provider: names the token REF (or the
/// default var), never a value.
fn vercel_token_error(pc: &thegn_core::config::EnvProviderConfig) -> anyhow::Error {
    let key = if pc.api_key_env.trim().is_empty() {
        "VERCEL_TOKEN"
    } else {
        pc.api_key_env.trim()
    };
    anyhow::anyhow!(
        "the Vercel API token {key:?} could not be resolved (or team_id/project_id is invalid)"
    )
}

/// Create a managed sandbox via the API provider and print its id. When the
/// provider can translate egress and the env declares a network allow/block
/// list, verify the lowered policy before admitting the sandbox.
fn provision(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let env = resolve_for(cfg, worktree.clone())?;
    let provider = api_provider(cfg, worktree)?;
    let rt = tokio::runtime::Runtime::new()?;
    let handle = rt.block_on(provider.create())?;
    let allow = &env.sandbox.network_allow;
    let block = &env.sandbox.network_block;
    if provider.caps().egress && (!allow.is_empty() || !block.is_empty()) {
        admit_egress(
            &ProviderEgressSteps {
                provider: &provider,
                runtime: &rt,
            },
            &handle.id,
            allow,
            block,
        )?;
    }
    if crate::egress_admit::lists_unenforced(provider.caps().egress, allow, block) {
        outln!(
            "{}",
            crate::egress_admit::unsupported_note(crate::egress_admit::provider_label(&provider))
        );
    }
    outln!("created sandbox: {}", handle.id);
    match handle.exec {
        thegn_svc::provider::ExecKind::Command(argv) => {
            outln!("exec via: {}", argv.join(" "));
        }
        thegn_svc::provider::ExecKind::Ssh(t) => {
            outln!("exec via ssh: {}:{}", t.host, t.port);
        }
    }
    if provider.caps().egress && (!allow.is_empty() || !block.is_empty()) {
        outln!(
            "applied and verified network policy: {} allow, {} block",
            allow.len(),
            block.len()
        );
    }
    outln!(
        "set `[env.<name>.provider] id = \"{}\"` to attach.",
        handle.id
    );
    Ok(())
}

struct ProviderEgressSteps<'a> {
    provider: &'a thegn_svc::provider::Provider,
    runtime: &'a tokio::runtime::Runtime,
}

impl EgressSteps for ProviderEgressSteps<'_> {
    fn apply(&self, id: &str, allow: &[String], block: &[String]) -> Result<()> {
        self.runtime
            .block_on(self.provider.set_network_policy(id, allow, block))
    }

    fn read_back(&self, id: &str) -> Result<Vec<thegn_svc::provider::PolicyRule>> {
        self.runtime.block_on(self.provider.get_network_policy(id))
    }

    fn destroy(&self, id: &str) -> Result<()> {
        self.runtime.block_on(self.provider.destroy(id))
    }
}

/// Resolve `(provider, sandbox_id)` for checkpoint ops — the sandbox id is the
/// env's configured `[env.<name>.provider] id`.
fn provider_and_id(
    cfg: &Config,
    worktree: Option<String>,
) -> Result<(thegn_svc::provider::Provider, String)> {
    let env = resolve_for(cfg, worktree.clone())?;
    // The resolved placement carries the per-worktree sandbox id.
    let id = match &env.placement {
        thegn_core::placement::Placement::Provider(p) => p.id.clone(),
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
    let env = resolve_for(cfg, worktree.clone())?;
    let wt = super::resolve_worktree(worktree.clone())
        .to_string_lossy()
        .into_owned();
    let (provider, id) = provider_and_id(cfg, worktree)?;
    let notes = crate::provider_git::destroy_guard(
        crate::provider_git::pull_before_destroy(cfg, &wt),
        &id,
        true,
    )
    .map_err(anyhow::Error::msg)?;
    for note in notes {
        if note.warn {
            msg::warn(&note.text);
        } else {
            outln!("{}", note.text);
        }
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(provider.restore(&id, cp))?;
    let egress = crate::egress_admit::reverify_restored_egress(
        &ProviderEgressSteps {
            provider: &provider,
            runtime: &rt,
        },
        provider.caps().egress,
        &id,
        cp,
        &env.sandbox.network_allow,
        &env.sandbox.network_block,
    )?;
    outln!("restored {id} to checkpoint {cp}");
    if crate::egress_admit::lists_unenforced(
        provider.caps().egress,
        &env.sandbox.network_allow,
        &env.sandbox.network_block,
    ) {
        outln!(
            "{}",
            crate::egress_admit::unsupported_note(crate::egress_admit::provider_label(&provider))
        );
    }
    if egress == crate::egress_admit::RestoredEgress::Verified {
        outln!(
            "verified network policy: {} allow, {} block",
            env.sandbox.network_allow.len(),
            env.sandbox.network_block.len()
        );
    }
    Ok(())
}

/// How `env down` leaves a provider sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownMode {
    /// Revoke, destroy and verify the sandbox (the default).
    Destroy,
    /// Park it (`--stop`): fly stop / machine0 suspend; marker and location kept.
    Stop,
}

/// `env down` flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DownOpts {
    mode: DownMode,
    /// Destroy even when the pull-back failed.
    force: bool,
}

enum Lifecycle {
    Up { shell_ready: bool },
    Down(DownOpts),
}

/// How long `env up` waits for a parked sandbox to resume.
const UP_RESUME_TIMEOUT: Duration = Duration::from_secs(180);

/// Provider kinds whose `Provider::suspend` actually parks the sandbox
/// (`fly` stop, `machine0` suspend, `vercel` stop-session: compute to $0, the
/// filesystem persists, and `resume` starts a new session); every other kind's
/// suspend is a no-op.
fn provider_has_stop(kind: &str) -> bool {
    matches!(kind.trim(), "fly" | "machine0" | "vercel")
}

/// The `env down --stop` refusal for a provider without a stop.
fn no_stop_message(kind: &str) -> String {
    let kind = if kind.trim().is_empty() {
        "this provider"
    } else {
        kind.trim()
    };
    format!("{kind} has no stop; the sandbox keeps running. Use `thegn env down` to destroy it")
}

/// Whether `env up` resumes a recorded sandbox first: only a stop-capable
/// provider whose sandbox this machine provisioned (the marker `env down
/// --stop` keeps), so a never-created sandbox is not asked to start. Pure.
fn resume_on_up(kind: &str, id: &str, provisioned_here: bool) -> bool {
    provider_has_stop(kind) && !id.trim().is_empty() && provisioned_here
}

/// The configured provider kind (`[env.<name>.provider] provider`).
fn provider_kind(cfg: &Config, env_name: &str) -> String {
    cfg.env
        .get(env_name)
        .map(|e| e.provider.provider.trim().to_string())
        .unwrap_or_default()
}

/// Resolve a worktree's environment and bring it up / tear it down.
fn lifecycle(cfg: &Config, worktree: Option<String>, action: Lifecycle) -> Result<()> {
    let env = resolve_for(cfg, worktree.clone())?;
    match action {
        Lifecycle::Up { shell_ready } => {
            outln!("bringing up env {} ({})…", env.name, env.placement.label());
            let wt = super::resolve_worktree(worktree)
                .to_string_lossy()
                .into_owned();
            resume_parked_sandbox(cfg, &env, &wt);
            bring_up(&env, &wt, |progress| {
                up_provision(cfg, &wt, shell_ready, progress)
            })?;
            crate::ssh_seed::seed_if_needed(cfg, &wt)
                .with_context(|| format!("env up failed: seeding {wt} on env {}", env.name))?;
            sshfs(&env, true)?;
            outln!("env {} is up", env.name);
        }
        Lifecycle::Down(opts) => {
            outln!("tearing down env {} ({})…", env.name, env.placement.label());
            sshfs(&env, false)?;
            if let thegn_core::placement::Placement::Provider(p) = &env.placement {
                let wt = super::resolve_worktree(worktree)
                    .to_string_lossy()
                    .into_owned();
                return down_provider(cfg, &env, &p.id, &wt, opts);
            }
            if opts.mode == DownMode::Stop {
                anyhow::bail!(
                    "env down --stop parks provider sandboxes; {} is {}",
                    env.name,
                    env.placement.label()
                );
            }
            env.placement
                .teardown()
                .map_err(|e| anyhow::anyhow!("env down failed: {e}"))?;
            outln!("env {} is down", env.name);
        }
    }
    Ok(())
}

/// `env up` on a sandbox `env down --stop` parked: resume it before the
/// provision path probes it. Best-effort and bounded: a failed resume is
/// reported and provisioning continues (it surfaces an unreachable sandbox).
fn resume_parked_sandbox(cfg: &Config, env: &thegn_core::env::Environment, wt: &str) {
    let thegn_core::placement::Placement::Provider(p) = &env.placement else {
        return;
    };
    let kind = provider_kind(cfg, &env.name);
    let id = p.id.as_str();
    if !resume_on_up(
        &kind,
        id,
        !id.trim().is_empty() && crate::provider_workdir::is_provisioned_locally(id),
    ) {
        return;
    }
    let provider = match api_provider(cfg, Some(wt.to_string())) {
        Ok(provider) => provider,
        Err(e) => {
            msg::warn(&format!("could not resume sandbox {id}: {e:#}"));
            return;
        }
    };
    msg::info(&format!("resuming sandbox {id}…"));
    let resumed = crate::agent::block_on_provider(|| async {
        tokio::time::timeout(UP_RESUME_TIMEOUT, provider.resume(id))
            .await
            .map_err(|_| anyhow::anyhow!("timed out after {}s", UP_RESUME_TIMEOUT.as_secs()))?
    });
    if let Err(e) = resumed {
        msg::warn(&format!(
            "could not resume sandbox {id}: {e:#}; provisioning continues"
        ));
    }
}

/// `env down` for a provider env: build the real steps and run
/// [`destroy_provider_sandbox`].
fn down_provider(
    cfg: &Config,
    env: &thegn_core::env::Environment,
    id: &str,
    wt: &str,
    opts: DownOpts,
) -> Result<()> {
    let env_name = env.name.as_str();
    if id.is_empty() {
        if opts.mode == DownMode::Stop {
            anyhow::bail!("env {env_name} has no recorded sandbox for {wt}; nothing to stop");
        }
        env.placement
            .teardown()
            .map_err(|e| anyhow::anyhow!("env down failed: {e}"))?;
        outln!("env {env_name} has no recorded sandbox for {wt}; nothing to tear down");
        return Ok(());
    }
    let kind = provider_kind(cfg, env_name);
    if opts.mode == DownMode::Stop && !provider_has_stop(&kind) {
        anyhow::bail!(no_stop_message(&kind));
    }
    if opts.mode == DownMode::Destroy
        && let Err(error) = thegn_core::provision_journal::cancel(id)
    {
        msg::warn(&format!(
            "could not mark deferred provider setup cancelled for {id}: {error}"
        ));
    }
    let provider_config = cfg
        .env
        .get(env_name)
        .ok_or_else(|| anyhow::anyhow!("env down failed: no [env.{env_name}] defined"))?
        .provider
        .clone();
    let provider = api_provider(cfg, Some(wt.to_string()))?;
    let provider = &provider;
    let mut pull = || crate::provider_git::pull_back_counted(cfg, wt, env_name);
    let mut teardown = || {
        env.placement
            .teardown()
            .map_err(|e| anyhow::anyhow!("env down failed: {e}"))
    };
    let mut suspend = || {
        crate::agent::block_on_provider(|| async { provider.suspend(id).await })
            .map_err(|e| anyhow::anyhow!("env down failed: stop {id} failed: {e:#}"))
    };
    let mut revoke = || {
        let result = crate::remote_enqueue_auth::revoke_for_sandbox(&provider_config, id, Some(wt))
            .map(|_| ())
            .context("route-to-host credential revocation failed; sandbox left intact");
        if result.is_err() {
            reset_deferred_journal(id, wt, env_name);
        }
        result
    };
    let mut destroy = || {
        let result = crate::agent::block_on_provider(|| async { provider.destroy(id).await })
            .map_err(|e| anyhow::anyhow!("env down failed: destroy {id} failed: {e}"));
        if result.is_err() {
            reset_deferred_journal(id, wt, env_name);
        }
        result
    };
    let mut list = || crate::agent::block_on_provider(|| async { provider.inventory().await });
    let mut sleep = std::thread::sleep;
    let mut forget = || forget_destroyed_sandbox(id, wt);
    let result = destroy_provider_sandbox(
        env_name,
        id,
        opts,
        DownSteps {
            pull: &mut pull,
            teardown: &mut teardown,
            suspend: &mut suspend,
            revoke: &mut revoke,
            destroy: &mut destroy,
            list: &mut list,
            sleep: &mut sleep,
            forget: &mut forget,
        },
    );
    if result.is_err() {
        crate::provision_bg::resume_cancelled(id, wt, env_name);
    }
    result
}

fn reset_deferred_journal(id: &str, worktree: &str, env_name: &str) {
    let pending = thegn_core::provision_journal::Journal::pending(worktree, env_name);
    if let Err(error) = thegn_core::provision_journal::write(id, &pending) {
        msg::warn(&format!(
            "could not restore deferred provider setup journal for {id}: {error}"
        ));
    }
}

/// Clear local state after a verified destroy. Failures warn: the sandbox is
/// already gone.
fn forget_destroyed_sandbox(id: &str, wt: &str) {
    crate::provider_workdir::clear_provisioned(id);
    if let Err(error) = thegn_core::provision_journal::clear(id) {
        tracing::debug!(target: "thegn::provider", %id, %error, "could not clear deferred provision journal");
    }
    crate::lifecycle::forget_destroyed_spare(id);
    match Db::open() {
        Ok(db) => {
            if let Err(e) = db.set_worktree_location(wt, "") {
                msg::warn(&format!(
                    "sandbox {id} destroyed, but the provider location for {wt} could not be cleared: {e:#}"
                ));
            }
        }
        Err(e) => msg::warn(&format!(
            "sandbox {id} destroyed, but the provider location for {wt} could not be cleared: {e:#}"
        )),
    }
}

/// How long `env down` waits for the provider to stop listing a destroyed
/// sandbox, and how often it asks.
const DOWN_VERIFY_TIMEOUT: Duration = Duration::from_secs(60);
const DOWN_VERIFY_POLL: Duration = Duration::from_secs(2);

/// The pull-back step `env down` runs first.
type PullStep<'a> = &'a mut dyn FnMut() -> Result<crate::provider_git::CountedPull>;
/// A fallible provider/side-effect step.
type DownStep<'a> = &'a mut dyn FnMut() -> Result<()>;
/// The provider inventory the destroy verification polls.
type ListStep<'a> = &'a mut dyn FnMut() -> Result<thegn_svc::provider::InventoryReport>;

/// The side effects of a provider `env down`, injected so tests drive the
/// ordering with fakes.
struct DownSteps<'a> {
    /// Pull the sandbox's commits back ([`crate::provider_git::pull_back_counted`]).
    pull: PullStep<'a>,
    /// The placement's configured `down_command` (no-op without one).
    teardown: DownStep<'a>,
    /// Park the sandbox (`--stop`).
    suspend: DownStep<'a>,
    /// Revoke the sandbox's route-to-host credential.
    revoke: DownStep<'a>,
    /// Destroy the sandbox through the provider API.
    destroy: DownStep<'a>,
    list: ListStep<'a>,
    sleep: &'a mut dyn FnMut(Duration),
    /// Clear the provisioned marker and provider location (after verify).
    forget: &'a mut dyn FnMut(),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownInventoryDecision {
    AlreadyGone,
    Proceed,
}

/// A complete inventory that lacks the sandbox proves it is already gone.
/// Partial inventories and listing errors prove nothing and keep the normal
/// pull/destroy path intact.
fn down_inventory_decision(
    inventory: Result<thegn_svc::provider::InventoryReport>,
    id: &str,
) -> DownInventoryDecision {
    match inventory {
        Ok(report)
            if matches!(
                crate::provider_git::sandbox_in_inventory(&report, id),
                Some(false)
            ) =>
        {
            DownInventoryDecision::AlreadyGone
        }
        Ok(_) | Err(_) => DownInventoryDecision::Proceed,
    }
}

/// The kept ref of a pull-back that could not fast-forward.
fn kept_ref(outcome: &crate::provider_git::PullOutcome) -> Option<&str> {
    use crate::provider_git::PullOutcome;
    match outcome {
        PullOutcome::Diverged { kept_ref, .. } | PullOutcome::LocalDirty { kept_ref } => {
            Some(kept_ref)
        }
        PullOutcome::UpToDate | PullOutcome::FastForwarded { .. } => None,
    }
}

/// The `env down` refusal when the pull-back failed and `--force` is absent.
fn pull_failed_refusal(id: &str, error: &anyhow::Error) -> String {
    format!(
        "env down: could not pull back from sandbox {id}: {error:#}; nothing was destroyed. Re-run with --force to destroy anyway"
    )
}

/// `env down` for a provider env (THE-2019). First the sandbox's commits are
/// pulled back: up to date / fast-forwarded continues, a diverged or
/// locally-dirty result keeps the VM head in a host ref and continues, and a
/// failed pull refuses unless `--force` (`--stop` only warns: parking loses
/// nothing). Then `--stop` parks the sandbox (marker and location kept), or the
/// default revokes the route-to-host credential, destroys the sandbox and
/// verifies the provider no longer lists it; only after verification are the
/// local provisioned marker and the worktree's provider location cleared.
fn destroy_provider_sandbox(
    env_name: &str,
    id: &str,
    opts: DownOpts,
    steps: DownSteps<'_>,
) -> Result<()> {
    if down_inventory_decision((steps.list)(), id) == DownInventoryDecision::AlreadyGone {
        (steps.teardown)()?;
        (steps.revoke)()?;
        if let Err(e) = (steps.destroy)() {
            msg::warn(&format!(
                "sandbox {id} is already gone, but its provider records could not be cleared: {e:#}; `thegn env reap` finishes them"
            ));
        }
        (steps.forget)();
        outln!("env {env_name}: sandbox {id} is already gone; nothing to pull back or destroy");
        return Ok(());
    }
    match (steps.pull)() {
        Ok((report, dirty)) => {
            outln!("pull-back: {}", report.summary());
            if let Some(kept) = kept_ref(&report.outcome) {
                outln!("VM commits kept at {kept}; merge them yourself");
            }
            if opts.mode == DownMode::Destroy {
                let notes = crate::provider_git::destroy_guard(
                    Ok(crate::provider_git::BeforeDestroy::Pulled {
                        report: Some(report),
                        sandbox_dirty: dirty,
                    }),
                    id,
                    opts.force,
                )
                .map_err(|e| {
                    anyhow::anyhow!("env down: {e} (`thegn env down --force` destroys it anyway)")
                })?;
                for note in notes.into_iter().filter(|note| note.warn) {
                    msg::warn(&note.text);
                }
            }
        }
        Err(e) if opts.mode == DownMode::Stop => msg::warn(&format!(
            "could not pull back from sandbox {id}: {e:#}; its commits stay in the parked sandbox"
        )),
        Err(e) if !opts.force => anyhow::bail!(pull_failed_refusal(id, &e)),
        Err(e) => msg::warn(&format!(
            "could not pull back from sandbox {id}: {e:#}; destroying it anyway (--force): commits made inside it that were not pulled back are lost"
        )),
    }
    if opts.mode == DownMode::Stop {
        (steps.suspend)()?;
        outln!("env {env_name} is stopped (sandbox {id} parked; `thegn env up` resumes it)");
        return Ok(());
    }
    (steps.teardown)()?;
    (steps.revoke)()?;
    (steps.destroy)()?;
    // A deadline of its own: never shared with any other wait.
    let deadline = Instant::now() + DOWN_VERIFY_TIMEOUT;
    wait_until_gone(steps.list, id, deadline, steps.sleep)?;
    (steps.forget)();
    outln!("env {env_name} is down (sandbox {id} destroyed and verified)");
    Ok(())
}

/// Poll `list` until it no longer reports `id`, sleeping [`DOWN_VERIFY_POLL`]
/// (capped at the time left) between asks. A `list` error before `deadline` is
/// retried; at the deadline a still-listed id, or a failing list, is an error.
fn wait_until_gone(
    list: &mut dyn FnMut() -> anyhow::Result<thegn_svc::provider::InventoryReport>,
    id: &str,
    deadline: Instant,
    sleep: &mut dyn FnMut(Duration),
) -> anyhow::Result<()> {
    loop {
        let listed = list();
        if let Ok(report) = &listed
            && crate::provider_git::sandbox_in_inventory(report, id) == Some(false)
        {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return match listed {
                Ok(_) => Err(anyhow::anyhow!(
                    "env down failed: sandbox {id} still exists after destroy (the provider still lists it); retry `thegn env down`"
                )),
                Err(e) => Err(anyhow::anyhow!(
                    "env down: destroy was sent but could not be verified: {e:#}"
                )),
            };
        }
        sleep(DOWN_VERIFY_POLL.min(deadline.saturating_duration_since(now)));
    }
}

/// The `env up` placement step. A **provider** env is provisioned in this
/// process through the same `provision_worktree` the TUI loading screen runs —
/// the only path that sets the sandbox up and writes the local provisioned
/// marker the attach gate checks — so a provider worktree is usable headlessly.
/// Idempotent: an already-provisioned sandbox returns quickly (auth refreshed).
/// Every other placement keeps `Placement::ensure`.
fn bring_up(
    env: &thegn_core::env::Environment,
    worktree: &str,
    provision: impl FnOnce(&mut dyn FnMut(&[ProvisionStepView])) -> Result<bool>,
) -> Result<()> {
    if !env.placement.is_provider() {
        return env
            .placement
            .ensure()
            .map_err(|e| anyhow::anyhow!("env up failed: {e}"));
    }
    env.placement
        .ensure()
        .map_err(|e| anyhow::anyhow!("env up failed: {e}"))?;
    let ran_up_command = matches!(&env.placement, thegn_core::placement::Placement::Provider(p) if !p.up_command.is_empty());
    let provisioned = provision_reporting(provision)
        .with_context(|| format!("env up failed: provisioning {worktree} (env {})", env.name))?;
    anyhow::ensure!(
        provisioned || ran_up_command,
        concat!(
            "env up failed: nothing was provisioned for {} (env {}): the provider could not ",
            "be driven (an unresolved API token, a managed ssh key failure, or no files API; ",
            "see the warning above), or the worktree no longer resolves to a provider env"
        ),
        worktree,
        env.name
    );
    Ok(())
}

fn up_provision(
    cfg: &Config,
    wt: &str,
    shell_ready: bool,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
) -> Result<bool> {
    let outcome = crate::agent::provision_worktree_mode(
        cfg,
        wt,
        crate::agent::ProvisionMode::Foreground,
        &mut *progress,
    )?;
    let Some(deferred) = outcome.deferred else {
        return Ok(outcome.provisioned);
    };
    msg::info(&format!(
        "provision: shell ready on {}; background steps follow",
        deferred.sandbox
    ));
    if shell_ready {
        msg::info(&format!(
            "provision: background steps pending; they finish on the next open or `thegn env up --worktree {wt}`"
        ));
        return Ok(true);
    }
    let mode = match deferred.lease {
        Some(lease) => crate::agent::ProvisionMode::Deferred(lease),
        None => crate::agent::ProvisionMode::All,
    };
    let finished = crate::agent::provision_worktree_mode(cfg, wt, mode, &mut *progress)?;
    Ok(finished.provisioned)
}

/// Run `provision`, printing each step transition as one stderr line.
fn provision_reporting(
    provision: impl FnOnce(&mut dyn FnMut(&[ProvisionStepView])) -> Result<bool>,
) -> Result<bool> {
    let mut reporter = StepReporter::default();
    let mut on_progress = |steps: &[ProvisionStepView]| {
        for line in reporter.changed(steps) {
            msg::info(&line);
        }
    };
    provision(&mut on_progress)
}

/// Turns the loading screen's full step snapshots into one line per step state
/// change (pending steps are silent).
#[derive(Default)]
struct StepReporter {
    seen: Vec<(String, ProvisionState)>,
    started: Vec<(String, Instant)>,
}

impl StepReporter {
    fn changed(&mut self, steps: &[ProvisionStepView]) -> Vec<String> {
        self.changed_at(steps, Instant::now())
    }

    fn changed_at(&mut self, steps: &[ProvisionStepView], now: Instant) -> Vec<String> {
        let mut lines = Vec::new();
        for step in steps {
            if step.state == ProvisionState::Pending {
                continue;
            }
            if step.state == ProvisionState::Active
                && !self.started.iter().any(|(label, _)| label == &step.label)
            {
                self.started.push((step.label.clone(), now));
            }
            let changed = match self.seen.iter_mut().find(|(label, _)| *label == step.label) {
                Some((_, state)) if *state == step.state => false,
                Some((_, state)) => {
                    *state = step.state;
                    true
                }
                None => {
                    self.seen.push((step.label.clone(), step.state));
                    true
                }
            };
            if changed {
                let elapsed = match step.state {
                    ProvisionState::Done | ProvisionState::Failed => self
                        .started
                        .iter()
                        .find(|(label, _)| label == &step.label)
                        .map(|(_, started)| now.saturating_duration_since(*started)),
                    ProvisionState::Pending | ProvisionState::Active => None,
                };
                lines.push(step_line(step, elapsed));
            }
        }
        lines
    }
}

fn step_line(step: &ProvisionStepView, elapsed: Option<Duration>) -> String {
    let state = match step.state {
        ProvisionState::Pending => "pending",
        ProvisionState::Active => "running",
        ProvisionState::Done => "done",
        ProvisionState::Failed => "FAILED",
    };
    let took = elapsed
        .map(|elapsed| format!(" ({:.1} s)", elapsed.as_secs_f64()))
        .unwrap_or_default();
    let detail = step
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| step.state == ProvisionState::Failed && !d.is_empty());
    match detail {
        Some(detail) => format!("provision: {} — {state}{took}: {detail}", step.label),
        None => format!("provision: {} — {state}{took}", step.label),
    }
}

/// Upper bound on a headless open's provisioning (each step is separately
/// bounded by `provision_step_timeout`; this caps the whole run).
const HEADLESS_PROVISION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// What a headless open did before the session started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadlessProvision {
    /// Not a provider env, or the sandbox was already provisioned here.
    None,
    /// A ready warm-pool spare now serves the worktree.
    ClaimedSpare,
    /// The sandbox was provisioned now.
    Provisioned,
}

impl HeadlessProvision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClaimedSpare => "claimed_spare",
            Self::Provisioned => "provisioned",
        }
    }
}

/// Before a headless `session open`: when `worktree` resolves to a provider env
/// whose sandbox is not provisioned on this host, provision it HERE (the CLI
/// process, which has the user's PATH and provider credentials), streaming the
/// steps to stderr. Without this the daemon's attach gate refuses forever —
/// only the TUI loading screen ever provisions. A no-op for every other env and
/// for an already-provisioned sandbox. Runs on a named worker thread, off the
/// async runtime, bounded by [`HEADLESS_PROVISION_DEADLINE`]; past the deadline
/// the worker is abandoned (it dies with the process) and the open fails.
pub(crate) async fn provision_for_headless_open(
    cfg: &Config,
    worktree: String,
) -> Result<HeadlessProvision> {
    let cfg = cfg.clone();
    tokio::task::spawn_blocking(move || {
        provision_headless_blocking(cfg, worktree, HEADLESS_PROVISION_DEADLINE)
    })
    .await
    .context("headless provisioning task failed")?
}

fn provision_headless_blocking(
    cfg: Config,
    worktree: String,
    deadline: std::time::Duration,
) -> Result<HeadlessProvision> {
    // An ssh `in_env` worktree is seeded onto its host before the open, so the
    // headless session lands in the checkout (a no-op for every other env).
    crate::ssh_seed::seed_if_needed(&cfg, &worktree).with_context(|| {
        format!(
            "seeding {worktree} onto its ssh host failed; `thegn env up --worktree {worktree}` retries it"
        )
    })?;
    if let Some((env_name, id)) = crate::provision_gate::provider_target(&cfg, &worktree)?
        && let Some(entry) = thegn_core::provision_journal::read(&id)
    {
        if entry.state == thegn_core::provision_journal::DeferredState::Cancelled {
            anyhow::bail!(
                "background setup for {worktree} was cancelled; reopen it or run `thegn env up --worktree {worktree}` to retry"
            );
        }
        msg::info(&format!(
            "provider env {env_name} for {worktree} has background setup pending — finishing before the headless open"
        ));
        crate::agent::finish_deferred_blocking(&cfg, &worktree, &mut |_| {})?;
        return Ok(HeadlessProvision::Provisioned);
    }
    if crate::provision_gate::claim_spare_for_headless(&cfg, &worktree) {
        msg::info(&format!("claimed a warm spare for {worktree}"));
        return Ok(HeadlessProvision::ClaimedSpare);
    }
    let pending = match crate::provision_gate::unprovisioned_provider_env(&cfg, &worktree)? {
        Some(env_name) => Some(env_name),
        None => crate::provision_gate::deferred_pending_env(&cfg, &worktree)?,
    };
    let Some(env_name) = pending else {
        return Ok(HeadlessProvision::None);
    };
    msg::info(&format!(
        "provider env {env_name} for {worktree} is not fully provisioned yet — provisioning before the headless open"
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    let wt = worktree.clone();
    thegn_core::thread_name::try_spawn_named("thegn-hl-prov", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive);
        let outcome =
            provision_reporting(|progress| crate::agent::provision_worktree(&cfg, &wt, progress));
        if tx.send(outcome).is_err() {
            // The receiver is gone only after the deadline already refused the open.
            tracing::debug!(target: "thegn::provider", worktree = %wt, "headless provision finished after its deadline");
        }
    })
    .context("could not start the headless provisioning thread")?;
    match rx.recv_timeout(deadline) {
        Ok(outcome) => {
            outcome.with_context(|| {
                format!(
                    "provisioning {worktree} (env {env_name}) failed; `thegn env up --worktree {worktree}` retries it"
                )
            })?;
            Ok(HeadlessProvision::Provisioned)
        }
        Err(_) => anyhow::bail!(
            "provisioning {worktree} (env {env_name}) did not finish within {} min; run `thegn env up --worktree {worktree}` and retry",
            deadline.as_secs() / 60
        ),
    }
}

/// Project (or unproject) the worktree for the env's data mode. Delegates to the
/// shared projection layer (`core::projection` plan → `svc::projection` action),
/// the same code path the pane lifecycle auto-runs. A no-op for non-projecting
/// data modes (`in_env`/`local_exec`); a misconfigured `sshfs` env reports why.
fn sshfs(env: &thegn_core::env::Environment, mount: bool) -> Result<()> {
    let Some(spec) = thegn_core::projection::for_environment(env) else {
        if env.data == thegn_core::config::DataMode::Sshfs {
            anyhow::bail!(
                "data = \"sshfs\" needs an ssh placement and [sandbox.remote] remote_dir (env {})",
                env.name
            );
        }
        return Ok(());
    };
    let backend = thegn_svc::projection::for_data_mode(&spec);
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
    let env = resolve_for(cfg, worktree)?;
    let Some(argv) = env.placement.port_forward_argv(spec) else {
        anyhow::bail!(
            "env {} ({}) does not support port forwarding",
            env.name,
            env.placement.label()
        );
    };
    outln!("forwarding {spec} via: {}", argv.join(" "));
    // CLI path: `thegn env forward` runs the tunnel in the foreground, no event loop.
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

/// Fetch the commits made inside the worktree's provider sandbox back into its
/// branch (fast-forward only; see [`crate::provider_git::pull_back`]).
fn pull(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = super::resolve_worktree(worktree)
        .to_string_lossy()
        .into_owned();
    let env = resolve_for(cfg, Some(wt.clone()))?;
    match &env.placement {
        thegn_core::placement::Placement::Local => {
            outln!("nothing to pull: {} runs on this machine", env.name);
            Ok(())
        }
        thegn_core::placement::Placement::Provider(placement) => {
            let report = crate::provider_git::pull_back(cfg, &wt, &env.name)?;
            outln!("{}", report.summary());
            if thegn_core::config::EnvProviderKind::of(placement.provider.trim())
                == thegn_core::config::EnvProviderKind::Vercel
            {
                crate::vercel_pull_marker::clear_for_worktree(cfg, &wt);
            }
            Ok(())
        }
        other => anyhow::bail!(
            "env pull works on provider sandboxes; {} is {}",
            env.name,
            other.label()
        ),
    }
}

/// Resolve the [`Environment`](thegn_core::env::Environment) for a worktree
/// (cwd default), honouring the DB worktree/workspace selection. A failed
/// selection read is an error.
pub(crate) fn resolve_for(
    cfg: &Config,
    worktree: Option<String>,
) -> Result<thegn_core::env::Environment> {
    // Shared resolver: arg → $THEGN_WORKTREE → git toplevel → cwd (returns PathBuf).
    let wt = super::resolve_worktree(worktree)
        .to_string_lossy()
        .into_owned();
    let loc = GitLoc::for_worktree_op(Path::new(&wt), "env resolve").map_err(anyhow::Error::msg)?;
    let repo_root = repo_root_for(&wt);
    let read =
        Db::open().and_then(|db| db.effective_env_checked(&wt, &repo_root.to_string_lossy()));
    resolve_with_selection(cfg, &wt, &repo_root, &loc, read)
}

/// Resolve `wt`'s environment from an already-performed selection read. A
/// failed read is an error naming the worktree, never the default environment.
fn resolve_with_selection(
    cfg: &Config,
    wt: &str,
    repo_root: &Path,
    loc: &GitLoc,
    read: anyhow::Result<Option<String>>,
) -> Result<thegn_core::env::Environment> {
    let selected = read.with_context(|| {
        format!(
            concat!(
                "could not read the environment selection for {wt}; ",
                "refusing to fall back to the default environment"
            ),
            wt = wt
        )
    })?;
    Ok(cfg.resolve_env(
        repo_root,
        loc,
        Path::new(wt),
        &thegn_core::sandbox_pin::EnvSelection {
            name: selected.clone(),
            profile: None,
        },
    ))
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
            thegn_core::config::PlacementMode::Local => String::new(),
            thegn_core::config::PlacementMode::Ssh => {
                format!(" ({})", placement_ssh_target(e, cfg))
            }
            thegn_core::config::PlacementMode::K8s => {
                let ns = if e.k8s.namespace.is_empty() {
                    "-".into()
                } else {
                    e.k8s.namespace.clone()
                };
                format!(" ({ns}/{})", e.k8s.pod)
            }
            thegn_core::config::PlacementMode::Provider => {
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

fn placement_ssh_target(e: &thegn_core::config::EnvConfig, cfg: &Config) -> String {
    if !e.ssh.host.trim().is_empty() {
        e.ssh.host.trim().to_string()
    } else if !cfg.sandbox.remote.host.trim().is_empty() {
        cfg.sandbox.remote.host.trim().to_string()
    } else {
        "<worktree target>".into()
    }
}

fn show(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = super::resolve_worktree(worktree)
        .to_string_lossy()
        .into_owned();
    let loc = GitLoc::for_worktree_op(Path::new(&wt), "env show").map_err(anyhow::Error::msg)?;
    let repo_root = repo_root_for(&wt);
    let read =
        Db::open().and_then(|db| db.effective_env_checked(&wt, &repo_root.to_string_lossy()));
    let env = resolve_with_selection(cfg, &wt, &repo_root, &loc, read)?;
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
    let location = match Db::open().and_then(|db| db.location_for(&wt)) {
        Ok(stored) => location_label(stored.as_deref()),
        Err(e) => format!("unreadable ({e:#})"),
    };
    // No network call: "provisioned" is this machine's local marker.
    let provisioned = match &env.placement {
        thegn_core::placement::Placement::Provider(p) if !p.id.trim().is_empty() => {
            Some(crate::provider_workdir::is_provisioned_locally(&p.id))
        }
        _ => None,
    };
    let labelled: Vec<(String, String)> = env
        .sandbox
        .mounts
        .iter()
        .enumerate()
        .map(|(i, spec)| (format!("sandbox.mounts[{i}]"), spec.clone()))
        .collect();
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from);
    let home = crate::platform::account_home().unwrap_or_else(thegn_core::util::home);
    let warnings = thegn_core::sandbox_runtime_escape::exposure_warnings(
        &labelled,
        runtime_dir.as_deref(),
        &home,
    );
    for line in show_lines(&env, &location, provisioned, &env.sandbox.git, &warnings) {
        outln!("{line}");
    }
    if let thegn_core::placement::Placement::Provider(p) = &env.placement
        && !p.id.trim().is_empty()
        && let Some(line) = crate::provision_bg::show_line(&p.id)
    {
        outln!("{line}");
    }
    Ok(())
}

/// The `location:` value: the stored location, or `local` when none is stored.
fn location_label(stored: Option<&str>) -> String {
    match stored.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "local".to_string(),
    }
}

/// What a sandbox may write in the repository's git directory under `mode`.
fn git_write_scope(mode: thegn_core::config::SandboxGitMode) -> &'static str {
    use thegn_core::config::SandboxGitMode;
    match mode {
        SandboxGitMode::ReadOnly => {
            "read-only: the sandbox writes nothing in the git directory; there is nothing to pull back"
        }
        SandboxGitMode::CommitOnly => {
            "commit-only: objects, this worktree's HEAD/index, refs/heads/<lane dir>/; main, tags, hooks and config are read-only"
        }
        SandboxGitMode::PushLane => {
            "push-lane: as commit-only, and a fast-forwarded pull-back pushes the lane branch (push_refs) to origin from the host"
        }
        SandboxGitMode::Full => "full: the whole git directory is writable",
    }
}

/// The `env show` lines after `isolation:` (THE-2007/THE-2009): the stored
/// location, the recorded sandbox and whether this machine provisioned it
/// (`provisioned` is `None` without a provider sandbox id), and the effective
/// `[sandbox.git]` policy, then one `warning:` line per mount exposure
/// warning (THE-2009). Pure.
fn show_lines(
    env: &thegn_core::env::Environment,
    location: &str,
    provisioned: Option<bool>,
    git: &thegn_core::config::SandboxGitConfig,
    warnings: &[String],
) -> Vec<String> {
    let mut lines = vec![format!("location:  {location}")];
    if let thegn_core::placement::Placement::Provider(p) = &env.placement {
        let sandbox = match provisioned {
            _ if p.id.trim().is_empty() => "none".to_string(),
            Some(true) => format!("{} (provisioned here)", p.id),
            Some(false) | None => {
                format!("{} (recorded, not provisioned on this machine)", p.id)
            }
        };
        lines.push(format!("sandbox:   {sandbox}"));
    }
    lines.push(format!(
        "git:       mode={} transport={} credential={}",
        git.mode, git.transport, git.credential
    ));
    lines.push(format!("           {}", git_write_scope(git.mode)));
    lines.extend(warnings.iter().map(|w| format!("warning:   {w}")));
    lines
}

/// The honest boundary class the resolved environment provides, or `None` when
/// the backend is `auto` (resolved at spawn). Placement owns the boundary first
/// (provider/k8s), otherwise it follows the concrete backend.
fn env_isolation(
    env: &thegn_core::env::Environment,
) -> Option<thegn_core::capabilities::IsolationClass> {
    use thegn_core::placement::Placement;
    match env.placement {
        Placement::Provider(_) | Placement::K8s(_) => Some(
            thegn_core::capabilities::Capabilities::from_parts(
                thegn_core::sandbox::Backend::None,
                &env.placement,
                thegn_core::config::Network::Nat,
                false,
                None,
            )
            .isolation,
        ),
        _ => {
            let backend = thegn_core::sandbox::Backend::from_config(env.sandbox.backend)?;
            let runtime = (!env.sandbox.oci_runtime.trim().is_empty())
                .then_some(env.sandbox.oci_runtime.as_str());
            Some(
                thegn_core::capabilities::Capabilities::from_parts(
                    backend,
                    &env.placement,
                    thegn_core::config::Network::Nat,
                    false,
                    runtime,
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
    // THE-2007: a stable-channel daemon would drop a dev-only env and refuse
    // every later session. Both the worktree and the `--workspace` selection.
    super::wt::refuse_dev_env_on_stable_daemon(cfg, Some(name))?;
    let wt = super::resolve_worktree(worktree)
        .to_string_lossy()
        .into_owned();
    let db = Db::open()?;
    if workspace {
        let repo_root = repo_root_for(&wt);
        let repo_s = repo_root.to_string_lossy().into_owned();
        db.set_workspace_env(&repo_s, name)?;
        if name.is_empty() {
            outln!("cleared project env for {repo_s}");
        } else {
            outln!("project {repo_s} → env {name}");
        }
        warn_provider_rows_in_workspace(&db, &repo_s);
    } else {
        // The env the worktree resolved to before the switch, for the id of a
        // provider sandbox it may leave behind. Unresolvable is not fatal here.
        let old_env = resolve_for(cfg, Some(wt.clone()));
        let old = crate::provider_git::bound_offline(cfg, &wt).ok();
        let stored_is_provider = is_provider_location(
            &wt,
            db.location_for(&wt)
                .with_context(|| format!("read the stored location of {wt}"))?
                .as_deref(),
        );
        let mut left_work = false;
        if let Some(crate::provider_git::OfflineBinding::Bound { sandbox }) = old.as_ref()
            && env_changed(&old_env, name)
        {
            match crate::provider_git::pull_before_destroy(cfg, &wt) {
                Ok(crate::provider_git::BeforeDestroy::Pulled {
                    report: Some(report),
                    sandbox_dirty,
                }) => {
                    outln!("pulled {}: {}", report.branch, report.summary());
                    if let Some(dirty) = sandbox_dirty.filter(|dirty| *dirty > 0) {
                        left_work = true;
                        msg::warn(&format!(
                            "sandbox {sandbox}: {dirty} uncommitted or untracked path(s) stay in the sandbox"
                        ));
                    } else if sandbox_dirty.is_none() {
                        left_work = true;
                    }
                }
                Err(error) => {
                    left_work = true;
                    msg::warn(&format!(
                        "could not pull sandbox {sandbox} before switching ({error:#}); it is kept with any commits not pulled — switch back and run `thegn env pull`, or `thegn env down`"
                    ));
                }
                _ => left_work = true,
            }
        }
        db.set_worktree_env(&wt, name)?;
        if name.is_empty() {
            outln!("cleared env for {wt}");
        } else {
            outln!("worktree {wt} → env {name}");
        }
        if env_changed(&old_env, name)
            && let Some(sandbox) = set_notice(old.as_ref(), stored_is_provider)
        {
            let old_name = old_env
                .as_ref()
                .map(|old_env| old_env.name.as_str())
                .unwrap_or(name);
            if left_work {
                msg::warn(&format!(
                    "the previous provider sandbox {sandbox} is still running and may hold work that was not pulled; switch back (`thegn env set {old_name} --worktree {wt}`) and run `thegn env down --worktree {wt}`, which pulls first"
                ));
            } else {
                msg::warn(&format!(
                    "the previous provider sandbox {sandbox} is still running; destroy it with `thegn env deprovision --env {old_name} {sandbox}`"
                ));
            }
        }
        reconcile_location_after_set(cfg, &db, &wt, old_env.as_ref().ok())?;
    }
    Ok(())
}

fn set_notice(
    old: Option<&crate::provider_git::OfflineBinding>,
    stored_is_provider: bool,
) -> Option<&str> {
    if stored_is_provider {
        return None;
    }
    match old {
        Some(crate::provider_git::OfflineBinding::Bound { sandbox }) => Some(sandbox),
        _ => None,
    }
}

fn env_changed(old_env: &Result<thegn_core::env::Environment>, name: &str) -> bool {
    old_env.as_ref().is_ok_and(|old_env| old_env.name != name)
}

/// What `env set` does with a worktree's stored location after the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocationAction {
    /// Leave the stored location as it is.
    Keep,
    /// Clear it: the worktree now runs from its local checkout.
    ClearToLocal,
}

/// A provider location left behind by a switch to a local env would make the
/// next launch bind the sandbox path (e.g. `/workspace`) on the host (D13).
fn location_action(old_location_is_remote: bool, new_env_is_local: bool) -> LocationAction {
    if old_location_is_remote && new_env_is_local {
        LocationAction::ClearToLocal
    } else {
        LocationAction::Keep
    }
}

/// After `env set` on a worktree: clear a stale provider location when the new
/// env runs locally. The old sandbox is never destroyed here; its id is named
/// so the user can deprovision it.
fn reconcile_location_after_set(
    cfg: &Config,
    db: &Db,
    wt: &str,
    old_env: Option<&thegn_core::env::Environment>,
) -> Result<()> {
    let stored = db
        .location_for(wt)
        .with_context(|| format!("read the stored location of {wt}"))?;
    let old_location_is_remote = is_provider_location(wt, stored.as_deref());
    if !old_location_is_remote {
        return Ok(());
    }
    // Resolve the new selection as if the worktree were local, so the stale
    // provider location cannot itself make the default env look remote.
    let repo_root = repo_root_for(wt);
    let selection = db.effective_env_checked(wt, &repo_root.to_string_lossy());
    let local = GitLoc::Local(PathBuf::from(wt));
    let new_env = resolve_with_selection(cfg, wt, &repo_root, &local, selection)?;
    if location_action(old_location_is_remote, new_env.placement.is_local()) == LocationAction::Keep
    {
        return Ok(());
    }
    db.set_worktree_location(wt, "")
        .with_context(|| format!("clear the provider location of {wt}"))?;
    outln!(
        "{wt} now runs locally (env {}); its provider location was cleared",
        new_env.name
    );
    if let Some(old) = old_env
        && let thegn_core::placement::Placement::Provider(p) = &old.placement
        && !p.id.is_empty()
    {
        msg::warn(&format!(
            "the previous provider sandbox {} is still running; destroy it with `thegn env deprovision --env {} {}`",
            p.id, old.name, p.id
        ));
    }
    Ok(())
}

/// After `env set --workspace`: rows are not rewritten, but a worktree still
/// carrying a provider location keeps binding its sandbox path. Warn once.
fn warn_provider_rows_in_workspace(db: &Db, repo_root: &str) {
    let rows = match db.worktrees() {
        Ok(rows) => rows,
        Err(e) => {
            msg::warn(&format!(
                "could not check {repo_root}'s worktrees for provider locations: {e:#}"
            ));
            return;
        }
    };
    let provider_rows: Vec<&str> = rows
        .iter()
        .filter(|row| row.repo_root == repo_root)
        .filter(|row| is_provider_location(&row.worktree, Some(&row.location)))
        .map(|row| row.worktree.as_str())
        .collect();
    if let Some(first) = provider_rows.first() {
        msg::warn(&format!(
            "{} worktree(s) of {repo_root} still carry a provider location (e.g. {first}); the project env does not rebind them: run `thegn env set <env>` on each",
            provider_rows.len()
        ));
    }
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

#[cfg(test)]
mod egress_tests {
    use super::{EgressSteps, admit_egress};
    use std::cell::RefCell;
    use thegn_svc::provider::{PolicyRule, rules_from};

    struct ScriptedEgress {
        calls: RefCell<Vec<String>>,
        apply_result: RefCell<Option<anyhow::Result<()>>>,
        read_result: RefCell<Option<anyhow::Result<Vec<PolicyRule>>>>,
        destroy_result: RefCell<Option<anyhow::Result<()>>>,
    }

    impl ScriptedEgress {
        fn new(
            apply_result: anyhow::Result<()>,
            read_result: anyhow::Result<Vec<PolicyRule>>,
            destroy_result: anyhow::Result<()>,
        ) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                apply_result: RefCell::new(Some(apply_result)),
                read_result: RefCell::new(Some(read_result)),
                destroy_result: RefCell::new(Some(destroy_result)),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl EgressSteps for ScriptedEgress {
        fn apply(&self, id: &str, _allow: &[String], _block: &[String]) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("apply:{id}"));
            self.apply_result
                .borrow_mut()
                .take()
                .expect("apply result is scripted once")
        }

        fn read_back(&self, id: &str) -> anyhow::Result<Vec<PolicyRule>> {
            self.calls.borrow_mut().push(format!("read:{id}"));
            self.read_result
                .borrow_mut()
                .take()
                .expect("read result is scripted once")
        }

        fn destroy(&self, id: &str) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("destroy:{id}"));
            self.destroy_result
                .borrow_mut()
                .take()
                .expect("destroy result is scripted once")
        }
    }

    fn policy_lists() -> (Vec<String>, Vec<String>) {
        (vec!["allowed.test".into()], vec!["blocked.test".into()])
    }

    #[test]
    fn matching_readback_admits_without_destroy() {
        let (allow, block) = policy_lists();
        let steps = ScriptedEgress::new(Ok(()), Ok(rules_from(&allow, &block).unwrap()), Ok(()));

        assert!(admit_egress(&steps, "sandbox-1", &allow, &block).is_ok());
        assert_eq!(steps.calls(), ["apply:sandbox-1", "read:sandbox-1"]);
    }

    #[test]
    fn apply_failure_destroys_and_errs() {
        let (allow, block) = policy_lists();
        let steps =
            ScriptedEgress::new(Err(anyhow::anyhow!("apply failed")), Ok(Vec::new()), Ok(()));

        let result = admit_egress(&steps, "sandbox-1", &allow, &block);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| { message.contains("was destroyed") })
        );
        assert_eq!(steps.calls(), ["apply:sandbox-1", "destroy:sandbox-1"]);
    }

    #[test]
    fn readback_error_destroys_and_errs() {
        let (allow, block) = policy_lists();
        let steps = ScriptedEgress::new(Ok(()), Err(anyhow::anyhow!("read failed")), Ok(()));

        let result = admit_egress(&steps, "sandbox-1", &allow, &block);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| { message.contains("was destroyed") })
        );
        assert_eq!(
            steps.calls(),
            ["apply:sandbox-1", "read:sandbox-1", "destroy:sandbox-1"]
        );
    }

    #[test]
    fn empty_readback_mismatch_destroys_and_errs() {
        let (allow, block) = policy_lists();
        let steps = ScriptedEgress::new(Ok(()), Ok(Vec::new()), Ok(()));

        let result = admit_egress(&steps, "sandbox-1", &allow, &block);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| {
                    message.contains("was destroyed")
                        && message.contains("expected 3 rules, observed 0")
                })
        );
        assert_eq!(
            steps.calls(),
            ["apply:sandbox-1", "read:sandbox-1", "destroy:sandbox-1"]
        );
    }

    #[test]
    fn reordered_readback_mismatch_destroys_and_errs() {
        let (allow, block) = policy_lists();
        let expected = rules_from(&allow, &block).unwrap();
        let observed = vec![
            expected[1].clone(),
            expected[0].clone(),
            expected[2].clone(),
        ];
        let steps = ScriptedEgress::new(Ok(()), Ok(observed), Ok(()));

        let result = admit_egress(&steps, "sandbox-1", &allow, &block);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| {
                    message.contains("was destroyed")
                        && message.contains("expected 3 rules, observed 3")
                })
        );
        assert_eq!(
            steps.calls(),
            ["apply:sandbox-1", "read:sandbox-1", "destroy:sandbox-1"]
        );
    }

    #[test]
    fn destroy_failure_is_reported_as_unrestricted() {
        let (allow, block) = policy_lists();
        let steps = ScriptedEgress::new(
            Err(anyhow::anyhow!("apply failed")),
            Ok(Vec::new()),
            Err(anyhow::anyhow!("destroy failed")),
        );

        let result = admit_egress(&steps, "sandbox-1", &allow, &block);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .map(|error| error.to_string())
                .is_some_and(|message| {
                    message.contains("WITHOUT the declared egress policy")
                        && message.contains("thegn env deprovision sandbox-1")
                })
        );
        assert_eq!(steps.calls(), ["apply:sandbox-1", "destroy:sandbox-1"]);
    }
}

#[cfg(test)]
mod vercel_surface_tests {
    use super::*;

    #[test]
    fn create_vercel_writes_ids_and_a_ref_not_a_value() {
        let _state = crate::testenv::IsolatedState::new();
        create_env(CreateArgs {
            name: "vc".into(),
            provider: "vercel".into(),
            team_id: Some("team_abc".into()),
            project_id: Some("prj_abc".into()),
            token_env: Some("VERCEL_TOKEN".into()),
            ..Default::default()
        })
        .expect("create");
        let text = std::fs::read_to_string(Config::path()).unwrap();
        assert!(text.contains("provider = \"vercel\""), "{text}");
        assert!(text.contains("team_id = \"team_abc\""), "{text}");
        assert!(text.contains("project_id = \"prj_abc\""), "{text}");
        assert!(
            text.contains("api_key_env = \"env:VERCEL_TOKEN\""),
            "{text}"
        );
    }

    #[test]
    fn vercel_token_error_names_the_ref_never_a_value() {
        let pc = thegn_core::config::EnvProviderConfig {
            provider: "vercel".into(),
            api_key_env: "env:MY_VERCEL_REF".into(),
            ..Default::default()
        };
        let msg = vercel_token_error(&pc).to_string();
        assert!(msg.contains("env:MY_VERCEL_REF"), "{msg}");
        let default = vercel_token_error(&thegn_core::config::EnvProviderConfig::default());
        assert!(default.to_string().contains("VERCEL_TOKEN"));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use thegn_core::config::{EnvConfig, PlacementMode};

    #[test]
    fn resolve_with_selection_refuses_an_unreadable_selection() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().expect("temporary worktree");
        let wt = dir.path().to_string_lossy().into_owned();
        let repo_root = dir.path();
        let loc = GitLoc::Local(dir.path().to_path_buf());

        let result = resolve_with_selection(
            &Config::default(),
            &wt,
            repo_root,
            &loc,
            Err(anyhow::anyhow!("database is locked")),
        );
        let Err(error) = result else {
            panic!("unreadable selection must refuse env resolution");
        };
        let error = format!("{error:#}");
        assert!(error.contains("could not read the environment selection for"));
        assert!(error.contains(&wt));
        assert!(error.contains("database is locked"));
    }

    #[test]
    fn resolve_with_selection_keeps_the_default_when_nothing_is_selected() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().expect("temporary worktree");
        let wt = dir.path().to_string_lossy().into_owned();
        let repo_root = dir.path();
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let cfg = Config::default();
        let expected = cfg.resolve_env(
            repo_root,
            &loc,
            dir.path(),
            &thegn_core::sandbox_pin::EnvSelection::none(),
        );

        let actual = resolve_with_selection(&cfg, &wt, repo_root, &loc, Ok(None))
            .expect("no selection keeps default resolution");
        assert_eq!(actual.name, expected.name);
    }

    #[test]
    fn resolve_with_selection_uses_the_selected_env_name() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().expect("temporary worktree");
        let wt = dir.path().to_string_lossy().into_owned();
        let repo_root = dir.path();
        let loc = GitLoc::Local(dir.path().to_path_buf());
        let mut cfg = Config::default();
        cfg.env.insert(
            "box".into(),
            EnvConfig {
                placement: PlacementMode::Local,
                ..Default::default()
            },
        );

        let actual = resolve_with_selection(&cfg, &wt, repo_root, &loc, Ok(Some("box".into())))
            .expect("selected env resolves");
        assert_eq!(actual.name, "box");
        assert!(!actual.unresolved_selection);
    }

    #[test]
    fn resolve_for_refuses_an_unrecognised_worktree_location() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open().unwrap();
        db.put_worktree(
            "repo/env",
            &dir.path().to_string_lossy(),
            &dir.path().to_string_lossy(),
            "feature",
            Some("garbage"),
            None,
        )
        .unwrap();

        let result = resolve_for(
            &Config::default(),
            Some(dir.path().to_string_lossy().into_owned()),
        );
        assert!(
            matches!(result, Err(error) if format!("{error:#}").contains("env resolve refused"))
        );
    }

    #[test]
    fn resolve_for_keeps_a_row_less_worktree_local() {
        // Positive control: passes at 774069ac2 too. repo_root_for opens the DB.
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_for(
            &Config::default(),
            Some(dir.path().to_string_lossy().into_owned()),
        );
        assert!(result.is_ok());
        let env = result.unwrap();
        assert_eq!(env.name, "default");
        assert!(env.placement.is_local());
    }
}

#[cfg(test)]
mod up_tests {
    use super::*;
    use thegn_core::placement::{Placement, ProviderPlacement};

    fn provider_env() -> thegn_core::env::Environment {
        thegn_core::env::Environment {
            name: "sprites".into(),
            placement: Placement::Provider(ProviderPlacement {
                provider: "fake".into(),
                id: "fake-sandbox".into(),
                interactive_prefix: Vec::new(),
                control_prefix: Vec::new(),
                up_command: Vec::new(),
                down_command: Vec::new(),
            }),
            sandbox: Default::default(),
            data: thegn_core::config::DataMode::InEnv,
            unresolved_selection: false,
        }
    }

    fn step(label: &str, state: ProvisionState, detail: Option<&str>) -> ProvisionStepView {
        ProvisionStepView {
            label: label.into(),
            state,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn env_up_on_a_provider_env_runs_the_provision_path() {
        let mut calls = 0;
        bring_up(&provider_env(), "/wt/app", |progress| {
            calls += 1;
            progress(&[step("workspace", ProvisionState::Done, None)]);
            Ok(true)
        })
        .expect("provider env up succeeds when provisioning does");
        assert_eq!(calls, 1);
    }

    #[test]
    fn env_up_surfaces_the_provision_error() {
        let result = bring_up(&provider_env(), "/wt/app", |_| {
            Err(anyhow::anyhow!("sprite create: 401 unauthorized"))
        });
        let error = format!("{:#}", result.expect_err("a failed provision fails env up"));
        assert!(error.contains("env up failed"), "{error}");
        assert!(error.contains("/wt/app"), "{error}");
        assert!(error.contains("401 unauthorized"), "{error}");
    }

    #[test]
    fn env_up_refuses_when_nothing_was_provisioned() {
        let result = bring_up(&provider_env(), "/wt/app", |_| Ok(false));
        let error = format!(
            "{:#}",
            result.expect_err("an un-provisioned provider env fails")
        );
        assert!(error.contains("nothing was provisioned"), "{error}");
    }

    #[test]
    fn env_up_on_a_local_env_never_provisions() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().to_string_lossy().into_owned();
        let env = resolve_for(&Config::default(), Some(wt.clone())).expect("local env");
        assert!(env.placement.is_local());
        bring_up(&env, &wt, |_| panic!("a local env must not provision"))
            .expect("local env up is unchanged");
    }

    #[test]
    fn step_reporter_prints_each_state_change_once() {
        let mut reporter = StepReporter::default();
        let started = Instant::now();
        let first = reporter.changed_at(
            &[
                step("workspace", ProvisionState::Active, None),
                step("clone", ProvisionState::Pending, None),
            ],
            started,
        );
        assert_eq!(first, vec!["provision: workspace — running".to_string()]);
        let again = reporter.changed_at(
            &[
                step("workspace", ProvisionState::Active, Some("mkdir")),
                step("clone", ProvisionState::Pending, None),
            ],
            started + Duration::from_secs(1),
        );
        assert!(again.is_empty(), "{again:?}");
        let next = reporter.changed_at(
            &[
                step("workspace", ProvisionState::Done, None),
                step("clone", ProvisionState::Failed, Some("git: auth failed")),
            ],
            started + Duration::from_secs(2),
        );
        assert_eq!(
            next,
            vec![
                "provision: workspace — done (2.0 s)".to_string(),
                "provision: clone — FAILED: git: auth failed".to_string(),
            ]
        );
    }

    #[test]
    fn step_reporter_includes_elapsed_time_for_failed_steps() {
        let mut reporter = StepReporter::default();
        let started = Instant::now();
        reporter.changed_at(&[step("clone", ProvisionState::Active, None)], started);
        let lines = reporter.changed_at(
            &[step(
                "clone",
                ProvisionState::Failed,
                Some("git: auth failed"),
            )],
            started + Duration::from_millis(1_234),
        );
        assert_eq!(
            lines,
            vec!["provision: clone — FAILED (1.2 s): git: auth failed".to_string()]
        );
    }

    #[test]
    fn show_lines_report_location_sandbox_and_git_policy() {
        use thegn_core::config::{SandboxGitConfig, SandboxGitMode};
        let git = SandboxGitConfig::default();
        let env = provider_env();
        assert_eq!(
            show_lines(&env, "provider:…", Some(true), &git, &[]),
            vec![
                "location:  provider:…".to_string(),
                "sandbox:   fake-sandbox (provisioned here)".to_string(),
                "git:       mode=commit-only transport=bundle credential=none".to_string(),
                "           commit-only: objects, this worktree's HEAD/index, refs/heads/<lane dir>/; main, tags, hooks and config are read-only".to_string(),
            ]
        );
        let recorded = show_lines(&env, "local", Some(false), &git, &[]);
        assert_eq!(
            recorded[1],
            "sandbox:   fake-sandbox (recorded, not provisioned on this machine)"
        );
        let mut no_id = provider_env();
        if let Placement::Provider(p) = &mut no_id.placement {
            p.id.clear();
        }
        assert_eq!(
            show_lines(&no_id, "local", None, &git, &[])[1],
            "sandbox:   none"
        );
        let local = thegn_core::env::Environment {
            placement: Placement::Local,
            ..provider_env()
        };
        let read_only = SandboxGitConfig {
            mode: SandboxGitMode::ReadOnly,
            ..SandboxGitConfig::default()
        };
        let lines = show_lines(&local, "local", None, &read_only, &[]);
        assert_eq!(lines.len(), 3, "no sandbox line off a provider: {lines:?}");
        assert_eq!(lines[0], "location:  local");
        assert!(
            lines[1].starts_with("git:       mode=read-only "),
            "{lines:?}"
        );
        assert!(lines[2].contains("nothing to pull back"), "{lines:?}");
    }

    #[test]
    fn show_lines_print_mount_exposure_warnings() {
        let git = thegn_core::config::SandboxGitConfig::default();
        let env = provider_env();
        let labelled = vec![("sandbox.mounts[0]".to_string(), "/run/user".to_string())];
        let warnings = thegn_core::sandbox_runtime_escape::exposure_warnings(
            &labelled,
            Some(std::path::Path::new("/run/user/1000")),
            std::path::Path::new("/home/someone"),
        );
        assert!(!warnings.is_empty(), "/run/user exposes the runtime dir");
        let plain = show_lines(&env, "local", Some(true), &git, &[]);
        let warned = show_lines(&env, "local", Some(true), &git, &warnings);
        assert_eq!(&warned[..plain.len()], &plain[..], "warnings come last");
        assert_eq!(warned.len(), plain.len() + warnings.len());
        for (line, warning) in warned[plain.len()..].iter().zip(&warnings) {
            assert_eq!(line, &format!("warning:   {warning}"));
        }
        assert!(
            warned.last().unwrap().contains("sandbox.mounts[0]"),
            "{warned:?}"
        );
    }

    #[test]
    fn location_label_defaults_to_local() {
        assert_eq!(location_label(None), "local");
        assert_eq!(location_label(Some("  ")), "local");
        assert_eq!(location_label(Some("ssh://box/wt")), "ssh://box/wt");
    }

    #[test]
    fn every_git_mode_names_its_write_scope() {
        use thegn_core::config::SandboxGitMode;
        for (mode, prefix) in [
            (SandboxGitMode::ReadOnly, "read-only:"),
            (SandboxGitMode::CommitOnly, "commit-only:"),
            (SandboxGitMode::PushLane, "push-lane:"),
            (SandboxGitMode::Full, "full:"),
        ] {
            assert!(git_write_scope(mode).starts_with(prefix), "{mode:?}");
        }
    }

    #[test]
    fn headless_provision_names() {
        assert_eq!(HeadlessProvision::None.as_str(), "none");
        assert_eq!(HeadlessProvision::ClaimedSpare.as_str(), "claimed_spare");
        assert_eq!(HeadlessProvision::Provisioned.as_str(), "provisioned");
    }

    #[test]
    fn headless_open_skips_a_local_worktree() {
        let _state = crate::testenv::IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().to_string_lossy().into_owned();
        provision_headless_blocking(Config::default(), wt, std::time::Duration::from_secs(5))
            .expect("a local worktree needs no provisioning");
    }
}

#[cfg(test)]
mod down_tests {
    use super::{
        DownInventoryDecision, DownMode, DownOpts, DownSteps, LocationAction,
        destroy_provider_sandbox, down_inventory_decision, is_provider_location, kept_ref,
        location_action, no_stop_message, provider_has_stop, pull_failed_refusal, resume_on_up,
        wait_until_gone,
    };
    use crate::provider_git::{PullOutcome, PullReport};
    use std::cell::RefCell;
    use std::time::{Duration, Instant};
    use thegn_svc::provider::{InventoryReport, SandboxInfo};

    type ListAnswer = anyhow::Result<InventoryReport>;

    fn inventory(ids: &[&str]) -> InventoryReport {
        InventoryReport::Complete(ids.iter().map(|id| SandboxInfo::id_only(*id)).collect())
    }

    fn report(outcome: PullOutcome) -> PullReport {
        PullReport {
            branch: "tg/lane".into(),
            outcome,
            push: None,
        }
    }

    fn destroy() -> DownOpts {
        DownOpts {
            mode: DownMode::Destroy,
            force: false,
        }
    }

    /// Run `env down` against recording fakes; returns the result and the
    /// order the steps ran in.
    fn run_down(
        answer: anyhow::Result<PullReport>,
        opts: DownOpts,
    ) -> (anyhow::Result<()>, Vec<&'static str>) {
        run_down_with_dirty(answer, Some(0), opts)
    }

    fn run_down_with_dirty(
        answer: anyhow::Result<PullReport>,
        dirty: Option<u64>,
        opts: DownOpts,
    ) -> (anyhow::Result<()>, Vec<&'static str>) {
        let calls = RefCell::new(Vec::new());
        let mut answer = Some(answer);
        let mut pull = || {
            calls.borrow_mut().push("pull");
            answer
                .take()
                .expect("one pull")
                .map(|report| (report, dirty))
        };
        let mut teardown = || -> anyhow::Result<()> {
            calls.borrow_mut().push("teardown");
            Ok(())
        };
        let mut suspend = || -> anyhow::Result<()> {
            calls.borrow_mut().push("suspend");
            Ok(())
        };
        let mut revoke = || -> anyhow::Result<()> {
            calls.borrow_mut().push("revoke");
            Ok(())
        };
        let mut destroy = || -> anyhow::Result<()> {
            calls.borrow_mut().push("destroy");
            Ok(())
        };
        let mut list_calls = 0;
        let mut list = || -> anyhow::Result<InventoryReport> {
            calls.borrow_mut().push("list");
            list_calls += 1;
            if list_calls == 1 {
                Ok(inventory(&["sbx-1"]))
            } else {
                Ok(inventory(&[]))
            }
        };
        let mut sleep = |_: Duration| calls.borrow_mut().push("sleep");
        let mut forget = || calls.borrow_mut().push("forget");
        let result = destroy_provider_sandbox(
            "fly-dev",
            "sbx-1",
            opts,
            DownSteps {
                pull: &mut pull,
                teardown: &mut teardown,
                suspend: &mut suspend,
                revoke: &mut revoke,
                destroy: &mut destroy,
                list: &mut list,
                sleep: &mut sleep,
                forget: &mut forget,
            },
        );
        let calls = calls.borrow().clone();
        (result, calls)
    }

    const DESTROYED: [&str; 7] = [
        "list", "pull", "teardown", "revoke", "destroy", "list", "forget",
    ];

    #[test]
    fn an_up_to_date_pull_destroys_and_forgets_the_sandbox() {
        let (result, calls) = run_down(Ok(report(PullOutcome::UpToDate)), destroy());
        result.expect("env down succeeds");
        assert_eq!(calls, DESTROYED);
    }

    #[test]
    fn a_fast_forwarded_pull_destroys() {
        let outcome = PullOutcome::FastForwarded {
            from: "a".repeat(40),
            to: "b".repeat(40),
            commits: 2,
        };
        let (result, calls) = run_down(Ok(report(outcome)), destroy());
        result.expect("env down succeeds");
        assert_eq!(calls, DESTROYED);
    }

    #[test]
    fn a_diverged_pull_keeps_the_ref_and_destroys() {
        let outcome = PullOutcome::Diverged {
            kept_ref: "refs/thegn/pull/lane-abc".into(),
            vm_head: "c".repeat(40),
        };
        assert_eq!(kept_ref(&outcome), Some("refs/thegn/pull/lane-abc"));
        let (result, calls) = run_down(Ok(report(outcome)), destroy());
        result.expect("nothing is lost: env down continues");
        assert_eq!(calls, DESTROYED);
    }

    #[test]
    fn a_locally_dirty_pull_keeps_the_ref_and_destroys() {
        let outcome = PullOutcome::LocalDirty {
            kept_ref: "refs/thegn/pull/lane-abc".into(),
        };
        assert_eq!(kept_ref(&outcome), Some("refs/thegn/pull/lane-abc"));
        assert_eq!(kept_ref(&PullOutcome::UpToDate), None);
        let (result, calls) = run_down(Ok(report(outcome)), destroy());
        result.expect("env down continues");
        assert_eq!(calls, DESTROYED);
    }

    #[test]
    fn a_failed_pull_refuses_without_force_and_destroys_nothing() {
        let (result, calls) = run_down(Err(anyhow::anyhow!("sandbox unreachable")), destroy());
        let error = format!("{:#}", result.expect_err("a failed pull refuses"));
        assert_eq!(calls, ["list", "pull"]);
        assert!(
            error.contains("could not pull back from sandbox sbx-1: sandbox unreachable"),
            "{error}"
        );
        assert!(error.contains("nothing was destroyed"), "{error}");
        assert!(error.contains("--force"), "{error}");
    }

    #[test]
    fn a_failed_pull_with_force_destroys_anyway() {
        let opts = DownOpts {
            mode: DownMode::Destroy,
            force: true,
        };
        let (result, calls) = run_down(Err(anyhow::anyhow!("sandbox unreachable")), opts);
        result.expect("--force destroys");
        assert_eq!(calls, DESTROYED);
    }

    #[test]
    fn a_dirty_sandbox_refuses_without_force() {
        let (result, calls) =
            run_down_with_dirty(Ok(report(PullOutcome::UpToDate)), Some(2), destroy());
        let error = format!("{}", result.expect_err("dirty sandbox refuses"));
        assert!(error.contains("uncommitted"), "{error}");
        assert_eq!(calls, ["list", "pull"]);
    }

    #[test]
    fn a_forced_dirty_sandbox_destroys() {
        let opts = DownOpts {
            mode: DownMode::Destroy,
            force: true,
        };
        let (result, calls) = run_down_with_dirty(Ok(report(PullOutcome::UpToDate)), Some(2), opts);
        result.expect("forced dirty sandbox destroys");
        assert_eq!(calls, DESTROYED);
    }

    #[test]
    fn stop_with_dirt_parks_sandbox() {
        let opts = DownOpts {
            mode: DownMode::Stop,
            force: false,
        };
        let (result, calls) = run_down_with_dirty(Ok(report(PullOutcome::UpToDate)), Some(2), opts);
        result.expect("stop does not refuse dirt");
        assert_eq!(calls, ["list", "pull", "suspend"]);
    }

    #[test]
    fn unknown_dirty_sandbox_refuses_without_force() {
        let (result, calls) =
            run_down_with_dirty(Ok(report(PullOutcome::UpToDate)), None, destroy());
        let error = format!("{}", result.expect_err("unknown dirt refuses"));
        assert!(error.contains("could not tell"), "{error}");
        assert_eq!(calls, ["list", "pull"]);
    }

    #[test]
    fn stop_suspends_and_keeps_the_marker() {
        let opts = DownOpts {
            mode: DownMode::Stop,
            force: false,
        };
        let (result, calls) = run_down(Ok(report(PullOutcome::UpToDate)), opts);
        result.expect("stop succeeds");
        assert_eq!(calls, ["list", "pull", "suspend"]);
    }

    #[test]
    fn stop_after_a_failed_pull_still_parks() {
        let opts = DownOpts {
            mode: DownMode::Stop,
            force: false,
        };
        let (result, calls) = run_down(Err(anyhow::anyhow!("no exec channel")), opts);
        result.expect("parking loses nothing");
        assert_eq!(calls, ["list", "pull", "suspend"]);
    }

    #[test]
    fn pull_refusal_names_the_sandbox_and_the_escape_hatch() {
        let text = pull_failed_refusal("sbx-9", &anyhow::anyhow!("boom"));
        assert_eq!(
            text,
            "env down: could not pull back from sandbox sbx-9: boom; nothing was destroyed. Re-run with --force to destroy anyway"
        );
    }

    #[test]
    fn only_fly_machine0_and_vercel_can_stop() {
        for (kind, expected) in [
            ("fly", true),
            ("machine0", true),
            ("vercel", true),
            (" vercel ", true),
            (" fly ", true),
            ("sprites", false),
            ("daytona", false),
            ("hetzner", false),
            ("", false),
        ] {
            assert_eq!(provider_has_stop(kind), expected, "{kind:?}");
        }
        assert_eq!(
            no_stop_message("sprites"),
            "sprites has no stop; the sandbox keeps running. Use `thegn env down` to destroy it"
        );
        assert!(no_stop_message("").starts_with("this provider has no stop"));
    }

    #[test]
    fn env_up_resumes_only_a_parked_stop_capable_sandbox() {
        for (kind, id, provisioned, expected) in [
            ("fly", "sbx-1", true, true),
            ("machine0", "vm-1", true, true),
            ("fly", "sbx-1", false, false),
            ("fly", "", true, false),
            ("sprites", "sbx-1", true, false),
            // Vercel parks via stop-session: resumed only with the local marker.
            ("vercel", "tg-wt-1", true, true),
            ("vercel", "tg-wt-1", false, false),
            ("vercel", "", true, false),
        ] {
            assert_eq!(
                resume_on_up(kind, id, provisioned),
                expected,
                "{kind} {id:?} provisioned={provisioned}"
            );
        }
    }

    #[test]
    fn destroy_all_prompt_names_the_listed_count() {
        assert_eq!(
            super::destroy_all_prompt(3),
            "Destroy every sandbox thegn can prove it created (3 listed)?"
        );
    }

    #[test]
    fn destroy_all_skips_an_unproven_name_with_a_warning() {
        let ids: Vec<String> = ["mine", "theirs", "broken"].map(String::from).to_vec();
        let mut deleted = Vec::new();
        let (mut out, mut warns) = (Vec::new(), Vec::new());
        let mut outcome = super::destroy_all(
            &ids,
            |_| Ok(()),
            |id| match id {
                "theirs" => Err(anyhow::anyhow!("no ledger record")
                    .context("sprites: refusing to delete theirs")),
                "broken" => Err(anyhow::anyhow!("sprites destroy failed (500)")),
                other => {
                    deleted.push(other.to_string());
                    Ok(())
                }
            },
            |line| out.push(line.to_string()),
            |line| warns.push(line.to_string()),
        );
        assert_eq!(deleted, ["mine"]);
        outcome.bound = 2;
        assert_eq!(
            outcome,
            super::DestroyAllOutcome {
                destroyed: 1,
                skipped: 1,
                failed: 1,
                bound: 2,
            }
        );
        assert_eq!(
            outcome.summary(),
            "destroyed 1, skipped 1 (no ownership record), failed 1, kept 2 (bound to a worktree)"
        );
        assert_eq!(out, ["destroyed sandbox: mine"]);
        assert!(
            warns.iter().any(|w| w.starts_with("skipped theirs:")),
            "{warns:?}"
        );
        assert!(
            warns.iter().any(|w| w.starts_with("destroy broken failed")),
            "{warns:?}"
        );
    }

    #[test]
    fn destroy_all_never_deletes_when_revocation_fails() {
        let ids = vec!["sbx-1".to_string()];
        let mut deletes = 0;
        let outcome = super::destroy_all(
            &ids,
            |_| Err(anyhow::anyhow!("db locked")),
            |_| {
                deletes += 1;
                Ok(())
            },
            |_| {},
            |_| {},
        );
        assert_eq!(deletes, 0);
        assert_eq!(
            outcome.summary(),
            "destroyed 0, skipped 0 (no ownership record), failed 1"
        );
    }

    /// A scripted `provider.list()`: each call pops the next answer.
    fn scripted(mut answers: Vec<ListAnswer>) -> RefCell<Vec<ListAnswer>> {
        answers.reverse();
        RefCell::new(answers)
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn gone_on_the_first_list_never_sleeps() {
        let answers = scripted(vec![Ok(inventory(&["other"]))]);
        let mut list = || answers.borrow_mut().pop().expect("one list call");
        let mut sleeps: Vec<Duration> = Vec::new();
        let mut sleep = |d: Duration| sleeps.push(d);
        wait_until_gone(&mut list, "sbx-1", far(), &mut sleep).expect("gone");
        assert!(sleeps.is_empty(), "{sleeps:?}");
        assert!(answers.borrow().is_empty());
    }

    #[test]
    fn gone_after_two_polls_sleeps_twice() {
        let answers = scripted(vec![
            Ok(inventory(&["sbx-1"])),
            Err(anyhow::anyhow!("transient 503")),
            Ok(inventory(&[])),
        ]);
        let mut list = || answers.borrow_mut().pop().expect("three list calls");
        let mut sleeps: Vec<Duration> = Vec::new();
        let mut sleep = |d: Duration| sleeps.push(d);
        wait_until_gone(&mut list, "sbx-1", far(), &mut sleep).expect("gone after polls");
        assert_eq!(sleeps, [Duration::from_secs(2), Duration::from_secs(2)]);
        assert!(answers.borrow().is_empty());
    }

    #[test]
    fn still_listed_at_the_deadline_is_an_error() {
        let answers = scripted(vec![Ok(inventory(&["sbx-1", "other"]))]);
        let mut list = || answers.borrow_mut().pop().expect("one list call");
        let mut sleeps: Vec<Duration> = Vec::new();
        let mut sleep = |d: Duration| sleeps.push(d);
        let error = wait_until_gone(&mut list, "sbx-1", Instant::now(), &mut sleep)
            .expect_err("a still-listed sandbox fails");
        let error = format!("{error:#}");
        assert!(
            error.contains("env down failed: sandbox sbx-1 still exists after destroy"),
            "{error}"
        );
        assert!(error.contains("retry `thegn env down`"), "{error}");
        assert!(sleeps.is_empty(), "{sleeps:?}");
    }

    #[test]
    fn a_list_error_at_the_deadline_is_unverified() {
        let answers = scripted(vec![Err(anyhow::anyhow!("401 unauthorized"))]);
        let mut list = || answers.borrow_mut().pop().expect("one list call");
        let mut sleeps: Vec<Duration> = Vec::new();
        let mut sleep = |d: Duration| sleeps.push(d);
        let error = wait_until_gone(&mut list, "sbx-1", Instant::now(), &mut sleep)
            .expect_err("an unverifiable destroy fails");
        let error = format!("{error:#}");
        assert!(
            error.contains("env down: destroy was sent but could not be verified"),
            "{error}"
        );
        assert!(error.contains("401 unauthorized"), "{error}");
        assert!(sleeps.is_empty(), "{sleeps:?}");
    }

    #[test]
    fn down_inventory_decision_requires_complete_absence() {
        let partial = InventoryReport::Partial {
            rows: vec![],
            reason: "page 2 failed".into(),
        };
        let cases = vec![
            (
                Ok(inventory(&["other"])),
                DownInventoryDecision::AlreadyGone,
            ),
            (Ok(inventory(&["sbx-1"])), DownInventoryDecision::Proceed),
            (Ok(partial), DownInventoryDecision::Proceed),
            (
                Err(anyhow::anyhow!("provider unavailable")),
                DownInventoryDecision::Proceed,
            ),
        ];
        for (listing, expected) in cases {
            assert_eq!(down_inventory_decision(listing, "sbx-1"), expected);
        }
    }

    #[test]
    fn already_gone_skips_pull_and_revokes_then_forgets() {
        let calls = RefCell::new(Vec::new());
        let mut pull = || -> anyhow::Result<crate::provider_git::CountedPull> {
            calls.borrow_mut().push("pull");
            Ok((report(PullOutcome::UpToDate), Some(0)))
        };
        let mut teardown = || -> anyhow::Result<()> {
            calls.borrow_mut().push("teardown");
            Ok(())
        };
        let mut suspend = || -> anyhow::Result<()> {
            calls.borrow_mut().push("suspend");
            Ok(())
        };
        let mut revoke = || -> anyhow::Result<()> {
            calls.borrow_mut().push("revoke");
            Ok(())
        };
        let mut destroy_call = || -> anyhow::Result<()> {
            calls.borrow_mut().push("destroy");
            Ok(())
        };
        let mut list = || -> anyhow::Result<InventoryReport> {
            calls.borrow_mut().push("list");
            Ok(inventory(&[]))
        };
        let mut sleep = |_: Duration| calls.borrow_mut().push("sleep");
        let mut forget = || calls.borrow_mut().push("forget");
        destroy_provider_sandbox(
            "fly-dev",
            "sbx-1",
            destroy(),
            DownSteps {
                pull: &mut pull,
                teardown: &mut teardown,
                suspend: &mut suspend,
                revoke: &mut revoke,
                destroy: &mut destroy_call,
                list: &mut list,
                sleep: &mut sleep,
                forget: &mut forget,
            },
        )
        .expect("already-gone down succeeds");
        assert_eq!(
            *calls.borrow(),
            ["list", "teardown", "revoke", "destroy", "forget"]
        );
    }

    #[test]
    fn location_action_clears_only_a_remote_location_under_a_local_env() {
        let table = [
            (true, true, LocationAction::ClearToLocal),
            (true, false, LocationAction::Keep),
            (false, true, LocationAction::Keep),
            (false, false, LocationAction::Keep),
        ];
        for (old_remote, new_local, expected) in table {
            assert_eq!(
                location_action(old_remote, new_local),
                expected,
                "old_remote={old_remote} new_local={new_local}"
            );
        }
    }

    #[test]
    fn only_a_provider_location_counts_as_a_stale_provider_location() {
        let provider = thegn_core::remote::GitLoc::provider_db_string(
            &["sprite".into(), "exec".into(), "--".into()],
            "/workspace",
        );
        let ssh = thegn_core::remote::GitLoc::remote_db_string("box.invalid", 22, false, "/srv/wt");
        assert!(is_provider_location("/wt", Some(&provider)));
        assert!(!is_provider_location("/wt", Some(&ssh)));
        assert!(!is_provider_location("/wt", Some("")));
        assert!(!is_provider_location("/wt", None));
    }
}

#[cfg(test)]
mod env_safety_tests {
    use super::{env_changed, owner_of, rm_refusal, set_notice};
    use crate::provider_git::OfflineBinding;

    #[test]
    fn owner_of_table() {
        let rows = vec![
            ("/one".into(), OfflineBinding::Unbound),
            (
                "/two".into(),
                OfflineBinding::Bound {
                    sandbox: "sbx-2".into(),
                },
            ),
        ];
        assert_eq!(owner_of("sbx-1", &rows), Ok(None));
        assert_eq!(owner_of("sbx-2", &rows), Ok(Some("/two")));

        let unknown = vec![
            ("/unknown".into(), OfflineBinding::BoundUnknown),
            (
                "/two".into(),
                OfflineBinding::Bound {
                    sandbox: "sbx-2".into(),
                },
            ),
        ];
        assert_eq!(owner_of("sbx-2", &unknown), Err("/unknown".into()));
    }

    #[test]
    fn rm_refusal_names_the_worktrees() {
        let using = vec![
            "/one".into(),
            "/two".into(),
            "/three".into(),
            "/four".into(),
            "/five".into(),
            "/six".into(),
        ];
        let refusal = rm_refusal("sprites", &using).expect("in-use env refuses");
        assert!(refusal.contains("6 registered worktree(s)"), "{refusal}");
        assert!(
            refusal.contains("/one, /two, /three, /four, /five"),
            "{refusal}"
        );
        assert!(!refusal.contains("/six"), "{refusal}");
        assert!(rm_refusal("sprites", &[]).is_none());
    }

    #[test]
    fn set_notice_table() {
        let bound = OfflineBinding::Bound {
            sandbox: "sbx-1".into(),
        };
        for (old, stored_is_provider, expected) in [
            (Some(&bound), false, Some("sbx-1")),
            (Some(&bound), true, None),
            (None, false, None),
            (Some(&OfflineBinding::Unbound), false, None),
            (Some(&OfflineBinding::BoundUnknown), false, None),
        ] {
            assert_eq!(set_notice(old, stored_is_provider), expected);
        }
    }

    #[test]
    fn set_notice_skips_an_unchanged_environment() {
        let old_env: anyhow::Result<thegn_core::env::Environment> =
            Ok(thegn_core::env::Environment {
                name: "sprites".into(),
                placement: thegn_core::placement::Placement::Local,
                sandbox: Default::default(),
                data: thegn_core::config::DataMode::InEnv,
                unresolved_selection: false,
            });
        assert!(!env_changed(&old_env, "sprites"));
        assert!(env_changed(&old_env, "daytona"));
    }
}

#[cfg(test)]
mod set_location_tests {
    use super::*;
    use thegn_core::config::{EnvConfig, PlacementMode};

    fn provider_location() -> String {
        GitLoc::provider_db_string(
            &[
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "x".into(),
                "--".into(),
            ],
            "/workspace",
        )
    }

    fn registered(location: Option<&str>) -> (tempfile::TempDir, String, Db) {
        let dir = tempfile::tempdir().expect("temporary worktree");
        let wt = dir.path().to_string_lossy().into_owned();
        let db = Db::open().expect("isolated db");
        db.put_worktree("repo/env", &wt, &wt, "feature", location, None)
            .expect("register worktree");
        (dir, wt, db)
    }

    #[test]
    fn switching_a_provider_row_to_the_local_default_clears_its_location() {
        let _state = crate::testenv::IsolatedState::new();
        let (_dir, wt, db) = registered(Some(&provider_location()));
        db.set_worktree_env(&wt, "").expect("clear env");
        reconcile_location_after_set(&Config::default(), &db, &wt, None)
            .expect("reconcile succeeds");
        let stored = db.location_for(&wt).expect("read location");
        assert!(!is_provider_location(&wt, stored.as_deref()), "{stored:?}");
    }

    #[test]
    fn a_provider_row_switched_to_a_provider_env_keeps_its_location() {
        let _state = crate::testenv::IsolatedState::new();
        let location = provider_location();
        let (_dir, wt, db) = registered(Some(&location));
        let mut cfg = Config::default();
        cfg.env.insert(
            "fly".into(),
            EnvConfig {
                placement: PlacementMode::Provider,
                ..Default::default()
            },
        );
        db.set_worktree_env(&wt, "fly").expect("select env");
        reconcile_location_after_set(&cfg, &db, &wt, None).expect("reconcile succeeds");
        assert_eq!(
            db.location_for(&wt).expect("read location").as_deref(),
            Some(location.as_str())
        );
    }

    #[test]
    fn a_local_row_is_left_alone() {
        let _state = crate::testenv::IsolatedState::new();
        let (_dir, wt, db) = registered(None);
        reconcile_location_after_set(&Config::default(), &db, &wt, None)
            .expect("reconcile succeeds");
        let stored = db.location_for(&wt).expect("read location");
        assert!(stored.as_deref().unwrap_or("").is_empty(), "{stored:?}");
    }
}

/// b162 CLI surface: `env adopt`, `agent run --keep-running` and
/// `land --release-sandbox` parse (or refuse) as documented.
#[cfg(test)]
mod b162_cli_tests {
    use clap::Parser as _;

    fn parse(args: &[&str]) -> Result<crate::Cli, clap::Error> {
        crate::Cli::try_parse_from(args)
    }

    #[test]
    fn env_adopt_parses_name_env_and_yes() {
        let Ok(cli) = parse(&["thegn", "env", "adopt", "foo", "--env", "s", "--yes"]) else {
            panic!("env adopt must parse");
        };
        match cli.command {
            Some(crate::Command::Env {
                action: super::Action::Adopt(args),
            }) => {
                assert_eq!(args.name, "foo");
                assert_eq!(args.env, "s");
                assert!(args.yes);
                assert!(!args.json);
            }
            _ => panic!("expected env adopt"),
        }
    }

    #[test]
    fn env_adopt_requires_env() {
        assert!(parse(&["thegn", "env", "adopt", "foo", "--yes"]).is_err());
    }

    #[test]
    fn env_up_shell_ready_is_optional() {
        for (args, shell_ready) in [
            (&["thegn", "env", "up", "--shell-ready"][..], true),
            (&["thegn", "env", "up"][..], false),
        ] {
            let Ok(cli) = parse(args) else {
                panic!("{args:?} must parse");
            };
            match cli.command {
                Some(crate::Command::Env {
                    action:
                        super::Action::Up {
                            shell_ready: actual,
                            ..
                        },
                }) => assert_eq!(actual, shell_ready, "{args:?}"),
                _ => panic!("expected env up"),
            }
        }
    }

    #[test]
    fn env_reap_unrecorded_conflicts_with_apply() {
        assert!(
            parse(&[
                "thegn",
                "env",
                "reap",
                "--provider",
                "sprites",
                "--unrecorded",
                "--apply",
            ])
            .is_err()
        );
    }

    #[test]
    fn agent_run_keep_running_requires_wait() {
        let base = [
            "thegn",
            "agent",
            "run",
            "--agent",
            "codex",
            "--worktree",
            "/w",
            "--prompt",
            "x",
        ];
        let mut bad = base.to_vec();
        bad.push("--keep-running");
        assert!(
            parse(&bad).is_err(),
            "--keep-running without --wait must be refused"
        );
        let mut ok = base.to_vec();
        ok.extend(["--wait", "--keep-running"]);
        let Ok(cli) = parse(&ok) else {
            panic!("agent run --wait --keep-running must parse");
        };
        match cli.command {
            Some(crate::Command::Agent {
                action:
                    crate::cmd::agent::Action::Run {
                        keep_running, wait, ..
                    },
            }) => assert!(keep_running && wait),
            _ => panic!("expected agent run"),
        }
    }

    #[test]
    fn land_release_sandbox_is_opt_in() {
        for (args, want) in [
            (&["thegn", "land", "--release-sandbox"][..], true),
            (&["thegn", "land"][..], false),
        ] {
            let Ok(cli) = parse(args) else {
                panic!("{args:?} must parse");
            };
            match cli.command {
                Some(crate::Command::Land {
                    release_sandbox, ..
                }) => assert_eq!(release_sandbox, want, "{args:?}"),
                _ => panic!("expected land"),
            }
        }
    }
}
