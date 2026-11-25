//! [`env_halt_reason`] — the cheap, **loop-safe** pre-spawn check that decides
//! whether a worktree's pane must HALT (with a warning modal) instead of opening
//! a host-degraded shell. Extracted from the pinned `agent.rs`; the failover
//! `halt`/`ask` policy and the `SandboxHalt` it returns live in `agent.rs`.

use std::path::{Path, PathBuf};

use thegn_core::channel::Feature;
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::remote::GitLoc;
use thegn_core::repo;
use thegn_core::store::WorkspaceStore;

use crate::agent::{SandboxHalt, force_host_requested, native_exec_healthy};

/// Cheap, **loop-safe** (no network/subprocess) check made right before a
/// worktree pane spawns: should this worktree HALT with a warning instead of
/// opening a (host-degraded) pane? Returns `Some` for unreadable worktree
/// isolation, or for a NON-LOCAL env whose
/// failover mode blocks (`halt`/`ask`) and whose bring-up is already known to be
/// impossible — its API token is unset, or its native exec is in the
/// post-failure cooldown (the live signal a recent connect/auth attempt failed,
/// e.g. the sprites 401). `None` (proceed normally) for local envs, when the mode
/// is `auto` (or the worktree was pinned to the host), or when there's no cheap
/// evidence of failure yet — in which case a later bring-up failure is caught in
/// `prepare_sandbox_env` / the native relay, which flip the health flag so the
/// next spawn halts here. An unreadable selection always halts rather than
/// resolving a default environment.
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
    // A repo `.thegn.*` that tries to define a `kind = "command"` metrics
    // collector is refused — command collectors are global config only, since
    // one would run an argv on opening the repo. Surface the rejection (naming
    // the target) so the ignored config is visible; nothing from the overlay is
    // ever merged into the scraper. Reads the same already-cached overlay file
    // as the parse-error check below, so it stays loop-safe.
    for w in cfg.repo_command_collector_warnings(&repo_root) {
        tracing::warn!(target: "thegn::config", path = %repo_root.display(), "{w}");
    }
    for w in cfg.repo_automation_warnings(&repo_root) {
        tracing::warn!(target: "thegn::config", path = %repo_root.display(), "{w}");
    }
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
            sealed_home: false,
        });
    }
    let selection = match Db::open()
        .and_then(|db| db.effective_env_selection(worktree, &repo_root.to_string_lossy()))
    {
        Ok(selection) => selection,
        Err(error) => {
            let halt = unreadable_selection_halt(worktree, &error);
            tracing::warn!(
                target: "thegn::sandbox",
                worktree,
                error = %format!("{error:#}"),
                "HALT: worktree isolation selection unreadable"
            );
            return Some(halt);
        }
    };
    let environment = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
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
                reason: unresolved_env_reason(&environment.name, remote_features_clamped()),
                ask: matches!(mode, thegn_core::config::FailoverMode::Ask),
                dormant: None,
                sealed_home: false,
            });
        }
    }
    if matches!(
        &environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) && let Some(refusal) = thegn_core::sandbox_floor::provider_refusal_for(
        true,
        &environment.sandbox,
        &environment.name,
        Some(worktree),
    ) {
        return Some(SandboxHalt {
            env_name: environment.name.clone(),
            placement: environment.placement.label(),
            reason: refusal.to_string(),
            ask: false,
            dormant: None,
            sealed_home: environment.sandbox.profile.hides_home(),
        });
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
                sealed_home: false,
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
                sealed_home: false,
            });
        }
        tracing::debug!(target: "thegn::sandbox", env = %environment.name, "env_halt_reason: provider OK, no halt");
    }
    None
}

/// An unreadable worktree selection is a typed halt: the sandbox pin cannot be
/// inferred safely, even for a local shell.
fn unreadable_selection_halt(worktree: &str, error: &anyhow::Error) -> SandboxHalt {
    let recovery = thegn_core::sandbox_floor::clear_profile_hint(worktree);
    SandboxHalt {
        env_name: "worktree isolation".into(),
        placement: worktree.to_string(),
        reason: format!(
            "thegn cannot read this worktree's isolation setting ({error:#}); refusing launch. Run `thegn doctor`, or once the database opens: {recovery}."
        ),
        ask: false,
        dormant: None,
        sealed_home: false,
    }
}

