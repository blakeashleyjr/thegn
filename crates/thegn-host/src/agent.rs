//! Native agent launching. The zellij-era `thegn pick-agent` ran inside a
//! freshly-created worktree pane, showed an fzf/gum picker, then `exec`'d the
//! choice so the selection became the pane's own process. The native host owns
//! the screen (raw mode), so the picker is the in-process command palette and
//! the pane *is* the spawned process — we compose the sandbox-wrapped argv +
//! env here and hand it to `Panes::spawn_argv_env` rather than exec-replacing.
//!
//! This module is the testable seam: `choices`, `resolve_command`, and
//! `launch_spec` are pure over `Config`/`Db`, so the wiring in `run.rs` stays a
//! thin call.

use crate::agent_configs::with_provision_timeout;
use crate::remote_sync::ssh_none_guard;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::db::Db;

// Teardown fns live in a sibling (file-size ratchet); same call paths.
pub use crate::agent_teardown::{checkpoint_on_close, destroy_provider_sandbox};
use thegn_core::remote::GitLoc;
use thegn_core::store::{PoolStore, WorkspaceStore};
use thegn_core::{bundle, devenv, repo, sandbox};
use thegn_svc::projection::ProjectionBackend;
use thegn_svc::vpn::VpnProvider;

/// The literal shell sentinel — distinct from any configured agent/tool name.
const SHELL: &str = "shell";

/// What the agent picker offers for a worktree: every configured agent, then
/// every tool, then a literal `shell`. Order matches the zellij `pick_agent`.
pub fn choices(cfg: &Config) -> Vec<String> {
    let mut labels: Vec<String> = cfg.agents.iter().map(|a| a.name.clone()).collect();
    labels.extend(cfg.tools.iter().map(|t| t.name.clone()));
    if !labels.iter().any(|l| l == SHELL) {
        labels.push(SHELL.into());
    }
    labels
}

/// Resolve a picker label to the command string to run inside the worktree.
/// `shell` (and any unknown label) resolves to the interactive login shell.
/// Always uses the host (non-OCI) form; callers that know the sandbox context
/// should use `compose_spec` instead.
pub fn resolve_command(cfg: &Config, choice: &str) -> String {
    if choice == SHELL {
        return shell_inner(false);
    }
    if let Some(c) = cfg.agent_command(choice) {
        return c.to_string();
    }
    if let Some(c) = cfg.tool_command(choice) {
        return c.to_string();
    }
    // Unknown label — drop to a shell rather than spawning a dead pane.
    shell_inner(false)
}

/// The `inner` program string for a plain shell pane (what `enter_argv` wraps).
///
/// `in_oci` must be `true` when the inner command will run inside an OCI
/// container (podman/docker).  In that case the host's absolute `$SHELL` path
/// (e.g. `/run/current-system/sw/bin/zsh`) is meaningless — and even using the
/// basename fails if the container image doesn't have that shell installed (e.g.
/// a bare Debian image has bash but not zsh).  We emit a POSIX sh snippet that
/// walks a preference chain at runtime inside the container and execs the first
/// one that exists; `/bin/sh` is always the last resort.
///
/// Non-OCI (`in_oci=false`): emit `${SHELL:-/bin/sh} -l` so `$SHELL` expands at the
/// exec site (remote-safe: baking the host's abs path → remote `exit 127`).
pub(crate) fn shell_inner(in_oci: bool) -> String {
    if in_oci {
        crate::shell_snippet::oci_login_snippet()
    } else {
        "${SHELL:-/bin/sh} -l".to_string() // deferred, remote-safe (see fn doc)
    }
}

/// Like [`shell_inner`] but uses an explicit override from the sandbox config.
/// Carries its own `exec` (like every branch of [`shell_inner`]'s probe chain)
/// so the composer can drop it in verbatim — never prefixed with another `exec`.
fn shell_inner_override(shell_override: &str) -> String {
    format!("exec {shell_override} -l")
}

/// The `inner` script for a **clean fallback shell**: a plain interactive shell
/// with NO user rc/profile. Used by the startup watchdog when a personal login
/// shell produces no output in time — typically a dotfile that hangs or errors
/// in a provisioned env (e.g. a host `.zshrc` sourcing `/nix/store/...` paths
/// that don't exist in the container). `bash --norc --noprofile` is the
/// requested fallback; `zsh -f` (NO_RCS — skips every startup file) covers
/// images without bash; `/bin/sh` is the universal last resort. None of these
/// read the user rc, so a broken dotfile can't hang the fallback. Unlike
/// [`shell_inner`], each branch carries its own `exec` and the script is used
/// verbatim (no outer `exec` prefix), so it is composed without that wrapper.
pub(crate) fn clean_shell_inner() -> String {
    "command -v bash >/dev/null 2>&1 && exec bash --norc --noprofile; \
     command -v zsh >/dev/null 2>&1 && exec zsh -f; \
     exec /bin/sh"
        .to_string()
}

/// A fully-resolved launch: the argv to spawn (sandbox/transport-wrapped when a
/// sandbox is configured, else a bare `$SHELL -lc <cmd>`), the cwd, and the env
/// the agent pane expects. Pure data so `run.rs` just spawns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// The effective containment backend used for this launch (`host` after fallback).
    pub backend: String,
    /// Human-visible notes when auto sandbox resolution fell through to another backend.
    pub warnings: Vec<String>,
    /// A managed-PROVIDER env failed and `auto`/run-on-host dropped this launch to
    /// the host — drives the `provider_degraded` notification.
    pub degraded: bool,
}

impl LaunchSpec {
    pub fn warning_summary(&self) -> Option<String> {
        (!self.warnings.is_empty()).then(|| self.warnings.join("; "))
    }
}

/// Why a NON-LOCAL environment (provider/k8s/ssh) could not be brought up while
/// failover is disabled (`[sandbox] failover = false`, or a per-env override).
/// Carried as the error so silent host degradation is refused — the spawn site
/// surfaces it as a warning modal instead of opening a host shell. See
/// [`env_halt_reason`] (the cheap proactive check) and `prepare_sandbox_env`
/// (the bring-up-failure path).
#[derive(Debug, Clone)]
pub struct SandboxHalt {
    pub env_name: String,
    /// Placement label, e.g. `provider:sprites` / `k8s` / `ssh`.
    pub placement: String,
    /// Human-readable cause (token missing, auth rejected, no runnable backend…).
    pub reason: String,
    /// `failover = "ask"`: the modal offers a "run on host" choice beside retry.
    pub ask: bool,
    /// Set when the reason we cannot contain this pane is a runtime that is
    /// installed but **not running**. The modal then offers to start it, which
    /// is a fix the user can take without leaving thegn. `None` for every other
    /// halt (a missing token, an unreachable host — nothing to start).
    pub dormant: Option<thegn_core::sandbox_dormant::DormantRuntime>,
}

impl std::fmt::Display for SandboxHalt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let allow = if self.ask {
            "choose \"run on host\", or set `failover = \"auto\"`"
        } else {
            "set `failover = \"ask\"` or `\"auto\"`"
        };
        write!(
            f,
            "environment '{}' ({}) could not be brought up: {} — thegn will not \
             silently fall back to the host. Fix the env, retry, {} ([sandbox] or \
             [env.{}]) to allow it.",
            self.env_name, self.placement, self.reason, allow, self.env_name
        )
    }
}

impl std::error::Error for SandboxHalt {}

/// The settled sandbox for a worktree: the resolved+ensured spec, or `None`
/// for the host fallback. `backend_label` is what the DB records ("host" when
/// no sandbox stuck); `warnings` are the human-visible fallback notes that
/// ride into [`LaunchSpec::warnings`].
#[derive(Debug, Clone)]
pub struct SandboxOutcome {
    pub spec: Option<sandbox::SandboxSpec>,
    pub backend_label: String,
    pub warnings: Vec<String>,
    /// The resolved env's sandbox shell override (`""` ⇒ host `$SHELL`).
    pub shell: String,
    /// Whether the env runs off the local host (ssh/k8s/provider). Drives the
    /// pane cwd: a remote placement has no local working directory.
    pub is_remote: bool,
    /// An explicit pane cwd that overrides the worktree path — set when the data
    /// mode projects the tree to a local mountpoint (`sshfs`/`sync`), so the pane
    /// runs locally *at the mountpoint* rather than over the raw placement.
    pub cwd_override: Option<PathBuf>,
    /// The DB `worktrees.location` blob to persist for this worktree (`None` =>
    /// local). Set for a `Placement::Provider` env so the chrome's git/fs reads
    /// route into the sandbox via [`GitLoc::Provider`](thegn_core::remote::GitLoc).
    pub location: Option<String>,
    /// True when a `Placement::Provider` env failed to come up and `failover`
    /// degraded this open to a bare host shell. Distinct from a genuinely-local
    /// (or ssh/k8s) env that also carries `location = None`: it tells the launch
    /// site to actively HEAL a stale remote `worktrees.location` back to local,
    /// so the chrome stops routing git/fs reads into the dead provider and the
    /// tab chip stops claiming the pane is remote when it is running on the host.
    pub degraded_from_provider: bool,
}

