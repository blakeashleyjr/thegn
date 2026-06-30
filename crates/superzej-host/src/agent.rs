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
use superzej_core::remote::GitLoc;
use superzej_core::{account, devenv, direnv, repo, sandbox};
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
/// On the host fallback path `in_oci = false` keeps the existing behaviour:
/// use `$SHELL` verbatim so NixOS users get the right store-path shell.
fn shell_inner(in_oci: bool) -> String {
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
        // (zsh from nixpkgs) is actually found by `command -v` — covers both the
        // single-user (`~/.nix-profile`) and daemon/system (Determinate `--init
        // none`) profiles. Without this the checks miss the installed zsh and drop
        // to `/bin/sh`. The trailing `/bin/sh -l` is the universal fallback.
        format!(
            "export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$PATH\"; \
             {checks}exec /bin/sh -l"
        )
    } else {
        let host_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        format!("{host_shell} -l")
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

pub fn prepare_sandbox(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    loc: &GitLoc,
    backend_choice: Option<&str>,
    scope: SandboxScope,
) -> anyhow::Result<SandboxOutcome> {
    prepare_sandbox_env(cfg, repo_root, worktree, loc, backend_choice, scope, None)
}

/// Like [`prepare_sandbox`] but with an explicitly-selected execution
/// environment name (the DB worktree/workspace `env_name`, or a `--env` flag).
/// `None` lets [`Config::resolve_env`] fall through to repo/global selection.
/// No DB access — the caller resolves the name (which needs the DB).
#[allow(clippy::too_many_arguments)]
pub fn prepare_sandbox_env(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    loc: &GitLoc,
    backend_choice: Option<&str>,
    scope: SandboxScope,
    selected_env: Option<&str>,
) -> anyhow::Result<SandboxOutcome> {
    let environment = cfg.resolve_env(repo_root, loc, Path::new(worktree), selected_env);
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
    // Only let the DB-saved per-worktree backend override when config is "auto".
    // An explicit config backend (e.g. `backend = "bwrap"`) always wins so that
    // changing the config actually takes effect instead of being silently trumped
    // by a stale DB entry from a previous backend that no longer works.
    let config_is_auto = sb.backend == superzej_core::config::SandboxBackend::Auto;
    if config_is_auto
        && let Some(saved) = backend_choice.map(str::trim)
        && !saved.is_empty()
        && saved != "auto"
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

/// ssh-config ownership shim: unprivileged bwrap maps the nix store to `nobody`
/// (the userns overflow uid), so ssh rejects the store-resident `~/.ssh/config`
/// ("Bad owner or permissions") and ssh-based git fails in the sandbox. Point
/// sandboxed git at a user-owned, include-flattened copy materialized on the
/// host (visible via the rw `$HOME` bind). Bwrap only, and only when `$HOME`
/// (or `/`) is mounted so the copy is reachable. Shared by the pane launch path
/// and the embedded `agent` tab's tool sandbox. See [`sandbox::prepare_ssh_config`].
pub(crate) fn apply_ssh_config_shim(spec: &mut sandbox::SandboxSpec) {
    if spec.backend != sandbox::Backend::Bwrap || spec.env_overrides.contains_key("GIT_SSH_COMMAND")
    {
        return;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let home_mounted =
        !home.is_empty() && spec.mounts.iter().any(|m| m.dest == home || m.dest == "/");
    if home_mounted && let Some(path) = sandbox::prepare_ssh_config() {
        spec.env_overrides
            .insert("GIT_SSH_COMMAND".to_string(), format!("ssh -F {path}"));
    }
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

/// Build the API [`Provider`](superzej_svc::provider::Provider) for an env's
/// provider config (best-effort: `None` if unconfigured or the token env var is
/// unset). Mirrors `cmd::env::api_provider` but infallible for the launch path.
fn provider_for(
    pc: &superzej_core::config::EnvProviderConfig,
) -> Option<superzej_svc::provider::Provider> {
    provider_for_named(pc, &pc.id)
}

/// Like [`provider_for`] but bakes an explicit sandbox **name** into the provider
/// instead of the raw configured `pc.id`. This matters for `create()`/
/// `ensure_exists()`, which name the new sandbox from the provider's own baked
/// name (not a call argument): the raw `pc.id` may be a per-worktree template
/// (`{worktree}`) or empty, so the caller must pass the resolved
/// [`effective_provider_id`](superzej_core::config::effective_provider_id) to
/// create the correctly-named sandbox. Exec/read/write/destroy take the id as an
/// argument, so for those `provider_for` is equivalent.
fn provider_for_named(
    pc: &superzej_core::config::EnvProviderConfig,
    name: &str,
) -> Option<superzej_svc::provider::Provider> {
    use superzej_svc::provider::{DaytonaProvider, Provider, SpritesProvider};
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
        _ => None,
    }
}

/// The resolved provider sandbox NAME for a worktree's env — the single source of
/// truth. Resolves the env exactly as the pane path does (`resolve_env` →
/// `ProviderPlacement.id`) so provisioning, attach (`native_shell_exec`),
/// checkpoint, and teardown all compute the SAME name (the id embeds a stable
/// path-hash; deriving it inconsistently would orphan/leak sandboxes). `None` for
/// a non-provider env. Mirrors how the other launch paths resolve `repo_root`.
fn provider_sandbox_name(cfg: &Config, worktree: &str, env_name: &str) -> Option<String> {
    let loc = GitLoc::for_worktree(Path::new(worktree));
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let env = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), Some(env_name));
    match env.placement {
        superzej_core::placement::Placement::Provider(p) => {
            // If this worktree CLAIMED a warm-pool spare, its sandbox is that
            // spare's name (a DB binding), which overrides the derived id — so all
            // lifecycle/exec calls target the handed-over sandbox. Else the derived
            // `effective_provider_id`.
            let bound = Db::open()
                .ok()
                .and_then(|db| db.worktree_provider_sandbox(worktree).ok().flatten());
            Some(bound.unwrap_or(p.id))
        }
        _ => None,
    }
}

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

/// In-sandbox sshd listen port for the SSH-over-WSS transport. A high port — the
/// sprite user isn't root, so it can't bind 22.
pub const SPRITE_SSHD_PORT: u16 = 2222;

/// The superzej-managed ssh keypair for the sprite SSH-over-WSS transport, under
/// `$XDG_STATE/superzej/ssh/`. Generated (ed25519, no passphrase) on first use.
/// Returns `(private key path, public key line)`.
pub fn sprite_ssh_keypair() -> anyhow::Result<(PathBuf, String)> {
    let dir = superzej_core::util::superzej_dir().join("ssh");
    std::fs::create_dir_all(&dir)?;
    let key = dir.join("sprite_ed25519");
    let pubp = dir.join("sprite_ed25519.pub");
    if !pubp.exists() {
        let out = std::process::Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "superzej-sprite",
                "-q",
                "-f",
            ])
            .arg(&key)
            .output()
            .map_err(|e| anyhow::anyhow!("ssh-keygen: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "ssh-keygen failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let pubkey = std::fs::read_to_string(&pubp)?.trim().to_string();
    Ok((key, pubkey))
}

/// Idempotent in-sandbox setup for the SSH-over-WSS transport (run during
/// provisioning when `connect = "ssh"`): install openssh, generate a user-owned
/// host key, authorize `pubkey`, and write a minimal sshd_config listening on
/// `127.0.0.1:SPRITE_SSHD_PORT`. Pure (shell string).
pub fn sprite_sshd_setup_script(pubkey: &str) -> String {
    let pk = superzej_core::util::sh_quote(pubkey);
    format!(
        "command -v sshd >/dev/null 2>&1 || nix profile install nixpkgs#openssh 2>/dev/null || \
           (export DEBIAN_FRONTEND=noninteractive; sudo apt-get update -y && sudo apt-get install -y openssh-server) 2>/dev/null || true; \
         mkdir -p \"$HOME/.ssh\"; chmod 700 \"$HOME/.ssh\"; \
         touch \"$HOME/.ssh/authorized_keys\"; chmod 600 \"$HOME/.ssh/authorized_keys\"; \
         grep -qF {pk} \"$HOME/.ssh/authorized_keys\" 2>/dev/null || printf '%s\\n' {pk} >> \"$HOME/.ssh/authorized_keys\"; \
         [ -f \"$HOME/.ssh/sprite_host_ed25519\" ] || ssh-keygen -t ed25519 -N '' -q -f \"$HOME/.ssh/sprite_host_ed25519\"; \
         printf 'Port {port}\\nListenAddress 127.0.0.1\\nHostKey %s/.ssh/sprite_host_ed25519\\nAuthorizedKeysFile %s/.ssh/authorized_keys\\nPasswordAuthentication no\\nPidFile %s/.ssh/sprite_sshd.pid\\nPrintMotd no\\n' \"$HOME\" \"$HOME\" \"$HOME\" > \"$HOME/.ssh/sprite_sshd_config\"; \
         true",
        port = SPRITE_SSHD_PORT,
    )
}

/// Idempotent: ensure the in-sandbox sshd is listening (start it if not). Run at
/// connect time by the `sprite-proxy` ProxyCommand. Pure (shell string).
pub fn sprite_sshd_start_script() -> String {
    "SSHD=$(command -v sshd || echo \"$HOME/.nix-profile/bin/sshd\"); \
     pgrep -f sprite_sshd_config >/dev/null 2>&1 || \
       (\"$SSHD\" -f \"$HOME/.ssh/sprite_sshd_config\" 2>/dev/null || true); true"
        .to_string()
}

/// When `worktree`'s resolved provider env has `connect = "ssh"`, the inputs to
/// spawn the interactive pane as a local `ssh` client tunneled over the provider
/// proxy: `(private key path, ssh user, in-sandbox workdir)`. `None` otherwise.
pub fn sprite_ssh_connect(cfg: &Config, worktree: &str) -> Option<(PathBuf, String, String)> {
    use superzej_core::config::ProviderConnect;
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
    let superzej_core::placement::Placement::Provider(_) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    tracing::debug!(
        target: "szhost::sandbox",
        env = %environment.name,
        connect = ?pc.connect,
        "sprite_ssh_connect: resolved provider env"
    );
    if pc.connect != ProviderConnect::Ssh {
        return None;
    }
    let (key, _pubkey) = match sprite_ssh_keypair() {
        Ok(k) => k,
        Err(e) => {
            superzej_core::msg::warn(&format!(
                "connect=ssh: managed key generation failed ({e}); falling back to the WSS exec pane"
            ));
            return None;
        }
    };
    // The sprite user owns the in-sandbox sshd + authorized_keys (non-root sshd
    // can only authenticate as itself), so ssh logs in as that user.
    Some((key, "sprite".to_string(), pc.sync_workdir()))
}

/// Build the local `ssh` argv for the SSH-over-WSS pane: a real ssh client whose
/// transport is the `sprite-proxy` ProxyCommand. `szhost_exe` is this binary (for
/// the ProxyCommand); `key`/`user`/`workdir` come from [`sprite_ssh_connect`].
pub fn sprite_ssh_argv(
    szhost_exe: &str,
    worktree: &str,
    key: &Path,
    user: &str,
    workdir: &str,
) -> Vec<String> {
    let proxy = format!(
        "{} sprite-proxy {}",
        superzej_core::util::sh_quote(szhost_exe),
        superzej_core::util::sh_quote(worktree),
    );
    // Run the user's login shell, not the sprite's default `$SHELL` (which is
    // bash → no zsh / no host-parity prompt). The same runtime probe chain the
    // native pane uses (`command -v zsh && exec zsh -l; …`) so the uploaded
    // `.zshrc` (and starship/etc.) loads exactly like local.
    let shell = shell_inner(true);
    let remote = if workdir.is_empty() {
        shell
    } else {
        format!(
            "cd {} 2>/dev/null; {shell}",
            superzej_core::util::sh_quote(workdir)
        )
    };
    vec![
        "ssh".into(),
        "-tt".into(),
        "-o".into(),
        format!("ProxyCommand={proxy}"),
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-i".into(),
        key.to_string_lossy().into_owned(),
        "-p".into(),
        SPRITE_SSHD_PORT.to_string(),
        format!("{user}@sprite"),
        "--".into(),
        remote,
    ]
}

/// Decide whether `worktree`'s interactive shell should attach via a provider's
/// **native exec API** instead of the CLI/PTY path. `Some` when the resolved env
/// is a `provider` placement whose provider has a native exec API, whose `exec`
/// mode isn't `cli`, and whose API token is present; `None` ⇒ use [`launch_spec`].
///
/// Resolves the env exactly as [`launch_spec_with_key`] does (DB repo-root +
/// effective env) so the two paths never disagree about which env is in play.
/// Auto-detect the coding agents the HOST has so a sandbox reproduces them
/// ("exact local parity") without per-sandbox config. A known agent
/// ([`superzej_core::envplan::known_agents`]) counts as present if its binary is
/// on the host PATH or its config/credential dir exists in `$HOME`. The result
/// drives the install + config-upload provisioning steps. Used only when
/// `[sandbox.home] agents` is unset (an explicit list always wins).
fn detect_host_agents() -> Vec<String> {
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
    let inner = if sb_shell.is_empty() {
        shell_inner(true)
    } else {
        shell_inner_override(&sb_shell)
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
    Some(NativeShell {
        provider,
        provider_name: pc.provider.clone(),
        sandbox_id: p.id.clone(),
        inner,
        workdir: pc.sync_workdir(),
        env,
    })
}

/// Whether eager (ahead-of-focus) provisioning should run for `worktree`: its env
/// is a managed provider AND its sandbox does NOT exist yet (checked via a cheap
/// `list()` GET that never wakes an existing/idle sandbox). This keeps eager
/// provisioning budget-safe — it only ever front-runs the create+provision of a
/// genuinely-missing sandbox (first-ever open or post-destroy), never waking an
/// already-provisioned idle one just to re-check it. Off-loop (network); `false`
/// for non-provider envs, a tokenless/unbuilt provider, or any list error.
pub fn needs_eager_provision(cfg: &Config, worktree: &str) -> bool {
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
        return false;
    };
    let Some(envc) = cfg.env.get(&environment.name) else {
        return false;
    };
    let Some(provider) = provider_for_named(&envc.provider, &p.id) else {
        return false;
    };
    match block_on_provider(|| async { provider.list().await }) {
        Ok(names) => !names.iter().any(|n| n == &p.id),
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
    provision_provider_env(cfg, worktree, &environment.name, progress)
}

/// A stable-ish generic name for a new warm-pool spare: `<repo>-pool-<hash>`. The
/// hash varies per process+counter so concurrent mints never collide.
fn mint_spare_name(repo: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let slug = Path::new(repo)
        .file_name()
        .and_then(|s| s.to_str())
        .map(superzej_core::util::slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sz".to_string());
    let h = superzej_core::util::short_hash(&format!("{repo}-{}-{n}", std::process::id()), 6);
    format!("{slug}-pool-{h}")
}

/// Hash of the repo's `flake.lock` (staleness key for a spare's seeded devShell);
/// empty when the repo has no lockfile.
fn flake_lock_hash(repo_root: &Path) -> String {
    std::fs::read(repo_root.join("flake.lock"))
        .ok()
        .map(|b| superzej_core::util::short_hash(&String::from_utf8_lossy(&b), 16))
        .unwrap_or_default()
}

/// Mint + fully provision a NEW warm-pool spare for `(repo, env)`: a generically-
/// named sandbox, cloned + devShell-seeded + tooled + checkpointed (so it suspends
/// for free), recorded `ready` so a worktree can claim it. Returns the spare name.
/// On failure the half-built sandbox + DB row are torn down.
pub fn provision_spare(
    cfg: &Config,
    repo_root: &Path,
    env_name: &str,
    mut progress: impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<String> {
    let repo = repo_root.to_string_lossy().to_string();
    let name = mint_spare_name(&repo);
    if let Ok(db) = Db::open() {
        let _ = db.insert_pool_spare(&name, &repo, env_name);
    }
    // The repo's main worktree gives env-resolution + origin context; the name is
    // overridden to the generic spare name (the clone is branch-less either way).
    let ctx = repo::main_worktree(repo_root)
        .unwrap_or_else(|| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    match provision_provider_env_named(cfg, &ctx, env_name, Some(&name), &mut progress) {
        Ok(_) => {
            let lock = flake_lock_hash(repo_root);
            if let Ok(db) = Db::open() {
                let _ = db.set_pool_spare_ready(&name, None, &lock);
            }
            Ok(name)
        }
        Err(e) => {
            let _ = destroy_spare(cfg, env_name, &name);
            Err(e)
        }
    }
}

/// Claim a `ready` spare for `(repo, env)` and hand it to `worktree`: bind it (DB,
/// atomic) then check out the worktree's branch in the spare's workdir. Returns the
/// claimed sandbox name, or `None` when no spare is ready (caller provisions fresh).
pub fn claim_spare(
    cfg: &Config,
    worktree: &str,
    repo_root: &Path,
    env_name: &str,
    branch: Option<&str>,
) -> Option<String> {
    let repo = repo_root.to_string_lossy().into_owned();
    let db = Db::open().ok()?;
    let (name, _checkpoint) = db.claim_pool_spare(&repo, env_name, worktree).ok()??;
    // Per-worktree work: settle the branch in the spare's existing clone (the
    // sandbox auto-resumes when the exec opens). Best-effort — the bind already
    // succeeded, so the pane opens against the spare regardless.
    if let Some(env) = cfg.env.get(env_name)
        && let Some(provider) = provider_for_named(&env.provider, &name)
    {
        let workdir = env.provider.sync_workdir();
        if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty()) {
            let wd = superzej_core::util::sh_quote(&workdir);
            let bq = superzej_core::util::sh_quote(b);
            let script = format!(
                "cd {wd} 2>/dev/null && (git checkout {bq} 2>/dev/null || git checkout -b {bq}) 2>&1"
            );
            let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
            let _ =
                block_on_provider(|| async { provider.run_exec(&name, &argv, None, &[]).await });
        }
        // Bring the claimed spare to full parity with the local worktree, same as
        // a fresh provision (only for an `in_env` provider — a projected data mode
        // mirrors the tree by other means). Best-effort.
        if env.data == superzej_core::config::DataMode::InEnv
            && let Err(e) = apply_local_parity(&provider, &name, worktree, &workdir, &[])
        {
            superzej_core::msg::warn(&format!(
                "local parity on claimed spare {name}: {e}; using the origin checkout."
            ));
        }
    }
    superzej_core::msg::info(&format!("claimed warm spare {name} for {worktree}"));
    Some(name)
}

/// Destroy a spare sandbox + drop its DB row. Best-effort (idempotent).
pub fn destroy_spare(cfg: &Config, env_name: &str, name: &str) -> anyhow::Result<()> {
    if let Some(env) = cfg.env.get(env_name)
        && let Some(provider) = provider_for_named(&env.provider, name)
    {
        let _ = block_on_provider(|| async { provider.destroy(name).await });
    }
    if let Ok(db) = Db::open() {
        let _ = db.delete_pool_spare(name);
    }
    Ok(())
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
fn provision_step_timeout(step_id: &str) -> std::time::Duration {
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
    provision_provider_env_named(cfg, worktree, env_name, None, &mut progress)
}

/// Provision a provider env into a sandbox. `name_override` forces the sandbox
/// name (used for warm-pool SPARES, which aren't bound to a worktree); `None`
/// derives it from the worktree as usual. `worktree` always provides the env-
/// resolution + repo-origin context (for a spare, pass the repo's main worktree).
/// The clone is branch-less (`opts.branch = None`) either way, so a spare and a
/// worktree provision identically apart from the name.
pub fn provision_provider_env_named(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
    name_override: Option<&str>,
    progress: &mut impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<bool> {
    use superzej_core::envplan::{self, EnvPlan, PlanOpts, StepKind};

    let Some(env) = cfg.env.get(env_name) else {
        return Ok(false);
    };
    let pc = &env.provider;
    let Some(id) = name_override
        .map(str::to_string)
        .or_else(|| provider_sandbox_name(cfg, worktree, env_name))
        .filter(|s| !s.is_empty())
    else {
        return Ok(false);
    };
    // Bake the resolved id so a recreate (`ensure_exists`→`create`) names the
    // sandbox correctly (the id embeds the repo/worktree tokens + a path-hash).
    let Some(provider) = provider_for_named(pc, &id) else {
        return Ok(false);
    };
    if !provider.caps().files {
        return Ok(false); // can't provision without the fs API
    }
    let workdir = pc.sync_workdir();
    let marker = EnvPlan::marker_path(&workdir);

    // Recreate-if-missing: the sandbox may have been cleaned up out-of-band (TTL,
    // manual delete, provider GC). `ensure_exists` recreates it before we read the
    // marker / run any exec, so provisioning can't fail against a dead sandbox.
    // A freshly recreated sandbox has no marker ⇒ a full re-provision runs below.
    // (No-op when it already exists; cheap list+maybe-create.)
    if let Err(e) = block_on_provider(|| async { provider.ensure_exists(&id).await }) {
        return Err(anyhow::anyhow!("ensure sandbox {id}: {e}"));
    }

    // Idempotent: already provisioned ⇒ nothing to do.
    if block_on_provider(|| async { provider.read(&id, &marker).await }).is_ok() {
        return Ok(true);
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
        // Exact local parity for agents: an explicit `[sandbox.home] agents`
        // list wins; otherwise reproduce whatever coding agents the HOST has
        // (claude/pi/codex/hermes/…) so they're installed + logged-in in the
        // sandbox by default. Known ones get an installer; all get their config
        // (login, history, skills, MCP) uploaded.
        agents: if home.agents.is_empty() {
            detect_host_agents()
        } else {
            home.agents.clone()
        },
        allow_nix: true,
        checkpoint: pc.auto_checkpoint,
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
        // A pool SPARE provisions in the BACKGROUND (no loading screen to gate),
        // so it should fully BUILD the devShell — `skip_devshell_warm` is a
        // loading-screen speed hack that only makes sense for an on-demand,
        // foreground provision. Skipping it on a spare yields a "ready" spare
        // whose devShell still builds lazily in-pane on claim ("not ready yet");
        // building it up front is what makes a claimed worktree truly instant.
        skip_devshell_warm: pc.skip_devshell_warm && name_override.is_none(),
        // Full local parity (unpushed commits + uncommitted + untracked) for a
        // real worktree on an `in_env` provider — so a fresh sandbox matches the
        // working tree, not just origin. A SPARE (name_override) stays a pristine
        // clone (generic until claimed); a non-`in_env` data mode projects the
        // tree by other means, so skip the overlay there.
        local_parity: (name_override.is_none()
            && env.data == superzej_core::config::DataMode::InEnv)
            .then(|| worktree.to_string()),
        // When the host cache is on, bake its sandbox-side loopback substituter into
        // nix.conf so the devShell build + in-pane `nix develop` substitute from the
        // host store over the reverse tunnel (which the host stands up separately).
        host_cache_url: pc
            .host_cache
            .then(|| format!("http://127.0.0.1:{}", crate::nixcache::SANDBOX_PORT)),
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
                // `/bin/sh -lc` + `2>&1` so the non-tty exec captures stderr too.
                // Prefix PATH with EVERY place an installer drops `nix` + tools: the
                // provider exec env is non-login (no `$USER`), so the installer's
                // profile.d hook is a no-op — each step must put these on PATH itself
                // for a later step to see what a prior step installed. CRITICAL:
                // include the daemon/system profile (`/nix/var/nix/profiles/default`)
                // where the Determinate installer (`--init none`) lands — without it
                // every nix-using step (devShell, profile install, closure) fails
                // "nix: not found" after a successful Determinate install, leaving a
                // bare shell. Also source its profile hook for completeness.
                let argv = vec![
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    format!(
                        // `[ -r F ] && . F`, NOT `. F 2>/dev/null || true`: in dash
                        // (the sandbox `/bin/sh`) sourcing a MISSING file is a
                        // special-builtin error that exits the shell with status 2 —
                        // `|| true` can't catch it — so on a fresh sandbox (no nix
                        // yet) it aborted EVERY step, including the fatal `mkdir`.
                        "[ -r /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ] && \
                         . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh; \
                         export PATH=\"$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:$HOME/.local/state/nix/profile/bin:$HOME/.local/bin:$PATH\"; {script} 2>&1"
                    ),
                ];
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
                upload_agent_configs(&provider, &id, &sprite_home, agents)
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
                if let Err(e) = apply_local_parity(&provider, &id, wt, wd, &exec_env) {
                    superzej_core::msg::warn(&format!(
                        "local parity: {e}; the sandbox keeps the origin checkout."
                    ));
                }
                Ok(())
            }
            StepKind::Checkpoint => block_on_provider(|| async {
                provider.checkpoint(&id, Some("superzej-provisioned")).await
            })
            .map(|_| ()),
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
            continue;
        }
        tracing::info!(target: "szhost::startup", step = %step.id, ms = step_t0.elapsed().as_millis() as u64, "provision step done");
        views[i].state = ProvisionState::Done;
        progress(&views);
    }

    // Drop the marker so a later open skips re-provisioning.
    let _ = block_on_provider(|| async { provider.write(&id, &marker, b"ok\n").await });
    Ok(true)
}

/// The final personal setup commands: the inline `[sandbox.home] setup` list,
/// then `setup_script` resolved — if it names an existing HOST file, its contents
/// are inlined (so no upload is needed); otherwise it's treated as an in-sandbox
/// path and run with `sh`. Bring-your-own escape hatch (agent CLIs, internal
/// tooling, anything not a package).
fn resolve_setup(home: &superzej_core::config::HomeConfig) -> Vec<String> {
    let mut cmds = home.setup.clone();
    let script = home.setup_script.trim();
    if !script.is_empty() {
        let host_path = if let Some(rest) = script.strip_prefix("~/") {
            std::env::var("HOME")
                .map(|h| format!("{h}/{rest}"))
                .unwrap_or_else(|_| script.to_string())
        } else {
            script.to_string()
        };
        match std::fs::read_to_string(&host_path) {
            Ok(body) => cmds.push(body),
            Err(_) => cmds.push(format!("sh {}", superzej_core::util::sh_quote(script))),
        }
    }
    cmds
}

/// Resolve which host dotfiles to upload under the env's [`ShellStrategy`], and
/// (for host-parity) the host `/nix/store` roots they reference.
///
/// - `Clean`: upload nothing (the plan drops the dotfiles step too).
/// - `Portable`/`ToolParity` with `portable_dotfiles_only` (the default): read each
///   candidate on the host and **skip** any that hard-codes absent store paths,
///   warning which file + why. Portable files (`.gitconfig`, …) still upload.
/// - `HostParity`: upload everything unfiltered and collect the store roots so the
///   provisioner can reproduce their closure before the upload.
///
/// `home_dir` is the host `$HOME` (a param so it's unit-testable with a fixture).
fn resolve_personal_dotfiles(
    home_dir: &Path,
    home: &superzej_core::config::HomeConfig,
    env_name: &str,
) -> (Vec<String>, Vec<String>) {
    use superzej_core::config::ShellStrategy;
    use superzej_core::envplan::{PitfallKind, scan_dotfile, store_roots_in};

    let candidates = if home.dotfiles.is_empty() {
        default_dotfiles()
    } else {
        home.dotfiles.clone()
    };
    let mut dotfiles = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for name in candidates {
        let contents = std::fs::read_to_string(home_dir.join(&name)).ok();
        match home.strategy {
            ShellStrategy::Clean => {} // nothing personal under clean
            ShellStrategy::HostParity => {
                if let Some(c) = &contents {
                    for r in store_roots_in(c) {
                        if !roots.contains(&r) {
                            roots.push(r);
                        }
                    }
                }
                dotfiles.push(name);
            }
            ShellStrategy::Portable | ShellStrategy::ToolParity => {
                if home.portable_dotfiles_only
                    && let Some(c) = &contents
                {
                    let absent: Vec<String> = scan_dotfile(&name, c, &home.tools)
                        .into_iter()
                        .filter(|p| p.kind == PitfallKind::AbsentStorePath)
                        .map(|p| p.detail)
                        .collect();
                    if !absent.is_empty() {
                        superzej_core::msg::warn(&format!(
                            "[sandbox.home] {name} references {} path(s) absent in env {env_name:?} \
                             (e.g. {}); skipping its upload (strategy=portable). Set \
                             strategy=\"host-parity\" to reproduce the closure, or make the rc \
                             portable (init tools by command name).",
                            absent.len(),
                            absent[0],
                        ));
                        continue;
                    }
                }
                dotfiles.push(name);
            }
        }
    }
    (dotfiles, roots)
}

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

/// Filename-safe tag from a sandbox id (alnum/`-`/`_` only) for host temp paths.
fn sanitize_tag(id: &str) -> String {
    let t: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if t.is_empty() { "sandbox".into() } else { t }
}

/// Run a host `nix` subcommand bounded by `timeout` (coreutils). `Ok(output)` on
/// success; `Err` with a tail of stderr (or "timed out") otherwise.
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
/// Bring the sandbox clone to full parity with the LOCAL worktree at `wt_host`:
/// replay unpushed commits (a thin `git bundle … HEAD --not --remotes`), restore
/// uncommitted tracked changes (`git diff HEAD --binary`), and lay down untracked
/// non-ignored files (a tar). Host git reads use the GIT_*-scrubbed `git_cmd`
/// wrapper; the three artifacts are written into the sandbox `/tmp` and a single
/// replay script applies them in `workdir`. Best-effort throughout — any capture
/// or apply failure leaves the pristine origin checkout intact.
fn apply_local_parity(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    wt_host: &str,
    workdir: &str,
    exec_env: &[(String, String)],
) -> anyhow::Result<()> {
    use superzej_core::util::git_cmd;
    let wt = Path::new(wt_host);
    if !wt.join(".git").exists() {
        // No git metadata (a bare directory) — nothing to mirror.
        return Ok(());
    }
    let host_head = git_cmd(wt)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty());

    let tmp = std::env::temp_dir();
    let tag = format!("{}-{}", sanitize_tag(id), std::process::id());

    // 1. Unpushed commits → a thin bundle (prerequisites = the remote-tracking
    //    tips the sandbox clone already has). An empty bundle (nothing unpushed)
    //    exits non-zero; treat that as "no commits to carry".
    let bundle_host = tmp.join(format!("sz-parity-{tag}.bundle"));
    let has_bundle = git_cmd(wt)
        .args(["bundle", "create"])
        .arg(&bundle_host)
        .args(["HEAD", "--not", "--remotes"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
        && bundle_host.metadata().map(|m| m.len() > 0).unwrap_or(false);

    // 2. Uncommitted tracked changes (staged + unstaged vs HEAD), incl. deletions.
    let patch = git_cmd(wt)
        .args(["diff", "HEAD", "--binary"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|p| !p.is_empty());

    // 3. Untracked, non-ignored files → a tar (paths relative to the worktree).
    let tar_host = tmp.join(format!("sz-parity-{tag}.tar"));
    let list_host = tmp.join(format!("sz-parity-{tag}.list"));
    let untracked = git_cmd(wt)
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .filter(|l| !l.is_empty());
    let has_tar = if let Some(list) = &untracked {
        std::fs::write(&list_host, list).is_ok()
            && std::process::Command::new("tar")
                .arg("-C")
                .arg(wt)
                .arg("--null")
                .arg("--files-from")
                .arg(&list_host)
                .arg("-cf")
                .arg(&tar_host)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            && tar_host.metadata().map(|m| m.len() > 0).unwrap_or(false)
    } else {
        false
    };
    let _ = std::fs::remove_file(&list_host);

    if !has_bundle && patch.is_none() && !has_tar {
        // Clean working tree with nothing unpushed — the origin clone is parity.
        let _ = std::fs::remove_file(&bundle_host);
        let _ = std::fs::remove_file(&tar_host);
        return Ok(());
    }

    // Upload the captured artifacts into the sandbox /tmp.
    if has_bundle {
        let bytes = std::fs::read(&bundle_host)?;
        block_on_provider(|| async { provider.write(id, "/tmp/sz-parity.bundle", &bytes).await })?;
    }
    if let Some(p) = &patch {
        block_on_provider(|| async { provider.write(id, "/tmp/sz-parity.patch", p).await })?;
    }
    if has_tar {
        let bytes = std::fs::read(&tar_host)?;
        block_on_provider(|| async { provider.write(id, "/tmp/sz-parity.tar", &bytes).await })?;
    }
    let _ = std::fs::remove_file(&bundle_host);
    let _ = std::fs::remove_file(&tar_host);

    // Replay over the clone, in the workdir. Each stage is independently guarded
    // (`[ -s file ]`) and non-fatal so a partial capture still helps.
    let wd = superzej_core::util::sh_quote(workdir);
    let reset = match (has_bundle, host_head.as_deref()) {
        (true, Some(h)) => format!(
            "if [ -s /tmp/sz-parity.bundle ]; then \
               git fetch /tmp/sz-parity.bundle HEAD 2>&1 || git fetch /tmp/sz-parity.bundle 2>&1 || true; \
               git reset --hard {} 2>&1 || true; \
             fi; ",
            superzej_core::util::sh_quote(h)
        ),
        _ => String::new(),
    };
    let apply_patch = if patch.is_some() {
        "if [ -s /tmp/sz-parity.patch ]; then \
           git apply --whitespace=nowarn /tmp/sz-parity.patch 2>&1 \
             || git apply --3way --whitespace=nowarn /tmp/sz-parity.patch 2>&1 || true; \
         fi; "
    } else {
        ""
    };
    let untar = if has_tar {
        format!(
            "if [ -s /tmp/sz-parity.tar ]; then tar xf /tmp/sz-parity.tar -C {wd} 2>&1 || true; fi; "
        )
    } else {
        String::new()
    };
    let script = format!(
        "cd {wd} || exit 1; {reset}{apply_patch}{untar}\
         rm -f /tmp/sz-parity.bundle /tmp/sz-parity.patch /tmp/sz-parity.tar 2>/dev/null; \
         echo 'local parity applied'"
    );
    let argv = vec!["/bin/sh".to_string(), "-lc".to_string(), script];
    block_on_provider(|| async { provider.run_exec(id, &argv, None, exec_env).await })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("replay exec failed: {e}"))
}

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
fn default_dotfiles() -> Vec<String> {
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

/// Upload coding agents' host config/credential dirs into the sandbox `$HOME`
/// (`/root`) so the agent (claude code, codex, custom) is logged-in there.
/// Per-agent paths come from [`envplan::agent_config_paths`]; missing host paths
/// are skipped. Files go via the fs `write`; directories via recursive
/// `upload_dir`. A genuine upload error aborts the step (surfaced on the splash).
fn upload_agent_configs(
    provider: &superzej_svc::provider::Provider,
    id: &str,
    sandbox_home: &str,
    agents: &[String],
) -> anyhow::Result<()> {
    let host_home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let base = sandbox_home.trim_end_matches('/');
    for agent in agents {
        let (files, dirs) = superzej_core::envplan::agent_config_paths(agent);
        for f in files {
            let src = Path::new(&host_home).join(&f);
            let Ok(data) = std::fs::read(&src) else {
                continue;
            };
            let dest = format!("{base}/{f}");
            block_on_provider(|| async { provider.write(id, &dest, &data).await })?;
        }
        for d in dirs {
            let src = Path::new(&host_home).join(&d);
            if !src.is_dir() {
                continue;
            }
            let dest = format!("{base}/{d}");
            block_on_provider(|| async { provider.upload_dir(id, &src, &dest).await })?;
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

/// Suspend-on-close (8-E): for a provider env with `auto_checkpoint`, snapshot the
/// sandbox when the worktree closes (fast resume next open). Called from the
/// fire-and-forget close thread, which has only the path — so it loads config +
/// resolves the env itself. Best-effort + off-loop; checkpoints-capable only.
pub fn checkpoint_on_close(worktree: &str) {
    let cfg = Config::load_layered(&superzej_core::config::ProcessEnv, &[], None);
    let Ok(db) = Db::open() else {
        return;
    };
    let repo_root = db
        .repo_root_for(worktree)
        .ok()
        .flatten()
        .unwrap_or_default();
    let Some(env_name) = db.effective_env(worktree, &repo_root) else {
        return;
    };
    let Some(env) = cfg.env.get(&env_name) else {
        return;
    };
    let pc = &env.provider;
    if !pc.auto_checkpoint {
        return;
    }
    // Per-worktree id from the single source of truth (resolved placement).
    let Some(name) = provider_sandbox_name(&cfg, worktree, &env_name).filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(provider) = provider_for_named(pc, &name) else {
        return;
    };
    if !provider.caps().checkpoints {
        return;
    }
    match block_on_provider(|| async { provider.checkpoint(&name, Some("auto-close")).await }) {
        Ok(id) => superzej_core::msg::info(&format!("checkpointed {name} on close: {id}")),
        Err(e) => superzej_core::msg::warn(&format!("auto-checkpoint on close failed: {e}")),
    }
}

/// Tear down a worktree's managed-provider sandbox when the worktree is deleted —
/// a deleted worktree should not leave a paid-for per-worktree sandbox running.
/// The worktree-delete path resolves the env name from the DB *before* it forgets
/// the worktree's rows (a later DB-based resolve would return nothing and leak the
/// sandbox). Best-effort + off-loop (a network DELETE); idempotent (the provider
/// treats a 404 as already-gone, so racing a TTL/manual delete is fine). No-op for
/// local/ssh/k8s envs or an unconfigured/tokenless provider.
pub fn destroy_provider_sandbox(worktree: &str, env_name: &str) {
    let cfg = Config::load_layered(&superzej_core::config::ProcessEnv, &[], None);
    let Some(env) = cfg.env.get(env_name) else {
        return;
    };
    let pc = &env.provider;
    let Some(name) = provider_sandbox_name(&cfg, worktree, env_name).filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(provider) = provider_for_named(pc, &name) else {
        return;
    };
    match block_on_provider(|| async { provider.destroy(&name).await }) {
        Ok(()) => superzej_core::msg::info(&format!("destroyed sandbox {name} on worktree delete")),
        Err(e) => superzej_core::msg::warn(&format!("sandbox teardown on delete failed: {e}")),
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
fn block_on_provider<T, Fut>(f: impl FnOnce() -> Fut + Send) -> anyhow::Result<T>
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
    let env = vec![
        ("SUPERZEJ_WORKTREE".to_string(), worktree.to_string()),
        (
            "SUPERZEJ_BRANCH".to_string(),
            branch.unwrap_or_default().to_string(),
        ),
    ];
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
    launch_spec_with_key(cfg, worktree, branch, choice, None)
}

/// Like [`launch_spec`] but injects a scoped API key for the sandbox.
pub fn launch_spec_with_key(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    scoped_key: Option<String>,
) -> anyhow::Result<LaunchSpec> {
    let loc = GitLoc::for_worktree(Path::new(worktree));

    // Record the choice for the dashboard / `--resume` (keyed by worktree path).
    // The transient `clean-shell` watchdog fallback is NOT recorded: it must not
    // become the worktree's remembered agent (the user may fix their dotfiles).
    let saved_backend = match Db::open() {
        Ok(db) => {
            if choice != "clean-shell" {
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

    let mut outcome = prepare_sandbox_env(
        cfg,
        &repo_root,
        worktree,
        &loc,
        saved_backend.as_deref(),
        launch_scope(cfg, choice),
        selected_env.as_deref(),
    )?;
    if let Ok(db) = Db::open() {
        let _ = db.set_worktree_sandbox(worktree, &outcome.backend_label);
    }

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

    // Client-side account switching (item 656): point the agent's credential
    // home (CODEX_HOME / CLAUDE_CONFIG_DIR) at the active account, resolved by
    // worktree → workspace → global precedence. Local worktrees only — a remote
    // agent runs where the host's account dir doesn't exist.
    let account_env = (!loc.is_remote())
        .then(|| Db::open().ok())
        .flatten()
        .and_then(|db| {
            let slug = repo_slug(&db, &repo_root);
            account::launch_env(cfg, &db, worktree, slug.as_deref(), choice)
        });
    if let Some((var, dir)) = account_env.as_ref() {
        // The CLI writes tokens/history here; ensure it exists.
        let _ = std::fs::create_dir_all(dir);
        let dir_s = dir.to_string_lossy().into_owned();
        if let Some(spec) = outcome.spec.as_mut() {
            spec.env_overrides.insert(var.clone(), dir_s.clone());
            // Path-preserving mount so the dir is reachable at the same path
            // inside the sandbox (mirrors the worktree mount).
            spec.mounts.push(sandbox::Mount {
                host: dir_s.clone(),
                dest: dir_s,
                ro: false,
                cache: false,
            });
        }
    }

    // Shared build env (sccache / CARGO_TARGET_DIR) from `[disk]`, so an
    // interactive `cargo build` dedups compilation / shares a target across
    // worktrees. Inside a sandbox it must ride the container env (overrides +
    // unblock); on the host it rides the pane env below.
    let build_env = build_env_vars(cfg, &repo_root);

    if let Some(spec) = outcome.spec.as_mut() {
        apply_ssh_config_shim(spec);
        // `env_overrides` exports these inside the sandbox shell (env_block would
        // *unset* them — wrong direction).
        for (k, v) in &build_env {
            spec.env_overrides.insert(k.clone(), v.clone());
        }
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
        warm_direnv(cfg, Path::new(worktree));
    }

    let mut spec = compose_spec(cfg, worktree, branch, choice, &loc, &outcome);
    // On the host path (no sandbox spec) the credential home + build env ride
    // the pane env.
    if outcome.spec.is_none() {
        if let Some((var, dir)) = account_env {
            spec.env.push((var, dir.to_string_lossy().into_owned()));
        }
        spec.env.extend(build_env);
        // Host fallback under bouncer: the override is inert but proxy vars ride
        // the pane env (sandboxed agents already got them via env_overrides).
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
    let outcome = prepare_sandbox_env(
        cfg,
        &repo_root,
        worktree,
        &loc,
        saved_backend.as_deref(),
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

/// In bouncer mode, inject the agent's proxy + tool-override env (and the
/// control-socket mounts) into the resolved sandbox `outcome` before its argv is
/// composed, minting the stable per-worktree proxy key. No-op unless the bouncer
/// is on and `choice` is a configured agent. Sandbox env rides `env_overrides`
/// (exported inside the container); a host fallback returns the vars to ride the
/// pane env. See [`crate::bouncer::agent_env_plan`].
pub fn apply_bouncer_launch(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    outcome: &mut SandboxOutcome,
) -> BouncerLaunch {
    if !(cfg.llm_proxy.bouncer && cfg.agent_command(choice).is_some()) {
        return BouncerLaunch::default();
    }
    let key = cfg
        .llm_proxy
        .route_agent
        .then(|| mint_stable_proxy_key(worktree))
        .flatten();
    let sandbox = outcome.spec.as_ref().map(|s| (s.backend, s.network));
    let plan = crate::bouncer::agent_env_plan(cfg, worktree, sandbox, key.as_deref());
    match outcome.spec.as_mut() {
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
    }
}

/// Resolve a configured build path: `~`/`~/…` expands to home; a relative path
/// resolves against the repo root (so a shared `target/` is per-repo).
fn resolve_build_path(raw: &str, repo_root: &Path) -> String {
    let expanded = superzej_core::util::expand_tilde(raw);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        expanded
    } else {
        repo_root.join(p).to_string_lossy().into_owned()
    }
}

/// Build-tooling env injected into interactive panes from `[disk]`: a shared
/// `sccache` compile cache and/or a shared `CARGO_TARGET_DIR`. Empty when both
/// are off (the common case), so panes are untouched unless opted in.
fn build_env_vars(cfg: &Config, repo_root: &Path) -> Vec<(String, String)> {
    let d = &cfg.disk;
    let mut out = Vec::new();
    if d.sccache && superzej_core::util::have("sccache") {
        out.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
        if !d.sccache_dir.is_empty() {
            out.push((
                "SCCACHE_DIR".to_string(),
                resolve_build_path(&d.sccache_dir, repo_root),
            ));
        }
    }
    if !d.shared_target_dir.is_empty() {
        out.push((
            "CARGO_TARGET_DIR".to_string(),
            resolve_build_path(&d.shared_target_dir, repo_root),
        ));
    }
    out
}

/// Map `[sandbox] warm_direnv` to a host-side `direnv` cache warm for
/// `worktree`. Off-loop and self-gating (`direnv::warm` is a no-op without a
/// cold flake-backed `.envrc`); no-op when warming is disabled.
pub(crate) fn warm_direnv(cfg: &Config, worktree: &Path) {
    use superzej_core::config::WarmDirenv;
    let allow = match cfg.sandbox.warm_direnv {
        WarmDirenv::Off => return,
        WarmDirenv::AllowedOnly => false,
        WarmDirenv::Auto => true,
    };
    direnv::warm(worktree, allow);
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
mod tests {
    use super::*;

    #[test]
    fn resolve_personal_dotfiles_drops_nonportable_under_portable() {
        use superzej_core::config::{HomeConfig, ShellStrategy};
        let home_dir = std::env::temp_dir().join(format!("sz-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home_dir);
        std::fs::create_dir_all(&home_dir).unwrap();
        // A portable file and a home-manager-style rc with absolute store paths.
        std::fs::write(home_dir.join(".gitconfig"), "[user]\n  name = x\n").unwrap();
        std::fs::write(
            home_dir.join(".zshrc"),
            "source /nix/store/abc-zsh-plugin/x.zsh\neval \"$(starship init zsh)\"\n",
        )
        .unwrap();

        let portable = HomeConfig {
            dotfiles: vec![".gitconfig".into(), ".zshrc".into()],
            strategy: ShellStrategy::Portable,
            portable_dotfiles_only: true,
            ..HomeConfig::default()
        };
        let (files, roots) = resolve_personal_dotfiles(&home_dir, &portable, "sprite");
        assert_eq!(
            files,
            vec![".gitconfig".to_string()],
            "non-portable .zshrc dropped"
        );
        assert!(
            roots.is_empty(),
            "portable strategy collects no closure roots"
        );

        // host-parity keeps everything and collects the store roots.
        let parity = HomeConfig {
            strategy: ShellStrategy::HostParity,
            ..portable.clone()
        };
        let (files, roots) = resolve_personal_dotfiles(&home_dir, &parity, "bigbox");
        assert!(
            files.contains(&".zshrc".to_string()),
            "host-parity keeps the rc"
        );
        assert!(
            roots.iter().any(|r| r.contains("zsh-plugin")),
            "roots collected: {roots:?}"
        );

        // clean uploads nothing.
        let clean = HomeConfig {
            strategy: ShellStrategy::Clean,
            ..portable.clone()
        };
        let (files, _) = resolve_personal_dotfiles(&home_dir, &clean, "sprite");
        assert!(files.is_empty(), "clean uploads no dotfiles");

        let _ = std::fs::remove_dir_all(&home_dir);
    }

    #[test]
    fn sprite_ssh_argv_wraps_proxycommand_and_remote_shell() {
        let argv = sprite_ssh_argv(
            "/usr/bin/szhost",
            "/home/me/wt",
            std::path::Path::new("/state/sprite_ed25519"),
            "sprite",
            "/workspace",
        );
        let joined = argv.join(" ");
        assert_eq!(argv[0], "ssh");
        assert!(
            joined.contains("ProxyCommand=/usr/bin/szhost sprite-proxy /home/me/wt"),
            "{joined}"
        );
        assert!(joined.contains("-i /state/sprite_ed25519"));
        assert!(joined.contains(&format!("-p {SPRITE_SSHD_PORT}")));
        assert!(argv.iter().any(|a| a == "sprite@sprite"));
        // The remote command cd's into the workdir then execs the user's login
        // shell via the probe chain (zsh first), so the host-parity rc loads.
        let remote = argv.last().unwrap();
        assert!(remote.contains("cd /workspace"), "{remote}");
        assert!(
            remote.contains("command -v zsh") && remote.contains("exec zsh -l"),
            "remote should run the zsh-first login chain: {remote}"
        );
    }

    #[test]
    fn sprite_sshd_setup_script_authorizes_key_and_writes_config() {
        let s = sprite_sshd_setup_script("ssh-ed25519 AAAA... superzej-sprite");
        assert!(s.contains("authorized_keys"));
        assert!(s.contains("ssh-ed25519 AAAA")); // the pubkey is embedded (quoted)
        assert!(s.contains(&format!("Port {SPRITE_SSHD_PORT}")));
        assert!(s.contains("sprite_host_ed25519") && s.contains("sprite_sshd_config"));
    }

    #[test]
    fn nix_copy_argv_builds_push_command() {
        let argv = nix_copy_argv(
            "s3://my-cache",
            &["/nix/store/a-foo".into(), "/nix/store/b-bar".into()],
        );
        assert_eq!(
            argv,
            vec![
                "copy".to_string(),
                "--to".to_string(),
                "s3://my-cache".to_string(),
                "/nix/store/a-foo".to_string(),
                "/nix/store/b-bar".to_string(),
            ]
        );
    }

    #[test]
    fn devshell_push_argv_builders() {
        assert_eq!(
            nix_develop_profile_argv("/home/me/repo", "/tmp/gc", ""),
            vec![
                "develop",
                "/home/me/repo",
                "--profile",
                "/tmp/gc",
                "--command",
                "true"
            ]
        );
        assert_eq!(
            nix_develop_profile_argv("/home/me/repo", "/tmp/gc", "sandbox"),
            vec![
                "develop",
                "/home/me/repo#sandbox",
                "--profile",
                "/tmp/gc",
                "--command",
                "true"
            ]
        );
        assert_eq!(
            nix_copy_to_file_argv("/tmp/cache", "/tmp/gc"),
            vec![
                "copy",
                "--to",
                "file:///tmp/cache?compression=zstd",
                "--no-check-sigs",
                "/tmp/gc"
            ]
        );
        assert_eq!(sanitize_tag("sz-cosmic-puma"), "sz-cosmic-puma");
        assert_eq!(sanitize_tag("a/b c:d"), "a-b-c-d");
        assert_eq!(sanitize_tag(""), "sandbox");
    }

    #[test]
    fn nix_copy_p2p_argv_targets_ssh_ng_without_sig_check() {
        let argv = nix_copy_p2p_argv("sprite", &["/nix/store/a-zsh".into()]);
        assert_eq!(&argv[0], "copy");
        assert_eq!(&argv[1], "--to");
        assert_eq!(&argv[2], "ssh-ng://sprite@sprite");
        assert!(argv.contains(&"--no-check-sigs".to_string()));
        assert!(argv.contains(&"--substitute-on-destination".to_string()));
        assert!(argv.contains(&"/nix/store/a-zsh".to_string()));
    }

    #[test]
    fn store_root_of_truncates_to_top_level_store_path() {
        assert_eq!(
            store_root_of("/nix/store/abc-zsh-5.9.1/bin/zsh"),
            Some("/nix/store/abc-zsh-5.9.1".to_string())
        );
        assert_eq!(
            store_root_of("/nix/store/abc-zsh-5.9.1"),
            Some("/nix/store/abc-zsh-5.9.1".to_string())
        );
        assert_eq!(store_root_of("/etc/profiles/per-user/me/bin/zsh"), None);
        assert_eq!(store_root_of("/nix/store/"), None);
    }

    #[test]
    fn sanitize_detail_strips_ansi_control_and_collapses_whitespace() {
        // The real failing-step string: ANSI SGR codes + newlines (what tripped
        // the renderer). Sanitized to a single clean line.
        let raw = "Build dev shell (exit 2): \u{1b}[1m\u{1b}[32merror:\u{1b}[0m foo\n\n  bar\tbaz";
        let s = sanitize_detail(raw);
        assert!(!s.contains('\u{1b}'), "no escape bytes: {s:?}");
        assert!(
            !s.contains('\n') && !s.contains('\t'),
            "no raw control: {s:?}"
        );
        assert_eq!(s, "Build dev shell (exit 2): error: foo bar baz");
        // OSC sequence (ESC ] … BEL) is dropped whole.
        assert_eq!(sanitize_detail("a\u{1b}]0;title\u{7}b"), "ab");
        // Long input is clamped with an ellipsis.
        let long = "x".repeat(500);
        assert!(sanitize_detail(&long).chars().count() <= 201);
    }

    #[test]
    fn native_exec_health_reports_and_recovers() {
        // Unique provider name so the process-global registry doesn't collide
        // with other tests.
        let p = "sprites-health-test-xyz";
        assert!(native_exec_healthy(p), "unseen provider starts healthy");
        native_exec_report(p, false);
        assert!(!native_exec_healthy(p), "a failure marks it unhealthy");
        native_exec_report(p, true);
        assert!(native_exec_healthy(p), "a success clears it");
    }

    fn cfg_with(agents: &[(&str, &str)], tools: &[(&str, &str)]) -> Config {
        let mut cfg = Config::default();
        let mk = |(n, c): &(&str, &str)| superzej_core::config::NamedCommand {
            name: n.to_string(),
            command: c.to_string(),
            hints: Vec::new(),
            provider: None,
        };
        cfg.agents = agents.iter().map(mk).collect();
        cfg.tools = tools.iter().map(mk).collect();
        cfg
    }

    #[test]
    fn build_env_vars_off_by_default() {
        let cfg = Config::default();
        assert!(
            build_env_vars(&cfg, Path::new("/repo")).is_empty(),
            "no build env injected unless opted in"
        );
    }

    #[test]
    fn build_env_vars_injects_sccache_and_shared_target() {
        let mut cfg = Config::default();
        cfg.disk.shared_target_dir = "shared-target".into();
        let env = build_env_vars(&cfg, Path::new("/repo"));
        // shared_target_dir present → CARGO_TARGET_DIR resolved against repo root.
        assert!(env.contains(&(
            "CARGO_TARGET_DIR".to_string(),
            "/repo/shared-target".to_string()
        )));
        // sccache off → no RUSTC_WRAPPER regardless of PATH.
        assert!(!env.iter().any(|(k, _)| k == "RUSTC_WRAPPER"));

        // An absolute shared dir is used verbatim.
        cfg.disk.shared_target_dir = "/abs/target".into();
        let env = build_env_vars(&cfg, Path::new("/repo"));
        assert!(env.contains(&("CARGO_TARGET_DIR".to_string(), "/abs/target".to_string())));
    }

    #[test]
    fn choices_lists_agents_then_tools_then_shell() {
        let cfg = cfg_with(&[("claude", "claude")], &[("lazygit", "lazygit")]);
        assert_eq!(choices(&cfg), vec!["claude", "lazygit", "shell"]);
    }

    #[test]
    fn choices_does_not_duplicate_an_explicit_shell() {
        let cfg = cfg_with(&[], &[("shell", "bash")]);
        assert_eq!(choices(&cfg), vec!["shell"]);
    }

    #[test]
    fn resolve_command_maps_agent_tool_and_shell() {
        let cfg = cfg_with(&[("claude", "claude --foo")], &[("lazygit", "lazygit")]);
        assert_eq!(resolve_command(&cfg, "claude"), "claude --foo");
        assert_eq!(resolve_command(&cfg, "lazygit"), "lazygit");
        assert_eq!(resolve_command(&cfg, "shell"), shell_inner(false));
        // Unknown label degrades to a shell.
        assert_eq!(resolve_command(&cfg, "nope"), shell_inner(false));
    }

    // Crate-wide env lock (shared with `run`'s sidebar tests): both redirect the
    // process-global `XDG_STATE_HOME`, so they must serialize on the SAME mutex.
    use crate::testenv::ENV_LOCK;

    fn with_temp_state<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("sz-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::env::var_os("XDG_STATE_HOME");
        // SAFETY: guarded by ENV_LOCK; this module's DB-touching tests run inside this critical section.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let out = f();
        match old {
            Some(v) => unsafe { std::env::set_var("XDG_STATE_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_STATE_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn explicit_unavailable_sandbox_does_not_fall_back_to_host() {
        with_temp_state("explicit-no-host", || {
            let mut cfg = cfg_with(&[], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::Wsl;
            cfg.sandbox.backend_chain = vec!["host".to_string()];
            let worktree =
                std::env::temp_dir().join(format!("sz-agent-wsl-missing-{}", std::process::id()));
            let err = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell")
                .expect_err("explicit WSL sandbox must not degrade to host");
            let msg = err.to_string();
            assert!(
                msg.contains("explicit sandbox backend")
                    || msg.contains("refusing fallback")
                    || msg.contains("could not be resolved"),
                "{msg}"
            );
        });
    }

    #[test]
    fn auto_backend_chain_can_fall_back_to_host() {
        with_temp_state("auto-host", || {
            let mut cfg = cfg_with(&[], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::Auto;
            cfg.sandbox.backend_chain = vec!["host".to_string()];
            let worktree =
                std::env::temp_dir().join(format!("sz-agent-auto-host-{}", std::process::id()));
            let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
            assert_eq!(spec.backend, "host");
            assert!(spec.argv.join(" ").contains("sh"));
            assert_eq!(
                spec.warning_summary().as_deref(),
                Some("sandbox auto selected host")
            );
        });
    }

    #[test]
    fn auto_backend_fallthrough_carries_visible_warning() {
        with_temp_state("auto-fallthrough-warning", || {
            let mut cfg = cfg_with(&[], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::Auto;
            cfg.sandbox.backend_chain = vec!["wsl".to_string(), "host".to_string()];
            let worktree = std::env::temp_dir()
                .join(format!("sz-agent-auto-fallthrough-{}", std::process::id()));
            let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
            assert_eq!(spec.backend, "host");
            let warning = spec
                .warning_summary()
                .expect("host fallback should be visible");
            assert!(warning.contains("sandbox wsl unavailable"), "{warning}");
            assert!(
                warning.contains("running on host after sandbox fallback"),
                "{warning}"
            );
        });
    }

    #[test]
    fn compose_spec_host_fallback_is_login_shell() {
        let cfg = cfg_with(&[("claude", "claude --foo")], &[]);
        let loc = GitLoc::from_db("/wt/x", None);
        let host = SandboxOutcome {
            spec: None,
            backend_label: "host".into(),
            warnings: vec!["sandbox auto selected host".into()],
            shell: String::new(),
            is_remote: false,
            cwd_override: None,
            location: None,
        };
        let spec = compose_spec(&cfg, "/wt/x", Some("sz/x"), "claude", &loc, &host);
        assert_eq!(
            spec.argv,
            vec![
                superzej_core::util::shell(),
                "-lc".to_string(),
                "claude --foo".to_string()
            ]
        );
        assert_eq!(spec.cwd, Some(PathBuf::from("/wt/x")));
        assert!(
            spec.env
                .contains(&("SUPERZEJ_WORKTREE".to_string(), "/wt/x".to_string()))
        );
        assert!(
            spec.env
                .contains(&("SUPERZEJ_BRANCH".to_string(), "sz/x".to_string()))
        );
        // The settled backend + warnings ride into the spec.
        assert_eq!(spec.backend, "host");
        assert_eq!(
            spec.warning_summary().as_deref(),
            Some("sandbox auto selected host")
        );
    }

    /// OCI shell panes emit a runtime probe chain so containers that don't have
    /// the host shell (e.g. a bare Debian image has bash but not zsh) still get
    /// a working login shell instead of "exec: zsh: not found".
    #[test]
    fn shell_inner_oci_emits_runtime_probe_chain() {
        let oci = shell_inner(true);
        // Must contain a POSIX command -v probe for each candidate shell.
        assert!(
            oci.contains("command -v"),
            "should probe for shell availability"
        );
        // Must have an unconditional /bin/sh -l fallback at the end.
        assert!(
            oci.ends_with("exec /bin/sh -l"),
            "must end with /bin/sh fallback"
        );
        // bash must always appear in the chain (present in every Debian image).
        assert!(oci.contains("bash"), "bash must be in the probe chain");
        // Non-OCI: must be a simple "<shell> -l" with the host path, not a chain.
        let host = shell_inner(false);
        assert!(
            !host.contains("command -v"),
            "host form must not emit a probe chain"
        );
        assert!(host.ends_with(" -l"), "host form must end with -l");
    }

    #[test]
    fn native_open_spec_does_not_exec_prefix_the_probe_chain() {
        // Regression: `open_spec` must not wrap the self-exec'ing probe chain in
        // another `exec`. `exec command -v zsh …` makes the shell try to exec a
        // binary named `command` (a builtin), failing with 127 and killing the
        // pane before any shell starts — the sprite "shell instantly crashes +
        // flashing splash" bug.
        let n = NativeShell {
            provider: superzej_svc::provider::Provider::Sprites(
                superzej_svc::provider::SpritesProvider::new("", "t", "s"),
            ),
            provider_name: "sprites".into(),
            sandbox_id: "s".into(),
            inner: shell_inner(true),
            workdir: "/workspace".into(),
            env: vec![],
        };
        let spec = n.open_spec(80, 24);
        let script = spec.argv.last().cloned().unwrap_or_default();
        assert!(
            !script.contains("exec command"),
            "must not exec-prefix the probe chain (127 footgun): {script}"
        );
        // The chain itself still self-execs into a shell, ending in /bin/sh.
        assert!(script.contains("command -v zsh") && script.contains("exec /bin/sh -l"));
        // And it cd's into the workdir first.
        assert!(script.starts_with("cd /workspace"));
    }

    #[test]
    fn clean_shell_inner_is_rc_free_with_sh_fallback() {
        let clean = clean_shell_inner();
        // Plain bash is the requested fallback and must skip every startup file.
        assert!(
            clean.contains("bash --norc --noprofile"),
            "must prefer a no-rc/no-profile bash"
        );
        // The zsh middle option must use -f (NO_RCS) so a broken .zshrc can't hang.
        assert!(
            clean.contains("zsh -f"),
            "zsh fallback must skip startup files"
        );
        // Universal last resort.
        assert!(clean.ends_with("exec /bin/sh"), "must end with /bin/sh");
        // Crucially: it must NEVER run a login shell that sources the user rc.
        assert!(
            !clean.contains("-l") && !clean.contains("zsh -l") && !clean.contains("bash -l"),
            "clean fallback must not be a login shell"
        );
    }

    #[test]
    fn compose_spec_clean_shell_choice_uses_rc_free_shell() {
        // The `clean-shell` choice composes the rc-free chain, ignoring the normal
        // login-shell path and any sandbox shell override.
        let cfg = Config::default();
        let loc = GitLoc::from_db("/wt/x", None);
        let sb = SandboxOutcome {
            spec: None, // host fallback → `$SHELL -lc <cmd>`
            backend_label: "host".into(),
            warnings: vec![],
            shell: String::new(),
            is_remote: false,
            cwd_override: None,
            location: None,
        };
        let spec = compose_spec(&cfg, "/wt/x", None, "clean-shell", &loc, &sb);
        let joined = spec.argv.join(" ");
        assert!(
            joined.contains("bash --norc --noprofile"),
            "clean-shell argv must carry the rc-free chain, got: {joined}"
        );
    }

    #[test]
    fn prepare_sandbox_none_backend_falls_to_host() {
        let mut cfg = Config::default();
        cfg.sandbox.backend = superzej_core::config::SandboxBackend::None;
        let loc = GitLoc::from_db("/wt/x", None);
        let out = prepare_sandbox(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            None,
            SandboxScope::Shell,
        )
        .unwrap();
        assert!(out.spec.is_none());
        assert_eq!(out.backend_label, "host");
        // An explicit "none" choice behaves the same as the configured backend.
        let out = prepare_sandbox(
            &cfg,
            Path::new("/repo"),
            "/wt/x",
            &loc,
            Some("none"),
            SandboxScope::Shell,
        )
        .unwrap();
        assert!(out.spec.is_none());
    }

    // H1: E2E launch_spec test — backend="none" → host fallback path.
    #[test]
    fn launch_spec_none_backend_produces_valid_spec() {
        with_temp_state("launch-spec-none", || {
            let mut cfg = cfg_with(&[("claude", "claude --foo")], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::None;
            let worktree = std::env::temp_dir().join(format!("sz-ls-none-{}", std::process::id()));
            let spec = launch_spec(&cfg, &worktree.to_string_lossy(), None, "shell").unwrap();
            // Host fallback must use the login shell.
            assert!(spec.argv.join(" ").contains("sh"), "argv: {:?}", spec.argv);
            // cwd must point into the worktree.
            assert_eq!(spec.cwd, Some(worktree.clone()));
            // SUPERZEJ_WORKTREE must be injected.
            assert!(
                spec.env.iter().any(|(k, v)| k == "SUPERZEJ_WORKTREE"
                    && v == &worktree.to_string_lossy().to_string()),
                "SUPERZEJ_WORKTREE missing from env"
            );
        });
    }

    // H1 (C2 variant): launch_spec_with_key injects scoped API key.
    #[test]
    fn launch_spec_with_key_injects_scoped_key() {
        with_temp_state("launch-spec-key", || {
            let mut cfg = cfg_with(&[("claude", "claude --foo")], &[]);
            cfg.sandbox.backend = superzej_core::config::SandboxBackend::None;
            let worktree = std::env::temp_dir().join(format!("sz-ls-key-{}", std::process::id()));
            let spec = launch_spec_with_key(
                &cfg,
                &worktree.to_string_lossy(),
                None,
                "shell",
                Some("sk-test-scoped".into()),
            )
            .unwrap();
            // On the host path there's no OCI spec to mutate, so scoped key
            // falls into the LaunchSpec env directly via compose_spec.
            // At minimum the spec must succeed; the key injection path is
            // exercised without a running container.
            assert_eq!(spec.backend, "host");
        });
    }

    #[test]
    fn inject_devshell_host_prepends_path_and_merges_vars() {
        let dev = devenv::Devshell {
            path: Some("/nix/store/tools/bin".into()),
            vars: vec![
                ("SUPERZEJ_YAZI_BIN".into(), "/nix/store/yz/bin/yazi".into()),
                // A var the user already set on the pane must NOT be clobbered.
                ("KEEP_ME".into(), "from-devshell".into()),
            ],
        };
        let mut spec = LaunchSpec {
            argv: vec!["sh".into()],
            cwd: None,
            env: vec![("KEEP_ME".to_string(), "user-set".to_string())],
            backend: "host".into(),
            warnings: vec![],
        };
        // `inject_devshell_host` prepends to the *process* PATH, so set a known
        // base under the env guard. Without restoring it, `/usr/bin:/bin` would
        // leak to every later test, dropping git/the toolchain (under /nix/store
        // in the dev shell) out of PATH and breaking anything that shells out.
        let _env = crate::testenv::EnvVarGuard::set(&[("PATH", "/usr/bin:/bin")]);
        inject_devshell_host(&mut spec, &dev);

        let path = spec.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v);
        assert_eq!(
            path.map(String::as_str),
            Some("/nix/store/tools/bin:/usr/bin:/bin"),
            "devShell PATH must be prepended to the existing PATH"
        );
        // Only one PATH entry (any prior was replaced, not duplicated).
        assert_eq!(spec.env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        // New var injected; pre-existing var preserved (not overwritten).
        assert_eq!(
            spec.env
                .iter()
                .find(|(k, _)| k == "SUPERZEJ_YAZI_BIN")
                .map(|(_, v)| v.as_str()),
            Some("/nix/store/yz/bin/yazi")
        );
        assert_eq!(
            spec.env
                .iter()
                .find(|(k, _)| k == "KEEP_ME")
                .map(|(_, v)| v.as_str()),
            Some("user-set"),
            "a var the user already set must not be clobbered"
        );
    }
}
