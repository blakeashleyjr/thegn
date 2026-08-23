//! [`env_halt_reason`] — the cheap, **loop-safe** pre-spawn check that decides
//! whether a worktree's pane must HALT (with a warning modal) instead of opening
//! a host-degraded shell. Extracted from the pinned `agent.rs`; the failover
//! `halt`/`ask` policy and the `SandboxHalt` it returns live in `agent.rs`.

use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::repo;
use thegn_core::store::WorkspaceStore;

use crate::agent::{SandboxHalt, force_host_requested, native_exec_healthy};

/// Cheap, **loop-safe** (no network/subprocess) check made right before a
/// worktree pane spawns: should this worktree HALT with a warning instead of
/// opening a (host-degraded) pane? Returns `Some` only for a NON-LOCAL env whose
/// failover mode blocks (`halt`/`ask`) and whose bring-up is already known to be
/// impossible — its API token is unset, or its native exec is in the
/// post-failure cooldown (the live signal a recent connect/auth attempt failed,
/// e.g. the sprites 401). `None` (proceed normally) for local envs, when the mode
/// is `auto` (or the worktree was pinned to the host), or when there's no cheap
/// evidence of failure yet — in which case a later bring-up failure is caught in
/// `prepare_sandbox_env` / the native relay, which flip the health flag so the
/// next spawn halts here.
pub fn env_halt_reason(cfg: &Config, worktree: &str) -> Option<SandboxHalt> {
    use thegn_core::config::ProviderExecMode;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    // Refuse to silently ignore a malformed repo `.thegn.*` overlay that was
    // SELECTING a non-local env: `load_repo_overlay` drops a file that fails to
    // parse, so its `env = "sprites"` selection vanishes and resolution falls back
    // to local (host) — exactly the silent degradation failover-off forbids. Catch
    // it BEFORE resolve_env (which has already lost the selection) and surface the
    // same warning modal. (No halt for a parse error that wasn't selecting a
    // non-local env, or when that env opts into failover.)
    if let Some(pe) = thegn_core::config::repo_overlay_parse_error(&repo_root)
        && !pe.selected_env.is_empty()
        && let Some(envc) = cfg.env.get(&pe.selected_env)
        && !matches!(envc.placement, thegn_core::config::PlacementMode::Local)
        && cfg.env_failover_mode(&repo_root, &pe.selected_env).blocks()
    {
        tracing::warn!(
            target: "thegn::sandbox",
            path = %pe.path.display(), env = %pe.selected_env,
            "HALT: repo overlay failed to parse, dropping a non-local env selection"
        );
        return Some(SandboxHalt {
            env_name: pe.selected_env.clone(),
            placement: format!("{:?}", envc.placement).to_lowercase(),
            reason: format!(
                "{} failed to parse ({}); the env selection was dropped",
                pe.path.display(),
                pe.error.lines().next().unwrap_or("").trim(),
            ),
            ask: matches!(
                cfg.env_failover_mode(&repo_root, &pe.selected_env),
                thegn_core::config::FailoverMode::Ask
            ),
            dormant: None,
        });
    }
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    // A non-default env was SELECTED (per-worktree pin, repo `.thegn.*` overlay, or
    // `default_env`) but didn't resolve to an `[env.<name>]` table, so resolution
    // fell back to Local carrying the requested name (`unresolved_selection`). That
    // silent drop is exactly what failover-off forbids — surface it like the repo
    // overlay parse error above, BEFORE the `is_local()` early return below swallows
    // it. `env_failover_mode` falls through to the global `[sandbox] failover` for a
    // name that has no `[env.<name>]`.
    if environment.unresolved_selection {
        let mode = cfg.env_failover_mode(&repo_root, &environment.name);
        if mode.blocks() && !force_host_requested(worktree) {
            tracing::warn!(
                target: "thegn::sandbox",
                env = %environment.name,
                "HALT: selected env is not defined under [env.<name>]; selection dropped"
            );
            return Some(SandboxHalt {
                env_name: environment.name.clone(),
                placement: format!("env {}", environment.name),
                reason: format!(
                    "env '{}' is not defined ([env.{}] missing); its selection was dropped",
                    environment.name, environment.name
                ),
                ask: matches!(mode, thegn_core::config::FailoverMode::Ask),
                dormant: None,
            });
        }
    }
    // Local envs never halt; `auto` degrades silently (no block); a worktree the
    // user pinned to the host ("run on host") must not re-block. Only `halt`/`ask`
    // surface a modal.
    let mode = cfg.env_failover_mode(&repo_root, &environment.name);
    if environment.placement.is_local() || !mode.blocks() || force_host_requested(worktree) {
        return None;
    }
    let ask = matches!(mode, thegn_core::config::FailoverMode::Ask);
    let placement = environment.placement.label();
    if let thegn_core::placement::Placement::Provider(_) = &environment.placement {
        let pc = &cfg.env.get(&environment.name)?.provider;
        // Token check: explicit `api_key_env`, else the provider's built-in default
        // (machine0 ⇒ MACHINE0_API_KEY). Resolve it exactly as the provider does
        // (`secret::resolve` — keyring:/file:/env:/bare env var), NOT a raw
        // `std::env::var` (which would read a `file:` ref as a bogus env-var name).
        let var = crate::env_ui::effective_token_env(pc);
        let token_present = crate::secret::resolve(&var).is_some();
        let healthy = native_exec_healthy(&pc.provider);
        tracing::debug!(
            target: "thegn::sandbox",
            env = %environment.name, %placement, provider = %pc.provider,
            token_var = %var, token_present, native_exec_healthy = healthy,
            exec = ?pc.exec,
            "env_halt_reason: evaluating provider env"
        );
        if !token_present {
            tracing::warn!(target: "thegn::sandbox", env = %environment.name, token_var = %var, "HALT: API token not set");
            return Some(SandboxHalt {
                env_name: environment.name.clone(),
                placement,
                reason: format!("API token {var} could not be resolved"),
                ask,
                dormant: None,
            });
        }
        // Connect-failure cooldown: a recent connect/auth failure (401, or an ssh
        // connect/resolve failure) marked the provider unhealthy. Failover off ⇒
        // surface the halt. Covers WSS native-exec (sprites, tracked in-process) AND
        // ssh-reached providers (machine0/fly/vps, tracked at pane-exit).
        let native_exec_tracked =
            pc.exec != ProviderExecMode::Cli && thegn_svc::provider::exec_api_by_name(&pc.provider);
        if (native_exec_tracked || thegn_core::config::ssh_reached_provider_kind(&pc.provider))
            && !healthy
        {
            tracing::warn!(target: "thegn::sandbox", env = %environment.name, provider = %pc.provider, "HALT: provider unhealthy (recent connection failure)");
            return Some(SandboxHalt {
                env_name: environment.name.clone(),
                placement,
                reason: format!(
                    "provider '{}' is unreachable or rejected authentication (recent connection failure)",
                    pc.provider
                ),
                ask,
                dormant: None,
            });
        }
        tracing::debug!(target: "thegn::sandbox", env = %environment.name, "env_halt_reason: provider OK, no halt");
    }
    None
}