/// Resolve and `ensure` the sandbox for `worktree` — the BLOCKING half of a
/// launch (container inspect/image pull/start can take seconds-to-minutes), so
/// callers must keep it off the event loop. No DB access: `backend_choice` is
/// the persisted/explicit backend label (empty or "auto" walks the chain).
///
/// Wraps in the worktree's sandbox/container (and/or the mosh/ssh transport
/// for a remote worktree). Auto walks the configured chain, collecting
/// human-visible fallback warnings; an explicit choice (config or
/// `backend_choice`) must not silently fall back — it errors instead. Host is
/// the last fallback for the auto chain only.
/// Prepare the sandbox for a worktree with an explicitly-selected execution
/// environment name (or `None` to fall through to repo/global selection).
/// Resolves via [`crate::handlers::repo_trust`] (honours TOFU approvals).
///
/// `choice_is_explicit`: the wizard's *fresh* pick passes `true` (always wins
/// over config); a relaunch passes it only when the DB value is a recorded
/// deliberate override, so an explicit config backend still beats a stale entry.
#[allow(clippy::too_many_arguments)]
pub fn prepare_sandbox_env(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    loc: &GitLoc,
    backend_choice: Option<&str>,
    choice_is_explicit: bool,
    selected_env: Option<&str>,
) -> anyhow::Result<SandboxOutcome> {
    use crate::handlers::repo_trust::resolve_env_trusted;
    let environment = resolve_env_trusted(cfg, repo_root, loc, worktree, selected_env);
    let mut placement = environment.placement.clone();
    let env_shell = environment.sandbox.shell.clone();
    // The worktree-projection plan (sshfs/sync) for this env's `data` mode, or
    // `None` for the default `in_env` (no projection). Captured before `sandbox`
    // is moved out of `environment` below.
    let projection = thegn_core::projection::for_environment(&environment);
    // Data mode + env name for the provider file-sync path below (a provider
    // placement isn't handled by `for_environment`, which is ssh-only).
    let env_data = environment.data;
    let env_name = environment.name.clone();
    // A non-default env was SELECTED but did not resolve to an `[env.<name>]`
    // table, so resolution fell back to Local carrying the requested `env_name`
    // (captured before `environment` is partially moved below). The provider/ssh
    // degrade paths further down never fire for it (placement is already Local),
    // so it's routed through the same failover decision explicitly — see the guard
    // after `warnings` is declared.
    let unresolved_selection = environment.unresolved_selection;
    // Failover: `halt`/`ask` block a bring-up failure; `auto`/`force_host` degrade.
    let failover_mode = cfg.env_failover_mode(repo_root, &env_name);
    let ask = matches!(failover_mode, thegn_core::config::FailoverMode::Ask);
    let degrade_allowed = matches!(failover_mode, thegn_core::config::FailoverMode::Auto)
        || force_host_requested(worktree);
    let placement_label = placement.label();
    // For a managed-provider env, persist a `GitLoc::Provider` location so the
    // chrome's git/fs reads route into the sandbox via the control-plane exec
    // prefix. `None` for local/ssh/k8s (their data plane is unchanged). The
    // worktree dir inside the env is the provider `workdir` (default /workspace).
    let mut location = match &placement {
        thegn_core::placement::Placement::Provider(p) => {
            // Resolve against the sandbox's cached `$HOME` (workspace lives under the
            // login user's home). Cold on the very first create (before provisioning
            // has probed the home) ⇒ the `/workspace` fallback; recomputed + rewritten
            // each open, so it self-heals to the real path once the home is cached.
            let workdir = cfg
                .env
                .get(&env_name)
                .map(|e| crate::provider_workdir::resolve(&e.provider, &p.id))
                .unwrap_or_else(|| "/workspace".to_string());
            Some(thegn_core::remote::GitLoc::provider_db_string(
                &p.control_prefix,
                &workdir,
            ))
        }
        _ => None,
    };
    // A data mode that projects the tree to a local mountpoint (sshfs/sync) means
    // the pane runs LOCALLY *at the mountpoint*: the (ssh) placement is used only
    // to establish the projection, while execution is local. So pin the pane cwd
    // to the mountpoint and resolve the backend against a Local exec placement.
    // (Intended with `backend = none`/host; combining OCI sandboxing with a
    // projected tree is a future combination.)
    let cwd_override = projection
        .as_ref()
        .map(|p| std::path::PathBuf::from(&p.mountpoint));
    let mut exec_placement = if cwd_override.is_some() {
        thegn_core::placement::Placement::Local
    } else {
        placement.clone()
    };
    let mut env_is_remote = environment.is_remote() && cwd_override.is_none();
    // Set when a provider bring-up failure degrades this open to the host (below),
    // so the outcome can heal a stale remote `worktrees.location` back to local.
    let mut degraded_from_provider = false;
    let mut sb = environment.sandbox;
    let mut explicit_backend =
        sandbox::Backend::from_config(sb.backend).filter(|b| *b != sandbox::Backend::None);
    // A fresh/explicit choice always wins over config; a non-explicit DB value
    // only overrides when config is "auto" (an explicit `backend = "bwrap"` must
    // beat a stale entry). An explicit choice may be "host"/"none" — keep those.
    // A saved backend is a LOCAL-session artifact (only the wizard writes it, for
    // a local choice), so it must not resurrect a backend on a remote/provider
    // placement — those default to `none` (the sandbox brings its own isolation).
    let config_is_auto = sb.backend == thegn_core::config::SandboxBackend::Auto;
    if placement.is_local()
        && let Some(saved) = backend_choice.map(str::trim)
        && !saved.is_empty()
        && (choice_is_explicit || (config_is_auto && saved != "auto"))
        && let Ok(b) = thegn_core::config::SandboxBackend::from_str_validated(saved)
    {
        explicit_backend =
            sandbox::Backend::from_config(b).filter(|b| *b != sandbox::Backend::None);
        sb.backend = b;
    }
    // A host-toolchain backend (bwrap, systemd-nspawn, win-native) only makes
    // sense on the LOCAL box — it isolates via host-namespace syscalls. On a
    // non-local placement (ssh / k8s / provider) the placement is already the
    // isolation boundary, so a nested bwrap is meaningless: probing for it over
    // the remote exec channel answers Unreachable and the resolver stalls, then
    // hard-fails "sandbox backend 'bwrap' could not be resolved". This value is
    // almost always inherited from the base `[sandbox]` (or a per-worktree choice
    // saved from a prior LOCAL session), not a deliberate ask for THIS env — so
    // drop it to Auto and let the chain pick an in-placement runtime or run
    // natively in the placement, instead of treating it as an explicit demand.
    if !placement.is_local() && explicit_backend.is_some_and(|b| b.is_host_toolchain()) {
        // Expected, not a problem the user must fix: running natively in the
        // placement IS correct here (the provider/pod/host is the sandbox). So
        // trace it for `THEGN_LOG` / debugging rather than warning on every open —
        // the noisy hard-failure this replaces is gone, and `config explain`
        // still reports the configured value honestly.
        tracing::debug!(
            target: "thegn::sandbox",
            backend = %sb.backend,
            placement = %placement.label(),
            "host-local backend can't nest in a non-local placement; running natively in the placement",
        );
        explicit_backend = None;
        sb.backend = thegn_core::config::SandboxBackend::Auto;
    }
    let mut explicit_choice = explicit_backend.is_some();
    let auto_choice = sb.backend == thegn_core::config::SandboxBackend::Auto;
    let mut warnings = Vec::new();
    // Selection dropped: the user asked for a non-default env that isn't defined
    // under `[env.<name>]`, so `resolve_env` fell back to Local. The Provider/ssh
    // bring-up degrade blocks below never fire (placement is already Local), so
    // without this the dropped selection is silently honored as a local shell.
    // Route it through the SAME failover decision (`env_name` already carries the
    // requested name, so `failover_mode`/`degrade_allowed`/`ask` above were
    // computed against it): halt/ask surface the modal; auto degrades but flags
    // `degraded_from_provider` so the `provider_degraded` notification + status
    // fire instead of vanishing.
    if unresolved_selection {
        if !degrade_allowed {
            return Err(SandboxHalt {
                env_name: env_name.clone(),
                placement: format!("env {env_name}"),
                reason: format!(
                    "env '{env_name}' is not defined ([env.{env_name}] missing); its selection was dropped"
                ),
                ask,
                dormant: None,
            }
            .into());
        }
        thegn_core::msg::warn(&format!(
            "env '{env_name}' is not configured; falling back to the host"
        ));
        warnings.push(format!("{env_name} not configured; running on the host"));
        degraded_from_provider = true;
    }
    let profile_slug = cfg.profile.trim();
    let base_cname = sandbox::container_name_with_profile(
        worktree,
        if profile_slug.is_empty() {
            None
        } else {
            Some(profile_slug)
        },
    );
    let hardening = sb.profile;
    let cname = base_cname;
    // Bring the execution placement up (k8s pod / provider sandbox) BEFORE
    // resolving the backend — a no-op for the default local `in_env` env. Warm-on-
    // open: create the API sandbox first if `auto_provision` is set so the
    // subsequent ensure/clone/connect find it live (8-E).
    if matches!(placement, thegn_core::placement::Placement::Provider(_))
        && let Err(e) = auto_provision_sandbox(cfg, &env_name, worktree)
    {
        // Provider won't provision (bad token, quota, API down): `halt`/`ask`
        // surface the REAL cause (`{e:#}`); `auto`/run-on-host degrade to host.
        if !degrade_allowed {
            return Err(SandboxHalt {
                env_name: env_name.clone(),
                placement: placement_label.clone(),
                reason: format!("{e:#}"),
                ask,
                dormant: None,
            }
            .into());
        }
        // Degrade to a BARE host shell (`none`): the git worktree exists locally,
        // a reliable fallback when the provider is down.
        thegn_core::msg::warn(&format!(
            "env '{env_name}' unavailable ({e:#}); falling back to the host"
        ));
        warnings.push(format!(
            "{env_name} unavailable ({e:#}); running on the host"
        ));
        placement = thegn_core::placement::Placement::Local;
        exec_placement = thegn_core::placement::Placement::Local;
        env_is_remote = false;
        // The pane now runs on the host, so the worktree is NOT at the provider
        // location computed above — drop it (and flag the degrade) so the DB row
        // heals to local instead of routing chrome reads at the dead provider.
        location = None;
        degraded_from_provider = true;
        // Degrading to the host retires the configured backend: an explicit OCI
        // backend (podman/smol — kept past line 316) would otherwise strand the
        // bare-host fallback with "explicit backend 'none' did not produce a
        // runnable sandbox". Clear it so the None candidate opens a plain shell.
        explicit_backend = None;
        explicit_choice = false;
        sb.backend = thegn_core::config::SandboxBackend::None;
    }
    if !placement.is_local()
        && let Err(e) = placement.ensure()
    {
        // Same failover-to-local rule for a placement (k8s pod / provider VM).
        if !degrade_allowed {
            return Err(SandboxHalt {
                env_name: env_name.clone(),
                placement: placement_label.clone(),
                reason: format!("{e:#}"),
                ask,
                dormant: None,
            }
            .into());
        }
        thegn_core::msg::warn(&format!(
            "env '{env_name}' placement bring-up failed ({e:#}); falling back to the host"
        ));
        warnings.push(format!(
            "{env_name} placement bring-up failed; running on the host"
        ));
        placement = thegn_core::placement::Placement::Local;
        exec_placement = thegn_core::placement::Placement::Local;
        env_is_remote = false;
        // Same as the auto-provision degrade above: heal the location to local
        // and drop any latched explicit backend so the host fallback runs.
        location = None;
        degraded_from_provider = true;
        explicit_backend = None;
        explicit_choice = false;
        sb.backend = thegn_core::config::SandboxBackend::None;
    }
    if let Some(pspec) = &projection {
        let backend = thegn_svc::projection::for_data_mode(pspec);
        match backend.mount(pspec) {
            // Record the live projection so the worktree-close thread (which only
            // has the path) can unmount it without re-resolving the env.
            Ok(_) => register_projection(worktree, pspec.clone()),
            Err(e) => {
                warnings.push(format!("projection ({}) failed: {e}", backend.kind()));
                thegn_core::msg::warn(&format!("projection mount failed for {worktree}: {e}"));
            }
        }
    }
    // Provider file-sync (`data = "sync"` on a managed provider): push the local
    // worktree into the sandbox fs before the pane execs (the pane runs IN the
    // sandbox, so there's no local cwd override). Best-effort: a failure warns,
    // never blocks the pane. Runs on a scoped thread with its own runtime so it
    // is safe regardless of the caller's async context.
    if env_data == thegn_core::config::DataMode::Sync
        && matches!(placement, thegn_core::placement::Placement::Provider(_))
        && let Some((provider, id, workdir)) = provider_sync_target(cfg, &env_name)
    {
        match block_on_provider(|| async {
            provider
                .upload_dir(&id, std::path::Path::new(worktree), &workdir)
                .await
        }) {
            Ok(()) => register_provider_sync(worktree, &env_name),
            Err(e) => {
                warnings.push(format!("provider sync upload failed: {e}"));
                thegn_core::msg::warn(&format!("provider sync upload failed for {worktree}: {e}"));
            }
        }
    }
    // Scope one probe pass over the whole backend-resolution walk (every
    // candidate's `resolve_placed` + the halt-path `placement_reachable` below).
    // Within it an unreachable placement is probed once per backend, not
    // re-probed for each of the N candidates — bounding a wedged-transport stall.
    let _probe_pass = thegn_core::sandbox::probe_pass_guard();
    for candidate in sandbox_candidates(&sb) {
        // `resolve_placed_exact`, not `resolve_placed`: `sandbox_candidates` has
        // ALREADY expanded the chain into one explicit candidate per entry, so a
        // resolver that degrades into the chain on a miss makes each candidate
        // re-walk every entry — N² probes and N copies of the host-fallback
        // warning for one spawn. The single fallback notice is emitted below,
        // after the last candidate.
        if let Some(mut spec) = sandbox::resolve_placed_exact(
            &candidate,
            loc,
            &cname,
            hardening,
            exec_placement.clone(),
        ) {
            if spec.backend == sandbox::Backend::None {
                // Local `none` = run on the host (plain-shell fallback below). A
                // remote SSH placement degrading to `none` fails loud (no
                // container for the synced worktree) — see ssh_none_guard.
                if spec.placement.is_local() {
                    break;
                }
                ssh_none_guard(
                    &spec,
                    sb.backend,
                    degrade_allowed,
                    ask,
                    &env_name,
                    &placement_label,
                )?;
                return Ok(SandboxOutcome {
                    backend_label: spec.backend.label().to_string(),
                    spec: Some(spec),
                    warnings,
                    shell: env_shell,
                    is_remote: env_is_remote,
                    cwd_override,
                    location,
                    degraded_from_provider,
                });
            }
            if let Some(expected) = explicit_backend
                && spec.backend != expected
            {
                anyhow::bail!(
                    "explicit sandbox backend '{}' resolved to '{}' for {worktree}; refusing fallback",
                    sb.backend,
                    spec.backend.label()
                );
            }
            // A Ready host's assets (digest-pinned image, warm volumes, remote
            // OCI url) pin the spec; explicit user values win inside.
            crate::host_flow::apply_ready(worktree, &mut spec);
            // Final pre-create spec fixups: remote-OCI worktree sync + runtime degrade.
            crate::remote_sync::finalize_spec_before_ensure(&mut spec, worktree, &mut warnings);
            // Say which hardening knobs this runtime can't express, rather than
            // shipping a quietly weaker profile than the config asked for.
            let dropped = thegn_core::sandbox::unsupported_hardening(&spec);
            if !dropped.is_empty() {
                warnings.push(format!(
                    "sandbox {}: {} unsupported by this runtime and not applied",
                    spec.backend.label(),
                    dropped.join(", ")
                ));
            }
            // VPN up BEFORE the container (joins the sidecar netns); failure bails.
            if let Err(e) = attach_vpn(&mut spec) {
                anyhow::bail!("sandbox vpn attach failed for {worktree}: {e}");
            }
            // `ensure` proves the container RUNNING, but not that OCI `exec` works
            // (broken keep-id/crun); probe so the real error surfaces, not a vanish.
            match sandbox::ensure(&spec).and_then(|()| {
                thegn_core::sandbox_preflight::preflight_exec(&spec)
                    .map_err(|e| anyhow::anyhow!("exec probe failed: {e}"))
            }) {
                Ok(()) => {
                    return Ok(SandboxOutcome {
                        backend_label: spec.backend.label().to_string(),
                        spec: Some(spec),
                        warnings,
                        shell: env_shell,
                        is_remote: env_is_remote,
                        cwd_override,
                        location,
                        degraded_from_provider,
                    });
                }
                Err(e) => {
                    let m = format!("sandbox {} failed: {e}", spec.backend.label());
                    if explicit_choice {
                        anyhow::bail!("{m} for {worktree}")
                    }
                    thegn_core::msg::warn(&format!("{m} for {worktree}; trying next backend"));
                    warnings.push(m);
                }
            }
        } else if candidate.backend == thegn_core::config::SandboxBackend::None {
            break;
        } else if explicit_choice {
            anyhow::bail!(
                "sandbox backend '{}' could not be resolved for {worktree}",
                candidate.backend
            );
        } else if candidate.backend != thegn_core::config::SandboxBackend::Auto {
            warnings.push(format!("sandbox {} unavailable", candidate.backend));
        }
    }
    if explicit_choice {
        anyhow::bail!(
            "explicit sandbox backend '{}' did not produce a runnable sandbox for {worktree}",
            sb.backend
        );
    }
    // Reaching here means no candidate produced a runnable sandbox and we'd fall
    // back to a bare host shell. For a NON-LOCAL env with failover off, that
    // silent drop is exactly what we refuse — halt with a warning instead.
    if !placement.is_local() && !degrade_allowed {
        let reachable = sandbox::placement_reachable(&exec_placement, &sb.backend_chain);
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: crate::remote_sync::no_backend_reason(reachable, &warnings),
            ask,
            dormant: None,
        }
        .into());
    }
    // A runtime that is installed but merely STOPPED is one command from
    // working, and until now the chain folded it into "absent" and opened a host
    // shell without a word. Offer it instead — start it, run on the host, or
    // cancel — per `[sandbox] on_dormant`. Only when this launch actually asked
    // for containment: an `auto`/`host` launch landing on the host is the
    // configured outcome, not a degradation, and must never nag.
    // …unless the user has ALREADY said that degrading to the host is fine:
    // `failover = "auto"`, or a worktree explicitly pinned to the host. Those are
    // standing answers to this exact question, and re-asking would nag someone
    // who configured their way out of it. `on_dormant` governs the default
    // (blocking) posture; an explicit degrade policy wins.
    if placement.is_local() && !degrade_allowed {
        // Only an EXPLICIT pick counts as asking for containment — a config
        // `backend = "podman-rootless"`, or the wizard's per-worktree/terminal
        // choice. `auto` means "walk the chain and land wherever", so landing on
        // the host is the configured outcome, not a degradation: treating it as
        // one raised this modal on every pane on any machine with a stopped
        // runtime, which is nagging, not honesty.
        let wanted = explicit_backend.is_some();
        let report = thegn_core::sandbox_support::support_report(
            &sb.backend_chain,
            &exec_placement,
            Some(sb.oci_runtime.as_str()).filter(|s| !s.is_empty()),
        );
        let offer = thegn_core::sandbox_dormant::first_dormant(&report).map(|d| {
            thegn_core::sandbox_dormant::runtime_for(
                d,
                thegn_core::sandbox_backend::host_os(),
                &|bin: &str| thegn_core::util::have(bin),
            )
        });
        match thegn_core::sandbox_dormant::decide(sb.on_dormant, wanted, offer) {
            thegn_core::sandbox_dormant::DormantAction::Proceed => {}
            // Unattended start: run it here (we are already off the event loop),
            // drop the cached "absent" probe, and let the caller retry — a
            // re-resolve inside this call would double every timeout budget.
            thegn_core::sandbox_dormant::DormantAction::Start(rt) => {
                if let Some(argv) = rt.start_argv.clone() {
                    thegn_core::msg::info(&format!("starting {} ({})…", rt.name, argv.join(" ")));
                    let started = crate::sandbox_start::run(&argv);
                    thegn_core::sandbox_backend::clear_probe_cache();
                    return Err(SandboxHalt {
                        env_name: rt.name.clone(),
                        placement: "local".into(),
                        reason: if started {
                            format!("{} was started — retry to use it", rt.name)
                        } else {
                            format!("could not start {} ({})", rt.name, rt.remedy)
                        },
                        ask: true,
                        dormant: Some(rt),
                    }
                    .into());
                }
            }
            thegn_core::sandbox_dormant::DormantAction::Ask(rt)
            | thegn_core::sandbox_dormant::DormantAction::Cancel(rt) => {
                let ask = matches!(sb.on_dormant, thegn_core::config::OnDormant::Ask);
                return Err(SandboxHalt {
                    env_name: rt.name.clone(),
                    placement: "local".into(),
                    reason: format!("{} is installed but not running — {}", rt.name, rt.remedy),
                    ask,
                    dormant: Some(rt),
                }
                .into());
            }
        }
    }
    // Every candidate is spent and we are opening a bare host shell. Say it once
    // here — under `Fallthrough::Exact` the resolver deliberately stays quiet,
    // because only this loop knows which candidate was the last one.
    thegn_core::sandbox_backend::host_fallback_notice(&sb, &exec_placement);
    if auto_choice && warnings.is_empty() {
        warnings.push("sandbox auto selected host".to_string());
    } else if auto_choice {
        warnings.push("running on host after sandbox fallback".to_string());
    }
    Ok(SandboxOutcome {
        spec: None,
        backend_label: "host".to_string(),
        warnings,
        shell: env_shell,
        is_remote: env_is_remote,
        cwd_override,
        location,
        degraded_from_provider,
    })
}

/// Bring up the worktree's VPN tunnel (if `[sandbox.vpn]` requested one) before
/// the sandbox container is created, and splice the result into `spec`:
/// userspace (proxy) tunnels get their `ALL_PROXY`/`HTTPS_PROXY` exports added
/// to `env_overrides`; the `--network container:<sidecar>` wiring is emitted by
/// `oci_create_opts` from the deterministic sidecar name.
///
/// Sidecar/proxy modes require a local OCI backend (the bring-up shells out to
/// the same container runtime). On other backends the tunnel can't be attached;
/// per `on_error` this either bails (`fail`), warns-and-continues (`warn`), or
/// forces the sandbox offline (`offline`). Tunnel bring-up itself runs here on
/// the (already off-event-loop) sandbox-prepare path.
fn attach_vpn(spec: &mut sandbox::SandboxSpec) -> anyhow::Result<()> {
    use thegn_core::config::{VpnMode, VpnOnError};
    let Some(vpn) = spec.vpn.clone() else {
        return Ok(());
    };
    let on_error = vpn.on_error;
    let local = spec.placement.is_local();
    let sidecar_capable =
        spec.backend.is_oci() && local && matches!(vpn.mode, VpnMode::Sidecar | VpnMode::Proxy);

    // Helper: apply the configured failure policy to an error/condition.
    let apply_on_error = |spec: &mut sandbox::SandboxSpec, msg: String| -> anyhow::Result<()> {
        match on_error {
            VpnOnError::Fail => Err(anyhow::anyhow!(msg)),
            VpnOnError::Warn => {
                thegn_core::msg::warn(&format!("{msg}; continuing (on_error=warn)"));
                Ok(())
            }
            VpnOnError::Offline => {
                thegn_core::msg::warn(&format!("{msg}; forcing network=none (on_error=offline)"));
                spec.network = thegn_core::config::Network::None;
                spec.vpn = None;
                Ok(())
            }
        }
    };

    if !sidecar_capable {
        return apply_on_error(
            spec,
            format!(
                "vpn: provider '{}' in mode '{}' needs a local OCI backend (got '{}')",
                vpn.provider,
                vpn.mode,
                spec.backend.label()
            ),
        );
    }

    let Some(prefix) = sandbox::oci_runtime_prefix(spec.backend) else {
        return apply_on_error(spec, "vpn: no OCI runtime for backend".to_string());
    };
    let rt = thegn_svc::vpn::OciRuntime::new(prefix);
    let sidecar = sandbox::vpn_sidecar_name(&spec.name);
    let provider = thegn_svc::vpn::for_provider(&vpn);

    let attach = match provider.up(&rt, &sidecar) {
        Ok(a) => a,
        Err(e) => return apply_on_error(spec, format!("vpn: bring-up failed: {e}")),
    };
    if let Err(e) = provider.ready(&rt, &sidecar, vpn.ready_timeout) {
        return apply_on_error(spec, format!("vpn: {e}"));
    }
    // Userspace tunnels: point the inner process at the SOCKS/HTTP proxy.
    if let Some(proxy) = &attach.proxy {
        for (k, v) in proxy.env_exports() {
            spec.env_overrides.insert(k, v);
        }
    }
    Ok(())
}

