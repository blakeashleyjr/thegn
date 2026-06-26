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
use superzej_core::{msg, outln, repo};

#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// List the defined `[env.<name>]` environments and their placement.
    List,
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
    /// Create a new managed-sandbox via the env's API provider (Daytona) and
    /// print its id. Requires `[env.<name>.provider] api_base` + `api_key_env`.
    Provision { worktree: Option<String> },
    /// Destroy a managed-sandbox by id via the env's API provider.
    Deprovision {
        id: String,
        worktree: Option<String>,
    },
}

pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List => list(cfg),
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
    }
}

/// Build the API provider for a worktree's resolved env, or explain why not.
fn api_provider(
    cfg: &Config,
    worktree: Option<String>,
) -> Result<superzej_svc::provider::DaytonaProvider> {
    let env = resolve_for(cfg, worktree);
    let envc = cfg
        .env
        .get(&env.name)
        .ok_or_else(|| anyhow::anyhow!("the default env has no API provider configured"))?;
    let pc = &envc.provider;
    if pc.provider != "daytona" {
        anyhow::bail!(
            "API provisioning currently supports the 'daytona' provider; env {} uses {:?}",
            env.name,
            pc.provider
        );
    }
    if pc.api_base.trim().is_empty() {
        anyhow::bail!(
            "[env.{}.provider] needs `api_base` (and `api_key_env`) for API provisioning",
            env.name
        );
    }
    let token = std::env::var(pc.api_key_env.trim()).map_err(|_| {
        anyhow::anyhow!(
            "the API token env var {:?} (api_key_env) is not set",
            pc.api_key_env
        )
    })?;
    Ok(superzej_svc::provider::DaytonaProvider::new(
        &pc.api_base,
        &token,
        &pc.template,
    ))
}

/// Create (id=None) or destroy (id=Some) a managed sandbox via the API provider.
fn provision(cfg: &Config, worktree: Option<String>, id: Option<String>) -> Result<()> {
    use superzej_svc::provider::RemoteProvider;
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

/// For a `data = "sshfs"` env on an ssh placement, mount (or unmount) the remote
/// worktree tree at a stable local path under the superzej dir. No-op for any
/// other data mode / placement.
fn sshfs(env: &superzej_core::env::Environment, mount: bool) -> Result<()> {
    use superzej_core::config::DataMode;
    use superzej_core::placement::{Placement, SshPlacement};
    if env.data != DataMode::Sshfs {
        return Ok(());
    }
    let Placement::Ssh(s) = &env.placement else {
        anyhow::bail!(
            "data = \"sshfs\" requires an ssh placement (env {})",
            env.name
        );
    };
    let remote_path = env.sandbox.remote.remote_dir.trim();
    if remote_path.is_empty() {
        anyhow::bail!(
            "sshfs needs [sandbox.remote] remote_dir set (env {})",
            env.name
        );
    }
    let mountpoint = sshfs_mountpoint(s, remote_path);
    let argv = if mount {
        std::fs::create_dir_all(&mountpoint).ok();
        s.sshfs_mount_argv(remote_path, &mountpoint)
    } else {
        SshPlacement::sshfs_unmount_argv(&mountpoint)
    };
    outln!(
        "{}: {}",
        if mount { "mounting" } else { "unmounting" },
        argv.join(" ")
    );
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {}: {e}", argv[0]))?;
    if !status.success() {
        anyhow::bail!(
            "sshfs {} failed ({status})",
            if mount { "mount" } else { "unmount" }
        );
    }
    Ok(())
}

/// A stable local mountpoint for a host+remote_path under `<superzej_dir>/mounts`.
fn sshfs_mountpoint(s: &superzej_core::placement::SshPlacement, remote_path: &str) -> String {
    let slug = superzej_core::util::slugify(&format!("{}-{remote_path}", s.host));
    superzej_core::util::superzej_dir()
        .join("mounts")
        .join(slug)
        .to_string_lossy()
        .into_owned()
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
fn resolve_for(cfg: &Config, worktree: Option<String>) -> superzej_core::env::Environment {
    let wt = resolve_worktree(worktree);
    let loc = GitLoc::for_worktree(Path::new(&wt));
    let repo_root = repo_root_for(&wt);
    let selected = Db::open()
        .ok()
        .and_then(|db| db.effective_env(&wt, &repo_root.to_string_lossy()));
    cfg.resolve_env(&repo_root, &loc, selected.as_deref())
}

fn list(cfg: &Config) -> Result<()> {
    let default = if cfg.sandbox.default_env.trim().is_empty() {
        "default"
    } else {
        cfg.sandbox.default_env.trim()
    };
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
    let env = cfg.resolve_env(&repo_root, &loc, selected.as_deref());
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
    Ok(())
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