/// True when the release channel clamps the remote/provider features.
pub(crate) fn remote_features_clamped() -> bool {
    crate::channel_state::clamped(Feature::Remote)
        || crate::channel_state::clamped(Feature::Providers)
}

pub(crate) fn unresolved_env_reason(name: &str, channel_clamped: bool) -> String {
    if channel_clamped {
        format!(
            "env '{name}' is not available on the stable channel (ssh, Kubernetes and cloud-provider environments other than Sprites and Vercel are dev-only: set THEGN_CHANNEL=dev), or [env.{name}] is missing; its selection was dropped"
        )
    } else {
        format!("env '{name}' is not defined ([env.{name}] missing); its selection was dropped")
    }
}

/// Which kind of fix a non-local halt needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HaltClass {
    ProviderAuth,
    ProviderFunds,
    Unprovisioned,
    Remote,
    LocalRuntime,
}

pub(crate) fn classify_halt(placement: &str, reason: &str) -> HaltClass {
    let placement = placement.to_ascii_lowercase();
    // `Placement::label()` shapes: `local`, `ssh:<host>`, `mosh:<host>`,
    // `k8s:<pod>`, `<provider>:<id>`; some gates use a bare `provider`/`ssh`/`k8s`.
    let scheme = placement
        .split_once(':')
        .map_or(placement.as_str(), |(scheme, _)| scheme);
    if matches!(scheme, "ssh" | "mosh" | "k8s") {
        return HaltClass::Remote;
    }
    let provider = scheme == "provider"
        || (placement.contains(':')
            && scheme != "local"
            && scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    if !provider {
        return HaltClass::LocalRuntime;
    }
    let r = reason.to_ascii_lowercase();
    let has = |keys: &[&str]| keys.iter().any(|k| r.contains(k));
    if has(&["insufficient_funds", "available: $", "balance"]) {
        HaltClass::ProviderFunds
    } else if has(&["token", "401", "403", "unauthorized", "forbidden"]) {
        HaltClass::ProviderAuth
    } else if has(&["never been provisioned", "not provisioned", "provisioning"]) {
        HaltClass::Unprovisioned
    } else {
        HaltClass::Remote
    }
}

pub(crate) fn remedy_text(class: HaltClass, env_name: &str) -> String {
    match class {
        HaltClass::ProviderFunds => format!(
            "top up the provider account, then retry (`thegn env test {env_name}` shows the provider's answer)"
        ),
        HaltClass::ProviderAuth => format!(
            "check the provider token (`thegn env test {env_name}`; `api_key_env` in [env.{env_name}.provider])"
        ),
        HaltClass::Unprovisioned => {
            "provision it first: `thegn env up` (headless) or open it once in the TUI".to_string()
        }
        HaltClass::Remote => format!(
            "check that the host is reachable (`thegn env test {env_name}`), or choose another env (`thegn env set`)"
        ),
        HaltClass::LocalRuntime => {
            "install/start a container runtime (`thegn doctor` shows which)".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HaltClass, classify_halt, remedy_text, unreadable_selection_halt, unresolved_env_reason,
    };
    use crate::agent::SandboxHalt;

    fn halt(placement: &str, reason: &str, ask: bool) -> SandboxHalt {
        SandboxHalt {
            env_name: "box".into(),
            placement: placement.into(),
            reason: reason.into(),
            ask,
            dormant: None,
            sealed_home: false,
        }
    }

    #[test]
    fn classify_halt_table() {
        use HaltClass::{LocalRuntime, ProviderAuth, ProviderFunds, Remote, Unprovisioned};
        let cases = [
            ("provider:sprites", "INSUFFICIENT_FUNDS", ProviderFunds),
            ("provider:sprites", "available: $0.00", ProviderFunds),
            ("provider:sprites", "Account Balance too low", ProviderFunds),
            ("provider:sprites", "API Token missing", ProviderAuth),
            ("provider:sprites", "HTTP 401", ProviderAuth),
            ("provider:sprites", "got 403", ProviderAuth),
            ("provider:sprites", "Unauthorized", ProviderAuth),
            ("provider:sprites", "FORBIDDEN", ProviderAuth),
            (
                "provider:sprites",
                "has never been provisioned",
                Unprovisioned,
            ),
            ("provider:sprites", "provisioning failed", Unprovisioned),
            ("provider:sprites", "timeout", Remote),
            ("PROVIDER:fly", "timeout", Remote),
            ("ssh", "refused", Remote),
            ("k8s", "refused", Remote),
            ("local", "no runtime", LocalRuntime),
            ("env box", "no runtime", LocalRuntime),
            ("sprites:sb-1", "INSUFFICIENT_FUNDS", ProviderFunds),
            ("fly:app-1", "HTTP 401", ProviderAuth),
            ("machine0:m1", "timeout", Remote),
            (
                "provider",
                "the sandbox is not provisioned yet",
                Unprovisioned,
            ),
            ("ssh:box.example", "refused", Remote),
            ("mosh:box.example", "refused", Remote),
            ("k8s:ns/pod", "refused", Remote),
            ("/home/u/wt", "x", LocalRuntime),
            ("c:\\wt", "x", LocalRuntime),
        ];
        for (placement, reason, want) in cases {
            assert_eq!(
                classify_halt(placement, reason),
                want,
                "{placement} {reason}"
            );
        }
    }

    #[test]
    fn provider_funds_halt_display_has_no_container_runtime() {
        let text = halt("sprites:sb-1", "INSUFFICIENT_FUNDS", false).to_string();
        assert!(!text.contains("container runtime"), "{text}");
        assert!(text.contains("top up the provider account"));
        assert!(!text.contains("backend = \"none\""));
    }

    #[test]
    fn local_halt_display_keeps_legacy_text() {
        let text = halt("local", "no runtime", false).to_string();
        assert_eq!(
            text,
            "environment 'box' (local) could not be brought up: no runtime — thegn will not \
             silently fall back to the host. Fix the env, retry, explicitly select host execution (`backend = \"none\"`, disable sandboxing, \
             set `isolation_floor = \"off\"`, or choose \"run on host\"), \
             or install/start a container runtime (`thegn doctor` shows which) ([sandbox] or \
             [env.box]) to allow it."
        );
    }

    #[test]
    fn remedy_text_names_env() {
        assert!(remedy_text(HaltClass::Remote, "box").contains("`thegn env test box`"));
        assert!(remedy_text(HaltClass::Unprovisioned, "box").contains("thegn env up"));
    }

    #[test]
    fn unreadable_selection_is_a_typed_halt_even_for_local_worktrees() {
        let error = anyhow::anyhow!("database is locked");
        let halt = unreadable_selection_halt("/wt/a", &error);
        assert!(
            halt.reason
                .contains("thegn cannot read this worktree's isolation setting")
        );
        assert!(halt.reason.contains("thegn doctor"));
    }

    #[test]
    fn unreadable_selection_halt_names_the_read_error() {
        let halt = unreadable_selection_halt("/wt/a tree", &anyhow::anyhow!("database is locked"));

        assert!(halt.reason.contains("database is locked"));
        // The recovery route is the pin-clear command (the `set standard`
        // downgrade is the sealed-refusal hint, not this one).
        assert!(
            halt.reason
                .contains("thegn sandbox profile clear --worktree '/wt/a tree'")
        );
        assert!(!halt.ask);
    }

    #[test]
    fn unresolved_reason_names_the_stable_channel_when_clamped() {
        let stable = unresolved_env_reason("box", true);
        assert!(stable.contains("stable channel"));
        assert!(stable.contains("THEGN_CHANNEL=dev"));
        assert_eq!(
            unresolved_env_reason("box", false),
            "env 'box' is not defined ([env.box] missing); its selection was dropped"
        );
    }
}