/// Best-effort: de-register a worktree's ephemeral VPN node before its sidecar
/// container is removed (e.g. `tailscale logout`). Called from the worktree-close
/// teardown thread, which only has the path — so we re-resolve the effective
/// config. A no-op when no VPN is configured. Ephemeral keys also auto-reap
/// server-side once the sidecar dies, so this is an optimization, not required.
pub(crate) fn deregister_vpn(path: &str) {
    let cfg = Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
    let sb = cfg.repo_sandbox(Path::new(path));
    if !sb.vpn.is_enabled() {
        return;
    }
    // Profile-aware: the sidecar was created from the container's REAL name
    // (`thegn-{profile}-{slug}` under a profile), so a plain-name teardown
    // missed it entirely.
    let name = sandbox::container_name_with_profile(path, Some(&thegn_core::profile::name()));
    let Some(vpn) = sandbox::build_vpn_spec(&sb.vpn, &name, sb.profile) else {
        return;
    };
    let sidecar = sandbox::vpn_sidecar_name(&name);
    let provider = thegn_svc::vpn::for_provider(&vpn);
    // We don't track which OCI runtime started the sidecar; try the likely ones.
    // `down` execs the de-register inside the sidecar, so a wrong runtime simply
    // fails to find the container and is ignored.
    for prefix in [
        vec!["podman".to_string()],
        vec!["docker".to_string()],
        vec!["sudo".to_string(), "-n".to_string(), "podman".to_string()],
    ] {
        let rt = thegn_svc::vpn::OciRuntime::new(prefix);
        let _ = provider.down(&rt, &sidecar);
    }
}

/// In-process registry of live worktree projections (path → resolved spec), so
/// the worktree-close teardown thread — which only has the path — can unmount the
/// projection (sshfs/sync) without re-resolving the named env. Best-effort: a
/// projection created in a previous process isn't tracked here (it auto-reaps
/// like the VPN ephemeral nodes, or is cleaned by `thegn env down`).
fn projection_registry() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, thegn_core::projection::ProjectionSpec>,
> {
    static REG: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, thegn_core::projection::ProjectionSpec>>,
    > = std::sync::OnceLock::new();
    REG.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn register_projection(worktree: &str, spec: thegn_core::projection::ProjectionSpec) {
    if let Ok(mut reg) = projection_registry().lock() {
        reg.insert(worktree.to_string(), spec);
    }
}

/// Tear down a worktree's projection (unmount sshfs / final sync) on close.
/// A no-op when nothing was projected. Called from the worktree-close teardown
/// thread alongside [`deregister_vpn`].
pub(crate) fn deproject(path: &str) {
    let spec = projection_registry()
        .lock()
        .ok()
        .and_then(|mut r| r.remove(path));
    if let Some(spec) = spec {
        let backend = thegn_svc::projection::for_data_mode(&spec);
        let _ = backend.unmount(&spec);
    }
}

// Provider construction + name resolution live in `provider_factory.rs`
// (extracted for the file-size ratchet); re-exported so call sites are unchanged.
pub(crate) use crate::provider_factory::{provider_for, provider_for_named, provider_sandbox_name};

/// Per-provider native-exec health: after a connect/exec failure, `exec = "auto"`
/// spawns skip the native path (use the CLI) for a cooldown, then retry — so one
/// flaky WSS connect degrades gracefully instead of husking every new pane.
/// `exec = "api"` ignores this (it always tries native). The relay/bridge report
/// outcomes via [`native_exec_report`]; the spawn decision consults
/// [`native_exec_healthy`].
mod native_health {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    const COOLDOWN: Duration = Duration::from_secs(30);

    fn reg() -> &'static Mutex<HashMap<String, Instant>> {
        static R: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
        R.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn report(provider: &str, ok: bool) {
        let mut g = reg().lock().unwrap();
        if ok {
            g.remove(provider);
        } else {
            g.insert(provider.to_string(), Instant::now());
        }
    }

    pub(super) fn healthy(provider: &str) -> bool {
        reg()
            .lock()
            .unwrap()
            .get(provider)
            .is_none_or(|t| t.elapsed() >= COOLDOWN)
    }
}

/// Report a native-exec connect/exec outcome for `provider` (drives the
/// `exec = "auto"` fallback cooldown). Called from the pane relay + the bridge.
pub(crate) fn native_exec_report(provider: &str, ok: bool) {
    native_health::report(provider, ok);
}

/// Whether `provider`'s native exec is currently considered healthy (no recent
/// failure within the cooldown). `exec = "auto"` skips native when this is false.
pub(crate) fn native_exec_healthy(provider: &str) -> bool {
    native_health::healthy(provider)
}

/// The provider to drive a resolved env's resident bridge over its **native exec
/// API** (CLI-free control plane), or `None` when the env isn't an exec_api
/// provider, opts out (`exec = "cli"`), or its token is unset. Used by the bridge
/// supervisor's `connect_native`; the sandbox id is the placement's `id`.
pub(crate) fn native_bridge_provider(
    cfg: &Config,
    env: &thegn_core::env::Environment,
) -> Option<thegn_svc::provider::Provider> {
    use thegn_core::config::ProviderExecMode;
    let thegn_core::placement::Placement::Provider(_) = &env.placement else {
        return None;
    };
    let pc = &cfg.env.get(&env.name)?.provider;
    if pc.exec == ProviderExecMode::Cli || !thegn_svc::provider::exec_api_by_name(&pc.provider) {
        return None;
    }
    // Auto backs off to the CLI bridge during a provider's failure cooldown; api
    // always tries native.
    if pc.exec == ProviderExecMode::Auto && !native_exec_healthy(&pc.provider) {
        return None;
    }
    provider_for(pc)
}

/// A resolved native-exec plan for a worktree's interactive shell: the built
/// provider, the sandbox id, the inner login-shell command to run inside it, the
/// in-sandbox working dir, and the pane env. Consumed by the host spawner to open
/// a CLI-free `Stream` pane (see `Panes::spawn_native`).
pub struct NativeShell {
    pub provider: thegn_svc::provider::Provider,
    /// The provider name (e.g. `"sprites"`), retained for session persistence.
    pub provider_name: String,
    pub sandbox_id: String,
    /// The login shell to exec inside the sandbox (basename form).
    pub inner: String,
    /// The worktree's path inside the sandbox (the provider `workdir`).
    pub workdir: String,
    pub env: Vec<(String, String)>,
}

impl NativeShell {
    /// The [`ExecSpec`](thegn_svc::provider::ExecSpec) to open a fresh login
    /// shell inside the sandbox: a `/bin/sh -lc` that cd's into the worktree's
    /// `workdir` then runs the resolved shell, with the pane env passed through.
    ///
    /// `inner` is a self-contained script that does its OWN `exec` — either the
    /// [`shell_inner`] runtime probe chain (`command -v zsh && exec zsh -l; …;
    /// exec /bin/sh -l`) or [`shell_inner_override`] (`exec <shell> -l`). It must
    /// NOT be prefixed with another `exec`: `exec command -v zsh …` makes the
    /// shell try to exec a binary named `command` (a builtin), which fails with
    /// 127 and kills the pane before any shell starts.
    pub fn open_spec(&self, cols: u16, rows: u16) -> thegn_svc::provider::ExecSpec {
        let script = if self.workdir.is_empty() {
            self.inner.clone()
        } else {
            format!("cd {} 2>/dev/null; {}", self.workdir, self.inner)
        };
        thegn_svc::provider::ExecSpec {
            // remote/sandbox target is Linux; POSIX sh is correct here
            argv: vec!["/bin/sh".to_string(), "-lc".to_string(), script],
            tty: true,
            cols,
            rows,
            env: self.env.clone(),
            cwd: (!self.workdir.is_empty()).then(|| self.workdir.clone()),
        }
    }

    /// Like [`open_spec`](Self::open_spec) but execs a **clean, rc-free** shell
    /// ([`clean_shell_inner`]) instead of the resolved login shell — the startup
    /// watchdog's fallback when the login shell hangs/errors on the user's
    /// dotfiles. The clean script carries its own `exec` chain, so it is dropped
    /// in after the `cd` without an outer `exec` wrapper.
    pub fn open_spec_clean(&self, cols: u16, rows: u16) -> thegn_svc::provider::ExecSpec {
        let inner = clean_shell_inner();
        let script = if self.workdir.is_empty() {
            inner
        } else {
            format!("cd {} 2>/dev/null; {}", self.workdir, inner)
        };
        thegn_svc::provider::ExecSpec {
            argv: vec!["/bin/sh".to_string(), "-lc".to_string(), script],
            tty: true,
            cols,
            rows,
            env: self.env.clone(),
            cwd: (!self.workdir.is_empty()).then(|| self.workdir.clone()),
        }
    }
}

/// Resolve `(provider, sandbox id, workdir)` for a worktree's PROVIDER env — for
/// the SSH-over-WSS proxy path (`[env.<name>.provider] connect = "ssh"`). Unlike
/// [`native_shell_exec`] it does NOT gate on the exec mode/health: it only needs
/// the provider handle + the resolved sandbox id to open the TCP proxy. `None`
/// when the env isn't a provider placement or the provider can't be built (e.g.
/// the API token isn't set). Resolves the env exactly like `native_shell_exec`.
pub fn provider_proxy_target(
    cfg: &Config,
    worktree: &str,
) -> Option<(thegn_svc::provider::Provider, String, String)> {
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    let thegn_core::placement::Placement::Provider(p) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    // Bound-aware id (like `native_exec_for`/`provider_sandbox_name`): a claimed
    // warm-pool spare targets that spare, not the derived husk the pool flow
    // destroyed — else the ssh proxy + p2p push relay into the wrong sandbox.
    let id = Db::open()
        .ok()
        .and_then(|db| db.worktree_provider_sandbox(worktree).ok().flatten())
        .unwrap_or_else(|| p.id.clone());
    let provider = provider_for_named(pc, &id)?;
    let workdir = crate::provider_workdir::resolve(pc, &id);
    Some((provider, id, workdir))
}

pub(crate) use crate::agent_ssh::{
    SPRITE_SSHD_PORT, mosh_setup_script, sprite_ssh_argv, sprite_ssh_connect, sprite_ssh_keypair,
    sprite_sshd_setup_script, sprite_sshd_start_script,
};

/// Decide whether `worktree`'s interactive shell should attach via a provider's
/// **native exec API** instead of the CLI/PTY path. `Some` when the resolved env
/// is a `provider` placement whose provider has a native exec API, whose `exec`
/// mode isn't `cli`, and whose API token is present; `None` ⇒ use [`launch_spec`].
///
/// Resolves the env exactly as [`launch_spec_full`] does (DB repo-root +
/// effective env) so the two paths never disagree about which env is in play.
/// The agent KINDS surfaced in the `[[agents]]` picker — what gets provisioned
/// into a sandbox (installed + config-carried). Each entry maps to its kind via
/// its explicit `provider` (e.g. the managed pi's `provider = "pi"`), else the
/// program basename of its command. The plain shell (`__shell__`) is skipped, and
/// kinds dedup (so "Agent" + "Vanilla Pi" → one `pi`). This makes the config the
/// source of truth: a custom agent you add is provisioned; one you remove is
/// disabled — instead of sniffing the host. `[sandbox.home] agents` overrides.
pub(crate) fn provisioned_agent_kinds(cfg: &Config) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for a in &cfg.agents {
        if a.name == "shell" || a.command.trim() == "__shell__" {
            continue;
        }
        let kind = a
            .provider
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| {
                a.command
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string()
            });
        let kind = kind.trim().to_string();
        if !kind.is_empty() && !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// Auto-detect the coding agents the HOST has — the FALLBACK when no `[[agents]]`
/// picker is configured at all (see [`provisioned_agent_kinds`]). A known agent
/// ([`thegn_core::envplan::known_agents`]) counts as present if its binary is
/// on the host PATH or its config/credential dir exists in `$HOME`.
pub(crate) fn detect_host_agents() -> Vec<String> {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs: Vec<&str> = path.split(':').filter(|s| !s.is_empty()).collect();
    thegn_core::envplan::known_agents()
        .iter()
        .filter(|a| {
            let on_path = dirs.iter().any(|d| Path::new(d).join(a).is_file());
            if on_path {
                return true;
            }
            let (files, cfg_dirs) = thegn_core::envplan::agent_config_paths(a);
            files
                .iter()
                .chain(cfg_dirs.iter())
                .any(|rel| home.join(rel).exists())
        })
        .map(|a| a.to_string())
        .collect()
}

pub fn native_shell_exec(cfg: &Config, worktree: &str) -> Option<NativeShell> {
    native_exec_for(cfg, worktree, None)
}

/// Like [`native_shell_exec`] but runs the AGENT `choice`'s command in the sandbox
/// over the native exec API — so the "Agent"/claude/codex/… picker entries run
/// INSIDE the sprite (where the code is), instead of via the provider CLI prefix
/// (`sprite exec …`) which isn't installed on the host. `None` for the plain shell
/// choice (use [`native_shell_exec`]) or a non-native-exec env. The agent inherits
/// the same in-sprite env as the shell (host secrets + proxy routing when set), so
/// e.g. the managed pi snippet's `$HOME/.thegn/pi` resolves to the sprite home.
pub fn native_agent_exec(cfg: &Config, worktree: &str, choice: &str) -> Option<NativeShell> {
    if choice.is_empty() || choice == SHELL || choice == "clean-shell" {
        return None;
    }
    // Only real agents (not tools) route through here.
    cfg.agent_command(choice)?;
    native_exec_for(cfg, worktree, Some(resolve_command(cfg, choice)))
}

/// Shared body: build a [`NativeShell`] for a native-exec provider worktree. With
/// `agent_cmd = None` the inner command is the login shell; with `Some(cmd)` it's
/// that agent command (run via the same `cd workdir; <cmd>` wrapper).
fn native_exec_for(cfg: &Config, worktree: &str, agent_cmd: Option<String>) -> Option<NativeShell> {
    use thegn_core::config::ProviderExecMode;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    let thegn_core::placement::Placement::Provider(p) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    if pc.exec == ProviderExecMode::Cli || !thegn_svc::provider::exec_api_by_name(&pc.provider) {
        return None;
    }
    // Auto backs off to the CLI pane during a provider's failure cooldown; api
    // always tries native.
    if pc.exec == ProviderExecMode::Auto && !native_exec_healthy(&pc.provider) {
        return None;
    }
    // Token missing ⇒ no provider built ⇒ fall back to the CLI path (which has
    // its own behavior when unconfigured); don't silently spawn a dead session.
    let provider = provider_for(pc)?;
    // The host's absolute $SHELL path won't exist in the sandbox, so use the
    // basename form (in_oci = true), honoring an explicit env shell override.
    let sb_shell = environment.sandbox.shell.trim().to_string();
    let inner = match agent_cmd {
        // An agent choice: run its command directly in the sandbox.
        Some(cmd) => cmd,
        // The plain shell pane.
        None if sb_shell.is_empty() => shell_inner(true),
        None => shell_inner_override(&sb_shell),
    };
    // Point each agent CLI at the ACTIVE account's uploaded credential home.
    let inner = format!(
        "{}{inner}",
        crate::agent_configs::account_pane_env_exports(cfg, worktree)
    );
    // Carry the host's passthrough secrets (GH_TOKEN, ANTHROPIC_API_KEY, …) into
    // the provider exec so the in-sprite shell + any agent it spawns (pi, claude
    // code, hermes) work like local. Remote-safe filter drops host-local socket
    // vars (SSH_AUTH_SOCK/GPG_*) that would dangle in the VM. THEGN_* win.
    let mut env = environment.sandbox.passthrough_env_remote();
    // Let an in-sandbox `nix develop` / direnv `use flake` fetch PRIVATE flake
    // inputs: nix's fetcher ignores git's credential helper, so without a
    // `github.com` access-token a private `github:org/repo` flake input 404s even
    // though the repo clone authenticated. Derive NIX_CONFIG from the token we
    // already carry in (runtime-only; never persisted to nix.conf/checkpoint).
    if let Some((_, tok)) = env
        .iter()
        .find(|(k, v)| (k == "GH_TOKEN" || k == "GITHUB_TOKEN") && !v.is_empty())
    {
        env.push((
            "NIX_CONFIG".to_string(),
            format!("access-tokens = github.com={tok}"),
        ));
    }
    env.push(("THEGN_WORKTREE".to_string(), worktree.to_string()));
    env.push(("THEGN_BRANCH".to_string(), String::new()));
    // A claimed warm-pool spare overrides the derived id (same rule as
    // `provider_sandbox_name`): the pane must attach to the sandbox the
    // worktree is actually bound to, not the derived husk.
    let bound = Db::open()
        .ok()
        .and_then(|db| db.worktree_provider_sandbox(worktree).ok().flatten());
    let sandbox_id = bound.unwrap_or_else(|| p.id.clone());
    // Resolve the pane cwd against the sandbox's cached `$HOME` (the workspace lives
    // under the login user's home — see `provider_workdir`), so `cd {workdir}` lands
    // in the repo and not a nonexistent `/workspace`.
    let workdir = crate::provider_workdir::resolve(pc, &sandbox_id);
    Some(NativeShell {
        provider,
        provider_name: pc.provider.clone(),
        sandbox_id,
        inner,
        workdir,
        env,
    })
}

/// Whether `worktree`'s provider env still needs its one-time provisioning: a
/// managed provider AND (its sandbox does NOT exist yet — a cheap `list()` GET —
/// OR the sandbox exists but is BARE, i.e. the provision marker is absent). Two
/// callers: the eager provisioner (fire the create+provision ahead of focus) and
/// the pre-warm spec task (SKIP — prewarm must never create a sprite nor attach
/// to a bare one; the focused materialize provisions it). Off-loop (network);
/// `false` for non-provider envs, a tokenless/unbuilt provider, or a list error.
pub fn provision_pending(cfg: &Config, worktree: &str) -> bool {
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    if !matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) {
        return false;
    }
    let Some(envc) = cfg.env.get(&environment.name) else {
        return false;
    };
    // Bound-aware name: a worktree that CLAIMED a pool spare is checked against
    // the SPARE (whose marker is present ⇒ not pending), not the derived id —
    // else eager would re-provision a sprite the worktree no longer uses.
    let Some(id) = provider_sandbox_name(cfg, worktree, &environment.name) else {
        return false;
    };
    let Some(provider) = provider_for_named(&envc.provider, &id) else {
        return false;
    };
    match block_on_provider(|| async { provider.list().await }) {
        // Missing ⇒ needs create + a full provision.
        Ok(names) if !names.iter().any(|n| n == &id) => true,
        // Exists — but is the TOOLCHAIN actually provisioned? `launch_spec`'s
        // `auto_provision` only `ensure_exists`es a BARE sprite (no nix/direnv/
        // agents), and a destroyed+recreated sprite is bare too. If we gate only on
        // existence, eager sees "exists" and SKIPS the splash-lock, so the pane
        // opens on a not-ready sprite — the premature shell. A missing provision
        // marker ⇒ still needs provisioning (which is idempotent: a present marker
        // short-circuits it). Only reached for an existing sandbox, once per
        // session per worktree (the `eager_inflight` guard), and for `Active*`
        // scope only the worktree we're opening anyway — so it doesn't wake idle
        // sandboxes wholesale.
        Ok(_) => {
            let workdir = crate::provider_workdir::resolve(&envc.provider, &id);
            let marker = thegn_core::envplan::EnvPlan::marker_path(&workdir);
            block_on_provider(|| async { provider.read(&id, &marker).await }).is_err()
        }
        Err(_) => false,
    }
}

