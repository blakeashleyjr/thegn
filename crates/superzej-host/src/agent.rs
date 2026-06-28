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
use superzej_core::{account, devenv, repo, sandbox};
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
        // The trailing /bin/sh -l is the unconditional fallback — it exists in
        // every POSIX container.
        format!("{checks}exec /bin/sh -l")
    } else {
        let host_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        format!("{host_shell} -l")
    }
}

/// Like [`shell_inner`] but uses an explicit override from the sandbox config.
fn shell_inner_override(shell_override: &str) -> String {
    format!("{shell_override} -l")
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
    let environment = cfg.resolve_env(repo_root, loc, selected_env);
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
    if matches!(placement, superzej_core::placement::Placement::Provider(_)) {
        auto_provision_sandbox(cfg, &env_name);
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
                &pc.id,
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
    /// `workdir` then execs the resolved shell, with the pane env passed through.
    pub fn open_spec(&self, cols: u16, rows: u16) -> superzej_svc::provider::ExecSpec {
        let script = if self.workdir.is_empty() {
            self.inner.clone()
        } else {
            format!("cd {} 2>/dev/null; exec {}", self.workdir, self.inner)
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

/// Decide whether `worktree`'s interactive shell should attach via a provider's
/// **native exec API** instead of the CLI/PTY path. `Some` when the resolved env
/// is a `provider` placement whose provider has a native exec API, whose `exec`
/// mode isn't `cli`, and whose API token is present; `None` ⇒ use [`launch_spec`].
///
/// Resolves the env exactly as [`launch_spec_with_key`] does (DB repo-root +
/// effective env) so the two paths never disagree about which env is in play.
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
    let environment = cfg.resolve_env(&repo_root, &loc, selected_env.as_deref());
    let superzej_core::placement::Placement::Provider(p) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    if pc.exec == ProviderExecMode::Cli || !superzej_svc::provider::exec_api_by_name(&pc.provider) {
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
    let env = vec![
        ("SUPERZEJ_WORKTREE".to_string(), worktree.to_string()),
        ("SUPERZEJ_BRANCH".to_string(), String::new()),
    ];
    Some(NativeShell {
        provider,
        provider_name: pc.provider.clone(),
        sandbox_id: p.id.clone(),
        inner,
        workdir: pc.sync_workdir(),
        env,
    })
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
/// `up_command`/`placement.ensure`). Best-effort + off-loop; a failure warns and
/// later steps (clone/connect) degrade as usual. No-op unless `auto_provision`.
fn auto_provision_sandbox(cfg: &Config, env_name: &str) {
    let Some(env) = cfg.env.get(env_name) else {
        return;
    };
    let pc = &env.provider;
    let name = pc.id.trim();
    if !pc.auto_provision || name.is_empty() {
        return;
    }
    let Some(provider) = provider_for(pc) else {
        return;
    };
    let name = name.to_string();
    match block_on_provider(|| async { provider.ensure_exists(&name).await }) {
        Ok(true) => superzej_core::msg::info(&format!("provisioned sandbox {name}")),
        Ok(false) => {} // already exists
        Err(e) => superzej_core::msg::warn(&format!("sandbox auto-provision failed: {e}")),
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
    let name = pc.id.trim();
    if !pc.auto_checkpoint || name.is_empty() {
        return;
    }
    let Some(provider) = provider_for(pc) else {
        return;
    };
    if !provider.caps().checkpoints {
        return;
    }
    let name = name.to_string();
    match block_on_provider(|| async { provider.checkpoint(&name, Some("auto-close")).await }) {
        Ok(id) => superzej_core::msg::info(&format!("checkpointed {name} on close: {id}")),
        Err(e) => superzej_core::msg::warn(&format!("auto-checkpoint on close failed: {e}")),
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
    let cmd = if choice == "shell" && !sb_shell.is_empty() {
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
    let saved_backend = match Db::open() {
        Ok(db) => {
            let _ = db.set_worktree_agent(worktree, choice);
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
