//! Native agent launching. The zellij-era `superzej pick-agent` ran inside a
//! freshly-created worktree pane, showed an fzf/gum picker, then `exec`'d the
//! choice so the selection became the pane's own process. The native host owns
//! the screen (raw mode), so the picker is the in-process command palette and
//! the pane *is* the spawned process — we compose the sandbox-wrapped argv +
//! env here and hand it to `Panes::spawn_argv_env` rather than exec-replacing.
//!
//! This module is the testable seam: `choices`, `resolve_command`, and
//! `launch_spec` are pure over `Config`/`Db`, so the wiring in `run.rs` stays a
//! thin call.

use std::path::{Path, PathBuf};
use superzej_core::config::Config;
use superzej_core::db::Db;

// Teardown fns live in a sibling (file-size ratchet); same call paths.
pub use crate::agent_teardown::{checkpoint_on_close, destroy_provider_sandbox};
use superzej_core::remote::GitLoc;
use superzej_core::store::{PoolStore, ProxyStore, WorkspaceStore};
use superzej_core::{bundle, devenv, repo, sandbox};
use superzej_svc::projection::ProjectionBackend;
use superzej_svc::vpn::VpnProvider;

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
        // Preference order: honour the host shell name if it's a known shell,
        // then try zsh/bash/fish/sh in that order.  The outer /bin/sh -lc
        // already provides a POSIX execution context, so this snippet is safe.
        let host_shell = std::env::var("SHELL").unwrap_or_default();
        let preferred = std::path::Path::new(&host_shell)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        // Build a chain that tries the host-preferred shell first (if it's a
        // known name) then falls through to bash → sh.
        let mut chain: Vec<&str> = Vec::new();
        if matches!(preferred, "zsh" | "bash" | "fish" | "dash" | "ksh" | "mksh") {
            chain.push(preferred);
        }
        for s in &["zsh", "bash", "sh"] {
            if !chain.contains(s) {
                chain.push(s);
            }
        }
        // Emit: for s in zsh bash sh; do command -v "$s" >/dev/null 2>&1 && exec "$s" -l; done
        let checks: String = chain
            .iter()
            .map(|s| format!("command -v {s} >/dev/null 2>&1 && exec {s} -l; "))
            .collect();
        // Put the nix profile dirs on PATH FIRST so a `nix profile install`ed shell
        // (zsh from nixpkgs) is actually found by `command -v` — covers the
        // single-user (`~/.nix-profile`), daemon/system (Determinate `--init none`,
        // `/nix/var/nix/profiles/default`), the Determinate per-user profile
        // (`~/.local/state/nix/profile/bin`, where `nix profile install` lands on
        // a Determinate install), and `~/.local/bin`. Without these the checks miss
        // the installed zsh/starship and drop to `/bin/sh`. The trailing
        // `/bin/sh -l` is the universal fallback.
        format!(
            "export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:\
             $HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; \
             {checks}exec /bin/sh -l"
        )
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
}