/// Worktrees the user pinned to the HOST via the failover `ask` modal's "run on
/// host" choice — the launch path degrades instead of blocking until an explicit
/// env retry ([`clear_force_host`]) un-pins it.
fn force_host_registry() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static R: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Pin `worktree` to run on the host for this session (the ask modal's `[h]`).
pub fn request_force_host(worktree: &str) {
    if let Ok(mut s) = force_host_registry().lock() {
        s.insert(worktree.to_string());
    }
}

/// Whether `worktree` is pinned to the host (see [`request_force_host`]).
pub(crate) fn force_host_requested(worktree: &str) -> bool {
    force_host_registry()
        .lock()
        .map(|s| s.contains(worktree))
        .unwrap_or(false)
}

/// Un-pin `worktree` from the host so an explicit retry re-attempts the real env.
pub fn clear_force_host(worktree: &str) {
    if let Ok(mut s) = force_host_registry().lock() {
        s.remove(worktree);
    }
}

/// Clear the failure cooldown for the worktree's provider native exec so a
/// retry actually re-attempts the connection (otherwise [`env_halt_reason`]
/// would re-halt immediately on the stale cooldown). No-op for non-provider
/// envs; the token check in `env_halt_reason` still gates a tokenless retry.
pub fn clear_native_exec_cooldown(cfg: &Config, worktree: &str) {
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    if let thegn_core::placement::Placement::Provider(_) = &environment.placement
        && let Some(envc) = cfg.env.get(&environment.name)
    {
        native_exec_report(&envc.provider.provider, true);
    }
}

/// Visual state of one provisioning step, surfaced to the loading screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionState {
    /// Not started yet — shows the original dot.
    Pending,
    /// Currently running — shows the spinner/working glyph.
    Active,
    Done,
    Failed,
}

/// A provisioning step as shown on the loading screen (label + live state).
#[derive(Debug, Clone)]
pub struct ProvisionStepView {
    pub label: String,
    pub state: ProvisionState,
    /// Sub-line under the step on the loading screen: a live status for the
    /// active step or the captured error for a failed one. `None` = no sub-line.
    pub detail: Option<String>,
}

/// Provision the worktree's environment if it runs on a managed **provider**
/// (sprites, …) — a no-op (`Ok(false)`) for local/ssh/k8s envs. Resolves the env
/// from the worktree, then delegates to [`provision_provider_env`]. This is the
/// entry point the run loop calls off-thread before resolving the pane's launch
/// spec, so a fresh sandbox is set up (and the loading screen streamed) before
/// the pane attaches.
pub fn provision_worktree(
    cfg: &Config,
    worktree: &str,
    progress: impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<bool> {
    // Tag every log line this (blocking) provisioning run emits with the
    // worktree, so a sprite/remote failure is attributable and the Logs panel
    // keeps it out of *other* worktrees' views.
    let _wt_log =
        thegn_core::log_trace::enter_wt(thegn_core::log_trace::wt_slug(Path::new(worktree)));
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected_env = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let environment = cfg.resolve_env(
        &repo_root,
        &loc,
        Path::new(worktree),
        selected_env.as_deref(),
    );
    if !matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) {
        return Ok(false);
    }
    // Flag the in-flight provision so the warm-claim fast path won't bind a
    // spare (and clear the splash) under this run — see `provision_gate`.
    let _live = crate::provision_gate::worktree_live_guard(worktree);
    provision_provider_env(cfg, worktree, &environment.name, progress)
}

/// Make a sandbox/remote env "just work" like local, by reproducing the repo's
/// **declared** environment inside it (see `thegn_core::envplan`): clone the
/// repo, install the declared toolchain (Nix devShell / mise / runtimes), sync
/// dotfiles, and checkpoint so the heavy install is one-time. Provider-agnostic
/// over the exec/fs APIs — no `sprite` CLI or musl bridge required.
///
/// Idempotent: a marker file under the workdir short-circuits a re-provision.
/// Runs OFF the event loop (network + minutes-long installs). `progress` is
/// called with the full step list after every state change so the caller can
/// render a live loading screen. Returns `Ok(true)` when the env is provisioned
/// (now or already), `Ok(false)` when not applicable (not a provider env / no
/// provider built), `Err` if a step failed.
/// Per-step ceiling for a provisioning exec. Build/network-bound steps (nix
/// devshell, clone, language runtimes) can legitimately run for many minutes; the
/// rest are quick, so a short ceiling there turns an otherwise-infinite hang (a
/// suspended sandbox, a lost exit frame) into a clear step failure.
pub(crate) fn provision_step_timeout(step_id: &str) -> std::time::Duration {
    use std::time::Duration;
    // Only `workspace` (mkdir) and `git_auth` (git config) are truly instant — a
    // stall there is the suspended-sandbox hang we want to catch fast. Everything
    // else can legitimately run for minutes (clone, package installs incl. the
    // openssh the ssh transport needs, npm agents, the nix devshell build, closure
    // substitution), so give it a generous ceiling that still bounds an infinite
    // hang rather than risk a false failure mid-build.
    let instant = matches!(step_id, "workspace" | "git_auth");
    if instant {
        Duration::from_secs(120) // 2 min — catches the suspended-sandbox hang fast
    } else {
        Duration::from_secs(1800) // 30 min — build/download-bound steps
    }
}

pub fn provision_provider_env(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
    mut progress: impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<bool> {
    provision_provider_env_named(cfg, worktree, env_name, None, &mut progress).map(|(ok, _)| ok)
}

/// Provision a provider env into a sandbox. `name_override` forces the sandbox
/// name (used for warm-pool SPARES, which aren't bound to a worktree); `None`
/// derives it from the worktree as usual. `worktree` always provides the env-
/// resolution + repo-origin context (for a spare, pass the repo's main worktree).
/// The clone is branch-less (`opts.branch = None`) either way, so a spare and a
/// worktree provision identically apart from the name. Returns `(provisioned,
/// checkpoint_id)` — the "thegn-provisioned" base checkpoint taken this run,
/// if any (recorded per (repo, env) and, for spares, on the pool row so a stale
/// spare can be recycled by restore-in-place instead of destroy+rebuild).
pub fn provision_provider_env_named(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
    name_override: Option<&str>,
    progress: &mut impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<(bool, Option<String>)> {
    use thegn_core::envplan::{self, EnvPlan, PlanOpts, StepKind};

    let Some(env) = cfg.env.get(env_name) else {
        return Ok((false, None));
    };
    let pc = &env.provider;
    let Some(id) = name_override
        .map(str::to_string)
        .or_else(|| provider_sandbox_name(cfg, worktree, env_name))
        .filter(|s| !s.is_empty())
    else {
        return Ok((false, None));
    };
    // Bake the resolved id so a recreate (`ensure_exists`→`create`) names the
    // sandbox correctly (the id embeds the repo/worktree tokens + a path-hash).
    let Some(provider) = provider_for_named(pc, &id) else {
        return Ok((false, None));
    };
    if !provider.caps().files {
        return Ok((false, None)); // can't provision without the fs API
    }
    // Serialize concurrent provisions of the same sandbox (eager vs focused
    // materialize): the loser blocks here (off-loop by contract), then the
    // marker short-circuit below makes its run a no-op. The marker alone only
    // guards SEQUENTIAL re-runs — it is written at the END of the pipeline.
    let _gate = crate::provision_gate::sandbox_lock(&id);

    // Recreate-if-missing: the sandbox may have been cleaned up out-of-band (TTL,
    // manual delete, provider GC). `ensure_exists` recreates it before we read the
    // marker / run any exec, so provisioning can't fail against a dead sandbox.
    // A freshly recreated sandbox has no marker ⇒ a full re-provision runs below.
    // (No-op when it already exists; cheap list+maybe-create.)
    // Surface the create/list phase (which precedes the step plan below) as a
    // labeled active step + a log breadcrumb — this is the phase that, untimed,
    // hung provisioning on startup, so make it observable rather than a blank.
    progress(&[ProvisionStepView {
        label: "Preparing sandbox".to_string(),
        state: ProvisionState::Active,
        detail: None,
    }]);
    tracing::debug!(target: "thegn::startup", %id, "provider ensure_exists (create/list)");
    let created = match block_on_provider(|| async { provider.ensure_exists(&id).await }) {
        Ok(created) => created,
        // `{e:#}` (alternate) flattens the whole anyhow cause chain into the
        // message — without it, Display keeps only the outermost context and the
        // real cause (e.g. machine0's `vm_create` `isError` text) is dropped
        // before it can reach the loading screen / logs.
        Err(e) => return Err(anyhow::anyhow!("ensure sandbox {id}: {e:#}")),
    };
    tracing::debug!(target: "thegn::startup", %id, created, "sandbox ensured");

    // A freshly created sprite cold-boots (Firecracker) for a few seconds; gate the
    // first fs/exec on a bounded readiness probe so we don't race the boot — the
    // pre-timeout race is exactly what hung provisioning "forever" on startup — and
    // surface a labeled active step so the loading screen isn't a frozen blank
    // during the wait. No-op for providers without a readiness notion.
    if created {
        const READY_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);
        let mut boot = vec![ProvisionStepView {
            label: "Waiting for sandbox to boot".to_string(),
            state: ProvisionState::Active,
            detail: None,
        }];
        progress(&boot);
        if let Err(e) = block_on_provider(|| async { provider.wait_ready(&id, READY_BUDGET).await })
        {
            boot[0].state = ProvisionState::Failed;
            boot[0].detail = Some(format!("{e:#}"));
            progress(&boot);
            return Err(anyhow::anyhow!("sandbox {id} not ready: {e:#}"));
        }
        boot[0].state = ProvisionState::Done;
        progress(&boot);
    }

    // Site the workspace under the sandbox login's real `$HOME` (a property of the
    // IMAGE: machine0's NixOS image logs in as `nix`/`/home/nix`, others as root) —
    // a bare `/workspace` at the root fs is not writable by a non-root login and
    // fails the "Prepare workspace" `mkdir`. Caches the home so the pane, chrome
    // reads, and marker checks all resolve the SAME path (`provider_workdir`).
    let (sprite_home, workdir) = crate::provider_workdir::probe_and_resolve(&provider, &id, pc);
    let marker = EnvPlan::marker_path(&workdir);

    // Idempotent: already provisioned ⇒ nothing to do (no new checkpoint) — but
    // still refresh auth creds: the host's OAuth token rotates, so the
    // provision-time snapshot goes stale and the in-sandbox agent 401s.
    if block_on_provider(|| async { provider.read(&id, &marker).await }).is_ok() {
        crate::provider_workdir::mark_provisioned(&id);
        crate::agent_configs::resync_agent_auth(&provider, &id, cfg, worktree, env_name);
        return Ok((true, None));
    }

    // Resolve the repo origin so the sprite can clone it.
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));

    let req = envplan::detect(Path::new(worktree));
    // Host secrets (GH_TOKEN, ANTHROPIC_API_KEY, …) carried into every provisioning
    // command so the clone authenticates against private repos and setup steps can
    // reach the network/model. Remote-safe (no host-local socket vars).
    let mut exec_env = cfg.repo_sandbox(&repo_root).passthrough_env_remote();
    crate::agent_configs::push_host_git_identity(&repo_root, &mut exec_env);
    // Which flake devShell the sandbox builds/enters ([sandbox] devshell, e.g.
    // "sandbox" for the lean build shell). Drives the seed build + realise so they
    // match the in-pane `.envrc` (which reads THEGN_DEVSHELL from exec_env).
    let devshell_attr = cfg.repo_sandbox(&repo_root).devshell.trim().to_string();
    // The generic, declarative personal layer ([sandbox.home]) — applied to every
    // sandbox so it feels like local. Resolve it PER-ENV (the env overlay may set a
    // different `strategy`, e.g. host-parity on a big box, clean on a sprite), then
    // resolve which dotfiles to upload under that strategy (drops a non-portable rc
    // with a warning; collects host store roots for host-parity).
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let home = cfg
        .resolve_env(&repo_root, &loc, Path::new(worktree), Some(env_name))
        .sandbox
        .home;
    let host_home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"));
    let (dotfiles, mut home_store_roots) = resolve_personal_dotfiles(&host_home, &home, env_name);
    // Host-parity transport selection. With no hosted binary cache configured but
    // `connect = "ssh"`, push the host store straight into the sandbox over the WSS
    // ssh tunnel (the host *is* the cache — no signing key, no hosted cache), then
    // `nix profile install` the shell + prompt tools by store path so they land on
    // `PATH`. Otherwise fall through to the cache-substitute / home-manager paths.
    let p2p_parity = home.strategy == thegn_core::config::ShellStrategy::HostParity
        && pc.connect == thegn_core::config::ProviderConnect::Ssh
        && pc.binary_cache_url.trim().is_empty();
    let home_profile_installs = if p2p_parity {
        let roots = host_shell_store_roots();
        // Push the binaries' closures too (not just what the rc sources).
        for r in &roots {
            if !home_store_roots.contains(r) {
                home_store_roots.push(r.clone());
            }
        }
        roots
    } else {
        Vec::new()
    };
    // SSH-over-WSS transport (`connect = "ssh"`): add the one-time in-sandbox sshd
    // setup (install openssh + host key + authorize our managed key + config) to
    // the personal-layer setup so it's baked into the checkpoint. The daemon is
    // (re)started at connect time by the `sprite-proxy` ProxyCommand.
    let mut setup = resolve_setup(&home);
    if pc.connect == thegn_core::config::ProviderConnect::Ssh
        && let Ok((_key, pubkey)) = sprite_ssh_keypair()
    {
        setup.push(sprite_sshd_setup_script(&pubkey));
    }
    // mosh transport: install mosh-server (else the pane falls back to plain ssh).
    if pc.transport == thegn_core::config::RemoteTransport::Mosh
        && thegn_core::config::ssh_reached_provider_kind(&pc.provider)
    {
        setup.push(mosh_setup_script());
    }
    // Check out the worktree's branch in the sandbox clone (so it's not stuck on
    // origin's default). `git checkout <b> || git checkout -b <b>` — if the branch
    // is on origin it lands with its commits; otherwise it's created off the
    // default (push the branch for its content). A SPARE (name_override) stays on
    // the default — it's generic until a worktree claims + rebranches it.
    // off-loop: provisioning entry — only called from provision_spare (pool
    // thread), provision_worktree (spawn_blocking), or the CLI.
    #[expect(clippy::disallowed_methods)]
    let branch = if name_override.is_some() {
        None
    } else {
        thegn_core::util::git_cmd(Path::new(worktree))
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|b| !b.is_empty() && b != "HEAD")
    };
    let opts = PlanOpts {
        workdir: workdir.clone(),
        origin: local_origin(&repo_root),
        branch,
        dotfiles,
        tools: home.tools.clone(),
        dotfiles_repo: home.dotfiles_repo.clone(),
        setup,
        // CONFIG-DRIVEN agents (you control which get installed + logged-in in
        // the sandbox): an explicit `[sandbox.home] agents` list wins; otherwise
        // the agents are derived from YOUR `[[agents]]` picker — every entry you
        // surface there is provisioned (a custom agent you add is installed; one
        // you remove is disabled). Only when no picker is configured at all do we
        // fall back to detecting the host's agents. Known kinds get an installer;
        // all get their config (login/history/skills/MCP) uploaded.
        agents: if !home.agents.is_empty() {
            home.agents.clone()
        } else {
            let from_picker = provisioned_agent_kinds(cfg);
            if from_picker.is_empty() {
                detect_host_agents()
            } else {
                from_picker
            }
        },
        allow_nix: true,
        // Only providers that CAN checkpoint get the plan step (a VPS has no
        // suspend — its "checkpoint" analog is the baked image, not a step).
        checkpoint: pc.auto_checkpoint && provider.caps().checkpoints,
        // Provisioning speedups (all no-ops unless configured).
        nix_installer: pc.nix_installer,
        nix_parallel: pc.nix_parallel(),
        binary_cache: (!pc.binary_cache_url.trim().is_empty()).then(|| {
            thegn_core::envplan::BinaryCache {
                url: pc.binary_cache_url.trim().to_string(),
                key: pc.binary_cache_key.trim().to_string(),
                push: pc.binary_cache_push,
            }
        }),
        strategy: home.strategy,
        nix_home_flake: home.nix_home_flake.clone(),
        home_store_roots,
        home_closure_p2p: p2p_parity,
        home_profile_installs,
        atuin: home.atuin,
        // The embedded host cache (a general substituter over the whole host store)
        // SUPERSEDES the one-shot devShell file:// push when on — keep push_devshell
        // only as the fallback for providers without the reverse tunnel.
        push_devshell: pc.push_devshell && !pc.host_cache,
        // devShell warm policy:
        //  • Real worktree: respect the configured `skip_devshell_warm` (the
        //    in-pane `direnv` realizes the devShell lazily).
        //  • SPARE (name_override): ALWAYS fully realize the devShell into the
        //    spare's own persistent fs, then checkpoint (the pipeline captures a
        //    provisioned-base checkpoint) — a true golden a claim resumes to a
        //    working shell in seconds, with NO per-worktree-tunnel dependency at
        //    claim time. Paid once in the background, amortized by checkpoint reuse
        //    (keyed on the flake.lock hash → self-refreshes on lock change); a
        //    persistent `binary_cache_url` turns the bake into a fast download.
        skip_devshell_warm: if name_override.is_some() {
            false
        } else {
            pc.skip_devshell_warm
        },
        // Full local parity (unpushed commits + uncommitted + untracked) for a
        // real worktree on an `in_env` provider — so a fresh sandbox matches the
        // working tree, not just origin. A SPARE (name_override) stays a pristine
        // clone (generic until claimed); a non-`in_env` data mode projects the
        // tree by other means, so skip the overlay there.
        local_parity: (name_override.is_none() && env.data == thegn_core::config::DataMode::InEnv)
            .then(|| worktree.to_string()),
        // A hibernated worktree resumes by overlaying its snapshot on the
        // fresh clone; the row flips to `restoring` here (deleted on success).
        snapshot_restore: (name_override.is_none())
            .then(|| crate::hibernator::begin_restore(worktree))
            .flatten(),
        // Host-cache loopback substituter for nix.conf — but only when the resident
        // musl bridge that carries the `:8484` tunnel is pushable; otherwise the port
        // is dead and nix wastes minutes timing out before falling back to source.
        host_cache_url: (pc.host_cache && crate::bridge_sup::bridge_binary_path().is_some())
            .then(|| format!("http://127.0.0.1:{}", crate::nixcache::SANDBOX_PORT)),
        toolchain: cfg.toolchain.clone(),
    };
    let plan = envplan::plan(&req, &opts);

    // Host-parity (Phase 2): push the host's home-shell closure to the configured
    // binary cache so the in-sandbox `home_closure` step can substitute it and the
    // exact host dotfiles resolve. Host-side (uses the host's `nix`), best-effort:
    // only when a push cache is set — nixpkgs paths otherwise substitute from the
    // default caches (cache.nixos.org) with no push. A failure just means the
    // sandbox falls back to whatever its substituters can serve.
    if opts.strategy == thegn_core::config::ShellStrategy::HostParity
        && !opts.home_store_roots.is_empty()
        && pc.binary_cache_push
        && !pc.binary_cache_url.trim().is_empty()
        && let Err(e) = push_home_closure(pc.binary_cache_url.trim(), &opts.home_store_roots)
    {
        thegn_core::msg::warn(&format!(
            "host-parity: pushing the home closure to {} failed: {e}; the sandbox will \
             substitute what it can from its default caches (e.g. cache.nixos.org).",
            pc.binary_cache_url.trim(),
        ));
    }

    // `sprite_home` (the sandbox login's real `$HOME`) was resolved + cached above,
    // right after the sandbox came Ready — reused here for the uploads.

    // The "thegn-provisioned" base checkpoint taken by this run's Checkpoint
    // step (if the plan has one) — persisted below + returned to the caller.
    let mut base_checkpoint: Option<String> = None;
    // Seed the loading screen with every step pending (the original dot).
    let mut views: Vec<ProvisionStepView> = plan
        .steps
        .iter()
        .map(|s| ProvisionStepView {
            label: s.label.clone(),
            state: ProvisionState::Pending,
            detail: None,
        })
        .collect();

    for (i, step) in plan.steps.iter().enumerate() {
        for (j, v) in views.iter_mut().enumerate() {
            v.state = match j.cmp(&i) {
                // A best-effort step that already failed stays Failed (with its
                // detail) — don't relabel a completed-with-warning step as Done.
                std::cmp::Ordering::Less if v.state == ProvisionState::Failed => {
                    ProvisionState::Failed
                }
                std::cmp::Ordering::Less => ProvisionState::Done, // check
                std::cmp::Ordering::Equal => ProvisionState::Active, // spinner
                std::cmp::Ordering::Greater => ProvisionState::Pending, // dot
            };
        }
        progress(&views);
        let step_t0 = std::time::Instant::now();
        tracing::info!(target: "thegn::startup", step = %step.id, "provision step start");

        let result: anyhow::Result<()> = match &step.kind {
            StepKind::Exec(script) => {
                // `/bin/sh -lc` + PATH-prefix + `2>&1` — see `provision_recover`.
                let argv = crate::provision_recover::exec_login_argv(script);
                // Bound every exec: a suspended/slow sandbox can leave `run_exec`
                // blocked on an exit frame that never comes, hanging the loading
                // screen forever. Quick steps (git auth, dotfiles repo) get a short
                // ceiling so a stall surfaces fast; build-bound steps (nix devshell)
                // a generous one.
                let to = provision_step_timeout(&step.id);
                block_on_provider(|| async {
                    match tokio::time::timeout(to, provider.run_exec(&id, &argv, None, &exec_env))
                        .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(anyhow::anyhow!("exec timed out after {}s", to.as_secs())),
                    }
                })
                .and_then(|(code, out)| {
                    if code == 0 {
                        Ok(())
                    } else {
                        // The splash detail is a clamped 4-line tail; log the FULL
                        // captured output so the exact failing command (e.g. which
                        // tool a nix `exit 127` couldn't find) is diagnosable from
                        // the log without the truncation.
                        tracing::warn!(
                            target: "thegn::startup",
                            step = %step.id,
                            code,
                            output = %out.trim(),
                            "exec provision step failed"
                        );
                        Err(anyhow::anyhow!(
                            "{} (exit {code}): {}",
                            step.label,
                            tail_lines(&out, 4)
                        ))
                    }
                })
            }
            StepKind::Dotfiles(files) => upload_dotfiles(&provider, &id, &sprite_home, files),
            StepKind::AgentConfigs(agents) => {
                crate::agent_configs::upload_agent_accounts(&provider, &id, &sprite_home, cfg);
                crate::agent_configs::upload_agent_configs(&provider, &id, &sprite_home, agents)
            }
            StepKind::AtuinSync => upload_atuin_creds(&provider, &id, &sprite_home, &exec_env),
            StepKind::DevShellClosurePush => {
                // Host-executed: build the repo's devShell on the host (a no-op for a
                // nix user who already has it) + transfer its closure into the sandbox
                // store, so the `devshell` warm below is a local store hit. Best-effort
                // — a failure just means the sandbox builds the devShell itself.
                if let Err(e) =
                    push_devshell_closure(&provider, &id, &repo_root, &workdir, &devshell_attr)
                {
                    thegn_core::msg::warn(&format!(
                        "devshell push: {e}; the sandbox will build the devShell itself."
                    ));
                }
                Ok(())
            }
            StepKind::LocalParity {
                worktree: wt,
                workdir: wd,
            } => {
                // Host-executed: capture the local worktree's unpushed commits +
                // uncommitted + untracked state and replay it over the clone.
                // Best-effort — a failure leaves the pristine origin checkout.
                if let Err(e) = crate::parity::apply_local_parity(&provider, &id, wt, wd, &exec_env)
                {
                    thegn_core::msg::warn(&format!(
                        "local parity: {e}; the sandbox keeps the origin checkout."
                    ));
                }
                Ok(())
            }
            StepKind::SnapshotRestore {
                worktree: wt,
                workdir: wd,
                snapshot_id: snap,
            } => {
                // Host-executed, NOT best-effort: a failure fails the step and
                // the row stays `hibernated` for the next open to retry.
                crate::hibernator::apply_snapshot_restore(
                    &provider, &id, cfg, wt, wd, snap, &exec_env,
                )
            }
            StepKind::Checkpoint => {
                with_provision_timeout("checkpoint", std::time::Duration::from_secs(600), || {
                    provider.checkpoint(&id, Some("thegn-provisioned"))
                })
                .map(|cp| base_checkpoint = Some(cp))
            }
            StepKind::HomeClosurePush(roots) => {
                // Host-executed: push the host store → sandbox store over the WSS
                // ssh tunnel (host's `nix copy --to ssh-ng://`). Best-effort — a
                // failure leaves the rc to source whatever the sandbox can resolve;
                // it must not abort provisioning, so warn + continue.
                match sprite_ssh_connect(cfg, worktree) {
                    Some((key, user, _workdir)) => {
                        let exe = std::env::current_exe()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|_| "thegn".into());
                        if let Err(e) = push_home_closure_p2p(&exe, worktree, &key, &user, roots) {
                            thegn_core::msg::warn(&format!(
                                "host-parity p2p: pushing the home closure to the sandbox \
                                 failed: {e}; the shell will use whatever the sandbox can \
                                 resolve. Ensure the sandbox was (re)created with connect=ssh."
                            ));
                        }
                        Ok(())
                    }
                    None => {
                        thegn_core::msg::warn(
                            "host-parity p2p: connect=ssh is required to push the home \
                             closure but the ssh tunnel is unavailable; skipping.",
                        );
                        Ok(())
                    }
                }
            }
        };

        if let Err(e) = result {
            // A pure pre-warm (devShell/cache/direnv-allow) that failed is invisible
            // to the user — the pane rebuilds it lazily — so don't alarm with a red
            // `Failed` row (its usual failure is an OOM/timeout on a heavy Nix build
            // in a pooled microVM). Mark it done with a "finishes in the shell" hint.
            let warm_only = crate::provision_recover::step_is_warm_only(&step.id);
            views[i].state = if warm_only {
                ProvisionState::Done
            } else {
                ProvisionState::Failed
            };
            views[i].detail = Some(if warm_only {
                "deferred — the dev shell builds on first entry".to_string()
            } else {
                crate::provision_recover::sanitize_detail(&e.to_string())
            });
            progress(&views);
            // Only the essential steps (the worktree dir + clone + nix) abort
            // creation. The rest — warming the devShell/direnv, personal tools,
            // dotfiles, the home-parity closure — are BEST-EFFORT: the shell still
            // comes up and these resolve lazily in the pane. A best-effort failure
            // warns + continues so one flaky `nix develop` can't kill the sandbox.
            if crate::provision_recover::step_is_fatal(&step.id) {
                tracing::warn!(target: "thegn::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, error = %e, "provision step failed (fatal — aborting)");
                return Err(e);
            }
            tracing::warn!(target: "thegn::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, error = %e, "provision step failed (best-effort — continuing)");
            // If the step likely restarted the sandbox VM (an OOM-killed Nix
            // install, exit 137), give it a bounded window to come back before
            // the next step — otherwise every remaining step burns its own
            // connect budget failing `exec ws connect` against a booting VM.
            if crate::provision_recover::step_signals_sandbox_restart(&e.to_string()) {
                tracing::warn!(target: "thegn::startup", step = %step.id, "step may have restarted the sandbox; waiting for it to become ready before continuing");
                let _ = block_on_provider(|| async {
                    provider
                        .wait_ready(&id, std::time::Duration::from_secs(90))
                        .await
                });
            }
            continue;
        }
        tracing::info!(target: "thegn::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, "provision step done");
        views[i].state = ProvisionState::Done;
        progress(&views);
    }

    // Drop the marker (+ local mirror for the attach gate) so a later open skips it.
    let _ = block_on_provider(|| async { provider.write(&id, &marker, b"ok\n").await });
    crate::provider_workdir::mark_provisioned(&id);
    // Record the provisioned-base checkpoint per (repo, env), keyed by the
    // flake.lock hash so a lockfile change invalidates it (see env_base_snapshots).
    if let Some(cp) = &base_checkpoint {
        let lock = crate::provision_gate::flake_lock_hash(&repo_root);
        if let Ok(db) = Db::open() {
            // best-effort: the DB is a cache
            let _ = db.set_base_snapshot(&repo_root.to_string_lossy(), env_name, cp, &lock);
        }
    }
    Ok((true, base_checkpoint))
}

pub(crate) use crate::agent_home::{resolve_personal_dotfiles, resolve_setup};

/// Pure: the `nix copy` argv to push the closure of `roots` to a binary cache.
/// (Split out so the command shape is unit-testable without invoking `nix`.)
fn nix_copy_argv(cache_url: &str, roots: &[String]) -> Vec<String> {
    let mut argv = vec![
        "copy".to_string(),
        "--to".to_string(),
        cache_url.to_string(),
    ];
    argv.extend(roots.iter().cloned());
    argv
}

/// Host-side host-parity push: copy the closure of the host `/nix/store` `roots`
/// the user's dotfiles reference to `cache_url`, so a sandbox can substitute them
/// from there. Runs the host's `nix` (the host has the closure + a writable
/// store). Best-effort — returns the error for the caller to warn on. `nix copy`
/// closes over each root's full runtime closure automatically.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn push_home_closure(cache_url: &str, roots: &[String]) -> anyhow::Result<()> {
    let argv = nix_copy_argv(cache_url, roots);
    let out = std::process::Command::new("nix")
        .args(&argv)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn `nix copy`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "nix copy exit {}: {}",
            out.status.code().unwrap_or(-1),
            tail_lines(&String::from_utf8_lossy(&out.stderr), 4)
        ))
    }
}