impl std::fmt::Display for SandboxHalt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "environment '{}' ({}) could not be brought up: {} — failover is off, \
             so superzej will not silently fall back to the host. Fix the env, or \
             set `failover = true` ([sandbox] or [env.{}]) to allow it.",
            self.env_name, self.placement, self.reason, self.env_name
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
    /// route into the sandbox via [`GitLoc::Provider`](superzej_core::remote::GitLoc).
    pub location: Option<String>,
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
/// Which container a sandbox is being prepared for. The interactive shell uses
/// the worktree's `profile`; the embedded agent uses `agent_profile` and, when
/// that differs, runs in its own separately-hardened container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxScope {
    Shell,
    Agent,
}

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
    scope: SandboxScope,
    selected_env: Option<&str>,
) -> anyhow::Result<SandboxOutcome> {
    use crate::handlers::repo_trust::resolve_env_trusted;
    let environment = resolve_env_trusted(cfg, repo_root, loc, worktree, selected_env);
    let placement = environment.placement.clone();
    let env_shell = environment.sandbox.shell.clone();
    // The worktree-projection plan (sshfs/sync) for this env's `data` mode, or
    // `None` for the default `in_env` (no projection). Captured before `sandbox`
    // is moved out of `environment` below.
    let projection = superzej_core::projection::for_environment(&environment);
    // Data mode + env name for the provider file-sync path below (a provider
    // placement isn't handled by `for_environment`, which is ssh-only).
    let env_data = environment.data;
    let env_name = environment.name.clone();
    // Failover policy for THIS env. When false (the default) and the env is
    // non-local, a bring-up failure halts (a `SandboxHalt` error) rather than
    // degrading to the host — a remote/managed env is often required, so a quiet
    // host drop is refused. `true` restores the historical chain→host fallback.
    let failover = cfg.env_failover(repo_root, &env_name);
    let placement_label = placement.label();
    // For a managed-provider env, persist a `GitLoc::Provider` location so the
    // chrome's git/fs reads route into the sandbox via the control-plane exec
    // prefix. `None` for local/ssh/k8s (their data plane is unchanged). The
    // worktree dir inside the env is the provider `workdir` (default /workspace).
    let location = match &placement {
        superzej_core::placement::Placement::Provider(p) => {
            let workdir = cfg
                .env
                .get(&env_name)
                .map(|e| e.provider.sync_workdir())
                .unwrap_or_else(|| "/workspace".to_string());
            Some(superzej_core::remote::GitLoc::provider_db_string(
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
    let exec_placement = if cwd_override.is_some() {
        superzej_core::placement::Placement::Local
    } else {
        placement.clone()
    };
    let env_is_remote = environment.is_remote() && cwd_override.is_none();
    let mut sb = environment.sandbox;
    let mut explicit_backend =
        sandbox::Backend::from_config(sb.backend).filter(|b| *b != sandbox::Backend::None);
    // A fresh/explicit choice always wins over config; a non-explicit DB value
    // only overrides when config is "auto" (an explicit `backend = "bwrap"` must
    // beat a stale entry). An explicit choice may be "host"/"none" — keep those.
    let config_is_auto = sb.backend == superzej_core::config::SandboxBackend::Auto;
    if let Some(saved) = backend_choice.map(str::trim)
        && !saved.is_empty()
        && (choice_is_explicit || (config_is_auto && saved != "auto"))
        && let Ok(b) = superzej_core::config::SandboxBackend::from_str_validated(saved)
    {
        explicit_backend =
            sandbox::Backend::from_config(b).filter(|b| *b != sandbox::Backend::None);
        sb.backend = b;
    }
    let explicit_choice = explicit_backend.is_some();
    let auto_choice = sb.backend == superzej_core::config::SandboxBackend::Auto;
    let mut warnings = Vec::new();
    let profile_slug = cfg.profile.trim();
    let base_cname = sandbox::container_name_with_profile(
        worktree,
        if profile_slug.is_empty() {
            None
        } else {
            Some(profile_slug)
        },
    );
    // Pick the hardening preset + container for this scope. The agent gets its
    // own (more-locked-down) container only when `agent_profile` differs from
    // the worktree `profile`; otherwise it reuses the worktree container.
    let agent_separate = scope == SandboxScope::Agent && sb.agent_profile != sb.profile;
    let hardening = match scope {
        SandboxScope::Agent => sb.agent_profile,
        SandboxScope::Shell => sb.profile,
    };
    let cname = if agent_separate {
        sandbox::agent_container_name(&base_cname)
    } else {
        base_cname
    };
    // Bring the execution placement up (k8s pod / provider sandbox) and project
    // the worktree into it BEFORE resolving the backend. Both are no-ops for the
    // default local `in_env` env, so this changes nothing for the common case;
    // for remote placements / `sshfs` it removes the previous need to run
    // `superzej env up` by hand. This runs on the (already off-event-loop)
    // sandbox-prepare path, mirroring the VPN sidecar bring-up below.
    // Warm-on-open: create the API sandbox first if `auto_provision` is set, so
    // the subsequent ensure/clone/connect find it live (8-E). No-op otherwise.
    if matches!(placement, superzej_core::placement::Placement::Provider(_))
        && let Err(e) = auto_provision_sandbox(cfg, &env_name, worktree)
    {
        // A provider that won't provision (bad token, quota, API down) can't host
        // the pane. With failover off, halt instead of silently degrading; with
        // failover on, keep the historical best-effort warn-and-continue.
        if !failover {
            return Err(SandboxHalt {
                env_name: env_name.clone(),
                placement: placement_label.clone(),
                reason: format!("auto-provision failed: {e}"),
            }
            .into());
        }
        warnings.push(format!("sandbox auto-provision failed: {e}"));
        superzej_core::msg::warn(&format!("sandbox auto-provision failed: {e}"));
    }
    if !placement.is_local()
        && let Err(e) = placement.ensure()
    {
        anyhow::bail!("env placement bring-up failed for {worktree}: {e}");
    }
    if let Some(pspec) = &projection {
        let backend = superzej_svc::projection::for_data_mode(pspec);
        match backend.mount(pspec) {
            // Record the live projection so the worktree-close thread (which only
            // has the path) can unmount it without re-resolving the env.
            Ok(_) => register_projection(worktree, pspec.clone()),
            Err(e) => {
                warnings.push(format!("projection ({}) failed: {e}", backend.kind()));
                superzej_core::msg::warn(&format!("projection mount failed for {worktree}: {e}"));
            }
        }
    }
    // Provider file-sync (`data = "sync"` on a managed provider): push the local
    // worktree into the sandbox fs before the pane execs (the pane runs IN the
    // sandbox, so there's no local cwd override). Best-effort: a failure warns,
    // never blocks the pane. Runs on a scoped thread with its own runtime so it
    // is safe regardless of the caller's async context.
    if env_data == superzej_core::config::DataMode::Sync
        && matches!(placement, superzej_core::placement::Placement::Provider(_))
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
                superzej_core::msg::warn(&format!(
                    "provider sync upload failed for {worktree}: {e}"
                ));
            }
        }
    }
    for candidate in sandbox_candidates(&sb) {
        if let Some(mut spec) =
            sandbox::resolve_placed(&candidate, loc, &cname, hardening, exec_placement.clone())
        {
            if spec.backend == sandbox::Backend::None {
                // A `none` backend on a *local* placement means "run on the host"
                // (the plain-shell fallback below). On a *remote* placement
                // (ssh/k8s/provider) the placement itself is the boundary — the
                // bare-shell spec carries the worktree into the pod/host, so use
                // it instead of falling back to a local host shell.
                if spec.placement.is_local() {
                    break;
                }
                return Ok(SandboxOutcome {
                    backend_label: spec.backend.label().to_string(),
                    spec: Some(spec),
                    warnings,
                    shell: env_shell,
                    is_remote: env_is_remote,
                    cwd_override,
                    location,
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
            // Bring the VPN tunnel up BEFORE the worktree container is created
            // (it joins the sidecar's netns). A tunnel failure must never fall
            // through to a less-isolated backend, so it bails the whole resolve.
            if let Err(e) = attach_vpn(&mut spec) {
                anyhow::bail!("sandbox vpn attach failed for {worktree}: {e}");
            }
            match sandbox::ensure(&spec) {
                Ok(()) => {
                    return Ok(SandboxOutcome {
                        backend_label: spec.backend.label().to_string(),
                        spec: Some(spec),
                        warnings,
                        shell: env_shell,
                        is_remote: env_is_remote,
                        cwd_override,
                        location,
                    });
                }
                Err(e) if explicit_choice => {
                    anyhow::bail!(
                        "sandbox {} failed for {worktree}: {e}",
                        spec.backend.label()
                    );
                }
                Err(e) => {
                    warnings.push(format!("sandbox {} failed: {e}", spec.backend.label()));
                    superzej_core::msg::warn(&format!(
                        "sandbox {} failed for {worktree}: {e}; trying next backend",
                        spec.backend.label()
                    ));
                }
            }
        } else if candidate.backend == superzej_core::config::SandboxBackend::None {
            break;
        } else if explicit_choice {
            anyhow::bail!(
                "sandbox backend '{}' could not be resolved for {worktree}",
                candidate.backend
            );
        } else if candidate.backend != superzej_core::config::SandboxBackend::Auto {
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
    if !placement.is_local() && !failover {
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: if warnings.is_empty() {
                "no usable backend produced a runnable sandbox".to_string()
            } else {
                warnings.join("; ")
            },
        }
        .into());
    }
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
    use superzej_core::config::{VpnMode, VpnOnError};
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
                superzej_core::msg::warn(&format!("{msg}; continuing (on_error=warn)"));
                Ok(())
            }
            VpnOnError::Offline => {
                superzej_core::msg::warn(&format!(
                    "{msg}; forcing network=none (on_error=offline)"
                ));
                spec.network = superzej_core::config::Network::None;
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
    let rt = superzej_svc::vpn::OciRuntime::new(prefix);
    let sidecar = sandbox::vpn_sidecar_name(&spec.name);
    let provider = superzej_svc::vpn::for_provider(&vpn);

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
    let cfg = Config::load_layered(&superzej_core::config::ProcessEnv, &[], None);
    let sb = cfg.repo_sandbox(Path::new(path));
    if !sb.vpn.is_enabled() {
        return;
    }
    let name = sandbox::container_name(path);
    let Some(vpn) = sandbox::build_vpn_spec(&sb.vpn, &name, sb.profile) else {
        return;
    };
    let sidecar = sandbox::vpn_sidecar_name(&name);
    let provider = superzej_svc::vpn::for_provider(&vpn);
    // We don't track which OCI runtime started the sidecar; try the likely ones.
    // `down` execs the de-register inside the sidecar, so a wrong runtime simply
    // fails to find the container and is ignored.
    for prefix in [
        vec!["podman".to_string()],
        vec!["docker".to_string()],
        vec!["sudo".to_string(), "-n".to_string(), "podman".to_string()],
    ] {
        let rt = superzej_svc::vpn::OciRuntime::new(prefix);
        let _ = provider.down(&rt, &sidecar);
    }
}

/// In-process registry of live worktree projections (path → resolved spec), so
/// the worktree-close teardown thread — which only has the path — can unmount the
/// projection (sshfs/sync) without re-resolving the named env. Best-effort: a
/// projection created in a previous process isn't tracked here (it auto-reaps
/// like the VPN ephemeral nodes, or is cleaned by `superzej env down`).
fn projection_registry() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, superzej_core::projection::ProjectionSpec>,
> {
    static REG: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<String, superzej_core::projection::ProjectionSpec>,
        >,
    > = std::sync::OnceLock::new();
    REG.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn register_projection(worktree: &str, spec: superzej_core::projection::ProjectionSpec) {
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
        let backend = superzej_svc::projection::for_data_mode(&spec);
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
    env: &superzej_core::env::Environment,
) -> Option<superzej_svc::provider::Provider> {
    use superzej_core::config::ProviderExecMode;
    let superzej_core::placement::Placement::Provider(_) = &env.placement else {
        return None;
    };
    let pc = &cfg.env.get(&env.name)?.provider;
    if pc.exec == ProviderExecMode::Cli || !superzej_svc::provider::exec_api_by_name(&pc.provider) {
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
    pub provider: superzej_svc::provider::Provider,
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
    /// The [`ExecSpec`](superzej_svc::provider::ExecSpec) to open a fresh login
    /// shell inside the sandbox: a `/bin/sh -lc` that cd's into the worktree's
    /// `workdir` then runs the resolved shell, with the pane env passed through.
    ///
    /// `inner` is a self-contained script that does its OWN `exec` — either the
    /// [`shell_inner`] runtime probe chain (`command -v zsh && exec zsh -l; …;
    /// exec /bin/sh -l`) or [`shell_inner_override`] (`exec <shell> -l`). It must
    /// NOT be prefixed with another `exec`: `exec command -v zsh …` makes the
    /// shell try to exec a binary named `command` (a builtin), which fails with
    /// 127 and kills the pane before any shell starts.
    pub fn open_spec(&self, cols: u16, rows: u16) -> superzej_svc::provider::ExecSpec {
        let script = if self.workdir.is_empty() {
            self.inner.clone()
        } else {
            format!("cd {} 2>/dev/null; {}", self.workdir, self.inner)
        };
        superzej_svc::provider::ExecSpec {
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
    pub fn open_spec_clean(&self, cols: u16, rows: u16) -> superzej_svc::provider::ExecSpec {
        let inner = clean_shell_inner();
        let script = if self.workdir.is_empty() {
            inner
        } else {
            format!("cd {} 2>/dev/null; {}", self.workdir, inner)
        };
        superzej_svc::provider::ExecSpec {
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
) -> Option<(superzej_svc::provider::Provider, String, String)> {
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
    let superzej_core::placement::Placement::Provider(p) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    let provider = provider_for_named(pc, &p.id)?;
    Some((provider, p.id.clone(), pc.sync_workdir()))
}

pub(crate) use crate::agent_ssh::{
    SPRITE_SSHD_PORT, sprite_ssh_argv, sprite_ssh_connect, sprite_ssh_keypair,
    sprite_sshd_setup_script, sprite_sshd_start_script,
};

/// Decide whether `worktree`'s interactive shell should attach via a provider's
/// **native exec API** instead of the CLI/PTY path. `Some` when the resolved env
/// is a `provider` placement whose provider has a native exec API, whose `exec`
/// mode isn't `cli`, and whose API token is present; `None` ⇒ use [`launch_spec`].
///
/// Resolves the env exactly as [`launch_spec_with_key`] does (DB repo-root +
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
/// ([`superzej_core::envplan::known_agents`]) counts as present if its binary is
/// on the host PATH or its config/credential dir exists in `$HOME`.
pub(crate) fn detect_host_agents() -> Vec<String> {
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_default();
    let dirs: Vec<&str> = path.split(':').filter(|s| !s.is_empty()).collect();
    superzej_core::envplan::known_agents()
        .iter()
        .filter(|a| {
            let on_path = dirs.iter().any(|d| Path::new(d).join(a).is_file());
            if on_path {
                return true;
            }
            let (files, cfg_dirs) = superzej_core::envplan::agent_config_paths(a);
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
/// e.g. the managed pi snippet's `$HOME/.superzej/pi` resolves to the sprite home.
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
    use superzej_core::config::ProviderExecMode;
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
    let superzej_core::placement::Placement::Provider(p) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    if pc.exec == ProviderExecMode::Cli || !superzej_svc::provider::exec_api_by_name(&pc.provider) {
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
    // Carry the host's passthrough secrets (GH_TOKEN, ANTHROPIC_API_KEY, …) into
    // the provider exec so the in-sprite shell + any agent it spawns (pi, claude
    // code, hermes) work like local. Remote-safe filter drops host-local socket
    // vars (SSH_AUTH_SOCK/GPG_*) that would dangle in the VM. SUPERZEJ_* win.
    let mut env = environment.sandbox.passthrough_env_remote();
    // Route any agent in the sprite (pi, claude code, hermes) through szproxy by
    // default — sets ANTHROPIC_BASE_URL etc. when a reachable remote proxy URL is
    // configured. No-op otherwise (the agent talks upstream directly).
    env.extend(cfg.llm_proxy.remote_agent_env(None));
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
    env.push(("SUPERZEJ_WORKTREE".to_string(), worktree.to_string()));
    env.push(("SUPERZEJ_BRANCH".to_string(), String::new()));
    // A claimed warm-pool spare overrides the derived id (same rule as
    // `provider_sandbox_name`): the pane must attach to the sandbox the
    // worktree is actually bound to, not the derived husk.
    let bound = Db::open()
        .ok()
        .and_then(|db| db.worktree_provider_sandbox(worktree).ok().flatten());
    Some(NativeShell {
        provider,
        provider_name: pc.provider.clone(),
        sandbox_id: bound.unwrap_or_else(|| p.id.clone()),
        inner,
        workdir: pc.sync_workdir(),
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
        superzej_core::placement::Placement::Provider(_)
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
            let workdir = envc.provider.sync_workdir();
            let marker = superzej_core::envplan::EnvPlan::marker_path(&workdir);
            block_on_provider(|| async { provider.read(&id, &marker).await }).is_err()
        }
        Err(_) => false,
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
    if let superzej_core::placement::Placement::Provider(_) = &environment.placement
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
        superzej_core::log_trace::enter_wt(superzej_core::log_trace::wt_slug(Path::new(worktree)));
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
        superzej_core::placement::Placement::Provider(_)
    ) {
        return Ok(false);
    }
    // Flag the in-flight provision so the warm-claim fast path won't bind a
    // spare (and clear the splash) under this run — see `provision_gate`.
    let _live = crate::provision_gate::worktree_live_guard(worktree);
    provision_provider_env(cfg, worktree, &environment.name, progress)
}

/// Make a sandbox/remote env "just work" like local, by reproducing the repo's
/// **declared** environment inside it (see `superzej_core::envplan`): clone the
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
/// checkpoint_id)` — the "superzej-provisioned" base checkpoint taken this run,
/// if any (recorded per (repo, env) and, for spares, on the pool row so a stale
/// spare can be recycled by restore-in-place instead of destroy+rebuild).
pub fn provision_provider_env_named(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
    name_override: Option<&str>,
    progress: &mut impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<(bool, Option<String>)> {
    use superzej_core::envplan::{self, EnvPlan, PlanOpts, StepKind};

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
    let workdir = pc.sync_workdir();
    let marker = EnvPlan::marker_path(&workdir);

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
    tracing::debug!(target: "szhost::startup", %id, "provider ensure_exists (create/list)");
    let created = match block_on_provider(|| async { provider.ensure_exists(&id).await }) {
        Ok(created) => created,
        Err(e) => return Err(anyhow::anyhow!("ensure sandbox {id}: {e}")),
    };
    tracing::debug!(target: "szhost::startup", %id, created, "sandbox ensured");

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
            boot[0].detail = Some(e.to_string());
            progress(&boot);
            return Err(anyhow::anyhow!("sandbox {id} not ready: {e}"));
        }
        boot[0].state = ProvisionState::Done;
        progress(&boot);
    }

    // Idempotent: already provisioned ⇒ nothing to do (no new checkpoint) — but
    // still refresh auth creds: the host's OAuth token rotates, so the
    // provision-time snapshot goes stale and the in-sandbox agent 401s.
    if block_on_provider(|| async { provider.read(&id, &marker).await }).is_ok() {
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
    let exec_env = cfg.repo_sandbox(&repo_root).passthrough_env_remote();
    // Which flake devShell the sandbox builds/enters ([sandbox] devshell, e.g.
    // "sandbox" for the lean build shell). Drives the seed build + realise so they
    // match the in-pane `.envrc` (which reads SUPERZEJ_DEVSHELL from exec_env).
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
    let p2p_parity = home.strategy == superzej_core::config::ShellStrategy::HostParity
        && pc.connect == superzej_core::config::ProviderConnect::Ssh
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
    if pc.connect == superzej_core::config::ProviderConnect::Ssh
        && let Ok((_key, pubkey)) = sprite_ssh_keypair()
    {
        setup.push(sprite_sshd_setup_script(&pubkey));
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
        superzej_core::util::git_cmd(Path::new(worktree))
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
            superzej_core::envplan::BinaryCache {
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
        //  • Real worktree: respect the configured `skip_devshell_warm`.
        //  • SPARE (name_override): a spare provisions in the BACKGROUND, so it
        //    would normally BUILD the devShell up front to make a claim instant.
        //    BUT a spare can't reach the host nix cache (the reverse tunnel is
        //    per-worktree, set up on focus), so with `host_cache` on it would build
        //    the whole devShell FROM SOURCE — slow, so the pool can't refill fast
        //    enough to stay warm. With the cache on, SKIP the spare's build: the
        //    spare provisions fast (clone + checkpoint), and the claimed worktree's
        //    in-pane `direnv` substitutes the devShell from the host cache (fast)
        //    once its tunnel is up. Build up front only when there's no cache.
        skip_devshell_warm: if name_override.is_some() {
            pc.host_cache
        } else {
            pc.skip_devshell_warm
        },
        // Full local parity (unpushed commits + uncommitted + untracked) for a
        // real worktree on an `in_env` provider — so a fresh sandbox matches the
        // working tree, not just origin. A SPARE (name_override) stays a pristine
        // clone (generic until claimed); a non-`in_env` data mode projects the
        // tree by other means, so skip the overlay there.
        local_parity: (name_override.is_none()
            && env.data == superzej_core::config::DataMode::InEnv)
            .then(|| worktree.to_string()),
        // A hibernated worktree resumes by overlaying its snapshot on the
        // fresh clone; the row flips to `restoring` here (deleted on success).
        snapshot_restore: (name_override.is_none())
            .then(|| crate::hibernator::begin_restore(worktree))
            .flatten(),
        // When the host cache is on, bake its sandbox-side loopback substituter into
        // nix.conf so the devShell build + in-pane `nix develop` substitute from the
        // host store over the reverse tunnel (which the host stands up separately).
        host_cache_url: pc
            .host_cache
            .then(|| format!("http://127.0.0.1:{}", crate::nixcache::SANDBOX_PORT)),
        // Provision the managed pi in the sandbox when a configured agent runs it
        // (its command references `~/.superzej/pi`), so the "Agent" entry's snippet
        // resolves in-sprite. A real worktree only (a spare stays generic).
        managed_pi: name_override.is_none()
            && cfg
                .agents
                .iter()
                .any(|a| a.command.contains(".superzej/pi")),
        toolchain: cfg.toolchain.clone(),
    };
    let plan = envplan::plan(&req, &opts);

    // Host-parity (Phase 2): push the host's home-shell closure to the configured
    // binary cache so the in-sandbox `home_closure` step can substitute it and the
    // exact host dotfiles resolve. Host-side (uses the host's `nix`), best-effort:
    // only when a push cache is set — nixpkgs paths otherwise substitute from the
    // default caches (cache.nixos.org) with no push. A failure just means the
    // sandbox falls back to whatever its substituters can serve.
    if opts.strategy == superzej_core::config::ShellStrategy::HostParity
        && !opts.home_store_roots.is_empty()
        && pc.binary_cache_push
        && !pc.binary_cache_url.trim().is_empty()
        && let Err(e) = push_home_closure(pc.binary_cache_url.trim(), &opts.home_store_roots)
    {
        superzej_core::msg::warn(&format!(
            "host-parity: pushing the home closure to {} failed: {e}; the sandbox will \
             substitute what it can from its default caches (e.g. cache.nixos.org).",
            pc.binary_cache_url.trim(),
        ));
    }

    // The sandbox user's real `$HOME` — uploads must land there (sprites exec as
    // user `sprite`, HOME=/home/sprite, NOT /root). Resolved once via the exec API.
    let sprite_home = block_on_provider(|| async {
        provider
            .run_exec(
                &id,
                &[
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    "printf %s \"$HOME\"".to_string(),
                ],
                None,
                &[],
            )
            .await
    })
    .ok()
    .map(|(_, out)| out.trim().to_string())
    .filter(|h| h.starts_with('/'))
    .unwrap_or_else(|| "/root".to_string());

    // The "superzej-provisioned" base checkpoint taken by this run's Checkpoint
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
        tracing::info!(target: "szhost::startup", step = %step.id, "provision step start");

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
                            target: "szhost::startup",
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
                    superzej_core::msg::warn(&format!(
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
                    superzej_core::msg::warn(&format!(
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
            StepKind::ManagedPi => {
                // Host-executed: provision the managed pi inside the sandbox so the
                // "Agent" entry's `$HOME/.superzej/pi` snippet resolves in-sprite.
                // Best-effort throughout — a failure just means the Agent entry won't
                // work in this sprite (the host one still does).
                if let Err(e) =
                    crate::agent_pi::provision_managed_pi(&provider, &id, &sprite_home, &exec_env)
                {
                    superzej_core::msg::warn(&format!(
                        "managed pi: {e}; the \"Agent\" entry may not work in this sandbox."
                    ));
                }
                Ok(())
            }
            StepKind::Checkpoint => block_on_provider(|| async {
                provider.checkpoint(&id, Some("superzej-provisioned")).await
            })
            .map(|cp| base_checkpoint = Some(cp)),
            StepKind::HomeClosurePush(roots) => {
                // Host-executed: push the host store → sandbox store over the WSS
                // ssh tunnel (host's `nix copy --to ssh-ng://`). Best-effort — a
                // failure leaves the rc to source whatever the sandbox can resolve;
                // it must not abort provisioning, so warn + continue.
                match sprite_ssh_connect(cfg, worktree) {
                    Some((key, user, _workdir)) => {
                        let exe = std::env::current_exe()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|_| "szhost".into());
                        if let Err(e) = push_home_closure_p2p(&exe, worktree, &key, &user, roots) {
                            superzej_core::msg::warn(&format!(
                                "host-parity p2p: pushing the home closure to the sandbox \
                                 failed: {e}; the shell will use whatever the sandbox can \
                                 resolve. Ensure the sandbox was (re)created with connect=ssh."
                            ));
                        }
                        Ok(())
                    }
                    None => {
                        superzej_core::msg::warn(
                            "host-parity p2p: connect=ssh is required to push the home \
                             closure but the ssh tunnel is unavailable; skipping.",
                        );
                        Ok(())
                    }
                }
            }
        };

        if let Err(e) = result {
            views[i].state = ProvisionState::Failed;
            views[i].detail = Some(sanitize_detail(&e.to_string()));
            progress(&views);
            // Only the essential steps (the worktree dir + clone + nix) abort
            // creation. The rest — warming the devShell/direnv, personal tools,
            // dotfiles, the home-parity closure — are BEST-EFFORT: the shell still
            // comes up and these resolve lazily in the pane. A best-effort failure
            // warns + continues so one flaky `nix develop` can't kill the sandbox.
            if step_is_fatal(&step.id) {
                tracing::warn!(target: "szhost::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, error = %e, "provision step failed (fatal — aborting)");
                return Err(e);
            }
            tracing::warn!(target: "szhost::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, error = %e, "provision step failed (best-effort — continuing)");
            // If the step likely restarted the sandbox VM (an OOM-killed Nix
            // install, exit 137), give it a bounded window to come back before
            // the next step — otherwise every remaining step burns its own
            // connect budget failing `exec ws connect` against a booting VM.
            if crate::provision_recover::step_signals_sandbox_restart(&e.to_string()) {
                tracing::warn!(target: "szhost::startup", step = %step.id, "step may have restarted the sandbox; waiting for it to become ready before continuing");
                let _ = block_on_provider(|| async {
                    provider
                        .wait_ready(&id, std::time::Duration::from_secs(90))
                        .await
                });
            }
            continue;
        }
        tracing::info!(target: "szhost::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, "provision step done");
        views[i].state = ProvisionState::Done;
        progress(&views);
    }

    // Drop the marker so a later open skips re-provisioning.
    let _ = block_on_provider(|| async { provider.write(&id, &marker, b"ok\n").await });
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
/// ProxyCommand (`<szhost> sprite-proxy <wt>`) can't go inline — the wrapper is a
/// single token. Returns its path.
fn write_proxy_wrapper(key: &Path, proxy_cmd: &str) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = key.parent().unwrap_or_else(|| Path::new("."));
    let script = dir.join("nix-copy-proxy.sh");
    std::fs::write(&script, format!("#!/bin/sh\nexec {proxy_cmd}\n"))?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
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
    szhost_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    roots: &[String],
) -> anyhow::Result<()> {
    let proxy = format!(
        "{} sprite-proxy {}",
        superzej_core::util::sh_quote(szhost_exe),
        superzej_core::util::sh_quote(worktree),
    );
    let proxy_script = write_proxy_wrapper(key, &proxy)?;
    let ssh_opts = format!(
        "-o ProxyCommand={} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -o LogLevel=ERROR -o ConnectTimeout=10 -o ServerAliveInterval=10 \
         -o ServerAliveCountMax=3 -i {} -p {}",
        superzej_core::util::sh_quote(&proxy_script.to_string_lossy()),
        superzej_core::util::sh_quote(&key.to_string_lossy()),
        SPRITE_SSHD_PORT,
    );
    let argv = nix_copy_p2p_argv(user, roots);
    // HARD timeout: this is a blocking host-side call on the provisioning path. If
    // the sandbox sshd isn't reachable yet (fresh sprite) or the closure is huge,
    // `nix copy` would otherwise hang the loading screen indefinitely. `timeout`
    // (coreutils) bounds it; `--kill-after` force-kills the ssh/ProxyCommand
    // children. On timeout the caller warns + continues (best-effort), so the
    // shell still opens — exact-parity is a nice-to-have, never a blocker.
    let out = std::process::Command::new("timeout")
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

/// Run a host `nix` subcommand bounded by `timeout` (coreutils). `Ok(output)` on
/// success; `Err` with a tail of stderr (or "timed out") otherwise.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn run_host_nix_timeout(secs: u32, argv: &[String]) -> anyhow::Result<std::process::Output> {
    let out = std::process::Command::new("timeout")
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
#[expect(clippy::disallowed_methods)]
fn push_devshell_closure(
    provider: &superzej_svc::provider::Provider,
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
    // 2. Resolve the devShell store path (what the sandbox must import).
    let pi = std::process::Command::new("nix")
        .args(["path-info", &gcroot_str])
        .output()
        .map_err(|e| anyhow::anyhow!("nix path-info: {e}"))?;
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
            superzej_core::msg::warn(&format!(
                "devshell push: cache pruning skipped ({e}); uploading the full closure."
            ));
        }
        let dest = "/tmp/sz-devshell-cache";
        block_on_provider(|| async { provider.upload_dir(id, &cache, dest).await })?;
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
        let (code, out) =
            block_on_provider(|| async { provider.run_exec(id, &argv, None, &[]).await })?;
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
    let out = std::process::Command::new("timeout")
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

/// Which provisioning steps are ESSENTIAL — a failure aborts creation — vs
/// best-effort (warn + continue; the shell still opens and the step resolves
/// lazily in the pane). Essentials: the worktree dir, git auth, the clone.
/// Everything else (nix install, devShell/direnv warm, personal tools, dotfiles,
/// the home-parity closure, checkpoint) is best-effort, so one flaky `nix
/// develop` / unreachable cache can't kill an otherwise-usable sandbox.
fn step_is_fatal(step_id: &str) -> bool {
    matches!(step_id, "workspace" | "git_auth" | "clone")
}

/// Sanitize a subprocess-derived message for display on the loading screen:
/// strip ANSI/OSC escape sequences and other control bytes (provisioning output
/// is full of them — they corrupt width math and have triggered renderer
/// `capacity overflow`s), collapse runs of whitespace/newlines to single spaces,
/// and clamp to a sane length. Pure + unit-tested.
fn sanitize_detail(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(256));
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI (`ESC[ … final`) / OSC (`ESC] … BEL/ST`) / other ESC seq: skip
            // the introducer and run to the terminating byte.
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if ('@'..='~').contains(&d) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for d in chars.by_ref() {
                        if d == '\u{7}' || d == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        // Control chars (incl. newlines/tabs) + spaces → a single space; collapse
        // runs so multi-line subprocess output reads as one tidy line.
        if c.is_control() || c == ' ' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
        if out.chars().count() >= 200 {
            out.push('…');
            break;
        }
    }
    out.trim().to_string()
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

/// Upload the present host dotfiles/dotdirs into the sandbox's `$HOME` (`/root`).
/// A basename that's a FILE is uploaded via the fs `write`; a DIRECTORY (e.g.
/// `.config/gcloud`, `.aws`) is uploaded recursively via `upload_dir` — so cloud
/// creds and multi-file config carry over too. Missing host paths are skipped; a
/// genuine upload failure aborts the step.
fn upload_dotfiles(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    files: &[String],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    for name in files {
        let src = Path::new(&host_home).join(name);
        let dest = format!("{base}/{name}");
        if src.is_dir() {
            block_on_provider(|| async { provider.upload_dir(id, &src, &dest).await })?;
        } else if let Ok(data) = std::fs::read(&src) {
            block_on_provider(|| async { provider.write(id, &dest, &data).await })?;
        }
    }
    Ok(())
}

/// Carry the host's atuin credentials + config into the sandbox so its shell
/// history joins atuin's own sync (host ↔ sprites). Opt-in (`[sandbox.home]
/// atuin = true`). Uploads the dereferenced `~/.config/atuin/config.toml` (the
/// home-manager `/nix/store` symlink is read THROUGH, so the real bytes land, not
/// a dangling link) + the auth/encryption files `~/.local/share/atuin/{key,
/// session}`. The history DBs are deliberately NOT copied — atuin's sync server
/// reconciles those. Best-effort: a missing source is skipped (only `key` and no
/// `session` is a normal state); a genuine upload error aborts (surfaced as a
/// best-effort step failure). Warns when there's nothing to carry.
fn upload_atuin_creds(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    // Config first, then the auth/encryption state. `provider.write` creates parent
    // dirs (mkdirParents), so the nested `.config/atuin` / `.local/share/atuin`
    // paths land without an explicit mkdir.
    //
    // `meta.db` is the SERVER-AUTH carrier: atuin >=18 keeps the sync session token
    // (the `hub_session` bearer for the sync server) in `meta.db`, NOT in a flat
    // `session` file (which modern atuin no longer writes). Without it the sandbox
    // has the encryption `key` but is logged OUT, so `auto_sync` can't authenticate
    // and Ctrl-R stays empty. We still carry `session` too (older atuin / other
    // hosts may have it) and deliberately skip the heavy history/records DBs — the
    // server reconciles those once authenticated. `meta.db` is small (~28K).
    let rels = [
        ".config/atuin/config.toml",
        ".local/share/atuin/key",
        ".local/share/atuin/session",
        ".local/share/atuin/meta.db",
    ];
    let mut carried = 0usize;
    let mut token_carried = false;
    for rel in rels {
        let src = Path::new(&host_home).join(rel);
        // `read` dereferences the symlink → the real bytes (the HM config.toml is a
        // `/nix/store` symlink that would dangle in the sandbox).
        if let Ok(data) = std::fs::read(&src) {
            let dest = format!("{base}/{rel}");
            block_on_provider(|| async { provider.write(id, &dest, &data).await })?;
            carried += 1;
            if rel.ends_with("meta.db") || rel.ends_with("session") {
                token_carried = true;
            }
        }
    }
    if carried == 0 {
        superzej_core::msg::warn(
            "atuin sync: no host atuin config/credentials found (~/.config/atuin, \
             ~/.local/share/atuin) — nothing to carry.",
        );
        return Ok(());
    }
    // Prime history at provision time so it's baked into the checkpoint and Ctrl-R
    // is populated the instant the pane opens (instead of waiting for the first
    // `auto_sync` tick). `sync -f` forces a full reconcile regardless of the carried
    // last-sync throttle, pulling the server's records into the sandbox's empty
    // store. Best-effort: a sync failure (offline, server hiccup) just means history
    // fills in on the next auto_sync. Skipped when no auth token was carried.
    if token_carried {
        let argv = vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "export PATH=\"$HOME/.local/bin:$HOME/.nix-profile/bin:$PATH\"; \
             command -v atuin >/dev/null 2>&1 && atuin sync -f 2>&1 || true"
                .to_string(),
        ];
        if let Err(e) =
            block_on_provider(|| async { provider.run_exec(id, &argv, None, exec_env).await })
        {
            superzej_core::msg::warn(&format!(
                "atuin sync: priming history failed ({e}); it will fill in on auto_sync."
            ));
        }
    }
    Ok(())
}

/// Last `n` non-empty lines of command output, for a compact error message.
fn tail_lines(out: &str, n: usize) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

/// `(key, value)` facts about where a worktree's pane is coming up, for the
/// loading screen's context block: env, placement, provider/sandbox, connect mode,
/// shell strategy, workdir. Loop-safe (a DB read + pure config resolution, no
/// network/subprocess). Empty for a plain local env (nothing interesting to show).
pub fn loading_context(cfg: &Config, worktree: &str) -> Vec<(String, String)> {
    use superzej_core::placement::Placement;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let selected = Db::open()
        .ok()
        .and_then(|db| db.effective_env(worktree, &repo_root.to_string_lossy()));
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), selected.as_deref());
    if env.placement.is_local() {
        return Vec::new();
    }
    let mut out = vec![
        ("env".to_string(), env.name.clone()),
        ("placement".to_string(), env.placement.label()),
    ];
    if let Placement::Provider(_) = &env.placement
        && let Some(ec) = cfg.env.get(&env.name)
    {
        let pc = &ec.provider;
        if !pc.provider.trim().is_empty() {
            out.push(("provider".to_string(), pc.provider.clone()));
        }
        if let Some(id) = provider_sandbox_name(cfg, worktree, &env.name).filter(|s| !s.is_empty())
        {
            out.push(("sandbox".to_string(), id));
        }
        out.push((
            "connect".to_string(),
            format!("{:?}", pc.connect).to_lowercase(),
        ));
        let wd = pc.sync_workdir();
        if !wd.trim().is_empty() {
            out.push(("workdir".to_string(), wd));
        }
    }
    let strategy = format!("{:?}", env.sandbox.home.strategy).to_lowercase();
    out.push(("shell".to_string(), strategy));
    out
}

/// Cheap, **loop-safe** (no network/subprocess) check made right before a
/// worktree pane spawns: should this worktree HALT with a warning instead of
/// opening a (host-degraded) pane? Returns `Some` only for a NON-LOCAL env with
/// failover disabled whose bring-up is already known to be impossible — its API
/// token is unset, or its native exec is in the post-failure cooldown (the live
/// signal a recent connect/auth attempt failed, e.g. the sprites 401). `None`
/// (proceed normally) for local envs, when failover is allowed, or when there's
/// no cheap evidence of failure yet — in which case a later bring-up failure is
/// caught in `prepare_sandbox_env` / the native relay, which flip the health flag
/// so the next spawn halts here.
pub fn env_halt_reason(cfg: &Config, worktree: &str) -> Option<SandboxHalt> {
    use superzej_core::config::ProviderExecMode;
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    // Refuse to silently ignore a malformed repo `.superzej.*` overlay that was
    // SELECTING a non-local env: `load_repo_overlay` drops a file that fails to
    // parse, so its `env = "sprites"` selection vanishes and resolution falls back
    // to local (host) — exactly the silent degradation failover-off forbids. Catch
    // it BEFORE resolve_env (which has already lost the selection) and surface the
    // same warning modal. (No halt for a parse error that wasn't selecting a
    // non-local env, or when that env opts into failover.)
    if let Some(pe) = superzej_core::config::repo_overlay_parse_error(&repo_root)
        && !pe.selected_env.is_empty()
        && let Some(envc) = cfg.env.get(&pe.selected_env)
        && !matches!(envc.placement, superzej_core::config::PlacementMode::Local)
        && !cfg.env_failover(&repo_root, &pe.selected_env)
    {
        tracing::warn!(
            target: "szhost::sandbox",
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
    // Local envs never halt; an env that opts into failover keeps the old behavior.
    if environment.placement.is_local() || cfg.env_failover(&repo_root, &environment.name) {
        return None;
    }
    let placement = environment.placement.label();
    if let superzej_core::placement::Placement::Provider(_) = &environment.placement {
        let pc = &cfg.env.get(&environment.name)?.provider;
        // Token check: the var the provider reads (defaults to SPRITES_TOKEN).
        let var = {
            let v = pc.api_key_env.trim();
            if v.is_empty() { "SPRITES_TOKEN" } else { v }
        };
        let token_present = std::env::var(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let healthy = native_exec_healthy(&pc.provider);
        tracing::debug!(
            target: "szhost::sandbox",
            env = %environment.name, %placement, provider = %pc.provider,
            token_var = %var, token_present, native_exec_healthy = healthy,
            exec = ?pc.exec,
            "env_halt_reason: evaluating provider env"
        );
        if !token_present {
            tracing::warn!(target: "szhost::sandbox", env = %environment.name, token_var = %var, "HALT: API token not set");
            return Some(SandboxHalt {
                env_name: environment.name.clone(),
                placement,
                reason: format!("API token ${var} is not set"),
            });
        }
        // Native-exec failure cooldown: a recent connect/auth failure (e.g. 401)
        // marked the provider unhealthy. With failover off we won't drop to host,
        // so surface the halt rather than spawn a doomed pane.
        if pc.exec != ProviderExecMode::Cli
            && superzej_svc::provider::exec_api_by_name(&pc.provider)
            && !healthy
        {
            tracing::warn!(target: "szhost::sandbox", env = %environment.name, provider = %pc.provider, "HALT: native exec unhealthy (recent failure)");
            return Some(SandboxHalt {
                env_name: environment.name.clone(),
                placement,
                reason: format!(
                    "provider '{}' is unreachable or rejected authentication (recent exec failure)",
                    pc.provider
                ),
            });
        }
        tracing::debug!(target: "szhost::sandbox", env = %environment.name, "env_halt_reason: provider OK, no halt");
    }
    None
}

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
            superzej_core::msg::warn(&format!(
                "bridge binary unreadable ({}): {e}",
                binary.display()
            ));
            return;
        }
    };
    match block_on_provider(|| async { provider.ensure_executable(&id, remote_path, &data).await })
    {
        Ok(true) => superzej_core::msg::info(&format!(
            "pushed resident bridge → {id}:{remote_path} ({} bytes)",
            data.len()
        )),
        Ok(false) => {} // already current — no re-push
        Err(e) => superzej_core::msg::warn(&format!("bridge binary push failed: {e}")),
    }
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
    let Some(provider) = provider_for_named(pc, &name) else {
        return Ok(());
    };
    match block_on_provider(|| async { provider.ensure_exists(&name).await }) {
        Ok(true) => {
            superzej_core::msg::info(&format!("provisioned sandbox {name}"));
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
    let script = superzej_core::remote::provision_repo_script(&origin, branch);
    // Prefer the resident bridge (CLI-free) when it's already up for this loc;
    // else fall back to the per-op CLI control prefix.
    if let Some(b) = superzej_svc::bridge::for_loc(loc) {
        match b.exec(&["/bin/sh", "-lc", &script], Some(&loc.path()), &[]) {
            Ok(r) if r.exit == 0 => return,
            Ok(r) => {
                superzej_core::msg::warn(&format!(
                    "provider repo provision failed: {}",
                    r.stderr.trim()
                ));
                return;
            }
            // Bridge hiccup — fall through to the CLI path below.
            Err(e) => superzej_core::msg::warn(&format!("provider repo provision via bridge: {e}")),
        }
    }
    match loc.sh_command(&script).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => superzej_core::msg::warn(&format!(
            "provider repo provision failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => superzej_core::msg::warn(&format!("provider repo provision spawn failed: {e}")),
    }
}

/// The local repo's `origin` remote URL, or `None` (no remote / not a repo).
// off-loop by contract: only called from the blocking provisioning path
// (provision_provider_env_named / launch_spec_with_key) — see note above.
#[expect(clippy::disallowed_methods)]
fn local_origin(repo_root: &Path) -> Option<String> {
    let out = superzej_core::util::git_cmd(repo_root)
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
) -> Option<(superzej_svc::provider::Provider, String, String)> {
    let pc = &cfg.env.get(env_name)?.provider;
    let id = pc.id.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let provider = provider_for(pc)?;
    if !provider.caps().files {
        return None;
    }
    Some((provider, id, pc.sync_workdir()))
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
    let cfg = Config::load_layered(&superzej_core::config::ProcessEnv, &[], None);
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
    // When running inside an OCI container the host's absolute $SHELL path
    // (e.g. /run/current-system/sw/bin/zsh) does not exist in the container
    // filesystem.  Pass in_oci=true so shell_inner() uses only the basename.
    let in_oci = sb.spec.as_ref().is_some_and(|s| s.backend.is_oci());
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
    // Local worktrees run in their own dir; a remote worktree (its `GitLoc`) or
    // a remote placement (ssh/k8s/provider env) has no local dir — the placement
    // cd's on the target — so the pane cwd stays unset.
    // A projected data mode (sshfs/sync) pins the pane to its local mountpoint;
    // otherwise a local worktree runs in its own dir and a remote one has none.
    let cwd = sb
        .cwd_override
        .clone()
        .or_else(|| (!loc.is_remote() && !sb.is_remote).then(|| PathBuf::from(worktree)));
    let mut env = vec![
        ("SUPERZEJ_WORKTREE".to_string(), worktree.to_string()),
        (
            "SUPERZEJ_BRANCH".to_string(),
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
        None => vec![superzej_core::util::shell(), "-lc".to_string(), cmd],
    };
    LaunchSpec {
        argv,
        cwd,
        env,
        backend: sb.backend_label.clone(),
        warnings: sb.warnings.clone(),
    }
}

/// Compose the [`LaunchSpec`] for running `choice` in `worktree`. Records the
/// choice (and any sandbox backend) in the DB, mirroring the zellij path's
/// side effects so the dashboard/`--resume` keep working. Errors when an
/// explicit sandbox choice cannot be honored (no silent host fallback).
///
/// `branch` is the worktree's branch (for the pane env + title); `None` falls
/// back to the worktree basename.
///
/// `scoped_key` is an optional per-agent API key (e.g. a virtual key from the
/// LLM proxy). When set, it is injected into the sandbox environment as
/// `ANTHROPIC_API_KEY`, masking the host passthrough value so the master key
/// is never exposed inside the sandbox.
pub fn launch_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    launch_spec_with_key(cfg, worktree, branch, choice, None, false)
}

/// Like [`launch_spec`] but injects a scoped API key for the sandbox.
///
/// `sync_warm` gates the `direnv` cache warm: `false` kicks the async
/// background warm (the first launch of a cold worktree falls back), `true`
/// warms synchronously + bounded before composing the spec (off-loop callers
/// only — see [`crate::direnv_warm::launch_spec_synced`]).
pub fn launch_spec_with_key(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    scoped_key: Option<String>,
    sync_warm: bool,
) -> anyhow::Result<LaunchSpec> {
    let loc = GitLoc::for_worktree(Path::new(worktree));

    // Record the choice for the dashboard / `--resume` (keyed by worktree path).
    // Two launches are deliberately NOT recorded as the worktree's remembered
    // agent: the transient `clean-shell` watchdog fallback (the user may fix
    // their dotfiles), and tool drawers (yazi/lazygit/editor/diff) — those are
    // overlays, not the worktree's agent, and are auto-prewarmed on every switch,
    // so recording them would clobber the real choice on every worktree.
    let saved_backend = match Db::open() {
        Ok(db) => {
            if choice != "clean-shell" && cfg.tool_command(choice).is_none() {
                let _ = db.set_worktree_agent(worktree, choice);
            }
            db.worktree_sandbox(worktree).ok().flatten()
        }
        Err(_) => None,
    };

    // The local repo root drives the per-repo sandbox overlay + slug. Prefer the
    // DB (carries remote worktrees with no local cwd), else climb from the path.
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));

    // The selected execution environment: the worktree's own `env_name`, else
    // its workspace's. `resolve_env` falls through to the repo/global default.
    let selected_env = Db::open()
        .ok()
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
        launch_scope(cfg, choice),
        selected_env.as_deref(),
    )?;
    // NB: the resolved backend is intentionally NOT written back. `sandbox_backend`
    // is a deliberate-override store (mirrors `env_name`, sole writer = the
    // wizard's divergence check); auto-stamping it would pin every auto worktree.

    // Provision the repo into a fresh provider env on open (8-A.3): clone origin
    // into the sandbox workdir so the chrome's git/files show real data. `outcome
    // .location` is set only for a `Placement::Provider` env; idempotent + a
    // no-op for `data=sync` (whose upload already populated the tree).
    if outcome.location.is_some() {
        provision_provider_repo(&repo_root, &loc, branch);
    }

    // Bouncer (opt-in): inject the agent's proxy + tool-override env into the
    // sealed container's `env_overrides` (+ the control-socket mounts) before the
    // argv is composed. No-op unless bouncer is on and `choice` is an agent.
    let bouncer = apply_bouncer_launch(cfg, worktree, choice, &mut outcome);

    // Apply per-agent credential scoping: when a virtual key is provided,
    // inject it as an override and mask the master key so it's never forwarded.
    if let (Some(key), Some(spec)) = (scoped_key, outcome.spec.as_mut()) {
        spec.env_overrides
            .insert("ANTHROPIC_API_KEY".to_string(), key);
        spec.env_block.push("ANTHROPIC_API_KEY".to_string());
        // Remove the master key from the spec's env vec too so it doesn't
        // reach the container via the passthrough path.
        spec.env.retain(|(k, _)| k != "ANTHROPIC_API_KEY");
    }

    // Environment bundles (AU): resolve the active bundle(s) for this scope into
    // env overrides + credential/config-dir redirection + account selection, and
    // fold them into the sandbox spec (or, on the host fallback, the pane env
    // below). This subsumes the old inline account-switch (item 656) — a plain
    // account selection is just a bundle with no bundle bound (`compose` folds
    // the legacy per-provider active account when nothing else set it). Local
    // worktrees only — a remote agent runs where the host's cred dirs don't exist.
    let resolved = (!loc.is_remote())
        .then(|| Db::open().ok())
        .flatten()
        .map(|db| {
            let slug = repo_slug(&db, &repo_root);
            // At launch (off the event loop) so secret resolvers may run.
            bundle::compose_at_launch(cfg, &db, worktree, slug.as_deref(), Some(choice))
        })
        .unwrap_or_default();
    // Credential/HOME dirs the child writes into must exist before launch.
    for dir in &resolved.ensure_dirs {
        let _ = std::fs::create_dir_all(dir);
    }
    // Tier-2 dotfiles: materialize each active bundle's dotfile tree into its
    // managed HOME (idempotent, off the event loop — launch_spec is blocking).
    if !loc.is_remote()
        && let Ok(db) = Db::open()
    {
        let slug = repo_slug(&db, &repo_root);
        for name in bundle::active_chain(cfg, &db, worktree, slug.as_deref()) {
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
        for (host, ro) in superzej_core::profile::sandbox_cred_mounts() {
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

    let mut spec = compose_spec(cfg, worktree, branch, choice, &loc, &outcome);
    // On the host path (no sandbox spec) the bundle identity + build env ride
    // the pane env (layered on the curated base in `spawn_with_env`).
    if outcome.spec.is_none() {
        spec.env.extend(resolved.env_pairs());
        spec.env.extend(build_env);
        // Host agent panes: proxy-routing vars ride the pane env (from bouncer
        // mode's host fallback, or plain `route_agent` host routing). Sandboxed
        // agents already got them via env_overrides; shells get nothing.
        spec.env.extend(bouncer.host_env);
    }
    // Host (no-sandbox) devShell injection rides the pane env directly.
    if outcome.spec.is_none()
        && let Some(dev) = &devshell
    {
        inject_devshell_host(&mut spec, dev);
    }
    Ok(spec)
}

/// Resolve `worktree`'s sandbox and run a one-shot shell command inside it,
/// returning combined stdout+stderr. Services ACP `terminal/create` so the
/// agent's shell commands run inside the same policy boundary (container /
/// bwrap / none) as its interactive pane — superzej is the agent's "hands and
/// bouncer". BLOCKING (sandbox resolution may ensure a container); callers must
/// run it off the event loop.
// off-loop: only called inside spawn_blocking (ACP terminal/create servicing, run.rs).
#[expect(clippy::disallowed_methods)]
pub fn run_in_sandbox(cfg: &Config, worktree: &str, command: &str) -> anyhow::Result<String> {
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let saved_backend = Db::open()
        .ok()
        .and_then(|db| db.worktree_sandbox(worktree).ok().flatten());
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

    // In bouncer mode the command came from the *sealed agent* — run it inside the
    // agent's own (`agent_profile`) container, the same boundary its interactive
    // pane runs in, not the worktree shell. Otherwise the worktree shell scope.
    let scope = if cfg.llm_proxy.bouncer {
        SandboxScope::Agent
    } else {
        SandboxScope::Shell
    };
    // A recorded per-worktree backend is a deliberate override (see
    // `launch_spec_with_key`); honour it so ACP shell commands run in the same
    // boundary as the interactive pane. Empty/NULL → auto (re-resolve vs config).
    let saved_is_override = saved_backend
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty() && s != "auto");
    let outcome = prepare_sandbox_env(
        cfg,
        &repo_root,
        worktree,
        &loc,
        saved_backend.as_deref(),
        saved_is_override,
        scope,
        selected_env.as_deref(),
    )?;

    let argv = match &outcome.spec {
        Some(spec) => sandbox::enter_argv(spec, command),
        None => vec![
            superzej_core::util::shell(),
            "-lc".to_string(),
            command.to_string(),
        ],
    };

    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    // Local worktree: run from its dir so relative paths resolve as the agent expects.
    if !loc.is_remote() && !outcome.is_remote {
        cmd.current_dir(worktree);
    }
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("spawn `{}` failed: {e}", argv.join(" ")))?;
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }
    Ok(combined)
}

/// Mint (or refresh) a per-worktree virtual key so the agent's model traffic
/// routes through `szproxy` scoped to `agent:pi:<worktree>` — the proxy then
/// attributes spend and enforces budgets per worktree. Returns the bearer token
/// to hand the agent (best-effort; `None` if the DB is unavailable). Revoke it
/// with [`revoke_agent_proxy_key`] when the agent disconnects. Used by the
/// non-bouncer (TCP) path, which holds the minted token in scope for revocation.
pub fn mint_agent_proxy_key(worktree: &str) -> Option<String> {
    let slug = superzej_core::util::slugify(worktree);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    put_proxy_key(worktree, &format!("szk-{slug}-{nanos}"))
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
pub fn mint_stable_proxy_key(worktree: &str) -> Option<String> {
    let key = agent_proxy_key_id(worktree);
    put_proxy_key(worktree, &key)
}

/// Persist a virtual key row for `worktree` and return the token. The proxy
/// looks up identity by the token itself; the hash column is stored for parity
/// with the schema (lookups don't verify it for a local daemon).
fn put_proxy_key(worktree: &str, key: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let db = Db::open().ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let token_hash = format!("{:016x}", hasher.finish());
    let scope = format!("agent:pi:{worktree}");
    db.put_proxy_virtual_key(
        key,
        &token_hash,
        &format!("pi agent {worktree}"),
        &scope,
        None,
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

/// The sandbox scope for launching `choice`: the sealed `agent_profile` when the
/// bouncer is on and `choice` is a configured agent (so the agent runs in its
/// own hardened container), else the worktree's interactive shell scope.
pub fn launch_scope(cfg: &Config, choice: &str) -> SandboxScope {
    if cfg.llm_proxy.bouncer && cfg.agent_command(choice).is_some() {
        SandboxScope::Agent
    } else {
        SandboxScope::Shell
    }
}

/// What a bouncer launch produced for the caller to carry forward.
#[derive(Debug, Default)]
pub struct BouncerLaunch {
    /// Env vars for a **host** (non-sandboxed) agent pane — already injected into
    /// the sandbox spec's `env_overrides` when sandboxed, so empty in that case.
    pub host_env: Vec<(String, String)>,
}

/// Whether `choice` is a real routable agent (not the plain/clean shell). The
/// picker carries "shell"/"clean-shell" as `[[agents]]` entries with the
/// `__shell__` sentinel command, so a name lookup alone isn't enough.
fn is_agent_choice(cfg: &Config, choice: &str) -> bool {
    !choice.is_empty()
        && choice != SHELL
        && choice != "clean-shell"
        && cfg
            .agent_command(choice)
            .map(|c| c.trim() != "__shell__")
            .unwrap_or(false)
}

/// Inject the agent's proxy env into the launch, for a configured agent `choice`
/// (never a plain shell). Two modes:
///
/// - **Bouncer on:** the full proxy + tool-override plan (+ control-socket mounts)
///   into the sealed sandbox's `env_overrides`, minting the stable per-worktree
///   proxy key superzej's own szproxy validates. A host fallback returns the vars
///   to ride the pane env.
/// - **Bouncer off, `route_agent` on, HOST pane:** point the agent at the proxy
///   over the host loopback directly (no tunnel/relay). Local sandboxes are left to
///   the bouncer; remote sprites to the native-exec path (`remote_agent_env`).
///
/// No-op otherwise. See [`crate::bouncer::agent_env_plan`] /
/// [`superzej_core::config::LlmProxyConfig::local_agent_env`].
pub fn apply_bouncer_launch(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    outcome: &mut SandboxOutcome,
) -> BouncerLaunch {
    // Only a configured agent routes through the proxy; a plain shell never does
    // (the picker lists "shell"/"clean-shell" as `[[agents]]` entries whose command
    // is the `__shell__` sentinel — `agent_command` finds them, so exclude them).
    if !is_agent_choice(cfg, choice) {
        return BouncerLaunch::default();
    }
    if cfg.llm_proxy.bouncer {
        let key = cfg
            .llm_proxy
            .route_agent
            .then(|| mint_stable_proxy_key(worktree))
            .flatten();
        let sandbox = outcome.spec.as_ref().map(|s| (s.backend, s.network));
        let plan = crate::bouncer::agent_env_plan(cfg, worktree, sandbox, key.as_deref());
        return match outcome.spec.as_mut() {
            Some(spec) => {
                for (k, v) in plan.vars {
                    spec.env_overrides.insert(k, v);
                }
                spec.mounts.extend(plan.mounts);
                BouncerLaunch::default()
            }
            // Host fallback (no isolation): the bouncer override is inert, but the
            // proxy vars still ride the pane env.
            None => BouncerLaunch {
                host_env: plan.vars,
            },
        };
    }
    // No bouncer, but `route_agent` on for a HOST pane: route the agent through the
    // proxy over the host loopback directly. (Sandboxed panes can't reach host
    // loopback without a relay — that's the bouncer's / sprite tunnel's job.)
    if cfg.llm_proxy.route_agent && outcome.spec.is_none() {
        return BouncerLaunch {
            host_env: cfg.llm_proxy.local_agent_env(),
        };
    }
    BouncerLaunch::default()
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
fn repo_slug(db: &Db, repo_root: &Path) -> Option<String> {
    let base = repo_root.file_name()?.to_string_lossy().into_owned();
    db.slug_for_repo(&repo_root.to_string_lossy(), &base).ok()
}

fn sandbox_candidates(
    sb: &superzej_core::config::SandboxConfig,
) -> Vec<superzej_core::config::SandboxConfig> {
    if sb.backend != superzej_core::config::SandboxBackend::Auto {
        return vec![sb.clone()];
    }
    let mut out = Vec::new();
    for name in &sb.backend_chain {
        if let Ok(backend) = superzej_core::config::SandboxBackend::from_str_validated(name) {
            let mut c = sb.clone();
            c.backend = backend;
            out.push(c);
        }
    }
    if !out
        .iter()
        .any(|c| c.backend == superzej_core::config::SandboxBackend::None)
    {
        let mut c = sb.clone();
        c.backend = superzej_core::config::SandboxBackend::None;
        out.push(c);
    }
    out
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