/// Pure: the `nix copy` argv for a **p2p** push straight into the sandbox's own
/// store over the WSS ssh tunnel (no hosted cache — the host store is the source).
/// `--no-check-sigs` (the host is trusted) and `--substitute-on-destination` (let
/// the sandbox fill public paths from its own substituters in parallel). The ssh
/// transport (key, port, ProxyCommand) is supplied via `NIX_SSHOPTS`.
fn nix_copy_p2p_argv(user: &str, roots: &[String]) -> Vec<String> {
    let mut argv = vec![
        "copy".to_string(),
        "--to".to_string(),
        format!("ssh-ng://{user}@sprite"),
        "--no-check-sigs".to_string(),
        "--substitute-on-destination".to_string(),
    ];
    argv.extend(roots.iter().cloned());
    argv
}

/// Pure: truncate a `/nix/store/<hash>-<name>/...` path to its top-level store
/// path (`/nix/store/<hash>-<name>`), which is what `nix copy` / `nix profile
/// install` accept. `None` for non-store paths.
fn store_root_of(p: &str) -> Option<String> {
    let rest = p.strip_prefix("/nix/store/")?;
    let entry = rest.split('/').next()?;
    (!entry.is_empty()).then(|| format!("/nix/store/{entry}"))
}

/// Resolve the host `/nix/store` roots for the user's interactive shell + the
/// ubiquitous prompt tools, so a host-parity p2p push carries the binaries
/// themselves (not just what the rc sources) and they can be `nix profile
/// install`ed by name in the sandbox. Host-only + best-effort (`command -v` →
/// canonicalize → store-root); non-store / missing tools are skipped.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn host_shell_store_roots() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(sh) = std::env::var("SHELL")
        && let Some(n) = Path::new(&sh).file_name().and_then(|s| s.to_str())
    {
        names.push(n.to_string());
    }
    for t in ["zsh", "starship", "atuin", "direnv", "fzf"] {
        if !names.iter().any(|s| s == t) {
            names.push(t.to_string());
        }
    }
    let mut roots: Vec<String> = Vec::new();
    for t in names {
        let Ok(out) = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {t}"))
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if path.is_empty() {
            continue;
        }
        if let Ok(real) = std::fs::canonicalize(&path)
            && let Some(root) = store_root_of(&real.to_string_lossy())
            && !roots.contains(&root)
        {
            roots.push(root);
        }
    }
    roots
}

/// Write a tiny no-arg wrapper script for ssh's `ProxyCommand` next to the managed
/// key. `NIX_SSHOPTS` is whitespace-split by `nix`, so a space-bearing
/// ProxyCommand (`<thegn> sprite-proxy <wt>`) can't go inline — the wrapper is a
/// single token. Returns its path.
fn write_proxy_wrapper(key: &Path, proxy_cmd: &str) -> anyhow::Result<PathBuf> {
    let dir = key.parent().unwrap_or_else(|| Path::new("."));
    let script = dir.join("nix-copy-proxy.sh");
    std::fs::write(&script, format!("#!/bin/sh\nexec {proxy_cmd}\n"))?;
    crate::managed_tool::make_executable(&script)?;
    Ok(script)
}

/// Host-side host-parity **p2p** push: copy the closure of `roots` straight into
/// the sandbox's store over the WSS ssh tunnel (the `sprite-proxy` ProxyCommand),
/// using the host's `nix`. No hosted cache or signing key — the host store is the
/// source. Best-effort; returns the error for the caller to warn on. Requires the
/// sandbox to have been (re)created with `connect = "ssh"` (so its sshd accepts
/// the managed key) and `nix` installed in it.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn push_home_closure_p2p(
    thegn_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    roots: &[String],
) -> anyhow::Result<()> {
    let proxy = format!(
        "{} sprite-proxy {}",
        thegn_core::util::sh_quote(thegn_exe),
        thegn_core::util::sh_quote(worktree),
    );
    let proxy_script = write_proxy_wrapper(key, &proxy)?;
    let ssh_opts = format!(
        "-o ProxyCommand={} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -o LogLevel=ERROR -o ConnectTimeout=10 -o ServerAliveInterval=10 \
         -o ServerAliveCountMax=3 -i {} -p {}",
        thegn_core::util::sh_quote(&proxy_script.to_string_lossy()),
        thegn_core::util::sh_quote(&key.to_string_lossy()),
        SPRITE_SSHD_PORT,
    );
    let argv = nix_copy_p2p_argv(user, roots);
    // HARD timeout: this is a blocking host-side call on the provisioning path. If
    // the sandbox sshd isn't reachable yet (fresh sprite) or the closure is huge,
    // `nix copy` would otherwise hang the loading screen indefinitely. `timeout`
    // (coreutils) bounds it; `--kill-after` force-kills the ssh/ProxyCommand
    // children. On timeout the caller warns + continues (best-effort), so the
    // shell still opens — exact-parity is a nice-to-have, never a blocker.
    let out = std::process::Command::new(timeout_bin()?)
        .arg("--kill-after=5")
        .arg(HOME_CLOSURE_PUSH_TIMEOUT_SECS.to_string())
        .arg("nix")
        .args(&argv)
        .env("NIX_SSHOPTS", ssh_opts)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn `nix copy` (p2p): {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let code = out.status.code().unwrap_or(-1);
    // `timeout` exits 124 when the deadline fired.
    let why = if code == 124 {
        format!(
            "timed out after {HOME_CLOSURE_PUSH_TIMEOUT_SECS}s (sandbox sshd not reachable, or closure too large)"
        )
    } else {
        format!(
            "exit {code}: {}",
            tail_lines(&String::from_utf8_lossy(&out.stderr), 6)
        )
    };
    Err(anyhow::anyhow!("nix copy (p2p) {why}"))
}

/// Hard ceiling for the host-side p2p closure push, in seconds. Past this the
/// step is abandoned (best-effort) so the first shell is never held hostage to a
/// slow/unreachable transfer.
const HOME_CLOSURE_PUSH_TIMEOUT_SECS: u32 = 75;

/// Ceiling (seconds) for each host-side `nix` invocation in the devShell push
/// (build+gcroot, then the `file://` copy). Generous: instant for a nix user who
/// already has the devShell, but a cold host build can take a while.
const DEVSHELL_PUSH_NIX_TIMEOUT_SECS: u32 = 600;

/// Pure: the `nix develop <ref> --profile <gcroot> --command true` argv — builds
/// the repo's devShell on the HOST and pins it behind a gcroot (so the copy can't
/// race nix GC). Instant when the devShell is already built locally. `attr`
/// selects the devShell (`<repo>#<attr>`, e.g. the lean `sandbox`); empty ⇒ the
/// flake default — matching what the sandbox `.envrc` will enter.
fn nix_develop_profile_argv(repo_root: &str, gcroot: &str, attr: &str) -> Vec<String> {
    let reference = if attr.trim().is_empty() {
        repo_root.to_string()
    } else {
        format!("{repo_root}#{}", attr.trim())
    };
    vec![
        "develop".into(),
        reference,
        "--profile".into(),
        gcroot.into(),
        "--command".into(),
        "true".into(),
    ]
}

/// Pure: `nix copy --to file://<dir> --no-check-sigs <path>` — write a
/// self-contained binary cache of `path`'s closure to a host dir for transfer.
fn nix_copy_to_file_argv(cache_dir: &str, path: &str) -> Vec<String> {
    vec![
        "copy".into(),
        "--to".into(),
        // `compression=zstd`: the cache is built then mostly PRUNED (rust + public
        // paths dropped), so we'd otherwise burn minutes xz-compressing ~600MB we
        // immediately delete. zstd is ~100x faster to compress (the discarded bulk
        // is then nearly free) and the kept paths still ship small. Modern nix on
        // the sandbox reads zstd NARs fine.
        format!("file://{cache_dir}?compression=zstd"),
        "--no-check-sigs".into(),
        path.into(),
    ]
}

pub(crate) use crate::parity::sanitize_tag;

/// The coreutils `timeout` binary, under whichever name this host has it.
///
/// Stock macOS ships **neither**: `timeout` is GNU coreutils, and Homebrew
/// installs it prefixed as `gtimeout`. Spawning a bare `timeout` there fails
/// with a raw `ENOENT` that surfaces as "spawn nix: No such file or directory" —
/// which points at `nix`, the one thing that WAS installed. `test/lib/pty.sh`
/// already resolves this pair for the test harness; this is the host-side copy.
///
/// `--kill-after=5` is GNU-only too, but both `timeout` and `gtimeout` are the
/// same GNU binary, so resolving the name resolves the flag with it.
fn timeout_bin() -> anyhow::Result<&'static str> {
    for bin in ["timeout", "gtimeout"] {
        if thegn_core::util::have(bin) {
            return Ok(bin);
        }
    }
    // Deliberately an error rather than running unbounded: these are blocking
    // host-side calls on the provisioning path, and an unbounded one hangs the
    // loading screen with no way out. Name the remedy instead.
    anyhow::bail!(
        "no `timeout` binary (GNU coreutils) on PATH — install coreutils \
         (macOS: `brew install coreutils` provides `gtimeout`)"
    )
}

/// Run a host `nix` subcommand bounded by `timeout` (coreutils). `Ok(output)` on
/// success; `Err` with a tail of stderr (or "timed out") otherwise.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn run_host_nix_timeout(secs: u32, argv: &[String]) -> anyhow::Result<std::process::Output> {
    let out = std::process::Command::new(timeout_bin()?)
        .arg("--kill-after=5")
        .arg(secs.to_string())
        .arg("nix")
        .args(argv)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn nix: {e}"))?;
    if out.status.success() {
        return Ok(out);
    }
    let code = out.status.code().unwrap_or(-1);
    let why = if code == 124 {
        format!("timed out after {secs}s")
    } else {
        format!("exit {code}")
    };
    Err(anyhow::anyhow!(
        "nix {} {}: {}",
        argv.first().map(String::as_str).unwrap_or("?"),
        why,
        tail_lines(&String::from_utf8_lossy(&out.stderr), 4)
    ))
}

/// Host-side devShell speedup: build the repo's devShell on the HOST (instant for a
/// nix user who already has it), serialize its closure to a `file://` binary cache,
/// upload that cache into the sandbox, and import it there — so the in-sandbox
/// devShell warm is a local store hit instead of a rebuild. Best-effort; the host
/// `nix` steps are timeout-bounded. Requires the sandbox store to be writable (the
/// `nix` step's `claim_store` ran first) + `nix` on the sandbox PATH.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
fn push_devshell_closure(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    repo_root: &Path,
    workdir: &str,
    devshell_attr: &str,
) -> anyhow::Result<()> {
    let repo = repo_root.to_string_lossy().into_owned();
    if repo.trim().is_empty() {
        return Err(anyhow::anyhow!("no repo root to build the devShell from"));
    }
    let tag = sanitize_tag(id);
    let tmp = std::env::temp_dir();
    let gcroot = tmp.join(format!("sz-devshell-gc-{tag}-{}", std::process::id()));
    let cache = tmp.join(format!("sz-devshell-cache-{tag}-{}", std::process::id()));
    let cache_str = cache.to_string_lossy().into_owned();
    let gcroot_str = gcroot.to_string_lossy().into_owned();

    // 1. Build + pin the devShell on the host (instant if already built). Build
    //    the SAME attr the sandbox will enter, so the seeded paths match.
    run_host_nix_timeout(
        DEVSHELL_PUSH_NIX_TIMEOUT_SECS,
        &nix_develop_profile_argv(&repo, &gcroot_str, devshell_attr),
    )?;
    // 2. Resolve the devShell store path (what the sandbox must import). Bounded
    //    like the sibling nix calls: a wedged daemon would hang path-info forever.
    let pi = run_host_nix_timeout(
        DEVSHELL_PUSH_NIX_TIMEOUT_SECS,
        &["path-info".to_string(), gcroot_str.clone()],
    )?;
    let store_path = String::from_utf8_lossy(&pi.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    // 3. Serialize the closure to a host file:// cache.
    let copy_res = run_host_nix_timeout(
        DEVSHELL_PUSH_NIX_TIMEOUT_SECS,
        &nix_copy_to_file_argv(&cache_str, &gcroot_str),
    );
    // 4. Upload the cache into the sandbox + realise it there, then clean up.
    let result = (|| -> anyhow::Result<()> {
        copy_res?;
        if store_path.is_empty() {
            return Err(anyhow::anyhow!("could not resolve the devShell store path"));
        }
        // SCOPE the push: drop every NAR cache.nixos.org already serves so the
        // upload carries only the paths public caches lack (the repo's from-source
        // builds + rust-overlay output) — far smaller than the full closure. The
        // sandbox fills the pruned paths from cache.nixos.org when it realises.
        // Best-effort: if pruning fails we just upload the full (correct) cache.
        if let Err(e) = prune_cache_to_public(&cache_str) {
            thegn_core::msg::warn(&format!(
                "devshell push: cache pruning skipped ({e}); uploading the full closure."
            ));
        }
        let dest = "/tmp/sz-devshell-cache";
        with_provision_timeout(
            "devshell cache upload",
            provision_step_timeout("devshell"),
            || provider.upload_dir(id, &cache, dest),
        )?;
        // `nix` is on PATH after `claim_store`. Realise the devShell via an
        // EVAL-based `nix develop` in the worktree, with the uploaded cache as an
        // extra substituter. This resolves the full closure from three sources at
        // once: our seeded paths (the repo's from-source builds) from the local
        // file:// cache, the rust toolchain rebuilt from the upstream Rust CDN
        // (its derivation, since we pruned it from the upload), and everything else
        // from cache.nixos.org. Eval-based (not `nix-store -r <path>`) so nix has
        // the derivations to build the pruned rust paths. Unsigned file:// paths are
        // fine — the sandbox user owns the store (single-user). Then reclaim /tmp.
        // Enter the SAME devShell attr the seed built (and the in-pane `.envrc`
        // will use) — `.#<attr>` for the lean sandbox shell, bare for the default.
        let dev_ref = if devshell_attr.is_empty() {
            String::new()
        } else {
            format!(".#{devshell_attr} ")
        };
        let import = format!(
            "export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
             cd {workdir} 2>/dev/null || exit 1; \
             nix develop {dev_ref}--command true --option extra-substituters file://{dest} \
             --option require-sigs false 2>&1; rc=$?; \
             rm -rf {dest}; exit $rc"
        );
        let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), import];
        let (code, out) = with_provision_timeout(
            "devshell realise",
            provision_step_timeout("devshell"),
            || provider.run_exec(id, &argv, None, &[]),
        )?;
        if code != 0 {
            return Err(anyhow::anyhow!(
                "sandbox realise (exit {code}): {}",
                tail_lines(&out, 4)
            ));
        }
        Ok(())
    })();
    // Host cleanup (best-effort): the gcroot symlink + the cache dir.
    let _ = std::fs::remove_dir_all(&cache);
    let _ = std::fs::remove_file(&gcroot);
    result
}

/// Prune a host `file://` binary cache down to ONLY the paths the sandbox can't
/// get cheaply elsewhere — the repo's own from-source builds (muse/openspec/…) —
/// so the scoped devShell push uploads ~tens of MB instead of the whole (multi-
/// hundred-MB) closure. Two passes drop what the sandbox can get cheaply itself:
/// the **rust-overlay toolchain** (rustc/cargo/rust-std/clippy/…), which the
/// sandbox rebuilds from the upstream Rust CDN (static.rust-lang.org) on its own
/// fast downstream — far quicker than shipping ~300MB over the host's upstream
/// (this is the bulk of a rust devShell) — and every path **cache.nixos.org**
/// already serves (a quick HEAD on `/<hash>.narinfo`), which the sandbox
/// substitutes from there. Best-effort (a missing tool / network blip just leaves
/// more in the cache — still correct, just larger); bounded so it can't wedge a
/// provision.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn prune_cache_to_public(cache_dir: &str) -> anyhow::Result<()> {
    // POSIX sh. Pass 1 is name-based (rust toolchain); pass 2 is a parallel
    // (`xargs -P`) HEAD against cache.nixos.org. `$1` is the cache dir.
    let script = r#"cd "$1" 2>/dev/null || exit 0
# Pass 1: drop rust-overlay toolchain paths (sandbox fetches them from the Rust CDN).
for ni in *.narinfo; do
  [ -e "$ni" ] || continue
  sp=$(sed -n 's/^StorePath: //p' "$ni"); name=${sp##*/}; name=${name#*-}
  case "$name" in
    rustc-*|cargo-*|rust-std-*|rust-docs-*|rust-default-*|rust-src-*|rust-analyzer*|clippy-preview-*|rustfmt-preview-*|llvm-tools-preview-*)
      nar=$(sed -n 's/^URL: //p' "$ni"); rm -f "$ni" "$nar" ;;
  esac
done
# Pass 2: drop paths cache.nixos.org already serves (parallel HEAD).
ls *.narinfo 2>/dev/null | xargs -P 16 -n1 sh -c '
  ni=$0; h=${ni%.narinfo}
  if curl -sfI --max-time 4 "https://cache.nixos.org/$h.narinfo" >/dev/null 2>&1; then
    nar=$(sed -n "s/^URL: //p" "$ni")
    rm -f "$ni" "$nar"
  fi
'
exit 0"#;
    let out = std::process::Command::new(timeout_bin()?)
        .arg("--kill-after=5")
        .arg("180")
        .arg("sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(cache_dir)
        .output()
        .map_err(|e| anyhow::anyhow!("spawn prune: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("exit {}", out.status.code().unwrap_or(-1)))
    }
}

/// Host dotfiles to carry into a sandbox `$HOME` so the shell feels like home.
/// Only those that exist on the host are uploaded (see [`upload_dotfiles`]).
pub(crate) fn default_dotfiles() -> Vec<String> {
    [
        ".gitconfig",
        ".zshrc",
        ".bashrc",
        ".profile",
        ".tmux.conf",
        ".vimrc",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// `upload_dotfiles` + `upload_atuin_creds` live in the `agent_configs` sibling
// (file-size ratchet); the provision loop calls them via these re-exports.
pub(crate) use crate::agent_configs::{upload_atuin_creds, upload_dotfiles};

/// Last `n` non-empty lines of command output, for a compact error message.
fn tail_lines(out: &str, n: usize) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

// `env_halt_reason` (cheap pre-spawn HALT check) lives in the `env_halt` sibling.
pub use crate::env_halt::env_halt_reason;

/// Idempotently install the resident bridge binary into a provider env so a
/// `Placement::Provider` bridge connect finds it at `remote_path`. Content-
/// addressed handshake (push only on fingerprint mismatch). Best-effort and
/// off-loop (its own runtime via `block_on_provider`): a failure warns and leaves
/// the per-op git path as the fallback. No-op for envs without a file-capable
/// provider. Called from `run.rs::connect_worktree_bridge` before `sup.connect`.
pub fn ensure_remote_bridge(cfg: &Config, env_name: &str, binary: &Path, remote_path: &str) {
    let Some((provider, id, _)) = provider_sync_target(cfg, env_name) else {
        return;
    };
    let data = match std::fs::read(binary) {
        Ok(d) => d,
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "bridge binary unreadable ({}): {e}",
                binary.display()
            ));
            return;
        }
    };
    match block_on_provider(|| async { provider.ensure_executable(&id, remote_path, &data).await })
    {
        Ok(true) => thegn_core::msg::info(&format!(
            "pushed resident bridge → {id}:{remote_path} ({} bytes)",
            data.len()
        )),
        Ok(false) => {} // already current — no re-push
        Err(e) => thegn_core::msg::warn(&format!("bridge binary push failed: {e}")),
    }
}

/// Whether a worktree's persisted remote `location` should be healed back to
/// local. True only when a provider env degraded to the host THIS open
/// (`degraded`) AND the stored row still carries a non-empty (remote) location.
/// Crucially gated on `degraded`: a genuine ssh/k8s worktree also resolves with
/// `outcome.location == None` yet legitimately keeps its remote location, so it
/// must never be clobbered here. Pure.
pub(crate) fn should_heal_degraded_location(degraded: bool, current: Option<&str>) -> bool {
    degraded && current.is_some_and(|l| !l.is_empty())
}

/// Warm-on-open (8-E): for a provider env with `auto_provision`, create the
/// sandbox if it doesn't exist yet (API providers — CLI providers use
/// `up_command`/`placement.ensure`). Runs off-loop. Returns `Err` if the provider
/// rejected the request (e.g. a bad/expired token → 401); the caller decides
/// whether that halts (failover off) or just warns (failover on). `Ok(())` on
/// success, already-exists, or a no-op (not a provider / `auto_provision` off).
fn auto_provision_sandbox(cfg: &Config, env_name: &str, worktree: &str) -> anyhow::Result<()> {
    let Some(env) = cfg.env.get(env_name) else {
        return Ok(());
    };
    let pc = &env.provider;
    if !pc.auto_provision {
        return Ok(());
    }
    // Per-worktree id from the single source of truth (resolved placement).
    let Some(name) = provider_sandbox_name(cfg, worktree, env_name).filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    // Pool-aware: an UNBOUND worktree with a ready spare waiting will claim it
    // in the materialize fast path — creating the derived sandbox now would
    // just mint a bare, billed orphan (destroyed again on claim). A worktree
    // that ends up claiming nothing provisions via the full pipeline, whose own
    // `ensure_exists` recreates the sandbox.
    if let Ok(db) = Db::open()
        && db
            .worktree_provider_sandbox(worktree)
            .ok()
            .flatten()
            .is_none()
        && let Some(root) = db.repo_root_for(worktree).ok().flatten()
        && db
            .pool_spares_for(&root, env_name)
            .map(|v| v.iter().any(|s| s.state == "ready"))
            .unwrap_or(false)
    {
        return Ok(());
    }
    // Bake the RESOLVED name so `ensure_exists`→`create` names the new sandbox
    // correctly (the raw `pc.id` is a template + embeds a path-hash).
    //
    // Reaching here means the placement already resolved to `Provider` (gated by
    // the caller), `auto_provision` is on (checked above), a real sandbox name
    // resolved, and no pool spare is waiting — so a `None` provider genuinely
    // means "the provider config is present but couldn't be built" (unresolved
    // token/keypair), NOT a legitimate no-op. Surface it as an error so the
    // caller's failover block (`prepare_sandbox_env`) halts or degrades-with-
    // notification instead of silently opening a host/bwrap shell.
    let Some(provider) = provider_for_named(pc, &name) else {
        anyhow::bail!(
            "env '{env_name}' provider '{}' could not be built (token or managed keypair unresolved)",
            pc.provider
        );
    };
    match block_on_provider(|| async { provider.ensure_exists(&name).await }) {
        Ok(true) => {
            thegn_core::msg::info(&format!("provisioned sandbox {name}"));
            Ok(())
        }
        Ok(false) => Ok(()), // already exists
        Err(e) => Err(e),
    }
}

/// Provision a fresh provider env's repo on open (8-A.3): clone the local repo's
/// `origin` into the worktree dir *inside the env* via the control-plane exec
/// (`GitLoc::sh_command`, which `cd`s into the workdir). Idempotent — the script
/// no-ops once the dir is a git repo, including after a `data=sync` upload (which
/// already lands a `.git`). Best-effort + blocking on the off-loop launch path:
/// the clone is the inherent first-open cost; a failure warns and leaves the env
/// as-is (the chrome just shows an empty tree until it succeeds). No-op when the
/// local repo has no `origin`.
// off-loop by contract: runs inside launch_spec_with_key, which is documented
// blocking and must be called off the event loop (materialize spawn_blocking /
// CLI). NOTE: some direct pane-spawn helpers still call launch_spec on the
// loop — see the sweep report; the fix belongs at those callers.
#[expect(clippy::disallowed_methods)]
fn provision_provider_repo(repo_root: &Path, loc: &GitLoc, branch: Option<&str>) {
    let Some(origin) = local_origin(repo_root) else {
        return;
    };
    let script = thegn_core::remote::provision_repo_script(&origin, branch);
    // Prefer the resident bridge (CLI-free) when it's already up for this loc;
    // else fall back to the per-op CLI control prefix.
    if let Some(b) = thegn_svc::bridge::for_loc(loc) {
        match b.exec(&["/bin/sh", "-lc", &script], Some(&loc.path()), &[]) {
            Ok(r) if r.exit == 0 => return,
            Ok(r) => {
                thegn_core::msg::warn(&format!(
                    "provider repo provision failed (exit {}): {}",
                    r.exit,
                    crate::provision_recover::stderr_gist(&r.stderr)
                ));
                return;
            }
            // Bridge hiccup — fall through to the CLI path below.
            Err(e) => thegn_core::msg::warn(&format!("provider repo provision via bridge: {e}")),
        }
    }
    match loc.sh_command(&script).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => thegn_core::msg::warn(&format!(
            "provider repo provision failed ({}): {}",
            o.status,
            crate::provision_recover::stderr_gist(&String::from_utf8_lossy(&o.stderr))
        )),
        Err(e) => thegn_core::msg::warn(&format!("provider repo provision spawn failed: {e}")),
    }
}

/// The local repo's `origin` remote URL, or `None` (no remote / not a repo).
// off-loop by contract: only called from the blocking provisioning path
// (provision_provider_env_named / launch_spec_with_key) — see note above.
#[expect(clippy::disallowed_methods)]
fn local_origin(repo_root: &Path) -> Option<String> {
    let out = thegn_core::util::git_cmd(repo_root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// `(provider, sandbox_id, workdir)` for a provider env that supports file-sync,
/// or `None` (unconfigured / no id / no token / provider can't do files).
fn provider_sync_target(
    cfg: &Config,
    env_name: &str,
) -> Option<(thegn_svc::provider::Provider, String, String)> {
    let pc = &cfg.env.get(env_name)?.provider;
    let id = pc.id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let provider = provider_for(pc)?;
    if !provider.caps().files {
        return None;
    }
    let workdir = crate::provider_workdir::resolve(pc, &id);
    Some((provider, id, workdir))
}

/// Run an async provider call to completion on a fresh OS thread with its own
/// tokio runtime — safe to call from any context (no nested-runtime panic), and
/// blocking from the caller's view (used on the off-loop prepare/close paths).
pub(crate) fn block_on_provider<T, Fut>(f: impl FnOnce() -> Fut + Send) -> anyhow::Result<T>
where
    T: Send,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    std::thread::scope(|s| {
        s.spawn(|| {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))?;
            rt.block_on(f())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("provider sync thread panicked"))?
    })
}

/// In-process registry of worktrees with a live provider file-sync (path →
/// env name), so the close thread can pull changes back without re-resolving.
fn provider_sync_registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>>
{
    static REG: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn register_provider_sync(worktree: &str, env_name: &str) {
    if let Ok(mut r) = provider_sync_registry().lock() {
        r.insert(worktree.to_string(), env_name.to_string());
    }
}

/// On worktree close, pull the sandbox fs back into the local worktree for a
/// provider `data = "sync"` env. Best-effort; a no-op when nothing was synced.
pub(crate) fn deprovision_sync(path: &str) {
    let env_name = provider_sync_registry()
        .lock()
        .ok()
        .and_then(|mut r| r.remove(path));
    let Some(env_name) = env_name else {
        return;
    };
    let cfg = Config::load_layered(&thegn_core::config::ProcessEnv, &[], None);
    let Some((provider, id, workdir)) = provider_sync_target(&cfg, &env_name) else {
        return;
    };
    let p = path.to_string();
    let _ = block_on_provider(|| async {
        provider
            .download_dir(&id, &workdir, std::path::Path::new(&p))
            .await
    });
}

/// Pure composition of the final [`LaunchSpec`] from a settled sandbox: argv
/// (sandbox-wrapped, or a bare login shell on the host fallback), cwd, env,
/// plus the effective backend label and any fallback warnings.
pub fn compose_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    loc: &GitLoc,
    sb: &SandboxOutcome,
) -> LaunchSpec {
    // If the resolved env's sandbox config has an explicit shell override, use
    // it for shell panes. Empty string = resolve from host $SHELL (the default).
    let sb_shell = sb.shell.trim().to_string();
    // Use the probe-chain + devShell entry (`shell_inner(true)`) whenever the pane
    // runs inside ANY sandbox spec — OCI container, bwrap, or a remote/provider
    // sprite. In all of these the host's absolute `$SHELL` path (or `$SHELL`
    // itself, in a sprite) isn't the user's zsh, so the bare `${SHELL:-/bin/sh}
    // -l` form fell through to `/bin/sh` (`sh-5.3$`) and never entered the flake
    // devShell. The probe chain finds zsh (from the injected/loaded devShell) and
    // the snippet loads the toolchain. Only a BARE-HOST pane (no sandbox spec,
    // `backend = none` local) keeps `${SHELL} -l` — there `$SHELL` is the user's
    // real zsh and the login files load the devShell via the rc-hook.
    let in_oci = sb.spec.is_some();
    let cmd = if choice == "clean-shell" {
        // Watchdog fallback: a plain rc-free shell. Ignores any `[sandbox] shell`
        // override on purpose — the override is part of what may be hanging.
        clean_shell_inner()
    } else if choice == "shell" && !sb_shell.is_empty() {
        shell_inner_override(&sb_shell)
    } else if choice == "shell" {
        shell_inner(in_oci)
    } else {
        resolve_command(cfg, choice)
    };
    // A provider pane lands at $HOME; prefix `cd <workdir>` so direnv/devShell load.
    let placement = sb.spec.as_ref().map(|s| &s.placement);
    let cmd = crate::provider_workdir::with_workdir_cd(choice, placement, cmd);
    // A projected data mode (sshfs/sync) pins the pane to its local mountpoint;
    // otherwise a local worktree runs in its own dir and a remote/provider one
    // has none — the placement cd's on the target — so the pane cwd stays unset.
    let cwd = sb
        .cwd_override
        .clone()
        .or_else(|| (!loc.is_remote() && !sb.is_remote).then(|| PathBuf::from(worktree)));
    let mut env = vec![
        ("THEGN_WORKTREE".to_string(), worktree.to_string()),
        (
            "THEGN_BRANCH".to_string(),
            branch.unwrap_or_default().to_string(),
        ),
    ];
    // Local bwrap gets its passthrough env (tokens, API keys) via the pane's
    // process env, not world-readable `--setenv` argv (enter_argv skips those).
    if let Some(spec) = &sb.spec
        && spec.backend == sandbox::Backend::Bwrap
        && spec.placement.is_local()
    {
        env.extend(spec.env.iter().cloned());
    }
    let argv = match &sb.spec {
        Some(spec) => sandbox::enter_argv(spec, &cmd),
        // Host fallback: run the command through a login shell so PATH/env expand.
        None => vec![thegn_core::util::shell(), "-lc".to_string(), cmd],
    };
    // The label must describe the argv, not the resolver's intent. For a LOCAL
    // placement the argv is authoritative, so reconcile against it: a resolver
    // that returned a container backend while composing a bare host shell is a
    // false containment claim, and this is where it stops being one. A remote
    // placement keeps the resolver's label — its runtime lives behind a
    // transport whose argv shape can't be read from here (see `sandbox_truth`).
    let local = sb.spec.as_ref().is_none_or(|s| s.placement.is_local());
    let truth = local.then(|| thegn_core::sandbox_truth::reconcile(&sb.backend_label, &argv));
    let mut warnings = sb.warnings.clone();
    let mut degraded = sb.degraded_from_provider;
    let backend = match truth {
        Some(t) => {
            if let Some(w) = t.warning {
                thegn_core::msg::warn(&w);
                warnings.push(w);
                degraded = true;
            }
            t.label
        }
        None => sb.backend_label.clone(),
    };
    LaunchSpec {
        argv,
        cwd,
        env,
        backend,
        warnings,
        degraded,
    }
}

/// Compose the [`LaunchSpec`] for running `choice` in `worktree`. Records the
/// choice (and any sandbox backend) in the DB, mirroring the zellij path's
/// side effects so the dashboard/`--resume` keep working. Errors when an
/// explicit sandbox choice cannot be honored (no silent host fallback).
///
/// `branch` is the worktree's branch (for the pane env + title); `None` falls
/// back to the worktree basename.
pub fn launch_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    // `daemon_persistent = false`: the convenience path serves the tool drawer,
    // one-shot CLI shells, and tests — in-process panes that keep the bwrap
    // `--die-with-parent` guard. Daemon-routed center tabs go through
    // `launch_spec_center` / `launch_spec_synced` / `terminal_launch_spec`,
    // which resolve the flag from the live daemon route.
    launch_spec_full(cfg, worktree, branch, choice, false, false)
}

/// [`launch_spec`] for a **daemon-routed center pane resolved ON the loop** —
/// a split (`spawn_worktree_shell_pane`) or the startup watchdog's clean-shell
/// fallback. Identical to `launch_spec` (async direnv warm, safe on the loop)
/// except that it marks the sandbox spec pane-daemon-owned when the daemon
/// route is active, so a local bwrap pane drops `--die-with-parent`.
///
/// That guard is `prctl(PR_SET_PDEATHSIG)`, keyed to the *thread* that forked
/// bwrap, not the process — on a daemon-owned pane it reaps a shell that is
/// supposed to survive, which is exactly the "switched away, came back, my
/// terminal started over" failure. `launch_spec` (⇒ `daemon_persistent =
/// false`) is correct only for panes that really do die with the compositor.
///
/// The off-loop sibling is [`crate::direnv_warm::launch_spec_synced`]; both
/// resolve the flag from the one source of truth,
/// [`crate::handlers::startup::daemon_active`].
pub fn launch_spec_center(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    let daemon_persistent = crate::handlers::startup::daemon_active(cfg);
    launch_spec_full(cfg, worktree, branch, choice, false, daemon_persistent)
}

/// Like [`launch_spec`] but with the full set of launch knobs.
///
/// `sync_warm` gates the `direnv` cache warm: `false` kicks the async
/// background warm (the first launch of a cold worktree falls back), `true`
/// warms synchronously + bounded before composing the spec (off-loop callers
/// only — see [`crate::direnv_warm::launch_spec_synced`]).
///
/// `daemon_persistent` marks the resolved sandbox spec as pane-daemon-owned so
/// a local bwrap pane drops `--die-with-parent` and survives UI detach instead
/// of being reaped with its forking thread. Set `true` for daemon-routed center
/// tabs, `false` for ephemeral in-process panes (drawer/CLI).
pub fn launch_spec_full(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    sync_warm: bool,
    daemon_persistent: bool,
) -> anyhow::Result<LaunchSpec> {
    let loc = GitLoc::for_worktree(Path::new(worktree));

    // One DB handle for the whole spec resolution (each open re-runs pragmas).
    let db = Db::open().ok();

    // Record the choice for the dashboard / `--resume` (keyed by worktree path).
    // Two launches are deliberately NOT recorded as the worktree's remembered
    // agent: the transient `clean-shell` watchdog fallback (the user may fix
    // their dotfiles), and tool drawers (yazi/lazygit/editor/diff) — those are
    // overlays, not the worktree's agent, and are auto-prewarmed on every switch,
    // so recording them would clobber the real choice on every worktree.
    let saved_backend = db.as_ref().and_then(|db| {
        if choice != "clean-shell" && cfg.tool_command(choice).is_none() {
            let _ = db.set_worktree_agent(worktree, choice);
        }
        db.worktree_sandbox(worktree).ok().flatten()
    });

    // The local repo root drives the per-repo sandbox overlay + slug. Prefer the
    // DB (carries remote worktrees with no local cwd), else climb from the path.
    let repo_root: PathBuf = db
        .as_ref()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));

    // The selected execution environment: the worktree's own `env_name`, else
    // its workspace's. `resolve_env` falls through to the repo/global default.
    let selected_env = db
        .as_ref()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));

    // A recorded per-worktree backend is a deliberate override (only the wizard
    // writes it, when divergent); honour it as explicit so it sticks across
    // restarts even against a non-"auto" config. Empty/NULL → auto (re-resolve).
    let saved_is_override = saved_backend
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty() && s != "auto");
    let mut outcome = prepare_sandbox_env(
        cfg,
        &repo_root,
        worktree,
        &loc,
        saved_backend.as_deref(),
        saved_is_override,
        selected_env.as_deref(),
    )?;
    // NB: the resolved backend is intentionally NOT written back. `sandbox_backend`
    // is a deliberate-override store (mirrors `env_name`, sole writer = the
    // wizard's divergence check); auto-stamping it would pin every auto worktree.

    // Provision the repo into a fresh provider env on open (8-A.3): clone origin
    // into the sandbox workdir so the chrome's git/files show real data. `outcome
    // .location` is set only for a `Placement::Provider` env; idempotent + a
    // no-op for `data=sync` (whose upload already populated the tree). Route it
    // through the RESOLVED location (rebound into the DB first, so chrome reads
    // heal immediately) — the stale row `loc` may point at a dead provider the
    // env no longer resolves to.
    if let Some(blob) = outcome.location.as_deref() {
        let fresh = crate::provision_gate::rebind_resolved_location(db.as_ref(), worktree, blob);
        if fresh.is_remote() {
            provision_provider_repo(&repo_root, &fresh, branch);
        }
    } else if let Some(db) = db.as_ref()
        && should_heal_degraded_location(
            outcome.degraded_from_provider,
            db.location_for(worktree).ok().flatten().as_deref(),
        )
    {
        // A provider env failed to come up and `failover` dropped this open to a
        // bare host shell. A prior successful open left a remote provider blob in
        // `worktrees.location`; leaving it would make the chrome route git/fs
        // reads into the now-dead provider and the tab chip claim the pane is
        // remote while it is really on the host. Heal the row to local (empty).
        // best-effort: the DB is a cache; a missed heal self-corrects next open.
        let _ = db.set_worktree_location(worktree, "");
        thegn_core::msg::info(&format!(
            "{worktree}: provider unavailable, worktree location healed to local"
        ));
    }

    // Never drop a bare shell onto an unprovisioned provider VM (resurrect/split/respawn).
    crate::provision_gate::guard_unprovisioned_attach(
        cfg,
        worktree,
        choice,
        outcome.location.is_some(),
    )?;

    // Environment bundles (AU): resolve the active bundle(s) for this scope into
    // env overrides + credential/config-dir redirection + account selection, and
    // fold them into the sandbox spec (or, on the host fallback, the pane env
    // below). This subsumes the old inline account-switch (item 656) — a plain
    // account selection is just a bundle with no bundle bound (`compose` folds
    // the legacy per-provider active account when nothing else set it). Local
    // worktrees only — a remote agent runs where the host's cred dirs don't exist.
    let resolved = (!loc.is_remote())
        .then_some(db.as_ref())
        .flatten()
        .map(|db| {
            let slug = repo_slug(db, &repo_root);
            // At launch (off the event loop) so secret resolvers may run.
            bundle::compose_at_launch(cfg, db, worktree, slug.as_deref(), Some(choice))
        })
        .unwrap_or_default();
    // Credential/HOME dirs the child writes into must exist before launch.
    for dir in &resolved.ensure_dirs {
        let _ = std::fs::create_dir_all(dir);
    }
    // Tier-2 dotfiles: materialize each active bundle's dotfile tree into its
    // managed HOME (idempotent, off the event loop — launch_spec is blocking).
    if !loc.is_remote()
        && let Some(db) = db.as_ref()
    {
        let slug = repo_slug(db, &repo_root);
        for name in bundle::active_chain(cfg, db, worktree, slug.as_deref()) {
            if let Some(b) = cfg.bundle.get(&name)
                && let Some(spec) = &b.dotfiles
            {
                bundle::materialize_dotfiles(spec, &bundle::managed_home(&name));
            }
        }
    }
    if let Some(spec) = outcome.spec.as_mut() {
        resolved.merge_into_spec(spec);
        // Profile credential firewall (H): mount the active profile's git/gh/gpg
        // config dirs path-preservingly so the container sees the profile
        // identity its rerooted GIT_CONFIG_GLOBAL/GH_CONFIG_DIR env points at.
        // No-op on the default profile.
        for (host, ro) in thegn_core::profile::sandbox_cred_mounts() {
            if !spec.mounts.iter().any(|m| m.dest == host) {
                spec.mounts.push(sandbox::Mount {
                    dest: host.clone(),
                    host,
                    ro,
                    cache: false,
                });
            }
        }
    }

    // Shared build env (sccache / CARGO_TARGET_DIR) from `[disk]`, so an
    // interactive `cargo build` dedups compilation / shares a target across
    // worktrees. Inside a sandbox it must ride the container env (overrides +
    // unblock); on the host it rides the pane env below.
    let build_env = crate::build_cache::build_env_vars(cfg, &repo_root);

    if let Some(spec) = outcome.spec.as_mut() {
        crate::ssh_shim::apply(spec);
        // `env_overrides` exports these inside the sandbox shell (env_block would
        // *unset* them — wrong direction).
        for (k, v) in &build_env {
            spec.env_overrides.insert(k.clone(), v.clone());
        }
        // Under a read-only $HOME the pre-commit hook toolchain (prek/sccache)
        // and any out-of-tree target dir can't write their caches — `git commit`
        // hooks then die "Read-only file system" and fall back to --no-verify.
        // Overmount those caches read-write (no-op on a writable-$HOME profile).
        crate::build_cache::inject_cache_mounts(spec, cfg, &repo_root);
    }

    // Tier A: inject the repo's flake `devShell` toolchain (PATH + safe vars) so
    // the pane gets the project's linters/formatters/compilers out of the box —
    // crucial inside a sandbox, which can't reach the Nix daemon to `nix develop`
    // itself. Resolved on the host + cached; a cold cache kicks a background
    // resolve the next launch picks up. Local worktrees only (remote panes run
    // where the host store isn't mounted). See [`devenv`].
    let devshell = (cfg.sandbox.inject_devshell && !loc.is_remote() && !outcome.is_remote)
        .then(|| devenv::cached(&repo_root))
        .flatten();
    match (&devshell, outcome.spec.as_mut()) {
        (Some(dev), Some(spec)) => inject_devshell_sandbox(spec, dev),
        // No cache yet — warm it in the background for the next launch.
        (None, _) if cfg.sandbox.inject_devshell && !loc.is_remote() && !outcome.is_remote => {
            devenv::prewarm(&repo_root);
        }
        _ => {}
    }

    // Pre-warm this worktree's `direnv` cache on the host so the in-sandbox
    // direnv hook replays it read-only instead of failing on the read-only
    // `/nix/store`. Off-loop, gated by `needs_warm`; local worktrees only (a
    // remote worktree's `.envrc` isn't on this host's filesystem).
    if !loc.is_remote() && !outcome.is_remote {
        crate::direnv_warm::warm_for_launch(cfg, Path::new(worktree), sync_warm);
    }

    // Mark the resolved sandbox as pane-daemon-owned when this pane is
    // daemon-routed, so `enter_argv` drops the bwrap `--die-with-parent` guard
    // (harmless no-op for non-bwrap backends). The daemon reaps its own
    // sessions, so the guard would only reap a shell that is meant to persist.
    if let Some(spec) = outcome.spec.as_mut() {
        spec.daemon_persistent = daemon_persistent;
    }

    let mut spec = compose_spec(cfg, worktree, branch, choice, &loc, &outcome);
    // On the host path (no sandbox spec) the bundle identity + build env ride
    // the pane env (layered on the curated base in `spawn_with_env`).
    if outcome.spec.is_none() {
        spec.env.extend(resolved.env_pairs());
        spec.env.extend(build_env);
    }
    // Host (no-sandbox) devShell injection rides the pane env directly.
    if outcome.spec.is_none()
        && let Some(dev) = &devshell
    {
        inject_devshell_host(&mut spec, dev);
    }
    // Record what this launch ACTUALLY enters, for every surface that displays
    // containment. `spec.backend` is argv-derived (see `compose_spec`), so this
    // column can never carry a sandbox the pane does not have. Deliberately a
    // different column from `sandbox_backend`, which stays the user's pick and
    // drives the next resolution. Best-effort: the DB is a cache, and failing to
    // record a label must never take down a launch.
    if let Some(db) = db.as_ref() {
        let _ = db.set_worktree_observed(worktree, &spec.backend);
    }
    Ok(spec)
}

/// Tier A inject for a sandboxed pane: prepend the devShell `PATH` via a raw
/// `init_script` line — `$PATH` expands to the sandbox's *own* base PATH, so it
/// works for OCI and bwrap alike without the host knowing the in-sandbox PATH —
/// and set other safe exported vars as overrides (never clobbering one the user
/// already pinned).
fn inject_devshell_sandbox(spec: &mut sandbox::SandboxSpec, dev: &devenv::Devshell) {
    if let Some(path) = &dev.path {
        let line = format!("export PATH=\"{path}:$PATH\"\n");
        spec.init_script = Some(match spec.init_script.take() {
            Some(existing) => format!("{line}{existing}"),
            None => line,
        });
    }
    for (k, v) in &dev.vars {
        spec.env_overrides
            .entry(k.clone())
            .or_insert_with(|| v.clone());
    }
}

/// Tier A inject for the host (no-sandbox) path: prepend the devShell `PATH` to
/// the pane env (base = the host's current `PATH`) and add other safe vars that
/// aren't already set on the spec.
fn inject_devshell_host(spec: &mut LaunchSpec, dev: &devenv::Devshell) {
    if let Some(path) = &dev.path {
        let base = std::env::var("PATH").unwrap_or_default();
        let merged = if base.is_empty() {
            path.clone()
        } else {
            format!("{path}:{base}")
        };
        spec.env.retain(|(k, _)| k != "PATH");
        spec.env.push(("PATH".to_string(), merged));
    }
    for (k, v) in &dev.vars {
        if !spec.env.iter().any(|(ek, _)| ek == k) {
            spec.env.push((k.clone(), v.clone()));
        }
    }
}

/// The persisted slug for a repo root (for per-workspace account defaults), or
/// `None` if the DB has no slug yet.
pub(crate) fn repo_slug(db: &Db, repo_root: &Path) -> Option<String> {
    let base = repo_root.file_name()?.to_string_lossy().into_owned();
    db.slug_for_repo(&repo_root.to_string_lossy(), &base).ok()
}

fn sandbox_candidates(
    sb: &thegn_core::config::SandboxConfig,
) -> Vec<thegn_core::config::SandboxConfig> {
    if sb.backend != thegn_core::config::SandboxBackend::Auto {
        return vec![sb.clone()];
    }
    let mut out = Vec::new();
    for name in &sb.backend_chain {
        if let Ok(backend) = thegn_core::config::SandboxBackend::from_str_validated(name) {
            let mut c = sb.clone();
            c.backend = backend;
            out.push(c);
        }
    }
    if !out
        .iter()
        .any(|c| c.backend == thegn_core::config::SandboxBackend::None)
    {
        let mut c = sb.clone();
        c.backend = thegn_core::config::SandboxBackend::None;
        out.push(c);
    }
    out
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
