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
use anyhow::Context;
use std::path::{Path, PathBuf};
use thegn_core::config::Config;
use thegn_core::db::Db;

// Teardown fns live in a sibling (kept flat); same call paths.
pub use crate::agent_teardown::checkpoint_on_close;
use thegn_core::remote::{GitLoc, SshTarget};
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

/// The program basename of an `[[agents]]` command (`/opt/bin/claude --x` → `claude`).
fn agent_program(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
}

/// Drop picker entries for KNOWN coding agents that `detected` (see
/// [`detect_host_agents`]) does not list, so a fresh machine does not offer an
/// agent it cannot run. Custom agents, tools and `shell` are always kept.
pub(crate) fn filter_undetected_agents(
    cfg: &Config,
    choices: Vec<String>,
    detected: &[String],
) -> Vec<String> {
    let known = thegn_core::envplan::known_agents();
    choices
        .into_iter()
        .filter(|name| {
            let Some(entry) = cfg.agents.iter().find(|a| &a.name == name) else {
                return true;
            };
            if entry.name == SHELL || entry.command.trim() == "__shell__" {
                return true;
            }
            let program = agent_program(&entry.command);
            !known.contains(&program) || detected.iter().any(|d| d == program)
        })
        .collect()
}

/// True when `choices` offers no coding agent at all (only tools and the shell).
pub(crate) fn has_no_agent(cfg: &Config, choices: &[String]) -> bool {
    !choices.iter().any(|c| {
        c != SHELL
            && cfg
                .agents
                .iter()
                .any(|a| &a.name == c && a.command.trim() != "__shell__")
    })
}

/// The empty-state hint shown under the picker when no coding agent was found.
pub(crate) const NO_AGENT_HINT: &str = "No coding agent found on PATH. Install claude \
(https://claude.com/product/claude-code) or codex (https://github.com/openai/codex), \
then run it once to log in.";

/// Resolve a picker label to the command string to run inside the worktree.
/// `shell` (and any unknown label) resolves to the interactive login shell.
/// Always uses the host (non-OCI) form; callers that know the sandbox context
/// should use `compose_spec` instead.
///
/// `Err` when the entry configures `permissions` the harness cannot grant
/// command-scoped (THE-440): that is the one resolution failure a launch must
/// NOT paper over, because launching the raw command would run the agent under
/// a policy the operator did not ask for. Every other resolution complaint (a
/// `model` on a flagless harness — a `config validate` error) still degrades to
/// the entry's raw command rather than a dead pane, exactly as before.
pub fn resolve_command(cfg: &Config, choice: &str) -> Result<String, String> {
    if choice == SHELL {
        return Ok(shell_inner(false));
    }
    if let Some(c) = cfg
        .agent_command(choice)
        .or_else(|| cfg.tool_command(choice))
    {
        let eff = thegn_core::agent_task::effective_agent(cfg, choice, None);
        // Fail closed on the permission grant, whatever else the resolver said:
        // the list is configured on the entry, so it is known even when the
        // entry does not resolve.
        if let Some(entry) = cfg
            .agents
            .iter()
            .chain(cfg.tools.iter())
            .find(|a| a.name == choice)
        {
            let harness = eff
                .as_ref()
                .map(|e| e.harness.clone())
                .unwrap_or_else(|_| thegn_core::agent_task::provider_id(entry));
            thegn_core::agent_task::permission_suffix(&harness, &entry.permissions)
                .map_err(|why| format!("{choice}: {why}"))?;
        }
        return Ok(eff
            .and_then(|e| e.interactive_command())
            .unwrap_or_else(|_| c.to_string()));
    }
    // Unknown label — drop to a shell rather than spawning a dead pane.
    Ok(shell_inner(false))
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
    /// Human-visible notes when an explicitly-authorized fallback was selected.
    pub warnings: Vec<String>,
    /// A managed-PROVIDER env failed and an explicit run-on-host choice dropped
    /// this launch to the host — drives the `provider_degraded` notification.
    pub degraded: bool,
    /// The pane runs on a non-local placement (ssh/k8s/provider). A remote
    /// native `Backend::None` still labels itself `host`, so "is this a LOCAL
    /// host shell?" is `backend == "host" && !remote` — see
    /// [`reject_host_prewarm`].
    pub remote: bool,
}

impl LaunchSpec {
    pub fn warning_summary(&self) -> Option<String> {
        (!self.warnings.is_empty()).then(|| self.warnings.join("; "))
    }
}

/// Why a NON-LOCAL environment (provider/k8s/ssh) could not be brought up while
/// failover cannot authorize host execution. Carried as the error so silent host
/// degradation is refused — the spawn site
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
    /// installed but failed its availability probe. The modal offers to start it, which
    /// is a fix the user can take without leaving thegn. `None` for every other
    /// halt (a missing token, an unreachable host — nothing to start).
    pub dormant: Option<thegn_core::sandbox_dormant::DormantRuntime>,
    /// This halt is a hides-home profile refusal that failover, backend `none`,
    /// disabled sandboxing, or an isolation-floor change cannot bypass.
    pub sealed_home: bool,
}

#[derive(Debug)]
pub(crate) struct WorktreeIsolationReadFailure {
    halt: SandboxHalt,
}

impl std::fmt::Display for WorktreeIsolationReadFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Short label only: the full halt (with its recovery hint) rides in
        // `source()`, which the halt-aware call sites (`sandbox_halt_in`,
        // `launch_state_gate_with_notice`) unpack. Printing the halt here too
        // would make every `{e:#}` on the CLI/daemon/pipeline paths show the
        // entire halt text, hint included, twice.
        formatter.write_str("worktree isolation setting unreadable")
    }
}

impl std::error::Error for WorktreeIsolationReadFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.halt)
    }
}

pub(crate) fn worktree_isolation_read_failure(
    worktree: &str,
    error: &anyhow::Error,
) -> anyhow::Error {
    anyhow::Error::new(WorktreeIsolationReadFailure {
        halt: SandboxHalt {
            env_name: "worktree isolation".into(),
            placement: worktree.to_string(),
            reason: format!(
                "thegn cannot read this worktree's isolation setting ({error:#}); refusing launch. Run `thegn doctor`, or once the database opens: {}",
                thegn_core::sandbox_floor::clear_profile_hint(worktree)
            ),
            ask: false,
            dormant: None,
            sealed_home: false,
        },
    })
}

/// A provider session was selected, but its trusted launch command could no
/// longer be constructed. This is a refusal, not a permission to reinterpret
/// the request as a host shell: the caller must prepare the provider again.
#[derive(Debug, Clone)]
pub(crate) struct DevcontainerLaunchRefused {
    pub reason: String,
}

impl std::fmt::Display for DevcontainerLaunchRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "devcontainer launch refused; prepare the environment again: {}",
            self.reason
        )
    }
}

impl std::error::Error for DevcontainerLaunchRefused {}

impl std::fmt::Display for SandboxHalt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.sealed_home {
            let fix = if self.reason.contains("thegn sandbox profile set standard") {
                String::new()
            } else {
                format!(
                    " Fix: {}",
                    thegn_core::sandbox_floor::standard_switch_hint(None)
                )
            };
            return write!(
                f,
                "environment '{}' ({}) could not be brought up: {} — thegn will not \
                 silently fall back to the host.{} If the required runtime is unavailable, \
                 install/start it (`thegn doctor` shows which).",
                self.env_name, self.placement, self.reason, fix
            );
        }
        use crate::env_halt::{HaltClass, classify_halt, remedy_text_for};
        let class = classify_halt(&self.placement, &self.reason);
        if class != HaltClass::LocalRuntime && !self.ask {
            return write!(
                f,
                "environment '{}' ({}) could not be brought up: {} — thegn will not \
                 silently fall back to the host. Fix the env, retry, or {}.",
                self.env_name,
                self.placement,
                self.reason,
                remedy_text_for(class, &self.env_name, &self.reason)
            );
        }
        let allow = if class != HaltClass::LocalRuntime {
            format!(
                "choose \"run on host\", set `backend = \"none\"`, disable sandboxing, or set `isolation_floor = \"off\"`; otherwise {}",
                remedy_text_for(class, &self.env_name, &self.reason)
            )
        } else if self.ask {
            "choose \"run on host\", set `backend = \"none\"`, disable sandboxing, or set `isolation_floor = \"off\"`; otherwise install/start a container runtime (`thegn doctor` shows which)".to_string()
        } else {
            "explicitly select host execution (`backend = \"none\"`, disable sandboxing, \
             set `isolation_floor = \"off\"`, or choose \"run on host\"), \
             or install/start a container runtime (`thegn doctor` shows which)"
                .to_string()
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
    /// The exact trusted SSH control target for an off-host pane. Carried out
    /// of environment preparation so route-credential installation cannot
    /// accidentally re-resolve a different host/config after bring-up.
    pub route_ssh_target: Option<SshTarget>,
    /// Proof that this launch passed the isolation-floor admission.
    pub(crate) admission: crate::sandbox_admission::Admission,
}

/// Actual startup/publication/fallback decision used below, after the existing
/// selection and trust gates. `Err` propagates before the OCI continuation;
/// private startup fixtures exercise this seam without a live trust database.
pub(crate) fn prepare_devcontainer(
    inputs: crate::devcontainer_provider::startup::StartInputs<'_>,
    worktree: &str,
    warnings: &mut Vec<String>,
) -> Result<bool, crate::devcontainer_provider::startup::StartFailure> {
    use crate::devcontainer_provider::startup::{StartFailure, StartReason};
    match crate::devcontainer_provider::DevcontainerSession::start(inputs)
        .and_then(|pending| pending.publish(worktree))
    {
        Ok(()) => Ok(true),
        Err(StartFailure::NotStarted(StartReason::ProbeUnavailable)) => Ok(false),
        Err(error @ StartFailure::NotStarted(_)) => {
            warnings.push(format!(
                "devcontainer CLI could not start the container ({error}); using OCI fallback"
            ));
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
struct VpnAttachFailed(String);

impl std::fmt::Display for VpnAttachFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sandbox vpn attach failed: {}", self.0)
    }
}

impl std::error::Error for VpnAttachFailed {}

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
    let mut selection =
        Db::open()?.effective_env_selection(worktree, &repo_root.to_string_lossy())?;
    if let Some(name) = selected_env {
        selection.name = Some(name.to_string());
    }
    prepare_sandbox_env_with_selection(
        cfg,
        repo_root,
        worktree,
        loc,
        backend_choice,
        choice_is_explicit,
        &selection,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_sandbox_env_with_selection(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    loc: &GitLoc,
    backend_choice: Option<&str>,
    choice_is_explicit: bool,
    selection: &thegn_core::sandbox_pin::EnvSelection,
    db: Option<&Db>,
) -> anyhow::Result<SandboxOutcome> {
    use crate::handlers::repo_trust::resolve_env_trusted;
    let generation =
        crate::config_startup::display().map_or(0, |admitted| admitted.revision().generation);
    let trusted = resolve_env_trusted(cfg, repo_root, loc, worktree, selection, db)?;
    let environment = trusted.environment;
    let devcontainer = trusted.devcontainer;
    // The MERGED pin (DB pin or an explicit caller profile): the sealed gate
    // below must refuse a disabled sandbox for THIS worktree's pin, which the
    // caller's `selection` argument alone does not carry (the launch path
    // passes `profile: None`).
    let pin = trusted.pin;
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
    // Failover never silently turns a requested sandbox into a host shell.
    // Host fallback is allowed only for a local explicit host/disabled policy or
    // the existing, deliberate run-on-host choice.
    let failover_mode = cfg.env_failover_mode(repo_root, &env_name);
    let ask = matches!(failover_mode, thegn_core::config::FailoverMode::Ask);
    let placement_label = placement.label();
    // A provider placement with no remote exec transport (daytona/custom with no
    // `exec_command`) would wrap every argv as a bare `/bin/sh -lc …` — a HOST
    // shell. Fail closed BEFORE any provisioning or billing, regardless of
    // `failover`: a provider placement is never a request for the host.
    if let Some(reason) = placement.transport_refusal() {
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason,
            ask: false,
            dormant: None,
            sealed_home: false,
        }
        .into());
    }
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
    let fallback_key =
        crate::sandbox_fallback_memo::Key::new(&env_name, &exec_placement.label(), worktree);
    crate::sandbox_fallback_memo::observe_generation(generation, &fallback_key);
    let mut env_is_remote = environment.is_remote() && cwd_override.is_none();
    let mut route_ssh_target = if env_is_remote {
        match &placement {
            thegn_core::placement::Placement::Ssh(ssh) => Some(SshTarget {
                host: ssh.host.clone(),
                port: ssh.port,
                forward_agent: ssh.forward_agent,
                ssh_config: ssh.ssh_config.clone(),
                jump_host: ssh.jump_host.clone(),
                identity: ssh.identity.clone(),
                extra_args: ssh.extra_args.clone(),
            }),
            _ => None,
        }
    } else {
        None
    };
    // Set when a provider bring-up failure degrades this open to the host (below),
    // so the outcome can heal a stale remote `worktrees.location` back to local.
    let mut degraded_from_provider = false;
    let request = thegn_core::launch_backend::launch_request(
        environment.sandbox,
        &placement,
        backend_choice,
        choice_is_explicit,
    );
    let mut sb = request.sandbox;
    let mut explicit_backend = request.explicit;
    let degrade_allowed = host_fallback_allowed(&placement, &sb, force_host_requested(worktree));
    let mut explicit_choice = explicit_backend.is_some();
    let auto_choice = sb.backend == thegn_core::config::SandboxBackend::Auto;
    let mut warnings = Vec::new();
    // Validate sources carried by the effective configuration before any
    // resolver/provider path can return `None` and turn the launch into a
    // host shell. An explicit disabled/`none` policy intentionally ignores
    // the sandbox volume map and keeps its historical host behavior.
    admit_configured_volume_sources(&sb)?;
    // The CLI provider is an optional, host-owned execution adapter. It is
    // considered only after core selection/trust and only for local, unprojected
    // worktrees. Its raw config path is never handed to the process when the
    // inventory contains a refused/reserved/unknown key (see repo_trust).
    if placement.is_local()
        && projection.is_none()
        && !unresolved_selection
        && crate::devcontainer_provider::can_honor_sandbox(&sb)
        && let Some(dc) = &devcontainer
        && dc.provider_eligible
    {
        // `can_honor_sandbox` only admits this provider for Off or SharedKernel
        // floors, and devcontainers are SharedKernel. Admit before bring-up so a
        // secure rejection cannot follow a container spawn.
        let admission = crate::sandbox_admission::admit(
            &sb,
            thegn_core::capabilities::IsolationClass::SharedKernel,
            worktree,
            force_host_requested(worktree),
            &mut warnings,
        )?;
        if prepare_devcontainer(
            crate::devcontainer_provider::startup::StartInputs {
                workspace: Path::new(worktree),
                config_path: &dc.config_path,
                digest: &dc.config_digest,
                content: &dc.config_content,
                env: &sb.passthrough_env(),
            },
            worktree,
            &mut warnings,
        )? {
            return Ok(SandboxOutcome {
                spec: None,
                backend_label: "devcontainer".to_string(),
                warnings,
                shell: env_shell,
                is_remote: false,
                cwd_override: None,
                location: None,
                degraded_from_provider: false,
                route_ssh_target: None,
                admission,
            });
        }
    }
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
                reason: crate::env_halt::unresolved_env_reason(
                    &env_name,
                    crate::env_halt::remote_features_clamped(),
                ),
                ask,
                dormant: None,
                sealed_home: sb.profile.hides_home(),
            }
            .into());
        }
        thegn_core::msg::warn(&format!(
            "env '{env_name}' is not configured; falling back to the host"
        ));
        warnings.push(format!("{env_name} not configured; running on the host"));
        degraded_from_provider = true;
    }
    let base_cname = sandbox::container_name_for_config_profile(worktree, &cfg.profile);
    let hardening = sb.profile;
    let cname = base_cname;
    if let Some(refusal) = static_tunnel_refusal(
        matches!(placement, thegn_core::placement::Placement::Provider(_)),
        hardening,
        &sb.vpn,
        &placement_label,
    ) {
        return Err(anyhow::Error::new(refusal));
    }
    let is_provider = matches!(placement, thegn_core::placement::Placement::Provider(_));
    if let Some(refusal) =
        thegn_core::sandbox_floor::provider_refusal_for(is_provider, &sb, &env_name, Some(worktree))
    {
        return Err(anyhow::Error::new(refusal));
    }
    if is_provider {
        let floor = thegn_core::sandbox_floor::floor_in_force(&sb, false);
        if floor != thegn_core::config::IsolationFloor::Off {
            warnings.push(format!(
                "{}: the isolation floor `{}` is not enforced by thegn on env `{env_name}`",
                thegn_core::sandbox_floor::PROVIDER_UNVERIFIED,
                floor.as_str()
            ));
        }
    }
    let mut provisioned = None;
    // Bring the execution placement up (k8s pod / provider sandbox) BEFORE
    // resolving the backend — a no-op for the default local `in_env` env. Warm-on-
    // open: create the API sandbox first if `auto_provision` is set so the
    // subsequent ensure/clone/connect find it live (8-E).
    if matches!(placement, thegn_core::placement::Placement::Provider(_)) {
        match auto_provision_sandbox(cfg, &env_name, worktree) {
            Ok(guard) => provisioned = guard,
            Err(e) => {
                // A one-time credential question is not a provider failure: keep
                // the typed error so the drain raises the consent menu instead
                // of a sandbox-halt modal.
                if let Some(prompt) = e
                    .chain()
                    .find_map(|src| src.downcast_ref::<ModelCredentialConsentRequired>())
                {
                    return Err(anyhow::Error::new(prompt.clone()));
                }
                // Provider won't provision (bad token, quota, API down): surface the
                // real cause (`{e:#}`); only an explicit local host choice may degrade.
                if !degrade_allowed {
                    return Err(SandboxHalt {
                        env_name: env_name.clone(),
                        placement: placement_label.clone(),
                        reason: format!("{e:#}"),
                        ask,
                        dormant: None,
                        sealed_home: false,
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
                route_ssh_target = None;
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
        }
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
                sealed_home: false,
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
        route_ssh_target = None;
        // Same as the auto-provision degrade above: heal the location to local
        // and drop any latched explicit backend so the host fallback runs.
        location = None;
        degraded_from_provider = true;
        explicit_backend = None;
        explicit_choice = false;
        sb.backend = thegn_core::config::SandboxBackend::None;
    }
    // Acquire the projection only after admission.
    let mut projection_lease: Option<ProjectionLease> = None;
    let mut provider_sync_done = false;
    // Scope one probe pass over the whole backend-resolution walk (every
    // candidate's `resolve_placed` + the halt-path `placement_reachable` below).
    // Within it an unreachable placement is probed once per backend, not
    // re-probed for each of the N candidates — bounding a wedged-transport stall.
    let _probe_pass = thegn_core::sandbox::probe_pass_guard();
    // A fail-closed isolation-floor miss on a non-explicit candidate: remembered
    // so the loop can try a stronger backend first, and surfaced as a halt below
    // only if none of them meets the floor.
    let mut floor_miss: Option<thegn_core::sandbox_floor::FloorMiss> = None;
    // Same, for a sealed profile whose `$HOME` confidentiality no candidate met.
    let mut home_miss: Option<String> = None;
    let mut compose_miss: Option<String> = None;
    // A provider placement resolves against its OWN location blob: a stale
    // Local `loc` (a DB row written while the provider had no transport) would
    // otherwise `cd` into the HOST worktree path on the remote.
    let provider_loc = match (&placement, &location, loc) {
        (thegn_core::placement::Placement::Provider(_), Some(blob), GitLoc::Local(_)) => {
            Some(GitLoc::from_db(worktree, Some(blob)))
        }
        _ => None,
    };
    let loc = provider_loc.as_ref().unwrap_or(loc);
    for candidate in thegn_core::launch_backend::candidates(&sb) {
        // `resolve_placed_exact`, not `resolve_placed`: `launch_backend::candidates` has
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
                if hardening.hides_home()
                    && !matches!(&spec.placement, thegn_core::placement::Placement::Ssh(_))
                {
                    return Err(SandboxHalt {
                        env_name: env_name.clone(),
                        placement: placement_label.clone(),
                        reason: format!(
                            "sealed profile `{}` needs a container runtime on this remote placement; refusing a bare shell for {worktree}",
                            hardening.as_str()
                        ),
                        ask,
                        dormant: None,
                        sealed_home: true,
                    }
                    .into());
                }
                ssh_none_guard(
                    &spec,
                    sb.backend,
                    degrade_allowed,
                    ask,
                    &env_name,
                    &placement_label,
                    hardening,
                )?;
                let admission = crate::sandbox_admission::admit(
                    &sb,
                    spec.capabilities().isolation,
                    worktree,
                    force_host_requested(worktree),
                    &mut warnings,
                )?;
                upload_provider_sync_once(
                    cfg,
                    env_data,
                    &placement,
                    &env_name,
                    worktree,
                    &mut provider_sync_done,
                    &mut warnings,
                );
                if let Some(guard) = provisioned.take() {
                    guard.commit();
                }
                return Ok(SandboxOutcome {
                    backend_label: spec.backend.label().to_string(),
                    spec: Some(spec),
                    warnings,
                    shell: env_shell,
                    is_remote: env_is_remote,
                    cwd_override,
                    location,
                    degraded_from_provider,
                    route_ssh_target,
                    admission,
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
            // Say so when the runtime about to hold this pane is one whose verbs
            // thegn has never run against the real thing. It got here only
            // because it was named explicitly (neither is in the default chain),
            // and its "available" answer came from a PATH probe, so a failure
            // downstream would otherwise look like thegn's bug rather than an
            // unfinished backend. Same rule as `unsupported_hardening`: report
            // the sandbox actually in force, not the one the config implies.
            if !spec.backend.verified() {
                warnings.push(format!(
                    "sandbox {}: unverified backend — thegn's commands for it have never been \
                     run against the real runtime, and it has no liveness check, so it was \
                     selected on PATH presence alone",
                    spec.backend.label()
                ));
            }
            // A Ready host's assets (digest-pinned image, warm volumes, remote
            // OCI url) pin the spec; explicit user values win inside.
            crate::host_flow::apply_ready(worktree, &mut spec);
            // An `oci_runtime` this engine cannot honor is ignored; say so
            // rather than implying a class the runtime will not deliver.
            thegn_core::sandbox::warn_unsupported_oci_runtime(&spec, &mut warnings);
            // Plan remote-OCI worktree sync only; the upload runs after admission.
            let recursive_submodules =
                crate::git_worktree::recursive_submodules_allowed(cfg, Path::new(worktree));
            let remote_plan = crate::remote_sync::plan_spec_before_ensure(
                &mut spec,
                worktree,
                recursive_submodules,
                &mut warnings,
            );
            // Validate the fully composed spec after Ready-host/remote fixups,
            // before VPN attachment or any ensure/secret-file/container effect.
            // A malformed named-volume source is terminal for this launch, not
            // an ordinary backend miss that may fall through to host execution.
            admit_final_sandbox_spec(&spec)?;
            // Isolation floor: compare the HONEST class of what this launch would
            // enter (after the runtime degrade `finalize_*` just applied) against
            // the demanded floor. A fail-closed miss must abort BEFORE anything
            // spawns on the host — so this sits ahead of `attach_vpn`/`ensure`.
            let admission = match crate::sandbox_admission::admit_candidate(
                &sb,
                candidate_floor_in_force(&sb, worktree),
                spec.capabilities().isolation,
                worktree,
                &mut warnings,
            ) {
                crate::sandbox_admission::CandidateAdmission::Admitted(admission) => admission,
                crate::sandbox_admission::CandidateAdmission::Miss(miss) => {
                    // Fail-closed. On an explicit pick, refuse now; otherwise
                    // remember the miss and try a stronger candidate. The
                    // end-of-loop halt fires only if no candidate meets the floor.
                    if explicit_choice {
                        anyhow::bail!("{} for {worktree}", miss.refusal());
                    }
                    floor_miss = Some(miss);
                    continue;
                }
            };
            // Degrade is fail-safe: admit_candidate flags and warns, then returns
            // an Admission with MissedDegrade rather than a Miss.
            // THE-215: a sealed launch must hide the host $HOME. Fail closed on a
            // backend/config that would expose it — same doctrine as the floor.
            if let Some(m) = thegn_core::sandbox_floor::home_gate(&spec) {
                if explicit_choice {
                    anyhow::bail!("{m} for {worktree}");
                }
                thegn_core::msg::warn(&format!("{m} for {worktree}; trying next backend"));
                home_miss = Some(m);
                continue;
            }
            if let Some(m) = thegn_core::sandbox_floor::strict_gate(&spec) {
                if explicit_choice {
                    anyhow::bail!("{m} for {worktree}");
                }
                thegn_core::msg::warn(&format!("{m} for {worktree}; trying next backend"));
                home_miss = Some(m);
                continue;
            }
            if let Some(m) = thegn_core::sandbox::compose_policy_refusal(&spec) {
                if explicit_choice {
                    anyhow::bail!("{m} for {worktree}");
                }
                thegn_core::msg::warn(&format!("{m} for {worktree}; trying next backend"));
                compose_miss = Some(m);
                continue;
            }
            if !spec.backend.is_oci() && spec.compose.is_some() {
                warnings.push(format!(
                    "sandbox {} does not support Compose; the Compose services were omitted",
                    spec.backend.label()
                ));
            }
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
            if let Some(note) = thegn_core::sandbox::hostname_rules_note(&spec) {
                warnings.push(note);
            }
            // The full candidate has passed the current floor, home and compose
            // gates above. Reuse only its prior ensure/exec failure; those gates
            // remain live for every resolution, including ones using this memo.
            if let Some(previous_warning) = crate::sandbox_fallback_memo::reusable_failure(
                explicit_choice,
                generation,
                &fallback_key,
                spec.backend.label(),
            ) {
                warnings.push(previous_warning);
                warnings.push(format!(
                    "sandbox {}: reused this session's earlier failed probe",
                    spec.backend.label()
                ));
                continue;
            }
            if let Some(plan) = remote_plan.as_ref() {
                crate::remote_sync::apply_remote_sync(&mut spec, plan, worktree, &mut warnings);
                // The retarget replaced the mounts: re-run the sealed-$HOME gate on what will be entered.
                if let Some(m) = thegn_core::sandbox_floor::home_gate(&spec) {
                    if explicit_choice {
                        anyhow::bail!("{m} for {worktree}");
                    }
                    thegn_core::msg::warn(&format!("{m} for {worktree}; trying next backend"));
                    home_miss = Some(m);
                    continue;
                }
            }
            upload_provider_sync_once(
                cfg,
                env_data,
                &placement,
                &env_name,
                worktree,
                &mut provider_sync_done,
                &mut warnings,
            );
            // Keep the final validation adjacent to the effects it protects:
            // VPN attachment and container ensure/exec are reached only after
            // the fully composed spec has passed the admission gate.
            let launch = admit_final_sandbox_spec_then(&mut spec, |spec| {
                if projection_lease.is_none()
                    && let Some(pspec) = &projection
                {
                    projection_lease =
                        ProjectionLease::acquire(worktree, pspec.clone(), &mut warnings);
                }
                // VPN up BEFORE the container (joins the sidecar netns); failure bails.
                if let Err(e) = attach_vpn(spec) {
                    return Err(anyhow::Error::new(VpnAttachFailed(e.to_string())));
                }
                // `ensure` proves the container RUNNING, but not that OCI `exec` works
                // (broken keep-id/crun); probe so the real error surfaces, not a vanish.
                Ok(run_with_vpn_rollback(spec, |spec| {
                    let socket_path = crate::daemon::socket_path(&cfg.daemon);
                    let socket_dir = socket_path.parent().map(Path::to_path_buf);
                    let extra = socket_dir.into_iter().collect::<Vec<_>>();
                    sandbox::ensure_attested(
                        spec,
                        &thegn_core::sandbox_compose::ComposeGuard::for_host(&extra),
                    ).and_then(|report| {
                        let isolation = thegn_core::sandbox_compose::compose_reported_isolation(
                            spec.capabilities().isolation,
                            &report,
                        );
                        if let Some(reason) = report.compose_unverified {
                            warnings.push(format!(
                                "sandbox {}: Compose config could not be inspected ({reason}); protected host binds were not checked; isolation reported as {}",
                                spec.backend.label(), isolation.as_str()
                            ));
                        }
                        thegn_core::sandbox_preflight::preflight_exec(spec)
                            // No prefix here: `preflight_exec` now owns its own error
                            // classing — a generic runtime failure still reads "exec
                            // probe failed: …", while a verified mount failure carries a
                            // headline + remedy that must arrive first in the
                            // width-fitted status line.
                            .map_err(|e| anyhow::anyhow!("{e}"))
                    })
                }))
            })?;
            match launch {
                Ok(()) => {
                    if let Some(lease) = projection_lease.take() {
                        lease.commit();
                    }
                    if let Some(guard) = provisioned.take() {
                        guard.commit();
                    }
                    return Ok(SandboxOutcome {
                        backend_label: spec.backend.label().to_string(),
                        spec: Some(spec),
                        warnings,
                        shell: env_shell,
                        is_remote: env_is_remote,
                        cwd_override,
                        location,
                        degraded_from_provider,
                        route_ssh_target,
                        admission,
                    });
                }
                Err(e) => {
                    if let Some(refusal) =
                        e.downcast_ref::<thegn_core::sandbox_compose::ComposeBindRefusal>()
                    {
                        let message = refusal.to_string();
                        if explicit_choice {
                            anyhow::bail!("{message} for {worktree}");
                        }
                        thegn_core::msg::warn(&format!(
                            "{message} for {worktree}; trying next backend"
                        ));
                        compose_miss = Some(message);
                        continue;
                    }
                    let m =
                        crate::sandbox_failure::sandbox_failed_message(spec.backend.label(), &e);
                    if explicit_choice {
                        anyhow::bail!("{m} for {worktree}")
                    }
                    if e.downcast_ref::<VpnAttachFailed>().is_none() {
                        crate::sandbox_fallback_memo::remember_failure(
                            generation,
                            fallback_key.clone(),
                            spec.backend.label(),
                            &m,
                        );
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
    // A fail-closed isolation floor that NO reachable backend could meet: refuse
    // to launch rather than drop to the host (the VPN `on_error = "fail"`
    // doctrine). This fires for local and remote alike — nothing has spawned.
    if let Some(m) = home_miss {
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: m,
            ask,
            dormant: None,
            sealed_home: true,
        }
        .into());
    }
    if let Some(m) = compose_miss {
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: m,
            ask,
            dormant: None,
            sealed_home: sb.profile.hides_home(),
        }
        .into());
    }
    if let Some(m) = floor_miss {
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: m.refusal(),
            ask,
            dormant: None,
            sealed_home: sb.profile.hides_home(),
        }
        .into());
    }
    // THE-215: a sealed profile never lands on the host. No spec, a `none`
    // backend or unreachable runtime all end here — refuse, regardless of
    // failover mode. A sealed worktree pin also refuses when global/env sandboxing
    // is disabled; an unpinned global sealed profile keeps its existing opt-out.
    if sealed_pin_requires_sandbox(sb.enabled, pin, hardening) {
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: if env_is_remote {
                format!(
                    "profile `{}` is a local-sandbox feature; this is a remote placement, so \
                     thegn cannot enforce hiding $HOME or removing the network for {worktree}; \
                     refusing the launch (see `thegn doctor`)",
                    hardening.as_str()
                )
            } else {
                format!(
                    "profile `{}` requires a sandbox that hides $HOME, but no backend produced \
                     one for {worktree}; refusing to open a host shell (see `thegn doctor`)",
                    hardening.as_str()
                )
            },
            ask,
            dormant: None,
            sealed_home: true,
        }
        .into());
    }
    // Reaching here means no candidate produced a runnable sandbox. A requested
    // sandbox must halt with the existing actionable error instead of becoming a
    // host login shell, regardless of failover mode. A chain-listed host is only
    // a fallback preference; the floor admission below still applies.
    if !degrade_allowed && !(placement.is_local() && auto_chain_names_host(&sb)) {
        let reachable = sandbox::placement_reachable(&exec_placement, &sb.backend_chain);
        return Err(SandboxHalt {
            env_name: env_name.clone(),
            placement: placement_label.clone(),
            reason: crate::remote_sync::no_backend_reason(reachable, &warnings),
            ask,
            dormant: None,
            sealed_home: false,
        }
        .into());
    }
    // An installed runtime failed its availability probe. A stopped service is
    // one possible cause; access or runtime failure can look the same. Offer
    // the configured recovery choices — start it, run on the host, or
    // cancel — per `[sandbox] on_dormant`. A host choice is explicit;
    // `failover = "auto"` is not a permission to bypass containment.
    if placement.is_local() && !degrade_allowed {
        // Only an explicit containment choice counts as asking for this recovery
        // path; reached only by an auto chain that names the host (the halt
        // above refuses every other miss), so `wanted` is false and this never
        // nags a default config.
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
                        sealed_home: false,
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
                    reason: format!("{} is installed but unavailable — inspect runtime health and access with `thegn doctor`", rt.name),
                    ask,
                    dormant: Some(rt),
                    sealed_home: false,
                }
                .into());
            }
        }
    }
    // No runnable candidate remains. Admission must compare the actual host
    // fallback even when no earlier candidate reached the floor check at all.
    let admission = crate::sandbox_admission::admit(
        &sb,
        thegn_core::capabilities::IsolationClass::HostProcess,
        worktree,
        force_host_requested(worktree),
        &mut warnings,
    )?;
    // Every candidate is spent and we are opening a bare host shell. Say it once
    // here — under `Fallthrough::Exact` the resolver deliberately stays quiet,
    // because only this loop knows which candidate was the last one.
    // An explicit `none`/disabled sandbox is already an intentional host
    // decision. Calling the notice in that case would apply `on_missing` to a
    // non-miss (including `OnMissing::Fail` → process exit) and probe runtimes
    // that the user did not request. Keep the notice for genuine auto-chain
    // fallback, while preserving the isolation-floor admission above.
    if sb.enabled && sb.backend != thegn_core::config::SandboxBackend::None {
        // `on_missing = "fail"` refuses THIS pane. The shared notice would
        // `msg::die` — exiting the whole TUI from a (possibly automatic prewarm)
        // worker thread with the terminal still in raw mode.
        // An explicit "run on host" answer (the halt modal's choice) still wins.
        if sb.on_missing == thegn_core::config::OnMissing::Fail && !force_host_requested(worktree) {
            return Err(SandboxHalt {
                env_name: env_name.clone(),
                placement: placement_label.clone(),
                reason: "no container backend available and `[sandbox] on_missing = \"fail\"`"
                    .to_string(),
                ask: false,
                dormant: None,
                sealed_home: false,
            }
            .into());
        }
        thegn_core::sandbox_backend::host_fallback_notice(&sb, &exec_placement);
    }
    if sb.enabled && auto_choice && warnings.is_empty() {
        warnings.push("sandbox auto selected host".to_string());
    } else if sb.enabled && auto_choice {
        warnings.push("running on host after sandbox fallback".to_string());
    }
    if projection_lease.is_none()
        && let Some(pspec) = &projection
    {
        projection_lease = ProjectionLease::acquire(worktree, pspec.clone(), &mut warnings);
    }
    if let Some(lease) = projection_lease.take() {
        lease.commit();
    }
    if let Some(guard) = provisioned.take() {
        guard.commit();
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
        route_ssh_target,
        admission,
    })
}

fn sealed_pin_requires_sandbox(
    sandbox_enabled: bool,
    pin: Option<thegn_core::config::SandboxProfile>,
    effective: thegn_core::config::SandboxProfile,
) -> bool {
    // Only a pin that itself hides $HOME refuses a disabled sandbox: the
    // effective profile may come from a repo overlay or zone default rather
    // than this worktree's choice, and a `standard` pin is not a sealed ask.
    effective.hides_home() && (sandbox_enabled || pin.is_some_and(|p| p.hides_home()))
}

/// Validate the effective configured sources before backend resolution. The
/// resolver is allowed to return `None` for a disabled/explicit-host policy,
/// but an enabled sandbox with configured sources must refuse malformed input
/// before that `None` can become a host fallback.
fn admit_configured_volume_sources(sb: &thegn_core::config::SandboxConfig) -> anyhow::Result<()> {
    if !sb.enabled || sb.backend == thegn_core::config::SandboxBackend::None {
        return Ok(());
    }
    thegn_core::sandbox::admit_volume_sources(sb.volumes.keys().map(String::as_str))
        .map_err(anyhow::Error::new)
        .context("sandbox configured volume admission failed")
}

/// Validate a fully composed spec at the last pure boundary before runtime
/// effects. Keeping this as the production gate also gives tests a behavioral
/// seam: an invalid spec returns before a caller can perform VPN, ensure,
/// secret-file, or OCI argv work.
fn admit_final_sandbox_spec(spec: &thegn_core::sandbox::SandboxSpec) -> anyhow::Result<()> {
    thegn_core::sandbox::admit_volumes(spec)
        .map_err(anyhow::Error::new)
        .context("sandbox volume admission failed")?;
    thegn_core::sandbox::tunnel_admission(spec).map_err(anyhow::Error::new)?;
    Ok(())
}

/// Run the effectful launch phase only after the final spec passes admission.
/// The callback form makes the no-effect guarantee directly testable without
/// starting an OCI runtime or VPN in unit tests.
fn admit_final_sandbox_spec_then<T>(
    spec: &mut thegn_core::sandbox::SandboxSpec,
    effects: impl FnOnce(&mut thegn_core::sandbox::SandboxSpec) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    admit_final_sandbox_spec(spec)?;
    effects(spec)
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
    let Some(vpn) = spec.vpn.clone() else {
        return Ok(());
    };
    let sidecar_capable = sidecar_capable(spec, vpn.mode);

    if !sidecar_capable {
        return apply_vpn_error(
            spec,
            vpn.on_error,
            false,
            format!(
                "vpn: provider '{}' in mode '{}' needs a local OCI backend (got '{}')",
                vpn.provider,
                vpn.mode,
                spec.backend.label()
            ),
        );
    }

    let Some(prefix) = sandbox::oci_runtime_prefix(spec.backend) else {
        return apply_vpn_error(
            spec,
            vpn.on_error,
            false,
            "vpn: no OCI runtime for backend".to_string(),
        );
    };
    let rt = thegn_svc::vpn::OciRuntime::new(prefix);
    let provider = thegn_svc::vpn::for_provider(&vpn);
    attach_vpn_with(spec, &provider, &rt, remove_vpn_sidecar)?;
    Ok(())
}

fn attach_vpn_with<P: VpnProvider>(
    spec: &mut sandbox::SandboxSpec,
    provider: &P,
    rt: &thegn_svc::vpn::OciRuntime,
    remove_sidecar: impl FnOnce(&thegn_svc::vpn::OciRuntime, &str) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(vpn) = spec.vpn.clone() else {
        return Ok(());
    };
    let sidecar = sandbox::vpn_sidecar_name(&spec.name);
    let on_error = vpn.on_error;
    let placement = if spec.placement.is_local() {
        "local".to_string()
    } else {
        spec.placement.label()
    };
    let attach = match provider.up_for_worktree(rt, &sidecar, &spec.worktree, &placement) {
        Ok(attach) => attach,
        Err(e) => {
            if e.downcast_ref::<thegn_svc::vpn::VpnSidecarDrift>()
                .is_some()
            {
                return apply_vpn_error(spec, on_error, true, e.to_string());
            }
            rollback_vpn(provider, rt, &sidecar, remove_sidecar);
            return apply_vpn_error(spec, on_error, true, format!("vpn: bring-up failed: {e}"));
        }
    };
    if let Err(e) = provider.ready(rt, &sidecar, vpn.ready_timeout) {
        rollback_vpn(provider, rt, &sidecar, remove_sidecar);
        return apply_vpn_error(spec, on_error, true, format!("vpn: {e}"));
    }
    // Userspace tunnels: point the inner process at the SOCKS/HTTP proxy.
    if let Some(proxy) = &attach.proxy {
        for (k, v) in proxy.env_exports() {
            spec.env_overrides.insert(k, v);
        }
    }
    Ok(())
}

fn apply_vpn_error(
    spec: &mut sandbox::SandboxSpec,
    on_error: thegn_core::config::VpnOnError,
    clear_vpn_on_warn: bool,
    message: String,
) -> anyhow::Result<()> {
    use thegn_core::config::VpnOnError;
    match on_error {
        VpnOnError::Fail => Err(anyhow::anyhow!(message)),
        VpnOnError::Warn => {
            thegn_core::msg::warn(&format!("{message}; continuing (on_error=warn)"));
            if clear_vpn_on_warn {
                spec.vpn = None;
            }
            Ok(())
        }
        VpnOnError::Offline => {
            thegn_core::msg::warn(&format!(
                "{message}; forcing network=none (on_error=offline)"
            ));
            spec.network = thegn_core::config::Network::None;
            spec.vpn = None;
            Ok(())
        }
    }
}

fn sidecar_capable(spec: &sandbox::SandboxSpec, mode: thegn_core::config::VpnMode) -> bool {
    spec.backend.is_oci()
        && spec.placement.is_local()
        && matches!(
            mode,
            thegn_core::config::VpnMode::Sidecar | thegn_core::config::VpnMode::Proxy
        )
}

fn rollback_vpn<P: VpnProvider>(
    provider: &P,
    rt: &thegn_svc::vpn::OciRuntime,
    sidecar: &str,
    remove_sidecar: impl FnOnce(&thegn_svc::vpn::OciRuntime, &str) -> anyhow::Result<()>,
) {
    if let Err(error) = provider.down(rt, sidecar) {
        tracing::warn!(%error, sidecar, "could not deregister failed VPN sidecar");
    }
    if let Err(error) = remove_sidecar(rt, sidecar) {
        tracing::warn!(%error, sidecar, "could not remove failed VPN sidecar");
    }
}

#[expect(clippy::disallowed_methods)]
fn remove_vpn_sidecar(rt: &thegn_svc::vpn::OciRuntime, sidecar: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut argv = rt.prefix.clone();
    argv.extend(["rm".to_string(), "-f".to_string(), sidecar.to_string()]);
    let output = std::process::Command::new(timeout_bin()?)
        .arg("--kill-after=5")
        .arg("10")
        .arg(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("run {} rm -f to remove VPN sidecar", argv[0]))?;
    if !output.status.success() {
        anyhow::bail!("{} rm -f exited with {}", argv[0], output.status);
    }
    Ok(())
}

fn run_with_vpn_rollback<T>(
    spec: &mut sandbox::SandboxSpec,
    effects: impl FnOnce(&mut sandbox::SandboxSpec) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let Some(vpn) = spec.vpn.clone() else {
        return effects(spec);
    };
    if !sidecar_capable(spec, vpn.mode) {
        return effects(spec);
    }
    let Some(prefix) = sandbox::oci_runtime_prefix(spec.backend) else {
        return effects(spec);
    };
    let rt = thegn_svc::vpn::OciRuntime::new(prefix);
    let provider = thegn_svc::vpn::for_provider(&vpn);
    run_with_vpn_rollback_using(spec, &provider, &rt, effects, remove_vpn_sidecar)
}

fn run_with_vpn_rollback_using<T, P: VpnProvider>(
    spec: &mut sandbox::SandboxSpec,
    provider: &P,
    rt: &thegn_svc::vpn::OciRuntime,
    effects: impl FnOnce(&mut sandbox::SandboxSpec) -> anyhow::Result<T>,
    remove_sidecar: impl FnOnce(&thegn_svc::vpn::OciRuntime, &str) -> anyhow::Result<()>,
) -> anyhow::Result<T> {
    let sidecar_enabled = spec
        .vpn
        .as_ref()
        .is_some_and(|vpn| sidecar_capable(spec, vpn.mode));
    let result = effects(spec);
    if result.is_err() && sidecar_enabled {
        let sidecar = sandbox::vpn_sidecar_name(&spec.name);
        rollback_vpn(provider, rt, &sidecar, remove_sidecar);
    }
    result
}

/// Best-effort: de-register a worktree's ephemeral VPN node before its sidecar
/// container is removed (e.g. `tailscale logout`). Called from the worktree-close
/// teardown thread, which only has the path — so we re-resolve the effective
/// config. A no-op when no VPN is configured. Ephemeral keys also auto-reap
/// server-side once the sidecar dies, so this is an optimization, not required.
pub(crate) fn deregister_vpn(path: &str) {
    let cfg = crate::config_startup::cleanup_config();
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
        let _ = provider.down(&rt, &sidecar); // best-effort: teardown pass: the next runtime attempt reports the real failure
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectionRollback {
    Unmount,
    Nothing,
}

pub(crate) fn rollback_action(mode: thegn_core::config::DataMode) -> ProjectionRollback {
    match mode {
        thegn_core::config::DataMode::InEnv => ProjectionRollback::Nothing,
        thegn_core::config::DataMode::LocalExec => ProjectionRollback::Nothing,
        thegn_core::config::DataMode::Sshfs => ProjectionRollback::Unmount,
        thegn_core::config::DataMode::Sync => ProjectionRollback::Nothing,
    }
}

type ProjectionRollbackFn =
    Box<dyn FnOnce(&thegn_core::projection::ProjectionSpec) -> anyhow::Result<()> + Send>;

pub(crate) struct ProjectionLease {
    worktree: String,
    spec: thegn_core::projection::ProjectionSpec,
    rollback: Option<ProjectionRollbackFn>,
}

impl ProjectionLease {
    fn acquire_with(
        worktree: &str,
        spec: thegn_core::projection::ProjectionSpec,
        mount: impl FnOnce(&thegn_core::projection::ProjectionSpec) -> anyhow::Result<()>,
        rollback: ProjectionRollbackFn,
        kind: &str,
        warnings: &mut Vec<String>,
    ) -> Option<Self> {
        if let Err(error) = mount(&spec) {
            warnings.push(format!("projection ({kind}) failed: {error}"));
            thegn_core::msg::warn(&format!("projection mount failed for {worktree}: {error}"));
            return None;
        }
        Some(Self {
            worktree: worktree.to_string(),
            spec,
            rollback: Some(rollback),
        })
    }

    fn acquire(
        worktree: &str,
        spec: thegn_core::projection::ProjectionSpec,
        warnings: &mut Vec<String>,
    ) -> Option<Self> {
        let backend = thegn_svc::projection::for_data_mode(&spec);
        let kind = backend.kind();
        Self::acquire_with(
            worktree,
            spec,
            |spec| {
                thegn_svc::projection::for_data_mode(spec)
                    .mount(spec)
                    .map(|_| ())
            },
            Box::new(|spec| match rollback_action(spec.mode) {
                ProjectionRollback::Unmount => {
                    thegn_svc::projection::for_data_mode(spec).unmount(spec)
                }
                ProjectionRollback::Nothing => Ok(()),
            }),
            kind,
            warnings,
        )
    }

    fn commit(mut self) {
        register_projection(&self.worktree, self.spec.clone());
        drop(self.rollback.take());
    }
}

impl Drop for ProjectionLease {
    fn drop(&mut self) {
        if let Some(rollback) = self.rollback.take()
            && let Err(error) = rollback(&self.spec)
        {
            tracing::warn!(
                target: "thegn::sandbox",
                %error,
                worktree = %self.worktree,
                "projection rollback failed"
            );
        }
    }
}

/// Automatic collection cannot tear down remote projections/sync from a path.
/// A poisoned registry is unknown ownership, never proof of no resources.
pub(crate) fn automatic_cleanup_resources_absent(path: &str) -> Result<(), String> {
    let projection = projection_registry()
        .lock()
        .map_err(|_| "projection registry unavailable")?;
    let sync = provider_sync_registry()
        .lock()
        .map_err(|_| "provider sync registry unavailable")?;
    if projection.contains_key(path) || sync.contains_key(path) {
        return Err("projection/provider sync requires explicit owned cleanup".into());
    }
    Ok(())
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
        let _ = backend.unmount(&spec); // best-effort: teardown: unmount of a possibly-already-gone mount
    }
}

// Provider construction + name resolution live in `provider_factory.rs`
// (extracted to keep it flat); re-exported so call sites are unchanged.
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

/// Resolve `(provider, sandbox id, workdir, provider kind, custody account)` for
/// a worktree's PROVIDER env — for
/// the SSH-over-WSS proxy path (`[env.<name>.provider] connect = "ssh"`). Unlike
/// [`native_shell_exec`] it does NOT gate on the exec mode/health: it only needs
/// the provider handle + the resolved sandbox id to open the TCP proxy. `None`
/// when the env isn't a provider placement or the provider can't be built (e.g.
/// the API token isn't set). Resolves the env exactly like `native_shell_exec`.
pub fn provider_proxy_target(
    cfg: &Config,
    worktree: &str,
) -> Option<(
    thegn_svc::provider::Provider,
    String,
    String,
    String,
    String,
)> {
    if !provider_envs_configured(cfg) {
        return None;
    }
    let environment = resolve_provider_env(cfg, worktree, "provider proxy target")?;
    let thegn_core::placement::Placement::Provider(p) = &environment.placement else {
        return None;
    };
    let pc = &cfg.env.get(&environment.name)?.provider;
    // Bound-aware id (like `native_exec_for`/`provider_sandbox_name`): a claimed
    // warm-pool spare targets that spare, not the derived husk the pool flow
    // destroyed — else the ssh proxy + p2p push relay into the wrong sandbox.
    let id = match crate::provider_factory::sandbox_id_for(worktree, &p.id) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, error = %format!("{error:#}"), "provider proxy refused: warm-pool sandbox binding unreadable");
            return None;
        }
    };
    let provider = provider_for_named(pc, &id)?;
    let workdir = crate::provider_workdir::resolve(pc, &id);
    let provider_kind = pc.provider.trim().to_string();
    let custody_account = match crate::provider_factory::managed_custody_record(pc, &id) {
        Ok(Some(record)) => record.account,
        Ok(None) => crate::provider_factory::managed_custody_account(pc),
        // Ambiguous custody cannot prove the exact account selected by the
        // worktree, so fail closed before opening a transport.
        Err(_) => return None,
    };
    Some((provider, id, workdir, provider_kind, custody_account))
}

/// Builds the `sprite-proxy` refusal message after [`provider_proxy_target`]
/// returned `None`. This is a second, best-effort read of the selection, not a
/// diagnosis of the refusal's own cause: a state-database, location, or
/// environment-selection read failure is named as such (the refusal
/// itself only reaches tracing, which has no sink in a ProxyCommand child);
/// otherwise the generic reasons stand.
pub(crate) fn provider_proxy_refusal(worktree: &str) -> String {
    if let Err(error) = GitLoc::for_worktree_op(Path::new(worktree), "provider proxy target") {
        return format!("sprite-proxy: {error}");
    }
    let read = Db::open().and_then(|db| {
        let repo_root: PathBuf = db
            .repo_root_for(worktree)?
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| repo::main_worktree(Path::new(worktree)))
            .unwrap_or_else(|| PathBuf::from(worktree));
        db.effective_env_checked(worktree, &repo_root.to_string_lossy())
    });
    proxy_refusal_reason(worktree, read)
}

fn proxy_refusal_reason(worktree: &str, read: anyhow::Result<Option<String>>) -> String {
    match read {
        Err(error) => format!(
            "sprite-proxy: {worktree}: the environment selection could not be read ({error:#}); refusing rather than resolving the default environment"
        ),
        Ok(_) => format!(
            "sprite-proxy: {worktree} has no provider proxy target: it is not a provider env, its API token is unset, or its warm-pool binding or custody record could not be read"
        ),
    }
}

pub(crate) use crate::agent_ssh::{
    SPRITE_SSHD_PORT, SpriteSshCustody, generate_managed_keypair_at, managed_ssh_keypair,
    mosh_setup_script, sprite_ssh_argv, sprite_ssh_argv_for_custody, sprite_ssh_connect,
    sprite_ssh_keypair, sprite_sshd_setup_script, sprite_sshd_start_script,
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
    native_exec_for(cfg, worktree, None).into_option()
}

pub(crate) fn native_shell_exec_for_pane(cfg: &Config, worktree: &str) -> PaneNativeExec {
    native_exec_for(cfg, worktree, None)
}

pub(crate) enum PaneNativeExec {
    Ready(Box<NativeShell>),
    Unavailable,
    AccountReadRefused(String),
}

impl PaneNativeExec {
    fn into_option(self) -> Option<NativeShell> {
        match self {
            Self::Ready(native) => Some(*native),
            Self::Unavailable | Self::AccountReadRefused(_) => None,
        }
    }
}

/// The answer a project gave to the model-credential question, kept in process
/// so a menu choice is visible to the retry before the on-disk write drains.
/// The DB remains the durable store (see
/// [`thegn_core::model_credential_consent`]).
pub(crate) type ModelConsent = thegn_core::model_credential_consent::Consent;
type ConsentMemo = std::sync::Mutex<std::collections::HashMap<String, ModelConsent>>;

fn consent_memo() -> &'static ConsentMemo {
    static MEMO: std::sync::OnceLock<ConsentMemo> = std::sync::OnceLock::new();
    MEMO.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn consent_key(provider: &str, scope: &str) -> String {
    format!("{}\u{1f}{scope}", provider.trim().to_ascii_lowercase())
}

/// Memoize an answer for a worktree and/or its repo root. Public so the menu
/// handler and the warm-pool claim can close the write-then-retry race.
pub(crate) fn memo_model_credential_consent(provider: &str, scope: &str, consent: ModelConsent) {
    if let Ok(mut memo) = consent_memo().lock() {
        memo.insert(consent_key(provider, scope), consent);
    }
}

fn memo_model_credential_consent_get(provider: &str, scope: &str) -> Option<ModelConsent> {
    consent_memo()
        .lock()
        .ok()?
        .get(&consent_key(provider, scope))
        .copied()
}

fn not_now_memo() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static NOT_NOW: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    NOT_NOW.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Withhold credentials for the next launch only. Stored separately from the
/// durable answer so it is consumed once and then the question returns.
pub(crate) fn withhold_model_credentials_once(provider: &str, worktree: &str) {
    if let Ok(mut memo) = not_now_memo().lock() {
        memo.insert(consent_key(provider, worktree));
    }
}

fn take_model_credentials_not_now(provider: &str, worktree: &str) -> bool {
    not_now_memo()
        .lock()
        .map(|mut memo| memo.remove(&consent_key(provider, worktree)))
        .unwrap_or(false)
}

/// Resolve an unset Sprites `model_credentials`: the one-shot "not now" wins
/// for this launch, then the in-process memo, then the durable per-project
/// answer. A failed read is logged and treated as no answer (fail closed).
fn resolve_sprites_model_credentials(
    worktree: &str,
    project: &str,
    provider: &str,
) -> Option<ModelConsent> {
    let provider = provider.trim().to_ascii_lowercase();
    if take_model_credentials_not_now(&provider, worktree) {
        return Some(ModelConsent::Never);
    }
    if let Some(consent) = memo_model_credential_consent_get(&provider, worktree) {
        return Some(consent);
    }
    if let Some(consent) = memo_model_credential_consent_get(&provider, project) {
        memo_model_credential_consent(&provider, worktree, consent);
        return Some(consent);
    }
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(
                target: "thegn::provider",
                %error,
                "model credential consent read failed; withholding"
            );
            return None;
        }
    };
    match thegn_core::model_credential_consent::load(&db, project, &provider) {
        Ok(Some(consent)) => {
            memo_model_credential_consent(&provider, project, consent);
            memo_model_credential_consent(&provider, worktree, consent);
            Some(consent)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(
                target: "thegn::provider",
                %error,
                "model credential consent read failed; withholding"
            );
            None
        }
    }
}

/// What an unset Sprites key should do for this launch. Callers only pass
/// Sprites envs; `consent` is `None` when nothing is remembered.
fn sprites_credential_launch(
    pc: &thegn_core::config::EnvProviderConfig,
    consent: Option<ModelConsent>,
    interactive: bool,
) -> thegn_core::model_credential_consent::Launch {
    thegn_core::model_credential_consent::decide(
        pc.model_credentials,
        pc.model_credentials_is_explicit(),
        consent,
        interactive,
    )
}

/// Map a resolved answer onto the provisioning mode. Only a remembered
/// `Forward` uploads; `Never` and "no answer yet" withhold.
fn sprites_credential_mode(consent: Option<ModelConsent>) -> thegn_core::config::ModelCredentials {
    match consent {
        Some(ModelConsent::Forward) => thegn_core::config::ModelCredentials::Upload,
        _ => thegn_core::config::ModelCredentials::None,
    }
}

/// The agents whose login files the provisioner would upload. Mirrors the
/// provisioning selection: the env overlay, then the global `[sandbox.home]`
/// list, then the `[[agents]]` picker, then host detection. The prompt must
/// name them; an empty list still reads as "agent login".
fn model_credential_agents(cfg: &Config, env: &thegn_core::config::EnvConfig) -> Vec<String> {
    let overlay = env
        .sandbox
        .home
        .as_ref()
        .and_then(|home| home.agents.clone());
    let mut agents = overlay.unwrap_or_else(|| cfg.sandbox.home.agents.clone());
    if agents.is_empty() {
        let picker = provisioned_agent_kinds(cfg);
        agents = if picker.is_empty() {
            detect_host_agents()
        } else {
            picker
        };
    }
    agents
        .into_iter()
        .map(|agent| agent.trim().to_string())
        .filter(|agent| !agent.is_empty())
        .collect()
}

/// The fixed model-credential env keys the provisioner would forward. Values
/// are never read here, so nothing secret can reach the prompt.
fn model_credential_keys(cfg: &Config, repo_root: &Path) -> Vec<String> {
    let mut keys: Vec<String> = cfg
        .repo_sandbox(repo_root)
        .passthrough_env_remote()
        .into_iter()
        .map(|(key, _)| key)
        .filter(|key| {
            thegn_core::provider_credentials::MODEL_CREDENTIAL_ENV.contains(&key.as_str())
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// The menu body wording. Safe fields only: agent names, fixed key names and
/// the sandbox name.
pub fn model_credential_question(prompt: &ModelCredentialConsentRequired) -> String {
    let logins = match prompt.agents.as_slice() {
        [] => "agent login".to_string(),
        [one] => format!("{one} login"),
        many => format!("{} logins", many.join(", ")),
    };
    let keys = if prompt.keys.is_empty() {
        "model API keys".to_string()
    } else {
        prompt.keys.join(", ")
    };
    format!("Forward your {logins} and {keys} to {}?", prompt.sandbox)
}

/// A launch refused until the user answers the one-time model-credential
/// question. Carried as a typed error so the drain can raise the menu on the
/// focused tab and a background prewarm can defer without prompting.
#[derive(Debug, Clone)]
pub struct ModelCredentialConsentRequired {
    pub env_name: String,
    pub project: String,
    pub provider: String,
    pub worktree: String,
    pub sandbox: String,
    pub agents: Vec<String>,
    pub keys: Vec<String>,
}

impl std::fmt::Display for ModelCredentialConsentRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("forwarding model credentials needs your approval")
    }
}

impl std::error::Error for ModelCredentialConsentRequired {}

/// Like [`native_shell_exec`] but runs the AGENT `choice`'s command in the sandbox
/// over the native exec API — so the "Agent"/claude/codex/… picker entries run
/// INSIDE the sprite (where the code is), instead of via the provider CLI prefix
/// (`sprite exec …`) which isn't installed on the host. `None` for the plain shell
/// choice (use [`native_shell_exec`]) or a non-native-exec env. The agent inherits
/// the same in-sprite env as the shell (host secrets + proxy routing when set), so
/// e.g. the managed pi snippet's `$HOME/.thegn/pi` resolves to the sprite home.
pub(crate) fn native_agent_exec_for_pane(
    cfg: &Config,
    worktree: &str,
    choice: &str,
) -> PaneNativeExec {
    if choice.is_empty() || choice == SHELL || choice == "clean-shell" {
        return PaneNativeExec::Unavailable;
    }
    // Only real agents (not tools) route through here.
    if cfg.agent_command(choice).is_none() {
        return PaneNativeExec::Unavailable;
    }
    // A grant the harness cannot take refuses the AGENT pane (THE-440) — the
    // caller falls back to a plain login shell, which runs no agent under the
    // wrong policy. Visible, never silent.
    let cmd = match resolve_command(cfg, choice) {
        Ok(cmd) => cmd,
        Err(why) => {
            tracing::warn!(target: "thegn::agent", agent = %choice, "{why}");
            thegn_core::msg::warn(&format!("{why}; no agent pane was started"));
            return PaneNativeExec::Unavailable;
        }
    };
    native_exec_for(cfg, worktree, Some(cmd))
}

/// Shared body: build a [`NativeShell`] for a native-exec provider worktree. With
/// `agent_cmd = None` the inner command is the login shell; with `Some(cmd)` it's
/// that agent command (run via the same `cd workdir; <cmd>` wrapper).
fn native_exec_for(cfg: &Config, worktree: &str, agent_cmd: Option<String>) -> PaneNativeExec {
    use thegn_core::config::ProviderExecMode;
    // Config-only fast path: `resolve_env` only yields `Placement::Provider`
    // from an `[env.*]` with `placement = "provider"`, so without one this
    // resolves to `Unavailable` and no database is opened (the pane spawn path).
    if !provider_envs_configured(cfg) {
        return PaneNativeExec::Unavailable;
    }
    let Some(environment) = resolve_provider_env(cfg, worktree, "native exec") else {
        return PaneNativeExec::Unavailable;
    };
    let thegn_core::placement::Placement::Provider(p) = &environment.placement else {
        return PaneNativeExec::Unavailable;
    };
    let Some(env_cfg) = cfg.env.get(&environment.name) else {
        return PaneNativeExec::Unavailable;
    };
    let pc = &env_cfg.provider;
    let kind = thegn_core::config::EnvProviderKind::of(&pc.provider);
    // The pane path runs on the event loop, so it reads the in-process memo only.
    // Provisioning (which always precedes the first pane launch) populates it.
    let mode = if kind == thegn_core::config::EnvProviderKind::Sprites
        && !pc.model_credentials_is_explicit()
    {
        sprites_credential_mode(memo_model_credential_consent_get(&pc.provider, worktree))
    } else {
        pc.model_credentials
    };
    let grant = thegn_core::provider_credentials::model_grant(kind, mode);
    if let Some(request) =
        thegn_core::sandbox_floor::provider_explicit_request(&environment.sandbox)
    {
        tracing::warn!(
            target: "thegn::sandbox",
            env = %environment.name,
            request = %request.describe(),
            "provider pane refused: explicit isolation request"
        );
        return PaneNativeExec::Unavailable;
    }
    if pc.exec == ProviderExecMode::Cli || !thegn_svc::provider::exec_api_by_name(&pc.provider) {
        return PaneNativeExec::Unavailable;
    }
    // Auto backs off to the CLI pane during a provider's failure cooldown; api
    // always tries native.
    if pc.exec == ProviderExecMode::Auto && !native_exec_healthy(&pc.provider) {
        return PaneNativeExec::Unavailable;
    }
    // Token missing ⇒ no provider built ⇒ fall back to the CLI path (which has
    // its own behavior when unconfigured); don't silently spawn a dead session.
    let Some(provider) = provider_for(pc) else {
        return PaneNativeExec::Unavailable;
    };
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
    let exports = match crate::agent_configs::account_exports_or_refuse(
        crate::agent_configs::account_pane_env_exports(cfg, worktree),
    ) {
        Ok(exports) => exports,
        Err(reason) => return PaneNativeExec::AccountReadRefused(reason),
    };
    let inner = format!(
        "{}{exports}{inner}",
        crate::remote_enqueue_auth::source_prefix(worktree)
    );
    // Carry the host's passthrough secrets (GH_TOKEN, ANTHROPIC_API_KEY, …) into
    // the provider exec so the in-sprite shell + any agent it spawns (pi, claude
    // code, hermes) work like local. Remote-safe filter drops host-local socket
    // vars (SSH_AUTH_SOCK/GPG_*) that would dangle in the VM. THEGN_* win.
    let mut env = environment.sandbox.passthrough_env_remote();
    if !grant.env_keys {
        env = thegn_core::provider_credentials::strip_model_env(env).0;
    }
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
    let host_locale = crate::remote_term::host_locale(&|k| std::env::var(k).ok());
    crate::remote_term::apply(
        &mut env,
        crate::remote_term::PANE_TERM,
        Some(crate::remote_term::PANE_COLORTERM),
        host_locale.as_deref(),
    );
    env.push(("THEGN_WORKTREE".to_string(), worktree.to_string()));
    env.push(("THEGN_BRANCH".to_string(), String::new()));
    // A claimed warm-pool spare overrides the derived id (same rule as
    // `provider_sandbox_name`): the pane must attach to the sandbox the
    // worktree is actually bound to, not the derived husk.
    let sandbox_id = match crate::provider_factory::sandbox_id_for(worktree, &p.id) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, error = %format!("{error:#}"), "native exec refused: warm-pool sandbox binding unreadable");
            return PaneNativeExec::Unavailable;
        }
    };
    // Resolve the pane cwd against the sandbox's cached `$HOME` (the workspace lives
    // under the login user's home — see `provider_workdir`), so `cd {workdir}` lands
    // in the repo and not a nonexistent `/workspace`.
    let workdir = crate::provider_workdir::resolve(pc, &sandbox_id);
    PaneNativeExec::Ready(Box::new(NativeShell {
        provider,
        provider_name: pc.provider.clone(),
        sandbox_id,
        inner,
        workdir,
        env,
    }))
}

/// Whether `worktree`'s provider env still needs its one-time provisioning: a
/// managed provider AND (its sandbox does NOT exist yet — a cheap `list()` GET —
/// OR the sandbox exists but is BARE, i.e. the provision marker is absent). Two
/// callers: the eager provisioner (fire the create+provision ahead of focus) and
/// the pre-warm spec task (SKIP — prewarm must never create a sprite nor attach
/// to a bare one; the focused materialize provisions it). Off-loop (network);
/// `false` for non-provider envs, a tokenless/unbuilt provider, or a list error.
pub fn provision_pending(cfg: &Config, worktree: &str) -> bool {
    provision_check(cfg, worktree) != ProvisionCheck::Ready
}

/// What [`provision_check`] found for a worktree's provider sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionCheck {
    /// Nothing to do: not a provider env, provisioned, or unknown (a list error).
    Ready,
    /// Missing, or present but bare (no provision marker).
    Pending,
    /// A Vercel sandbox that exists but is not running: probing its marker
    /// would resume it (buying compute), so it is left parked. Prewarm still
    /// skips it ([`provision_pending`] is true); the eager provisioner and the
    /// spare claim do not act on it (only an explicit open resumes).
    Parked,
}

/// [`provision_pending`] with the parked-Vercel case split out.
pub fn provision_check(cfg: &Config, worktree: &str) -> ProvisionCheck {
    let loc = match GitLoc::for_worktree_op(Path::new(worktree), "provision check") {
        Ok(loc) => loc,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, %error, "provision check skipped");
            return ProvisionCheck::Ready;
        }
    };
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let read =
        Db::open().and_then(|db| db.effective_env_checked(worktree, &repo_root.to_string_lossy()));
    let Some(selected_env) = env_selection_or_refuse(worktree, "provision check", read) else {
        return ProvisionCheck::Ready;
    };
    let Some(selection) = selection_with_profile(worktree, &repo_root, selected_env.clone()) else {
        return ProvisionCheck::Ready;
    };
    let environment = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
    if !matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) {
        return ProvisionCheck::Ready;
    }
    let Some(envc) = cfg.env.get(&environment.name) else {
        return ProvisionCheck::Ready;
    };
    // Bound-aware name: a worktree that CLAIMED a pool spare is checked against
    // the SPARE (whose marker is present ⇒ not pending), not the derived id —
    // else eager would re-provision a sprite the worktree no longer uses.
    let Some(id) = provider_sandbox_name(cfg, worktree, &environment.name) else {
        return ProvisionCheck::Ready;
    };
    let Some(provider) = provider_for_named(&envc.provider, &id) else {
        return ProvisionCheck::Ready;
    };
    match block_on_provider(|| async { provider.list().await }) {
        // Missing ⇒ needs create + a full provision.
        Ok(names) if !names.iter().any(|n| n == &id) => ProvisionCheck::Pending,
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
            // A Vercel marker read resumes a stopped sandbox: probe its status
            // with the never-resuming GET first and leave a parked one alone.
            if let thegn_svc::provider::Provider::Vercel(vercel) = &provider {
                let live = block_on_provider(|| async { vercel.reaper_view(&id).await })
                    .ok()
                    .flatten()
                    .is_some_and(|view| !crate::vercel_bridge::needs_resume(view.status));
                if !live {
                    return ProvisionCheck::Parked;
                }
            }
            let workdir = crate::provider_workdir::resolve(&envc.provider, &id);
            let marker = thegn_core::envplan::EnvPlan::marker_path(&workdir);
            if block_on_provider(|| async { provider.read(&id, &marker).await }).is_err() {
                ProvisionCheck::Pending
            } else {
                ProvisionCheck::Ready
            }
        }
        Err(_) => ProvisionCheck::Ready,
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
    crate::sandbox_fallback_memo::clear_worktree(worktree);
}

/// Whether `worktree` is pinned to the host (see [`request_force_host`]).
pub(crate) fn force_host_requested(worktree: &str) -> bool {
    force_host_registry()
        .lock()
        .map(|s| s.contains(worktree))
        .unwrap_or(false)
}

/// Floor used while considering each resolved sandbox candidate.
fn candidate_floor_in_force(
    sandbox: &thegn_core::config::SandboxConfig,
    worktree: &str,
) -> thegn_core::config::IsolationFloor {
    thegn_core::sandbox_floor::floor_in_force(sandbox, force_host_requested(worktree))
}

/// Whether an unavailable sandbox may intentionally become a host shell.
/// Remote/provider placement is always a sandbox request, even when its nested
/// backend is `none`; only a local disabled/`none` policy or the existing
/// explicit run-on-host choice authorizes host execution.
fn host_fallback_allowed(
    placement: &thegn_core::placement::Placement,
    sandbox: &thegn_core::config::SandboxConfig,
    force_host: bool,
) -> bool {
    placement.is_local()
        && (force_host
            || !sandbox.enabled
            || sandbox.backend == thegn_core::config::SandboxBackend::None)
}

/// Whether an enabled `auto` sandbox's configured `backend_chain` explicitly
/// lists the host (`host`/`none`). Only the user's own chain counts — the
/// implicit host tail `launch_backend::candidates` appends is not consent.
fn auto_chain_names_host(sandbox: &thegn_core::config::SandboxConfig) -> bool {
    thegn_core::launch_backend::auto_chain_names_host(sandbox)
}

/// Un-pin `worktree` from the host so an explicit retry re-attempts the real env.
pub fn clear_force_host(worktree: &str) {
    if let Ok(mut s) = force_host_registry().lock() {
        s.remove(worktree);
    }
    crate::sandbox_fallback_memo::clear_worktree(worktree);
}

/// Clear the failure cooldown for the worktree's provider native exec so a
/// retry actually re-attempts the connection (otherwise [`env_halt_reason`]
/// would re-halt immediately on the stale cooldown). No-op for non-provider
/// envs; the token check in `env_halt_reason` still gates a tokenless retry.
pub fn clear_native_exec_cooldown(cfg: &Config, worktree: &str) {
    let loc = match GitLoc::for_worktree_op(Path::new(worktree), "native exec cooldown clear") {
        Ok(loc) => loc,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, %error, "native exec cooldown clear skipped");
            clear_configured_provider_cooldowns(cfg);
            return;
        }
    };
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let read =
        Db::open().and_then(|db| db.effective_env_checked(worktree, &repo_root.to_string_lossy()));
    let Some(selected_env) = env_selection_or_refuse(worktree, "native exec cooldown clear", read)
    else {
        clear_configured_provider_cooldowns(cfg);
        return;
    };
    let Some(selection) = selection_with_profile(worktree, &repo_root, selected_env.clone()) else {
        clear_configured_provider_cooldowns(cfg);
        return;
    };
    let environment = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
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

/// How much of a provider provision one call runs (prompt-first).
pub(crate) enum ProvisionMode {
    All,
    Foreground,
    Deferred(thegn_core::provision_journal::RunLease),
}

impl ProvisionMode {
    fn phase(&self) -> thegn_core::provision_journal::Phase {
        match self {
            Self::All => thegn_core::provision_journal::Phase::All,
            Self::Foreground => thegn_core::provision_journal::Phase::Foreground,
            Self::Deferred(_) => thegn_core::provision_journal::Phase::Deferred,
        }
    }
}

pub(crate) struct DeferredHandoff {
    pub(crate) sandbox: String,
    pub(crate) lease: Option<thegn_core::provision_journal::RunLease>,
}

pub(crate) struct ProvisionOutcome {
    pub(crate) provisioned: bool,
    pub(crate) checkpoint: Option<String>,
    pub(crate) deferred: Option<DeferredHandoff>,
}

impl ProvisionOutcome {
    pub(crate) fn not_provisioned() -> Self {
        Self {
            provisioned: false,
            checkpoint: None,
            deferred: None,
        }
    }

    pub(crate) fn ready(checkpoint: Option<String>) -> Self {
        Self {
            provisioned: true,
            checkpoint,
            deferred: None,
        }
    }

    pub(crate) fn pending(
        id: &str,
        lease: Option<thegn_core::provision_journal::RunLease>,
    ) -> Self {
        Self {
            provisioned: true,
            checkpoint: None,
            deferred: Some(DeferredHandoff {
                sandbox: id.to_string(),
                lease,
            }),
        }
    }
}

/// Provision the worktree's environment if it runs on a managed **provider**
/// (sprites, …) — a no-op (`Ok(false)`) for local/ssh/k8s envs. Resolves the env
/// from the worktree, then delegates to `provision_provider_env_inner`. This is the
/// entry point the run loop calls off-thread before resolving the pane's launch
/// spec, so a fresh sandbox is set up (and the loading screen streamed) before
/// the pane attaches.
pub fn provision_worktree(
    cfg: &Config,
    worktree: &str,
    progress: impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<bool> {
    provision_worktree_mode(cfg, worktree, ProvisionMode::All, progress).map(|o| o.provisioned)
}

pub(crate) fn provision_worktree_mode(
    cfg: &Config,
    worktree: &str,
    mode: ProvisionMode,
    mut progress: impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<ProvisionOutcome> {
    // Tag every log line this (blocking) provisioning run emits with the
    // worktree, so a sprite/remote failure is attributable and the Logs panel
    // keeps it out of *other* worktrees' views.
    let _wt_log =
        thegn_core::log_trace::enter_wt(thegn_core::log_trace::wt_slug(Path::new(worktree)));
    let loc = match GitLoc::for_worktree_op(Path::new(worktree), "provisioning") {
        Ok(loc) => loc,
        Err(_)
            if !cfg
                .env
                .values()
                .any(|env| env.placement == thegn_core::config::PlacementMode::Provider) =>
        {
            return Ok(ProvisionOutcome::not_provisioned());
        }
        Err(error) => return Err(anyhow::anyhow!(error)),
    };
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    let read =
        Db::open().and_then(|db| db.effective_env_checked(worktree, &repo_root.to_string_lossy()));
    let selected_env = match provision_selection(cfg, read)? {
        ProvisionSelection::Resolve(selected_env) => selected_env,
        ProvisionSelection::Skip => return Ok(ProvisionOutcome::not_provisioned()),
    };
    let mut selection =
        Db::open()?.effective_env_selection(worktree, &repo_root.to_string_lossy())?;
    if selected_env.is_some() {
        selection.name = selected_env;
    }
    let environment = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
    if !matches!(
        environment.placement,
        thegn_core::placement::Placement::Provider(_)
    ) {
        return Ok(ProvisionOutcome::not_provisioned());
    }
    if let Some(refusal) = thegn_core::sandbox_floor::provider_refusal_for(
        true,
        &environment.sandbox,
        &environment.name,
        Some(worktree),
    ) {
        return Err(anyhow::Error::new(refusal));
    }
    // Flag the in-flight provision so the warm-claim fast path won't bind a
    // spare (and clear the splash) under this run — see `provision_gate`.
    let _live = crate::provision_gate::worktree_live_guard(worktree);
    provision_provider_env_inner(cfg, worktree, &environment.name, None, mode, &mut progress)
}

/// Finish an existing deferred provision without creating or recreating its
/// sandbox. The run lease is acquired before `provision_worktree_mode` takes
/// `sandbox_lock`, so callers may use this from headless and attach paths.
pub(crate) fn finish_deferred_blocking(
    cfg: &Config,
    worktree: &str,
    progress: &mut dyn FnMut(&[ProvisionStepView]),
) -> anyhow::Result<ProvisionOutcome> {
    let Some((_, id)) = crate::provision_gate::provider_target(cfg, worktree)? else {
        return Ok(ProvisionOutcome::not_provisioned());
    };
    let Some(entry) = thegn_core::provision_journal::read(&id) else {
        return Ok(ProvisionOutcome::not_provisioned());
    };
    if entry.state == thegn_core::provision_journal::DeferredState::Cancelled {
        anyhow::bail!(
            "background setup for {worktree} was cancelled; reopen it or run `thegn env up --worktree {worktree}` to retry"
        );
    }
    let lease = thegn_core::provision_journal::run_lease(
        &id,
        thegn_core::provision_journal::LeaseWait::Block,
    )?
    .ok_or_else(|| anyhow::anyhow!("could not acquire the deferred provision lease for {id}"))?;
    provision_worktree_mode(
        cfg,
        worktree,
        ProvisionMode::Deferred(lease),
        &mut *progress,
    )
}

/// The env selection for a provider exec/proxy/cooldown decision. `None` when
/// it cannot be read: the caller refuses instead of resolving the default env.
fn env_selection_or_refuse(
    worktree: &str,
    what: &str,
    read: anyhow::Result<Option<String>>,
) -> Option<Option<String>> {
    match read {
        Ok(selected) => Some(selected),
        Err(error) => {
            tracing::warn!(
                target: "thegn::provider",
                worktree,
                error = %format!("{error:#}"),
                "{what} refused: the environment selection could not be read"
            );
            None
        }
    }
}

fn selection_with_profile(
    worktree: &str,
    repo_root: &Path,
    selected_name: Option<String>,
) -> Option<thegn_core::sandbox_pin::EnvSelection> {
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, error = %format!("{error:#}"), "provider path refused: worktree sandbox selection unreadable");
            return None;
        }
    };
    selection_with_profile_in(&db, worktree, repo_root, selected_name)
}

/// As [`selection_with_profile`], over a caller's open handle.
fn selection_with_profile_in(
    db: &Db,
    worktree: &str,
    repo_root: &Path,
    selected_name: Option<String>,
) -> Option<thegn_core::sandbox_pin::EnvSelection> {
    let mut selection = match db.effective_env_selection(worktree, &repo_root.to_string_lossy()) {
        Ok(selection) => selection,
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, error = %format!("{error:#}"), "provider path refused: worktree sandbox selection unreadable");
            return None;
        }
    };
    if selected_name.is_some() {
        selection.name = selected_name;
    }
    Some(selection)
}

/// The env selection for provisioning: a failed read is an error naming the cause.
enum ProvisionSelection {
    Resolve(Option<String>),
    Skip,
}

fn provision_selection(
    cfg: &Config,
    read: anyhow::Result<Option<String>>,
) -> anyhow::Result<ProvisionSelection> {
    match read {
        Ok(selected) => Ok(ProvisionSelection::Resolve(selected)),
        Err(_) if !cfg.env.values().any(|env| {
            env.placement == thegn_core::config::PlacementMode::Provider
        }) => Ok(ProvisionSelection::Skip),
        Err(error) => Err(error).context(
            "provisioning refused: the environment selection could not be read (the default environment is never used instead)",
        ),
    }
}

/// Whether any remote placement may apply to a worktree under `cfg`: a non-local or host-pinned `[env.<name>]`, the
/// placement engine, or a `[sandbox.remote]` host. The union of the batch-78 gates in `provision_worktree`,
/// `host_flow::resolve_binding` and `placement_flow::plan_selection`. The set
/// is used by those placement decisions; unreadable worktree isolation is
/// independently a typed launch halt.
pub(crate) fn remote_placement_configured(cfg: &Config) -> bool {
    cfg.placement.enabled
        || cfg.sandbox.remote.is_remote()
        || cfg.env.values().any(|env| {
            env.placement != thegn_core::config::PlacementMode::Local || !env.host.trim().is_empty()
        })
}

/// The worktree's location for a provider pane or connect path. `None` (logged) when it cannot be read: the caller
/// treats the worktree as having no provider target, and the pane launch then refuses visibly in `launch_spec_full`.
///
/// Production paths share one handle through [`provider_pane_loc_in`]; this
/// opening form stays as the reference the equivalence tests compare against.
#[cfg(test)]
pub(crate) fn provider_pane_loc(worktree: &str, what: &str) -> Option<GitLoc> {
    match GitLoc::for_worktree_op(Path::new(worktree), what) {
        Ok(loc) => Some(loc),
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, %error, "{what}: location unreadable");
            None
        }
    }
}

/// True when any `[env.*]` has `placement = "provider"`. Config-only, no I/O.
///
/// `Config::resolve_env` builds a `Placement::Provider` only in
/// `envbuild::build_env_placement`, reached only for a selected name that is an
/// `[env.<name>]` table whose `placement` is `Provider`; the unresolved/default
/// arm uses `placement_from_loc`, which yields only `Local` or `Ssh` (a
/// recorded provider *location* alone never makes a provider placement). So a
/// config with no such table can never resolve a provider env, and the provider
/// pane/proxy/connect paths may skip every database read.
pub(crate) fn provider_envs_configured(cfg: &Config) -> bool {
    cfg.env
        .values()
        .any(|env| env.placement == thegn_core::config::PlacementMode::Provider)
}

/// The worktree's location for a provider pane or connect path, over an open
/// handle. `None` (logged) when it cannot be read: the caller treats the
/// worktree as having no provider target.
pub(crate) fn provider_pane_loc_in(db: &Db, worktree: &str, what: &str) -> Option<GitLoc> {
    match GitLoc::for_worktree_op_in(db, Path::new(worktree), what) {
        Ok(loc) => Some(loc),
        Err(error) => {
            tracing::warn!(target: "thegn::provider", worktree, %error, "{what}: location unreadable");
            None
        }
    }
}

/// The one state-DB handle a provider pane/proxy/connect resolution shares.
/// `None` (logged like an unreadable location) when it cannot be opened: the
/// caller refuses, exactly as the separate opens it replaces did.
#[track_caller]
pub(crate) fn provider_resolution_db(worktree: &str, what: &str) -> Option<Db> {
    match Db::open() {
        Ok(db) => Some(db),
        Err(error) => {
            let error = format!(
                "{what} refused: cannot read where {worktree} lives (open worktree location DB: {error:#}); nothing was run against the local checkout"
            );
            tracing::warn!(target: "thegn::provider", worktree, %error, "{what}: location unreadable");
            None
        }
    }
}

/// The worktree's repo root read through `db`, falling back to git's main
/// worktree and then the worktree itself (a read error falls back too).
pub(crate) fn repo_root_in(db: &Db, worktree: &str) -> PathBuf {
    db.repo_root_for(worktree)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree))
}

/// Resolve the execution environment a provider pane/proxy path acts on, over
/// ONE database handle (location, repo root, env selection, profile). `None`
/// (logged) when any read fails: the caller refuses instead of resolving the
/// default env. The handle is dropped before `resolve_env` runs.
fn resolve_provider_env(
    cfg: &Config,
    worktree: &str,
    what: &str,
) -> Option<thegn_core::env::Environment> {
    let (loc, repo_root, selection) = {
        let db = provider_resolution_db(worktree, what)?;
        let loc = provider_pane_loc_in(&db, worktree, what)?;
        let repo_root = repo_root_in(&db, worktree);
        let read = db.effective_env_checked(worktree, &repo_root.to_string_lossy());
        let selected_env = env_selection_or_refuse(worktree, what, read)?;
        let selection = selection_with_profile_in(&db, worktree, &repo_root, selected_env)?;
        (loc, repo_root, selection)
    };
    Some(cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection))
}

/// Retry cannot identify the selected provider while the DB read is failing.
/// Clear configured provider cooldowns so the retry can proceed once selection
/// becomes readable again.
fn clear_configured_provider_cooldowns(cfg: &Config) {
    for env in cfg
        .env
        .values()
        .filter(|env| env.placement == thegn_core::config::PlacementMode::Provider)
    {
        native_exec_report(&env.provider.provider, true);
    }
}

#[cfg(test)]
mod env_selection_tests {
    use super::{
        ProvisionSelection, clear_configured_provider_cooldowns, env_selection_or_refuse,
        native_exec_healthy, native_exec_report, provision_selection, proxy_refusal_reason,
    };
    use thegn_core::config::{Config, EnvConfig, PlacementMode};

    #[test]
    fn env_selection_or_refuse_refuses_an_unreadable_selection() {
        assert_eq!(
            env_selection_or_refuse(
                "/wt",
                "native exec",
                Err(anyhow::anyhow!("database is locked")),
            ),
            None
        );
    }

    #[test]
    fn env_selection_or_refuse_passes_a_read_through() {
        assert_eq!(
            env_selection_or_refuse("/wt", "native exec", Ok(Some("box".into()))),
            Some(Some("box".into()))
        );
        assert_eq!(
            env_selection_or_refuse("/wt", "native exec", Ok(None)),
            Some(None)
        );
    }

    #[test]
    fn provision_selection_errors_on_an_unreadable_selection() {
        let cfg = Config {
            env: std::collections::BTreeMap::from([(
                "box".into(),
                EnvConfig {
                    placement: PlacementMode::Provider,
                    ..EnvConfig::default()
                },
            )]),
            ..Config::default()
        };
        let result = provision_selection(&cfg, Err(anyhow::anyhow!("database is locked")));
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("selection read must be refused"),
        };
        let message = format!("{error:#}");
        assert!(message.contains("provisioning refused"));
        assert!(message.contains("database is locked"));
    }

    #[test]
    fn provision_selection_passes_a_read_through() {
        let cfg = Config::default();
        assert!(matches!(
            provision_selection(&cfg, Ok(None)).unwrap(),
            ProvisionSelection::Resolve(None)
        ));
        assert_eq!(
            match provision_selection(&cfg, Ok(Some("box".into()))).unwrap() {
                ProvisionSelection::Resolve(selected) => selected,
                ProvisionSelection::Skip => panic!("successful read must be resolved"),
            },
            Some("box".into())
        );
    }

    #[test]
    fn provision_selection_allows_unreadable_selection_without_provider_envs() {
        let cfg = Config::default();
        assert!(
            matches!(
                provision_selection(&cfg, Err(anyhow::anyhow!("database is locked"))),
                Ok(ProvisionSelection::Skip)
            ),
            "a local-only config must skip provisioning so a plain shell can launch"
        );
    }

    #[test]
    fn clear_configured_provider_cooldowns_clears_the_retry_cooldown() {
        let cfg = Config {
            env: std::collections::BTreeMap::from([(
                "box".into(),
                EnvConfig {
                    placement: PlacementMode::Provider,
                    provider: thegn_core::config::EnvProviderConfig {
                        provider: "env-selection-test-provider".into(),
                        ..thegn_core::config::EnvProviderConfig::default()
                    },
                    ..EnvConfig::default()
                },
            )]),
            ..Config::default()
        };
        native_exec_report("env-selection-test-provider", false);
        assert!(!native_exec_healthy("env-selection-test-provider"));
        clear_configured_provider_cooldowns(&cfg);
        assert!(native_exec_healthy("env-selection-test-provider"));
    }

    #[test]
    fn proxy_refusal_reason_names_an_unreadable_selection() {
        let message = proxy_refusal_reason("/wt", Err(anyhow::anyhow!("database is locked")));
        assert!(message.contains("could not be read"));
        assert!(message.contains("database is locked"));
        assert!(!message.contains("not a provider env"));
    }

    #[test]
    fn proxy_refusal_reason_keeps_the_generic_reason() {
        assert!(proxy_refusal_reason("/wt", Ok(None)).contains("not a provider env"));
        assert!(proxy_refusal_reason("/wt", Ok(Some("box".into()))).contains("not a provider env"));
    }
}

/// The sub-line shown under a step that is known to take a while on a first
/// create, so the wait reads as work in progress rather than a hang. `None`
/// for steps that finish in seconds.
fn slow_step_hint(step_id: &str) -> Option<&'static str> {
    match step_id {
        "agents_install" => {
            Some("first run only: downloads the agent CLIs (about 1 min); later opens skip this")
        }
        "nix" | "toolchain" => Some("first run only: later opens restore the snapshot"),
        "checkpoint" => Some("saving a snapshot so the next open is fast"),
        _ => None,
    }
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
    // Only `workspace` (mkdir) is truly instant — a stall there is the
    // suspended-sandbox hang we want to catch fast. Everything else can
    // legitimately run for minutes (`git_auth` installs git on an image that
    // lacks it, clone, package installs incl. the openssh the ssh transport
    // needs, npm agents, the nix devshell build, closure substitution), so give
    // it a generous ceiling that still bounds an infinite hang rather than risk
    // a false failure mid-build.
    let instant = matches!(step_id, "workspace");
    if instant {
        Duration::from_secs(120) // 2 min — catches the suspended-sandbox hang fast
    } else {
        Duration::from_secs(1800) // 30 min — build/download-bound steps
    }
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
    provision_provider_env_inner(
        cfg,
        worktree,
        env_name,
        name_override,
        ProvisionMode::All,
        progress,
    )
    .map(|o| (o.provisioned, o.checkpoint))
}

fn provision_provider_env_inner(
    cfg: &Config,
    worktree: &str,
    env_name: &str,
    name_override: Option<&str>,
    mode: ProvisionMode,
    progress: &mut impl FnMut(&[ProvisionStepView]),
) -> anyhow::Result<ProvisionOutcome> {
    use thegn_core::envplan::{self, EnvPlan, PlanOpts, StepKind};
    use thegn_core::model_credential_consent::Launch;
    use thegn_core::provision_journal::{self as journal, Phase};

    let Some(env) = cfg.env.get(env_name) else {
        return Ok(ProvisionOutcome::not_provisioned());
    };
    let pc = &env.provider;
    let kind = thegn_core::config::EnvProviderKind::of(&pc.provider);
    // Host-local repository membership is needed both by provisioning and by
    // the post-checkpoint return-credential policy.
    let repo_root: PathBuf = Db::open()
        .ok()
        .and_then(|db| db.repo_root_for(worktree).ok().flatten())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));
    // Credential-free kinds (Vercel): the repo goes in only as a host bundle and
    // no host credential is uploaded. Refuse BEFORE anything billable exists
    // when the bundle seed is impossible — never fall back to an origin clone.
    let (force_bundle, credential_free) = provision_credential_policy(kind);
    if force_bundle {
        // off-loop: provisioning entry (see the branch lookup below).
        #[expect(clippy::disallowed_methods)]
        let on_branch = thegn_core::util::git_cmd(Path::new(worktree))
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .is_some_and(|b| !b.is_empty() && b != "HEAD");
        credential_free_seed_precheck(
            name_override.is_some(),
            Path::new(worktree).join(".git").exists(),
            on_branch,
        )?;
    }
    let id = match name_override {
        Some(name) => Some(name.to_string()),
        None => crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env_name)?,
    };
    let Some(id) = id.filter(|s| !s.is_empty()) else {
        return Ok(ProvisionOutcome::not_provisioned());
    };
    // Bake the resolved id so a recreate (`ensure_exists`→`create`) names the
    // sandbox correctly (the id embeds the repo/worktree tokens + a path-hash).
    let owner = if name_override.is_none() {
        crate::provider_factory::provider_sandbox_owner(cfg, worktree, env_name)
    } else {
        None
    };
    let Some(provider) =
        crate::provider_factory::provider_for_named_owned(pc, &id, owner.as_deref())
    else {
        return Ok(ProvisionOutcome::not_provisioned());
    };
    if !provider.caps().files {
        return Ok(ProvisionOutcome::not_provisioned()); // can't provision without the fs API
    }
    // An omitted Sprites `model_credentials` asks once per project+provider. The
    // question is raised from the drain, not here: a background/deferred or
    // warm-pool provision declines instead of prompting, and the focused launch
    // asks. Explicit `"upload"`/`"none"` never reach this block.
    let mut consent_settled = false;
    let consent = if kind == thegn_core::config::EnvProviderKind::Sprites
        && !pc.model_credentials_is_explicit()
    {
        let project = repo_root.to_string_lossy().into_owned();
        let remembered = resolve_sprites_model_credentials(worktree, &project, &pc.provider);
        match sprites_credential_launch(pc, remembered, thegn_core::msg::tui_active()) {
            Launch::Ask => {
                let agents = model_credential_agents(cfg, env);
                let keys = model_credential_keys(cfg, &repo_root);
                if agents.is_empty() && keys.is_empty() {
                    // Nothing to forward: skip a pointless question.
                    consent_settled = true;
                    None
                } else if name_override.is_some() {
                    return Ok(ProvisionOutcome::not_provisioned());
                } else if matches!(&mode, ProvisionMode::Deferred(_)) {
                    // The foreground phase already asked (or consumed a
                    // one-shot "not now"). Withhold and finish the deferred
                    // steps rather than abandoning them.
                    None
                } else {
                    return Err(ModelCredentialConsentRequired {
                        env_name: env_name.to_string(),
                        project,
                        provider: pc.provider.trim().to_ascii_lowercase(),
                        worktree: worktree.to_string(),
                        sandbox: name_override.unwrap_or(id.as_str()).to_string(),
                        agents,
                        keys,
                    }
                    .into());
                }
            }
            Launch::Forward => {
                consent_settled = true;
                memo_model_credential_consent(&pc.provider, worktree, ModelConsent::Forward);
                Some(ModelConsent::Forward)
            }
            Launch::Withhold => {
                // A spare provisioned without the answer would be useless (the
                // claim path only takes spares whose creds match the answer).
                consent_settled = remembered.is_some();
                if name_override.is_some() {
                    return Ok(ProvisionOutcome::not_provisioned());
                }
                None
            }
        }
    } else {
        None
    };
    let credential_mode = if kind == thegn_core::config::EnvProviderKind::Sprites
        && !pc.model_credentials_is_explicit()
    {
        sprites_credential_mode(consent)
    } else {
        pc.model_credentials
    };
    let grant = thegn_core::provider_credentials::model_grant(kind, credential_mode);
    let loc = GitLoc::for_worktree_op(Path::new(worktree), "provider provisioning")
        .map_err(anyhow::Error::msg)?;
    // Prompt-first fast path: a pane opened during the deferred phase never waits on sandbox_lock (D-pf-c).
    if matches!(&mode, ProvisionMode::Foreground)
        && name_override.is_none()
        && crate::provider_workdir::is_provisioned_locally(&id)
        && journal::read(&id).is_some()
    {
        match journal::run_lease(&id, journal::LeaseWait::Try) {
            Ok(lease) => return Ok(ProvisionOutcome::pending(&id, lease)),
            Err(error) => {
                tracing::warn!(
                    target: "thegn::provider",
                    %id,
                    %error,
                    "D-pf-f: could not acquire the deferred run lease; continuing with the locked path"
                );
            }
        }
    }
    let phase = mode.phase();
    let mut _held = match mode {
        ProvisionMode::All | ProvisionMode::Foreground => None,
        ProvisionMode::Deferred(lease) => Some(lease),
    };
    let mut deferred_only = phase == Phase::Deferred;
    // Serialize concurrent provisions of the same sandbox (eager vs focused
    // materialize): the loser blocks here (off-loop by contract), then the
    // marker short-circuit below makes its run a no-op. The marker alone only
    // guards SEQUENTIAL re-runs — it is written at the END of the pipeline.
    // Blocking acquisition belongs off the event loop. In particular, do not
    // wait for a run lease while this sandbox lock is held.
    let mut _gate = Some(crate::provision_gate::sandbox_lock(&id));

    // A hibernated worktree resumes by overlaying its snapshot on the fresh
    // clone. Read it before any sandbox create: an unreadable record must
    // refuse here rather than provision an empty clone over the snapshot.
    let snapshot_restore = if !deferred_only && name_override.is_none() {
        crate::hibernator::begin_restore(worktree, pc.hibernate_enabled())?
    } else {
        None
    };

    // THE-2020: a provider at its `[placement.fleet_budget] max_hourly` refuses
    // a NEW sandbox before anything billable is created (resume never refused).
    if !deferred_only {
        crate::fleet_read::admit_new_sandbox(cfg, env_name, &id).map_err(anyhow::Error::msg)?;
    }

    // Recreate-if-missing: the sandbox may have been cleaned up out-of-band (TTL,
    // manual delete, provider GC). `ensure_exists` recreates it before we read the
    // marker / run any exec, so provisioning can't fail against a dead sandbox.
    // A freshly recreated sandbox has no marker ⇒ a full re-provision runs below.
    // (No-op when it already exists; cheap list+maybe-create.)
    // Surface the create/list phase (which precedes the step plan below) as a
    // labeled active step + a log breadcrumb — this is the phase that, untimed,
    // hung provisioning on startup, so make it observable rather than a blank.
    let created = if deferred_only {
        false
    } else {
        progress(&[ProvisionStepView {
            label: "Preparing sandbox".to_string(),
            state: ProvisionState::Active,
            detail: None,
        }]);
        tracing::debug!(target: "thegn::startup", %id, "provider ensure_exists (create/list)");
        let create_started = std::time::Instant::now();
        match block_on_provider(|| async { provider.ensure_exists(&id).await }) {
            Ok(created) => {
                tracing::info!(
                    target: "thegn::startup",
                    %id,
                    created,
                    elapsed_ms = create_started.elapsed().as_millis() as u64,
                    "phase: sandbox create/list"
                );
                progress(&[ProvisionStepView {
                    label: "Preparing sandbox".to_string(),
                    state: ProvisionState::Done,
                    detail: None,
                }]);
                created
            }
            // `{e:#}` (alternate) flattens the whole anyhow cause chain into the
            // message — without it, Display keeps only the outermost context and the
            // real cause (e.g. machine0's `vm_create` `isError` text) is dropped
            // before it can reach the loading screen / logs.
            Err(e) => return Err(anyhow::anyhow!("ensure sandbox {id}: {e:#}")),
        }
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
        let boot_started = std::time::Instant::now();
        let ready = block_on_provider(|| async { provider.wait_ready(&id, READY_BUDGET).await });
        tracing::info!(
            target: "thegn::startup",
            %id,
            ok = ready.is_ok(),
            elapsed_ms = boot_started.elapsed().as_millis() as u64,
            "phase: wait for sandbox boot"
        );
        if let Err(e) = ready {
            boot[0].state = ProvisionState::Failed;
            boot[0].detail = Some(format!("{e:#}"));
            progress(&boot);
            return Err(anyhow::anyhow!("sandbox {id} not ready: {e:#}"));
        }
        boot[0].state = ProvisionState::Done;
        progress(&boot);
    }

    let selection = selection_with_profile(worktree, &repo_root, Some(env_name.to_string()))
        .ok_or_else(|| anyhow::anyhow!("the worktree's sandbox profile could not be read"))?;
    let resolved = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
    let allow = &resolved.sandbox.network_allow;
    let block = &resolved.sandbox.network_block;
    let admission =
        crate::egress_admit::egress_admission(created, provider.caps().egress, allow, block);
    if admission == crate::egress_admit::EgressAdmission::SkipUnsupported {
        crate::egress_admit::warn_unsupported_once(
            crate::egress_admit::provider_label(&provider),
            &id,
        );
    }
    if !deferred_only
        && let crate::egress_admit::EgressAdmission::AdmitFresh
        | crate::egress_admit::EgressAdmission::VerifyExisting = admission
    {
        let egress_steps = crate::egress_admit::BlockingProviderEgress {
            provider: &provider,
        };
        let mut network_policy = ProvisionStepView {
            label: "Network policy".into(),
            state: ProvisionState::Active,
            detail: None,
        };
        progress(&[network_policy.clone()]);
        let admission_result = if admission == crate::egress_admit::EgressAdmission::AdmitFresh {
            crate::egress_admit::admit_egress(&egress_steps, &id, allow, block)
        } else {
            crate::egress_admit::verify_existing_egress(&egress_steps, &id, allow, block)
        };
        if let Err(e) = admission_result {
            network_policy.state = ProvisionState::Failed;
            network_policy.detail = Some(format!("{e:#}"));
            progress(&[network_policy]);
            return Err(anyhow::anyhow!("sandbox {id}: {e:#}"));
        }
        network_policy.state = ProvisionState::Done;
        progress(&[network_policy]);
    }

    // Site the workspace under the sandbox login's real `$HOME` (a property of the
    // IMAGE: machine0's NixOS image logs in as `nix`/`/home/nix`, others as root) —
    // a bare `/workspace` at the root fs is not writable by a non-root login and
    // fails the "Prepare workspace" `mkdir`. Caches the home so the pane, chrome
    // reads, and marker checks all resolve the SAME path (`provider_workdir`).
    let home_probe_started = std::time::Instant::now();
    let (sprite_home, workdir) = crate::provider_workdir::probe_and_resolve(&provider, &id, pc);
    tracing::info!(
        target: "thegn::startup",
        %id,
        elapsed_ms = home_probe_started.elapsed().as_millis() as u64,
        "phase: probe sandbox home"
    );
    let marker = EnvPlan::marker_path(&workdir);

    let read_marker = || {
        block_on_provider(|| async { provider.read(&id, &marker).await })
            .ok()
            .map(|bytes| envplan::marker_state(&bytes))
    };
    let mut action = journal::phase_action(phase, read_marker());
    if action == journal::PhaseAction::WaitThenRunDeferred {
        drop(_gate.take());
        _held = journal::run_lease(&id, journal::LeaseWait::Block)?;
        _gate = Some(crate::provision_gate::sandbox_lock(&id));
        deferred_only = true;
        action = journal::phase_action(Phase::Deferred, read_marker());
    }
    match action {
        journal::PhaseAction::ShortCircuit => {
            if let Err(error) = journal::clear(&id) {
                tracing::debug!(target: "thegn::provider", %id, %error, "could not clear completed provision journal");
            }
            crate::provider_workdir::mark_provisioned(&id);
            // A credential-free kind never receives agent auth, and never holds a
            // route-to-host bearer (`false` revokes + removes any installed one).
            if !credential_free && grant.login_files {
                crate::agent_configs::resync_agent_auth(
                    &provider, &id, cfg, worktree, &loc, env_name,
                );
            }
            crate::remote_enqueue_auth::reconcile_provider_credential(
                cfg,
                &provider,
                pc,
                &id,
                &repo_root,
                worktree,
                name_override.is_none() && !credential_free,
            )?;
            return Ok(ProvisionOutcome::ready(None));
        }
        journal::PhaseAction::AlreadyDone => {
            if let Err(error) = journal::clear(&id) {
                tracing::debug!(target: "thegn::provider", %id, %error, "could not clear completed provision journal");
            }
            return Ok(ProvisionOutcome::ready(None));
        }
        journal::PhaseAction::Abandon => {
            if let Err(error) = journal::clear(&id) {
                tracing::debug!(target: "thegn::provider", %id, %error, "could not clear abandoned provision journal");
            }
            crate::provider_workdir::clear_provisioned(&id);
            return Ok(ProvisionOutcome::not_provisioned());
        }
        journal::PhaseAction::ReportPending => {
            crate::provider_workdir::mark_provisioned(&id);
            if journal::read(&id).is_none() {
                let pending = journal::Journal::pending(worktree, env_name);
                if let Err(error) = journal::write(&id, &pending) {
                    tracing::warn!(target: "thegn::provider", %id, %error, "could not write deferred provision journal");
                }
            }
            let lease = match journal::run_lease(&id, journal::LeaseWait::Try) {
                Ok(lease) => lease,
                Err(error) => {
                    tracing::warn!(
                        target: "thegn::provider",
                        %id,
                        %error,
                        "D-pf-f: could not acquire the deferred run lease; reporting pending"
                    );
                    None
                }
            };
            return Ok(ProvisionOutcome::pending(&id, lease));
        }
        journal::PhaseAction::RunAll
        | journal::PhaseAction::RunForeground
        | journal::PhaseAction::RunDeferred => {}
        journal::PhaseAction::WaitThenRunDeferred => unreachable!("wait action is resolved above"),
    }
    let run_started_ms = thegn_core::util::now_ms();
    if deferred_only
        && let Some(entry) = journal::read(&id)
        && entry.state == journal::DeferredState::Cancelled
        && entry.updated_ms + 600_000 < run_started_ms
    {
        let pending = journal::Journal::pending(worktree, env_name);
        if let Err(error) = journal::write(&id, &pending) {
            tracing::warn!(
                target: "thegn::provider",
                %id,
                %error,
                "could not reset stale cancelled deferred provision journal"
            );
        }
    }

    // Resolve the repo origin so the sprite can clone it.
    let req = envplan::detect(Path::new(worktree));
    // Host secrets (ANTHROPIC_API_KEY, …) carried into every provisioning
    // command so setup steps can reach the network/model. GH_TOKEN/GITHUB_TOKEN
    // ride along only when `[sandbox.git]` grants a token (mode = "full" with
    // credential = "repo-token" | "user-token"); by default credentials stay
    // with thegn. Remote-safe (no host-local socket vars).
    let repo_sandbox = cfg.repo_sandbox(&repo_root);
    // A credential-free kind never gets a git token, and its provisioning
    // commands carry no host passthrough secret (only terminal/locale/devshell).
    let git_token = repo_sandbox.git.admits_git_token() && !credential_free;
    let mut exec_env = repo_sandbox.passthrough_env_remote();
    if credential_free {
        exec_env = credential_free_exec_env(exec_env);
    }
    let mut withheld = Vec::new();
    if !grant.env_keys {
        let (kept, names) = thegn_core::provider_credentials::strip_model_env(exec_env);
        exec_env = kept;
        withheld.extend(names);
    }
    if !grant.login_files {
        withheld.push("agent logins".to_string());
    }
    // Only an unanswered unset key gets the hint; a remembered "never" is not
    // nagged on every headless launch.
    if !deferred_only
        && !consent_settled
        && kind == thegn_core::config::EnvProviderKind::Sprites
        && let Some(message) = sprites_withheld_notice(env_name, &withheld)
    {
        thegn_core::msg::info(&message);
    }
    crate::agent_configs::push_host_git_identity(&repo_root, &mut exec_env);
    // The generic, declarative personal layer ([sandbox.home]) — applied to every
    // sandbox so it feels like local. Resolve it PER-ENV (the env overlay may set a
    // different `strategy`, e.g. host-parity on a big box, clean on a sprite), then
    // resolve which dotfiles to upload under that strategy (drops a non-portable rc
    // with a warning; collects host store roots for host-parity).
    let home = resolved.sandbox.home;
    let host_home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"));
    let (dotfiles, mut home_store_roots) = resolve_personal_dotfiles(&host_home, &home, env_name);
    if credential_free {
        // No host closure reaches a credential-free sandbox, by any transport.
        home_store_roots.clear();
    }
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
    let managed_ssh = (pc.connect == thegn_core::config::ProviderConnect::Ssh)
        .then(|| {
            let account = crate::provider_factory::managed_key_account(pc);
            managed_ssh_keypair(
                cfg.credentials.ssh.managed_key_scope,
                &pc.provider,
                &account,
            )
            .map(|(key, public)| {
                let custody_account = thegn_core::managed_ssh::account_label(&key);
                (custody_account, key, public)
            })
        })
        .transpose()?;
    if let Some((_, _, pubkey)) = &managed_ssh {
        setup.push(sprite_sshd_setup_script(pubkey)?);
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
    // Credential-free transport (THE-2021): a real worktree whose local checkout
    // is a git worktree on a branch is seeded from a host-side `git bundle`
    // uploaded over the files API, instead of cloning origin inside the sandbox
    // (which needs an ssh key or token there). A SPARE (branch `None`) keeps the
    // origin clone: it is generic until a claim rebranches it. `[sandbox.git]
    // transport = "clone"` (per env) opts out of the bundle seed.
    // A credential-free kind (Vercel) ALWAYS bundle-seeds, whatever the
    // transport config says, and has no origin URL in the sandbox.
    let bundle_seed = branch
        .as_ref()
        .filter(|_| {
            force_bundle
                || resolved.sandbox.git.transport == thegn_core::config::GitTransport::Bundle
        })
        .filter(|_| name_override.is_none() && Path::new(worktree).join(".git").exists())
        .map(|b| envplan::BundleSeed {
            worktree: worktree.to_string(),
            branch: b.clone(),
        });
    // Refused before the hibernation row flips to `restoring`.
    if force_bundle && bundle_seed.is_none() {
        anyhow::bail!(
            "vercel: the repo can only reach the sandbox as a host bundle; this worktree has no \
             checked-out branch to bundle"
        );
    }
    if snapshot_restore.is_some() {
        crate::hibernator::mark_restoring(worktree);
    }
    let opts = PlanOpts {
        workdir: workdir.clone(),
        origin: if credential_free {
            None
        } else {
            local_origin(&repo_root)
        },
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
        // `push_devshell` is retained as a parse-compatible config key but no
        // longer creates a host-side Nix evaluation or transfer step.
        push_devshell: false,
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
        snapshot_restore,
        // Host-cache loopback substituter for nix.conf — but only when the resident
        // musl bridge that carries the `:8484` tunnel is pushable; otherwise the port
        // is dead and nix wastes minutes timing out before falling back to source.
        host_cache_url: (pc.host_cache && crate::bridge_sup::bridge_binary_path().is_some())
            .then(|| format!("http://127.0.0.1:{}", crate::nixcache::SANDBOX_PORT)),
        host_cache_key: if pc.host_cache && crate::bridge_sup::bridge_binary_path().is_some() {
            match crate::nix_signing::ensure_install_key() {
                Ok(key) => Some(key.public),
                Err(error) => {
                    thegn_core::msg::warn(&format!(
                        "host cache: no installation signing key ({error}); the sandbox will not trust the host cache"
                    ));
                    None
                }
            }
        } else {
            None
        },
        toolchain: cfg.toolchain.clone(),
        // No token credential helper unless `[sandbox.git]` grants a token.
        git_token,
        bundle_seed,
    };
    let mut plan = envplan::plan(&req, &opts);
    if credential_free {
        // Labels only (fixed kind names): never a path, dotfile or store root.
        let removed = thegn_core::provider_credentials::strip_credential_steps(&mut plan);
        if !removed.is_empty() {
            tracing::info!(
                target: "thegn::sandbox",
                provider = %pc.provider,
                removed = ?removed,
                "credential-free provider: stripped credential-bearing provision steps"
            );
        }
    }

    // Host-parity (Phase 2): push the host's home-shell closure to the configured
    // binary cache so the in-sandbox `home_closure` step can substitute it and the
    // exact host dotfiles resolve. Host-side (uses the host's `nix`), best-effort:
    // only when a push cache is set — nixpkgs paths otherwise substitute from the
    // default caches (cache.nixos.org) with no push. A failure just means the
    // sandbox falls back to whatever its substituters can serve.
    if !deferred_only
        && opts.strategy == thegn_core::config::ShellStrategy::HostParity
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

    let (foreground_steps, deferred_steps) = plan.split_deferred(name_override.is_none());
    let bg_has_checkpoint = deferred_steps.iter().any(|step| step.id == "checkpoint");
    let fg_len = if deferred_only {
        0
    } else {
        foreground_steps.len()
    };
    let steps: Vec<thegn_core::envplan::ProvisionStep> = if deferred_only {
        deferred_steps.clone()
    } else {
        foreground_steps
            .iter()
            .chain(deferred_steps.iter())
            .cloned()
            .collect()
    };

    // `sprite_home` (the sandbox login's real `$HOME`) was resolved + cached above,
    // right after the sandbox came Ready — reused here for the uploads.

    // The "thegn-provisioned" base checkpoint taken by this run's Checkpoint
    // step (if the plan has one) — persisted below + returned to the caller.
    let mut base_checkpoint: Option<String> = None;
    // Seed the loading screen with every step pending (the original dot).
    let shown = if phase == Phase::Foreground {
        fg_len
    } else {
        steps.len()
    };
    let mut views: Vec<ProvisionStepView> = steps[..shown]
        .iter()
        .map(|s| ProvisionStepView {
            label: s.label.clone(),
            state: ProvisionState::Pending,
            detail: None,
        })
        .collect();

    let fg_tail = |base: Option<&str>| -> anyhow::Result<(
        Option<thegn_core::provision_journal::RunLease>,
        bool,
        bool,
    )> {
        // Install only after the base checkpoint step so a bearer token is never
        // captured in a reusable image/artifact. This is a fatal provisioning
        // boundary: route mode must not open a remote shell that could silently
        // enqueue into a sprite-local queue.
        // A credential-free kind never holds a route-to-host bearer; `false` revokes and removes one.
        if !bg_has_checkpoint {
            crate::remote_enqueue_auth::reconcile_provider_credential(
                cfg,
                &provider,
                pc,
                &id,
                &repo_root,
                worktree,
                name_override.is_none() && !credential_free,
            )?;
        }
        let mut lease = None;
        let mut journaled = false;
        let mut busy = false;
        if !deferred_steps.is_empty() {
            match journal::run_lease(&id, journal::LeaseWait::Try) {
                Ok(Some(found)) => lease = Some(found),
                Ok(None) => busy = true,
                Err(error) => {
                    tracing::warn!(
                        target: "thegn::provider",
                        %id,
                        %error,
                        "D-pf-f: could not acquire the deferred run lease; continuing in line"
                    );
                }
            }
            if lease.is_some() {
                let pending = journal::Journal::pending(worktree, env_name);
                journaled = match journal::write(&id, &pending) {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(
                            target: "thegn::provider",
                            %id,
                            %error,
                            "could not write deferred provision journal; continuing in foreground"
                        );
                        false
                    }
                };
            }
        }
        let marker_bytes = if deferred_steps.is_empty() {
            envplan::MARKER_DONE
        } else {
            envplan::MARKER_DEFERRED
        };
        if let Err(error) =
            block_on_provider(|| async { provider.write(&id, &marker, marker_bytes).await })
        {
            tracing::debug!(target: "thegn::provider", %id, %error, "could not write provision marker");
        }
        if deferred_steps.is_empty()
            && let Err(error) = journal::clear(&id)
        {
            tracing::debug!(target: "thegn::provider", %id, %error, "could not clear completed provision journal");
        }
        if let Some((account, key, public)) = &managed_ssh {
            // Verify that the exact key made it into the remote authorization file
            // before publishing custody. A failed SSH setup must not become an
            // apparently-authorized ledger row.
            let key_id = crate::managed_ssh_rotation::key_id(public)?;
            let script = crate::managed_ssh_rotation::authorized_probe_script(&key_id);
            let argv = vec!["sh".to_string(), "-lc".to_string(), script];
            let (code, _) =
                block_on_provider(|| async { provider.run_exec(&id, &argv, None, &[]).await })?;
            if code != 0 {
                anyhow::bail!(
                    "managed SSH key was not authorized on {} sandbox {id}",
                    pc.provider
                );
            }
            thegn_core::managed_ssh::record_authorized_with_proxy_worktree(
                pc.provider.trim(),
                account,
                &id,
                key,
                public,
                Some(worktree),
            )?;
        }
        crate::provider_workdir::mark_provisioned(&id);
        if deferred_steps.is_empty()
            && let Some(cp) = base
        {
            let lock = crate::provision_gate::flake_lock_hash(&repo_root);
            if let Ok(db) = Db::open()
                && let Err(error) =
                    db.set_base_snapshot(&repo_root.to_string_lossy(), env_name, cp, &lock)
            {
                tracing::debug!(target: "thegn::provider", %id, %error, "could not record base snapshot");
            }
        }
        Ok((lease, journaled, busy))
    };

    let mut tail_done = false;
    let mut bg_failed = false;
    let mut cancelled = false;
    let mut deferred_failure: Option<(String, String)> = None;

    for (i, step) in steps.iter().enumerate() {
        if i == fg_len && !deferred_only && !tail_done {
            let (lease, journaled, busy) = fg_tail(base_checkpoint.as_deref())?;
            if phase == Phase::Foreground && journaled {
                return Ok(ProvisionOutcome {
                    provisioned: true,
                    checkpoint: None,
                    deferred: Some(DeferredHandoff {
                        sandbox: id.clone(),
                        lease,
                    }),
                });
            }
            if busy {
                anyhow::bail!(
                    "{id}: another thegn process is finishing this sandbox's background setup; retry when it is done"
                );
            }
            _held = lease;
            if shown < steps.len() {
                views.extend(steps[shown..].iter().map(|step| ProvisionStepView {
                    label: step.label.clone(),
                    state: ProvisionState::Pending,
                    detail: None,
                }));
            }
            tail_done = true;
        }
        if i >= fg_len {
            if journal::read(&id)
                .is_some_and(|journal| journal.state == journal::DeferredState::Cancelled)
            {
                cancelled = true;
                break;
            }
            if step.id == "checkpoint" && bg_failed {
                views[i].state = ProvisionState::Failed;
                views[i].detail = Some("skipped: an earlier background step failed".into());
                progress(&views);
                continue;
            }
        }
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
        if let Some(hint) = slow_step_hint(&step.id) {
            views[i].detail = Some(hint.to_string());
        }
        progress(&views);
        let step_t0 = std::time::Instant::now();
        tracing::info!(target: "thegn::startup", step = %step.id, "provision step start");

        // Defence in depth behind the strip above: a credential-bearing step
        // never runs against a credential-free provider.
        if let Some(refusal) = credential_step_refusal(credential_free, step) {
            return Err(refusal);
        }
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
                if !grant.login_files {
                    Ok(())
                } else {
                    let account_failures = crate::agent_configs::upload_agent_accounts(
                        &provider,
                        &id,
                        &sprite_home,
                        cfg,
                    );
                    let configs = crate::agent_configs::upload_agent_configs(
                        &provider,
                        &id,
                        &sprite_home,
                        agents,
                        &pc.login_homes,
                    );
                    crate::agent_configs::agent_configs_step_result(configs, &account_failures)
                }
            }
            StepKind::AtuinSync => upload_atuin_creds(&provider, &id, &sprite_home, &exec_env),
            StepKind::LocalParity {
                worktree: wt,
                workdir: wd,
            } => {
                // Host-executed: capture the local worktree's unpushed commits +
                // uncommitted + untracked state and replay it over the clone.
                // Best-effort — the failure note reflects the replay stage. A
                // bundle-seeded sandbox already has the branch tip, so only the
                // working-tree state rides (no thin commit bundle).
                let mode = if opts.bundle_seed.is_some() {
                    crate::parity::ParityMode::WorkingTreeOnly
                } else {
                    crate::parity::ParityMode::Full
                };
                let result =
                    crate::parity::apply_local_parity_mode(&provider, &id, wt, wd, &exec_env, mode);
                crate::parity::record_outcome(wt, &result);
                if let Err(e) = &result {
                    thegn_core::msg::warn(&format!(
                        "local parity: {e}; {}; autonomous agents are held on this worktree until parity succeeds.",
                        crate::parity::failure_note(e)
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
            StepKind::BundleSeed {
                worktree: wt,
                workdir: wd,
                branch: b,
            } => {
                // Host-executed, NOT best-effort (fatal like `clone`): without it
                // the sandbox has no repository.
                crate::provider_git::seed_from_bundle(&provider, &id, wt, wd, b, &exec_env)
            }
            StepKind::Checkpoint => {
                with_provision_timeout("checkpoint", std::time::Duration::from_secs(600), || {
                    provider.checkpoint(&id, Some("thegn-provisioned"))
                })
                .map(|cp| base_checkpoint = Some(cp))
            }
            StepKind::HomeClosurePush(roots) => {
                // Host-executed: sign the host closure and push it over the WSS
                // ssh tunnel. Best-effort: without signing or installed trust,
                // skip the push rather than import unsigned paths.
                match sprite_ssh_connect(cfg, worktree) {
                    Some((key, user, _workdir)) => {
                        if let Err(error) =
                            signed_home_closure_push(&provider, &id, worktree, &key, &user, roots)
                        {
                            let error = format!("{error:#}");
                            let hint = if error.starts_with("copy:") {
                                "; ensure the sandbox was (re)created with connect=ssh"
                            } else {
                                ""
                            };
                            thegn_core::msg::warn(&format!(
                                "host-parity p2p: {error}; skipping the closure push{hint}"
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
            if i >= fg_len {
                bg_failed = true;
                if deferred_failure.is_none() {
                    deferred_failure = Some((
                        step.label.clone(),
                        crate::provision_recover::sanitize_detail(&e.to_string()),
                    ));
                }
            }
            // A pure target-side devShell pre-warm that failed is invisible
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
                    // best-effort: advisory re-wait: the next provision step reports its own connect error
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

    if !deferred_only && !tail_done {
        fg_tail(base_checkpoint.as_deref())?;
        return Ok(ProvisionOutcome::ready(base_checkpoint));
    }

    let tail = journal::deferred_tail(journal::DeferredOutcome {
        cancelled,
        failed: bg_failed,
        checkpoint_planned: bg_has_checkpoint,
        checkpoint_ok: base_checkpoint.is_some(),
    });
    if tail.reconcile_credential {
        crate::remote_enqueue_auth::reconcile_provider_credential(
            cfg,
            &provider,
            pc,
            &id,
            &repo_root,
            worktree,
            name_override.is_none() && !credential_free,
        )?;
        if journal::read(&id)
            .is_some_and(|journal| journal.state == journal::DeferredState::Cancelled)
        {
            if let Err(error) =
                crate::remote_enqueue_auth::revoke_for_sandbox(pc, &id, Some(worktree))
            {
                tracing::warn!(target: "thegn::provider", %id, %error, "could not revoke cancelled sandbox credential");
            }
            return Ok(ProvisionOutcome::not_provisioned());
        }
    }
    if tail.write_done_marker {
        if let Err(error) =
            block_on_provider(|| async { provider.write(&id, &marker, envplan::MARKER_DONE).await })
        {
            tracing::debug!(target: "thegn::provider", %id, %error, "could not write completed provision marker");
        }
        if let Some(cp) = &base_checkpoint {
            let lock = crate::provision_gate::flake_lock_hash(&repo_root);
            if let Ok(db) = Db::open()
                && let Err(error) =
                    db.set_base_snapshot(&repo_root.to_string_lossy(), env_name, cp, &lock)
            {
                tracing::debug!(target: "thegn::provider", %id, %error, "could not record base snapshot");
            }
        }
    }
    match tail.journal {
        journal::JournalAction::Clear => {
            if let Err(error) = journal::clear(&id) {
                tracing::debug!(target: "thegn::provider", %id, %error, "could not clear completed provision journal");
            }
        }
        journal::JournalAction::MarkFailed => {
            if let Some((label, detail)) = deferred_failure
                && !journal::read(&id)
                    .is_some_and(|entry| entry.state == journal::DeferredState::Cancelled)
            {
                let failed = journal::Journal::failed(worktree, env_name, &label, &detail);
                if let Err(error) = journal::write(&id, &failed) {
                    tracing::warn!(target: "thegn::provider", %id, %error, "could not record failed deferred provision");
                }
            }
        }
        journal::JournalAction::Keep => {}
    }
    let deferred = if tail.write_done_marker {
        None
    } else {
        Some(DeferredHandoff {
            sandbox: id,
            lease: None,
        })
    };
    Ok(ProvisionOutcome {
        provisioned: true,
        checkpoint: base_checkpoint,
        deferred,
    })
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
/// The closure is signed with the installation key and the destination checks
/// signatures; `--substitute-on-destination` lets it fill public paths from its
/// own substituters in parallel. The ssh transport is supplied via `NIX_SSHOPTS`.
fn nix_copy_p2p_argv(user: &str, roots: &[String]) -> Vec<String> {
    let mut argv = vec![
        "copy".to_string(),
        "--to".to_string(),
        format!("ssh-ng://{user}@sprite"),
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

/// Probe a shell/tool token without the old `sh -c 'command -v …'` wrapper.
/// Absolute paths must be checked directly: `which_path` requires PATH to be
/// set, while `command -v /absolute/path` does not. For bare names,
/// `which_path` deliberately keeps its existing semantics — unlike shell
/// `command -v`, it does not check executable bits and returns no result when
/// PATH is unset.
fn path_for_shell_token(token: &str) -> Option<String> {
    let path = Path::new(token);
    if path.is_absolute() {
        return path.is_file().then(|| token.to_owned());
    }
    thegn_core::util::which_path(token)
}

/// Resolve the host `/nix/store` roots for the user's interactive shell + the
/// ubiquitous prompt tools, so a host-parity p2p push carries the binaries
/// themselves (not just what the rc sources) and they can be `nix profile
/// install`ed by name in the sandbox. Host-only + best-effort (path probe →
/// canonicalize → store-root); non-store / missing tools are skipped.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
fn host_shell_store_roots() -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(sh) = std::env::var("SHELL") {
        if Path::new(&sh).is_absolute() {
            names.push(sh);
        } else if let Some(n) = Path::new(&sh).file_name().and_then(|s| s.to_str()) {
            names.push(n.to_string());
        }
    }
    for t in ["zsh", "starship", "atuin", "direnv", "fzf"] {
        if !names.iter().any(|s| s == t) {
            names.push(t.to_string());
        }
    }
    let mut roots: Vec<String> = Vec::new();
    for t in names {
        let Some(path) = path_for_shell_token(&t) else {
            continue;
        };
        if let Ok(real) = std::fs::canonicalize(&path)
            && let Some(root) = store_root_of(&real.to_string_lossy())
            && !roots.contains(&root)
        {
            roots.push(root);
        }
    }
    roots
}

#[cfg(test)]
mod shell_path_probe_tests {
    use super::*;

    #[test]
    fn absolute_shell_path_does_not_depend_on_path() {
        let executable = std::env::current_exe().expect("test executable path");
        let token = executable.to_string_lossy().into_owned();
        assert_eq!(path_for_shell_token(&token), Some(token));
    }
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
/// using the host's `nix`, signing with the per-installation key. No hosted cache;
/// the host store is the source. Best-effort; returns the error for the caller to
/// warn on. Requires the sandbox to have been (re)created with `connect = "ssh"` (so its sshd accepts
/// the managed key) and `nix` installed in it.
// off-loop: provisioning path — reached only via spawn_blocking / the pool thread / CLI.
#[expect(clippy::disallowed_methods)]
fn push_home_closure_p2p(
    thegn_exe: &str,
    worktree: &str,
    ssh_key: &Path,
    user: &str,
    roots: &[String],
) -> anyhow::Result<()> {
    let proxy = format!(
        "{} sprite-proxy {}",
        thegn_core::util::sh_quote(thegn_exe),
        thegn_core::util::sh_quote(worktree),
    );
    let proxy_script = write_proxy_wrapper(ssh_key, &proxy)?;
    // Host-key policy from the one chokepoint (LoopbackTunneled over the
    // authenticated sprite WSS transport). See `thegn_core::hostkey`.
    let hostkey_opts = thegn_core::hostkey::host_key_opts_str(
        thegn_core::hostkey::HostKeyClass::LoopbackTunneled,
        &thegn_core::hostkey::HostKeyContext::default(),
    );
    let ssh_opts = format!(
        "-o ProxyCommand={} {} \
         -o LogLevel=ERROR -o ConnectTimeout=10 -o ServerAliveInterval=10 \
         -o ServerAliveCountMax=3 -i {} -p {}",
        thegn_core::util::sh_quote(&proxy_script.to_string_lossy()),
        hostkey_opts,
        thegn_core::util::sh_quote(&ssh_key.to_string_lossy()),
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

fn signed_home_closure_push(
    provider: &thegn_svc::provider::Provider,
    id: &str,
    worktree: &str,
    ssh_key: &Path,
    user: &str,
    roots: &[String],
) -> anyhow::Result<()> {
    let signing_key = crate::nix_signing::ensure_install_key()
        .map_err(|error| anyhow::anyhow!("no installation signing key: {error:#}"))?;
    let trust_argv = [
        "sh".to_string(),
        "-c".to_string(),
        crate::nix_signing::trust_script(&signing_key.public),
    ];
    match block_on_provider(|| async { provider.run_exec(id, &trust_argv, None, &[]).await }) {
        Ok((0, _)) => {}
        Ok((code, output)) => {
            return Err(anyhow::anyhow!(
                "trust: could not install the Nix signing trust key (exit {code}): {output}"
            ));
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "trust: could not install the Nix signing trust key: {error}"
            ));
        }
    }
    sign_home_closure_p2p(&signing_key, roots)
        .map_err(|error| anyhow::anyhow!("sign: {error:#}"))?;
    let exe = thegn_core::util::self_exe_str();
    push_home_closure_p2p(&exe, worktree, ssh_key, user, roots)
        .map_err(|error| anyhow::anyhow!("copy: {error:#}"))
}

#[expect(
    clippy::disallowed_methods,
    reason = "signing must wait for nix before copying"
)]
fn sign_home_closure_p2p(
    signing_key: &crate::nix_signing::InstallKey,
    roots: &[String],
) -> anyhow::Result<()> {
    let sign_argv = crate::nix_signing::sign_argv(&signing_key.secret, roots);
    let sign = std::process::Command::new(timeout_bin()?)
        .arg("--kill-after=5")
        .arg(HOME_CLOSURE_PUSH_TIMEOUT_SECS.to_string())
        .arg("nix")
        .args(&sign_argv)
        .output()
        .map_err(|error| anyhow::anyhow!("spawn `nix store sign` (p2p): {error}"))?;
    if !sign.status.success() {
        let stderr = tail_lines(&String::from_utf8_lossy(&sign.stderr), 6);
        return Err(anyhow::anyhow!(
            "nix store sign (p2p) failed: {stderr}; the host user must be a Nix trusted user to sign store paths, so the closure push is skipped"
        ));
    }
    Ok(())
}

/// Hard ceiling for the host-side p2p closure push, in seconds. Past this the
/// step is abandoned (best-effort) so the first shell is never held hostage to a
/// slow/unreachable transfer.
const HOME_CLOSURE_PUSH_TIMEOUT_SECS: u32 = 75;

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
// (kept flat); the provision loop calls them via these re-exports.
pub(crate) use crate::agent_configs::{upload_atuin_creds, upload_dotfiles};

/// The provisioning policy for a provider kind: `(force_bundle, strip)`.
/// `force_bundle` seeds the repo only from a host-built `git bundle` (never an
/// in-sandbox origin clone); `strip` removes and refuses every
/// credential-bearing step (and the git token, passthrough secrets and agent
/// auth resync). Both are true exactly for credential-free kinds (Vercel).
fn provision_credential_policy(kind: thegn_core::config::EnvProviderKind) -> (bool, bool) {
    let credential_free = kind.credential_free();
    (credential_free, credential_free)
}

#[cfg(test)]
fn provision_model_env(
    kind: thegn_core::config::EnvProviderKind,
    mode: thegn_core::config::ModelCredentials,
    env: Vec<(String, String)>,
) -> (Vec<(String, String)>, Vec<String>) {
    let grant = thegn_core::provider_credentials::model_grant(kind, mode);
    if grant.env_keys {
        (env, Vec::new())
    } else {
        thegn_core::provider_credentials::strip_model_env(env)
    }
}

fn sprites_withheld_notice(env_name: &str, names: &[String]) -> Option<String> {
    (!names.is_empty()).then(|| {
        format!(
            "sprites env {env_name}: model credentials not sent ({}); set model_credentials = \"upload\" in [env.{env_name}.provider] to send them",
            names.join(", ")
        )
    })
}

/// Refuse a credential-free provision that cannot bundle-seed: a SPARE
/// (`name_override`), a worktree without a local git dir, or one not on a
/// branch (detached HEAD). There is no origin clone fallback.
fn credential_free_seed_precheck(
    is_spare: bool,
    has_git_dir: bool,
    on_branch: bool,
) -> anyhow::Result<()> {
    if is_spare || !has_git_dir {
        anyhow::bail!(
            "vercel: the repo can only reach the sandbox as a host bundle; this worktree has no local git dir"
        );
    }
    if !on_branch {
        anyhow::bail!(
            "vercel: the repo can only reach the sandbox as a host bundle; this worktree has no \
             checked-out branch to bundle"
        );
    }
    Ok(())
}

/// The applier's refusal for a credential-bearing step on a credential-free
/// provider (`None` = the step may run).
fn credential_step_refusal(
    credential_free: bool,
    step: &thegn_core::envplan::ProvisionStep,
) -> Option<anyhow::Error> {
    (credential_free && thegn_core::provider_credentials::refuses_step(&step.kind)).then(|| {
        anyhow::anyhow!(
            "refused: {} would upload host credentials to a credential-free provider",
            step.label
        )
    })
}

/// The provisioning exec env for a credential-free provider: only terminal,
/// locale and devshell selection survive; every other passthrough (API keys,
/// tokens, credential locators) is dropped.
fn credential_free_exec_env(env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.into_iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "TERM" | "COLORTERM" | "LANG" | "LC_ALL" | "LC_CTYPE" | "THEGN_DEVSHELL"
            )
        })
        .collect()
}

#[cfg(test)]
mod credential_policy_tests {
    use super::*;
    use thegn_core::config::EnvProviderKind;
    use thegn_core::envplan::{ProvisionStep, StepKind};

    #[test]
    fn vercel_forces_bundle_and_strips() {
        assert_eq!(
            provision_credential_policy(EnvProviderKind::of("vercel")),
            (true, true)
        );
        for other in ["sprites", "fly", "machine0", "hetzner", "custom-thing"] {
            assert_eq!(
                provision_credential_policy(EnvProviderKind::of(other)),
                (false, false),
                "{other}"
            );
        }
    }

    #[test]
    fn seed_precheck_refuses_spares_and_missing_git_dir() {
        assert!(credential_free_seed_precheck(false, true, true).is_ok());
        for (spare, git, on_branch) in [
            (true, true, true),
            (false, false, true),
            (true, false, true),
            (false, true, false),
            (true, true, false),
        ] {
            let err = credential_free_seed_precheck(spare, git, on_branch).expect_err("refused");
            assert!(
                format!("{err}").contains("only reach the sandbox as a host bundle"),
                "{err}"
            );
        }
    }

    #[test]
    fn applier_refuses_credential_steps_only_when_credential_free() {
        let step = |kind| ProvisionStep {
            id: "s".into(),
            label: "Upload agent configs".into(),
            kind,
        };
        for kind in [
            StepKind::AgentConfigs(vec!["claude".into()]),
            StepKind::Dotfiles(vec![".gitconfig".into()]),
            StepKind::AtuinSync,
            StepKind::HomeClosurePush(vec!["/nix/store/x".into()]),
        ] {
            let s = step(kind);
            let err = credential_step_refusal(true, &s).expect("refused");
            assert!(
                format!("{err}").starts_with("refused: Upload agent configs"),
                "{err}"
            );
            assert!(credential_step_refusal(false, &s).is_none());
        }
        assert!(credential_step_refusal(true, &step(StepKind::Exec("true".into()))).is_none());
        assert!(credential_step_refusal(true, &step(StepKind::Checkpoint)).is_none());
    }

    #[test]
    fn credential_free_exec_env_drops_secrets() {
        let env = credential_free_exec_env(vec![
            ("TERM".into(), "xterm-256color".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-x".into()),
            ("GH_TOKEN".into(), "ghp_x".into()),
            ("THEGN_DEVSHELL".into(), "build".into()),
        ]);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["TERM", "THEGN_DEVSHELL"]);
    }

    #[test]
    fn sprites_model_credential_modes_filter_only_none() {
        let env = vec![
            ("TERM".into(), "xterm-256color".into()),
            ("ANTHROPIC_API_KEY".into(), "sentinel-b167".into()),
        ];
        let (none, names) = provision_model_env(
            EnvProviderKind::Sprites,
            thegn_core::config::ModelCredentials::None,
            env.clone(),
        );
        assert_eq!(none, vec![("TERM".into(), "xterm-256color".into())]);
        assert_eq!(names, vec!["ANTHROPIC_API_KEY"]);
        assert!(!names.iter().any(|name| name.contains("sentinel-b167")));
        let (upload, names) = provision_model_env(
            EnvProviderKind::Sprites,
            thegn_core::config::ModelCredentials::Upload,
            env,
        );
        assert_eq!(upload.len(), 2);
        assert!(names.is_empty());
        let notice = sprites_withheld_notice("remote", &["ANTHROPIC_API_KEY".into()]).unwrap();
        assert!(notice.contains("ANTHROPIC_API_KEY"));
        assert!(!notice.contains("sentinel-b167"));
    }
}

/// Last `n` non-empty lines of command output, for a compact error message.
fn tail_lines(out: &str, n: usize) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join(" | ")
}

// `env_halt_reason` (cheap pre-spawn HALT check) lives in the `env_halt` sibling.
pub use crate::env_halt::env_halt_reason;

/// Idempotently install and verify the resident bridge binary in a provider env
/// before connecting to it. Work runs off-loop via `block_on_provider`; `None`
/// means the caller must skip bridge setup and use the per-operation fallback.
pub fn ensure_remote_bridge(
    cfg: &Config,
    env_name: &str,
    binary: &Path,
) -> Option<crate::bridge_sup::ResidentBridge> {
    let (provider, id, _) = provider_sync_target(cfg, env_name)?;
    // Vercel never runs a resident bridge (exec rides `vercel-exec`), and the
    // install's file calls would resume a stopped sandbox on every switch.
    if matches!(provider, thegn_svc::provider::Provider::Vercel(_)) {
        return None;
    }
    let data = match std::fs::read(binary) {
        Ok(d) => d,
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "bridge binary unreadable ({}): {e}",
                binary.display()
            ));
            return None;
        }
    };
    let bridge = crate::bridge_sup::resident_bridge_for(&data);
    match block_on_provider(|| async {
        provider
            .ensure_private_exec_dir(
                &id,
                crate::bridge_sup::BRIDGE_ROOT,
                &crate::bridge_sup::resident_bridge_dir(&bridge),
            )
            .await?;
        provider.ensure_executable(&id, bridge.path(), &data).await
    }) {
        Ok(true) => thegn_core::msg::info(&format!(
            "pushed resident bridge → {id}:{} ({} bytes)",
            bridge.path(),
            data.len()
        )),
        Ok(false) => {} // already current — digest-verified install remains valid
        Err(e) => {
            thegn_core::msg::warn(&format!(
                "bridge binary install failed; not connecting the resident bridge: {e:#}"
            ));
            return None;
        }
    }
    // The legacy /workspace/.tg/thegn path is no longer launched.
    Some(bridge)
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

fn static_tunnel_refusal(
    is_provider: bool,
    profile: thegn_core::config::SandboxProfile,
    vpn: &thegn_core::config::VpnConfig,
    backend: &str,
) -> Option<thegn_core::sandbox::EnterError> {
    if is_provider
        && profile == thegn_core::config::SandboxProfile::SealedTunnel
        && vpn.is_enabled()
    {
        Some(thegn_core::sandbox::EnterError::Refused(
            thegn_core::sandbox::Refusal {
                class: thegn_core::sandbox::RefusalClass::TunnelUnenforced,
                backend: backend.to_string(),
                path: None,
                message: thegn_core::sandbox::TUNNEL_ONLY_REFUSAL.to_string(),
            },
        ))
    } else {
        None
    }
}

type ProvisionDestroyFn = Box<dyn FnOnce(&str) -> anyhow::Result<()> + Send>;

pub(crate) struct ProvisionedSandbox {
    name: String,
    destroy: Option<ProvisionDestroyFn>,
}

impl ProvisionedSandbox {
    fn new(name: String, destroy: ProvisionDestroyFn) -> Self {
        Self {
            name,
            destroy: Some(destroy),
        }
    }

    fn commit(mut self) {
        self.destroy = None;
    }
}

fn provision_guard_for_ensure_result(
    created: bool,
    name: String,
    destroy: ProvisionDestroyFn,
) -> Option<ProvisionedSandbox> {
    created.then(|| ProvisionedSandbox::new(name, destroy))
}

impl Drop for ProvisionedSandbox {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy.take()
            && let Err(error) = destroy(&self.name)
        {
            tracing::warn!(
                target: "thegn::sandbox",
                %error,
                name = %self.name,
                "could not destroy a provider sandbox created by a refused launch"
            );
        }
    }
}

/// Warm-on-open (8-E): for a provider env with `auto_provision`, create the
/// sandbox if it doesn't exist yet (API providers — CLI providers use
/// `up_command`/`placement.ensure`). Runs off-loop. Returns `Err` if the provider
/// rejected the request (e.g. a bad/expired token → 401); the caller decides
/// whether that halts (failover off) or just warns (failover on). Returns a
/// rollback guard only when this call created the sandbox; pre-existing
/// sandboxes and no-op paths return `None`. A pre-existing sandbox's egress
/// policy is verified, and re-applied on drift, before return; an unverifiable
/// one is an error (never destroyed).
fn auto_provision_sandbox(
    cfg: &Config,
    env_name: &str,
    worktree: &str,
) -> anyhow::Result<Option<ProvisionedSandbox>> {
    let Some(env) = cfg.env.get(env_name) else {
        return Ok(None);
    };
    let pc = &env.provider;
    if !pc.auto_provision {
        return Ok(None);
    }
    // A failed location or provider-binding read is an error the caller's
    // failover surfaces (halt, or degrade with a notice), never a silent no-op.
    let Some(name) =
        crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env_name)?
            .filter(|s| !s.is_empty())
    else {
        return Ok(None);
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
        return Ok(None);
    }
    let loc = GitLoc::for_worktree_op(Path::new(worktree), "sandbox auto-provision")
        .map_err(anyhow::Error::msg)?;
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
    let owner = crate::provider_factory::provider_sandbox_owner(cfg, worktree, env_name);
    let Some(provider) =
        crate::provider_factory::provider_for_named_owned(pc, &name, owner.as_deref())
    else {
        anyhow::bail!(
            "env '{env_name}' provider '{}' could not be built (token or managed keypair unresolved)",
            pc.provider
        );
    };
    match block_on_provider(|| async { provider.ensure_exists(&name).await }) {
        Ok(created) => {
            // Rebuild the provider during rollback: provider implementations do
            // not share a common ownership contract, while the config is cloneable.
            let destroy_config = pc.clone();
            let provisioned_name = name.clone();
            let guard = provision_guard_for_ensure_result(
                created,
                name.clone(),
                Box::new(move |sandbox_name| {
                    let destroy_provider = provider_for_named(&destroy_config, sandbox_name)
                        .ok_or_else(|| {
                            anyhow::anyhow!("provider could not be rebuilt for destroy")
                        })?;
                    block_on_provider(
                        || async move { destroy_provider.destroy(sandbox_name).await },
                    )
                }),
            );
            let repo_root: PathBuf = Db::open()
                .ok()
                .and_then(|db| db.repo_root_for(worktree).ok().flatten())
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(|| repo::main_worktree(Path::new(worktree)))
                .unwrap_or_else(|| PathBuf::from(worktree));
            let selection =
                selection_with_profile(worktree, &repo_root, Some(env_name.to_string()))
                    .ok_or_else(|| {
                        anyhow::anyhow!("the worktree's sandbox profile could not be read")
                    })?;
            let resolved = cfg.resolve_env(&repo_root, &loc, Path::new(worktree), &selection);
            let allow = &resolved.sandbox.network_allow;
            let block = &resolved.sandbox.network_block;
            let admission = crate::egress_admit::gate_handed_sandbox(
                &crate::egress_admit::BlockingProviderEgress {
                    provider: &provider,
                },
                created,
                provider.caps().egress,
                &name,
                allow,
                block,
            )?;
            if admission == crate::egress_admit::EgressAdmission::SkipUnsupported {
                crate::egress_admit::warn_unsupported_once(
                    crate::egress_admit::provider_label(&provider),
                    &name,
                );
            }
            if created {
                thegn_core::msg::info(&format!("provisioned sandbox {provisioned_name}"));
            }
            Ok(guard)
        }
        Err(e) => Err(e),
    }
}

/// Provision a fresh provider env's repo on open (8-A.3): clone the local repo's
/// `origin` into the worktree dir *inside the env*. A no-op when the plan already
/// provisioned the sandbox (its local marker is set: the plan seeded the repo
/// from a git bundle or cloned it), or when the local repo has no `origin`.
/// Prefers the resident bridge; otherwise runs the clone over the provider API
/// exec (the same channel provisioning uses), never the vendor CLI (THE-2014).
/// When neither channel is available the open fails with
/// [`no_exec_channel_message`](crate::provider_git::no_exec_channel_message)
/// instead of continuing onto an empty sandbox. A clone that runs and fails
/// warns and leaves the env as-is.
// off-loop by contract: runs inside launch_spec_with_key, which is documented
// blocking and must be called off the event loop (materialize spawn_blocking /
// CLI). NOTE: some direct pane-spawn helpers still call launch_spec on the
// loop — see the sweep report; the fix belongs at those callers.
fn provision_provider_repo(
    cfg: &Config,
    repo_root: &Path,
    worktree: &str,
    env_name: &str,
    loc: &GitLoc,
    branch: Option<&str>,
) -> anyhow::Result<()> {
    // Credential-free kinds (Vercel) get the repo only as a host bundle from
    // provision_provider_env_named: never an in-sandbox origin clone, and never
    // the host passthrough env.
    if cfg.env.get(env_name).is_some_and(|e| {
        thegn_core::config::EnvProviderKind::of(&e.provider.provider).credential_free()
    }) {
        return Ok(());
    }
    let id = match crate::provider_factory::provider_sandbox_name_checked(cfg, worktree, env_name) {
        Ok(id) => id.filter(|id| !id.is_empty()),
        Err(error) => {
            tracing::debug!(target: "thegn::provider", error = %format!("{error:#}"), worktree, "provider sandbox id unreadable");
            None
        }
    };
    if id
        .as_deref()
        .is_some_and(crate::provider_workdir::is_provisioned_locally)
    {
        return Ok(());
    }
    let Some(origin) = local_origin(repo_root) else {
        return Ok(());
    };
    let script = thegn_core::remote::provision_repo_script(&origin, branch);
    // Prefer the resident bridge (CLI-free) when it's already up for this loc.
    if let Some(b) = thegn_svc::bridge::for_loc(loc) {
        match b.exec(&["/bin/sh", "-lc", &script], Some(&loc.path()), &[]) {
            Ok(r) if r.exit == 0 => {
                initialize_provider_submodules(cfg, repo_root, loc);
                return Ok(());
            }
            Ok(r) => {
                thegn_core::msg::warn(&format!(
                    "provider repo provision failed (exit {}): {}",
                    r.exit,
                    crate::provision_recover::stderr_gist(&r.stderr)
                ));
                return Ok(());
            }
            // Bridge hiccup — fall through to the provider API exec below.
            Err(e) => thegn_core::msg::warn(&format!("provider repo provision via bridge: {e}")),
        }
    }
    let cli = match loc {
        GitLoc::Provider { control_prefix, .. } => control_prefix
            .first()
            .cloned()
            .unwrap_or_else(|| "provider".to_string()),
        _ => "provider".to_string(),
    };
    let provider = id.as_deref().and_then(|id| {
        let pc = &cfg.env.get(env_name)?.provider;
        let owner = crate::provider_factory::provider_sandbox_owner(cfg, worktree, env_name);
        crate::provider_factory::provider_for_named_owned(pc, id, owner.as_deref())
            .map(|provider| (provider, id.to_string()))
    });
    let Some((provider, id)) = provider else {
        return Err(anyhow::anyhow!(
            crate::provider_git::no_exec_channel_message(&cli)
        ));
    };
    let wd = thegn_core::util::sh_quote(&loc.path());
    let argv =
        crate::provision_recover::exec_login_argv(&format!("mkdir -p {wd} && cd {wd} && {script}"));
    let pc = &cfg
        .env
        .get(env_name)
        .expect("provider env checked above")
        .provider;
    let kind = thegn_core::config::EnvProviderKind::of(&pc.provider);
    let mode = if kind == thegn_core::config::EnvProviderKind::Sprites
        && !pc.model_credentials_is_explicit()
    {
        let project = repo_root.to_string_lossy().into_owned();
        sprites_credential_mode(resolve_sprites_model_credentials(
            worktree,
            &project,
            &pc.provider,
        ))
    } else {
        pc.model_credentials
    };
    let grant = thegn_core::provider_credentials::model_grant(kind, mode);
    let exec_env = cfg.repo_sandbox(repo_root).passthrough_env_remote();
    let exec_env = if grant.env_keys {
        exec_env
    } else {
        thegn_core::provider_credentials::strip_model_env(exec_env).0
    };
    match block_on_provider(|| async { provider.run_exec(&id, &argv, None, &exec_env).await }) {
        Ok((0, _)) => initialize_provider_submodules(cfg, repo_root, loc),
        Ok((code, out)) => thegn_core::msg::warn(&format!(
            "provider repo provision failed (exit {code}): {}",
            crate::provision_recover::stderr_gist(&out)
        )),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "{} (provider API exec failed: {error:#})",
                crate::provider_git::no_exec_channel_message(&cli)
            ));
        }
    }
    Ok(())
}

/// A spawn failure naming the program that could not be started: a bare
/// `No such file or directory` from a daemon whose PATH lacks the provider CLI
/// is otherwise undiagnosable.
// Test-only since THE-2014 removed the open path's vendor-CLI fallback.
#[cfg(test)]
pub(crate) fn spawn_failure_message(
    what: &str,
    program: &std::ffi::OsStr,
    error: &std::io::Error,
) -> String {
    let program = program.to_string_lossy();
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "{what} spawn failed: `{program}` not found ({error}) — is it installed and on this process's PATH?"
        )
    } else {
        format!("{what} spawn failed: could not run `{program}`: {error}")
    }
}

fn initialize_provider_submodules(cfg: &Config, repo_root: &Path, loc: &GitLoc) {
    if let Err(error) =
        crate::git_worktree::initialize(cfg, repo_root, Path::new(&loc.path()), Some(loc))
    {
        thegn_core::msg::warn(&format!("provider submodules not initialized: {error}"));
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

/// A runtime for one short-lived, blocking provider call.
///
/// **Current-thread, deliberately.** `Runtime::new()` builds a *multi-threaded*
/// runtime with one worker per core — 24 on the machine this was measured on —
/// which is a lot of thread creation and teardown to run a single network call.
/// Measured live: ~7 `thegn-rt` threads born per minute at idle, from exactly
/// these call sites.
///
/// Nothing here needs CPU parallelism. Provider work is network-bound, and the
/// one call site that fans out (`agent_configs`'s `join_all` upload batch) wants
/// **concurrency**, which a current-thread runtime provides — those futures
/// interleave at their await points just as well; they simply do not occupy
/// separate cores, which for an SSH upload is not a cost. `spawn_blocking` still
/// works, on the blocking pool, exactly as before.
///
/// `enable_all` because these futures use both the I/O driver and timers
/// (`tokio::time::timeout` in the upload batch).
pub(crate) fn provider_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))
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
            let rt = provider_runtime()?;
            rt.block_on(f())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("provider sync thread panicked"))?
    })
}

/// In-process registry of worktrees with a live provider file-sync (path →
/// env name), so the close thread can pull changes back without re-resolving.
#[derive(Clone)]
struct ProviderSyncRecord {
    env_name: String,
    in_flight: bool,
    /// What the upload sent; `None` = no baseline, the close sync overlays as before.
    baseline: Option<std::sync::Arc<thegn_svc::provider::SyncBaseline>>,
}

fn provider_sync_registry()
-> &'static std::sync::Mutex<std::collections::HashMap<String, ProviderSyncRecord>> {
    static REG: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, ProviderSyncRecord>>,
    > = std::sync::OnceLock::new();
    REG.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// How long a `pulling` entry written by another process blocks a second pull.
const SYNC_PULL_FRESH_MS: i64 = 15 * 60 * 1000;

fn provider_kind_name(pc: &thegn_core::config::EnvProviderConfig) -> String {
    pc.provider.trim().to_ascii_lowercase()
}

fn journal_entry(
    worktree: &str,
    env_name: &str,
    provider: &str,
    sandbox: &str,
    workdir: &str,
    baseline: &thegn_svc::provider::SyncBaseline,
    now_ms: i64,
) -> thegn_svc::sync_journal::SyncJournalEntry {
    thegn_svc::sync_journal::SyncJournalEntry {
        version: thegn_svc::sync_journal::JOURNAL_VERSION,
        worktree: worktree.to_owned(),
        env_name: env_name.to_owned(),
        provider: provider.to_owned(),
        sandbox: sandbox.to_owned(),
        workdir: workdir.to_owned(),
        baseline: thegn_svc::sync_journal::baseline_to_hex(baseline),
        state: thegn_svc::sync_journal::SyncState::Uploaded,
        uploaded_at_ms: now_ms,
        updated_at_ms: now_ms,
    }
}

/// Ok when the entry still names the env's current target; Err otherwise.
fn journal_target_matches(
    entry: &thegn_svc::sync_journal::SyncJournalEntry,
    provider: &str,
    sandbox: &str,
    workdir: &str,
) -> Result<(), String> {
    if entry.provider != provider {
        return Err("provider changed".to_owned());
    }
    if entry.sandbox != sandbox {
        return Err("sandbox changed".to_owned());
    }
    if entry.workdir != workdir {
        return Err("workdir changed".to_owned());
    }
    Ok(())
}

/// True when an entry is still in the short handoff window owned by another
/// process.
fn journal_pull_is_fresh(entry: &thegn_svc::sync_journal::SyncJournalEntry, now_ms: i64) -> bool {
    matches!(&entry.state, thegn_svc::sync_journal::SyncState::Pulling)
        && now_ms.saturating_sub(entry.updated_at_ms) < SYNC_PULL_FRESH_MS
}

fn journal_failure_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
        .collect()
}

fn set_journal_failed(dir: &Path, path: &str, reason: &str) {
    let state = thegn_svc::sync_journal::SyncState::Failed {
        reason: journal_failure_reason(reason),
    };
    if let Err(error) =
        thegn_svc::sync_journal::set_state_at(dir, path, state, thegn_core::util::now_ms())
    {
        tracing::warn!(
            target: "thegn::lifecycle",
            worktree = %path,
            %error,
            "provider sync journal failure state could not be written"
        );
    }
}

fn finish_provider_sync(
    dir: &Path,
    path: &str,
    env_name: &str,
    result: Result<(), String>,
) -> Result<(), String> {
    let mut registry = provider_sync_registry()
        .lock()
        .map_err(|_| "provider sync registry unavailable after pull".to_string())?;
    match result {
        Ok(()) => {
            registry.remove(path);
            drop(registry);
            if let Err(error) = thegn_svc::sync_journal::remove_at(dir, path) {
                tracing::warn!(
                    target: "thegn::lifecycle",
                    worktree = %path,
                    %error,
                    "provider sync journal entry could not be removed after pull"
                );
            }
            Ok(())
        }
        Err(error) => {
            if let Some(record) = registry.get_mut(path) {
                record.in_flight = false;
            }
            drop(registry);
            set_journal_failed(dir, path, &error);
            tracing::warn!(
                target: "thegn::lifecycle",
                worktree = %path,
                environment = %env_name,
                %error,
                "provider sync failed during close; sync record retained"
            );
            Err(error)
        }
    }
}

/// Upload the local worktree into a provider sandbox for `data = "sync"`. Called
/// only AFTER the launch is admitted, and at most once per resolution (`done`),
/// so a refused or failed candidate never transmits the worktree.
fn upload_provider_sync_once(
    cfg: &Config,
    env_data: thegn_core::config::DataMode,
    placement: &thegn_core::placement::Placement,
    env_name: &str,
    worktree: &str,
    done: &mut bool,
    warnings: &mut Vec<String>,
) {
    if *done
        || env_data != thegn_core::config::DataMode::Sync
        || !matches!(placement, thegn_core::placement::Placement::Provider(_))
    {
        return;
    }
    *done = true;
    #[cfg(test)]
    provider_sync_attempts_for_test()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(worktree.to_string());
    let Some((provider, id, workdir)) = provider_sync_target(cfg, env_name) else {
        return;
    };
    match block_on_provider(|| async {
        provider
            .upload_dir_with_baseline(&id, std::path::Path::new(worktree), &workdir)
            .await
    }) {
        Ok(baseline) => {
            register_provider_sync(
                worktree,
                env_name,
                Some(std::sync::Arc::new(baseline.clone())),
            );
            if let Some(env) = cfg.env.get(env_name) {
                let entry = journal_entry(
                    worktree,
                    env_name,
                    &provider_kind_name(&env.provider),
                    &id,
                    &workdir,
                    &baseline,
                    thegn_core::util::now_ms(),
                );
                if let Err(error) =
                    thegn_svc::sync_journal::record_at(&thegn_svc::sync_journal::dir(), &entry)
                {
                    let warning = format!(
                        "provider sync journal not written: {error}; a restart before close will not pull back"
                    );
                    warnings.push(warning.clone());
                    tracing::warn!(
                        target: "thegn::lifecycle",
                        worktree = %worktree,
                        %error,
                        "provider sync journal not written"
                    );
                    thegn_core::msg::warn(&warning);
                }
            }
        }
        Err(e) => {
            warnings.push(format!("provider sync upload failed: {e}"));
            thegn_core::msg::warn(&format!("provider sync upload failed for {worktree}: {e}"));
        }
    }
}

#[cfg(test)]
fn provider_sync_attempts_for_test() -> &'static std::sync::Mutex<Vec<String>> {
    static LOG: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    LOG.get_or_init(Default::default)
}

fn register_provider_sync(
    worktree: &str,
    env_name: &str,
    baseline: Option<std::sync::Arc<thegn_svc::provider::SyncBaseline>>,
) {
    if let Ok(mut r) = provider_sync_registry().lock() {
        r.insert(
            worktree.to_string(),
            ProviderSyncRecord {
                env_name: env_name.to_string(),
                in_flight: false,
                baseline,
            },
        );
    }
}

pub(crate) fn forget_provider_sync(worktree: &str) {
    if let Ok(mut registry) = provider_sync_registry().lock() {
        registry.remove(worktree);
    }
    if let Err(error) =
        thegn_svc::sync_journal::remove_at(&thegn_svc::sync_journal::dir(), worktree)
    {
        tracing::warn!(
            target: "thegn::lifecycle",
            worktree = %worktree,
            %error,
            "provider sync journal entry could not be removed"
        );
    }
}

/// On worktree close, pull the sandbox fs back into the local worktree for a
/// provider `data = "sync"` env. A failed pull is surfaced and retained in the
/// in-process registry so a later close can retry it.
pub(crate) fn deprovision_sync(path: &str) -> Result<(), String> {
    deprovision_sync_in(
        &thegn_svc::sync_journal::dir(),
        &crate::config_startup::cleanup_config(),
        path,
    )
}

/// Test seam for close-time provider sync. The caller is always an off-loop
/// lifecycle path; the provider call remains on its blocking runtime thread.
fn deprovision_sync_in(dir: &Path, cfg: &Config, path: &str) -> Result<(), String> {
    let sync_record = {
        let mut registry = provider_sync_registry()
            .lock()
            .map_err(|_| "provider sync registry unavailable".to_string())?;
        match registry.get_mut(path) {
            Some(record) => {
                if record.in_flight {
                    return Err("provider sync is already in progress".into());
                }
                record.in_flight = true;
                Some((record.env_name.clone(), record.baseline.clone()))
            }
            None => None,
        }
    };

    let download = |provider: &thegn_svc::provider::Provider,
                    id: &str,
                    workdir: &str,
                    baseline: Option<&thegn_svc::provider::SyncBaseline>| {
        let p = path.to_string();
        block_on_provider(|| async {
            provider
                .download_dir_reconciled(id, workdir, std::path::Path::new(&p), baseline)
                .await
        })
        .map_err(|error| format!("{error:#}"))
    };

    let Some((env_name, baseline)) = sync_record else {
        let entry = match thegn_svc::sync_journal::load_at(dir, path) {
            Ok(Some(entry)) => entry,
            Ok(None) => return Ok(()),
            Err(error) => {
                let file = thegn_svc::sync_journal::entry_path(dir, path);
                return Err(format!(
                    "provider sync journal entry for {path} is unreadable ({error:#}); remove {} to continue without a pull-back",
                    file.display()
                ));
            }
        };
        if journal_pull_is_fresh(&entry, thegn_core::util::now_ms()) {
            return Err("provider sync pull is already running in another thegn process".into());
        }

        let Some(env) = cfg.env.get(&entry.env_name) else {
            return Err(format!(
                "provider sync journal entry for {path} names removed environment {}; keeping the entry",
                entry.env_name
            ));
        };
        let Some((provider, id, workdir)) = provider_sync_target(cfg, &entry.env_name) else {
            return Err(format!(
                "provider sync journal entry for {path} names environment {} whose provider sync target is unavailable; keeping the entry",
                entry.env_name
            ));
        };
        if let Err(reason) =
            journal_target_matches(&entry, &provider_kind_name(&env.provider), &id, &workdir)
        {
            let file = thegn_svc::sync_journal::entry_path(dir, path);
            return Err(format!(
                "provider sync journal entry for {path} target no longer matches environment {} ({reason}); keep {}",
                entry.env_name,
                file.display()
            ));
        }
        let baseline = match thegn_svc::sync_journal::baseline_from_hex(&entry.baseline) {
            Ok(baseline) => baseline,
            Err(error) => {
                let reason = format!("provider sync journal baseline is unreadable: {error:#}");
                set_journal_failed(dir, path, &reason);
                return Err(reason);
            }
        };
        {
            let mut registry = provider_sync_registry()
                .lock()
                .map_err(|_| "provider sync registry unavailable".to_string())?;
            if registry.get(path).is_some_and(|record| record.in_flight) {
                return Err("provider sync is already in progress".into());
            }
            registry.insert(
                path.to_owned(),
                ProviderSyncRecord {
                    env_name: entry.env_name.clone(),
                    in_flight: true,
                    baseline: Some(std::sync::Arc::new(baseline.clone())),
                },
            );
        }
        if let Err(error) = thegn_svc::sync_journal::set_state_at(
            dir,
            path,
            thegn_svc::sync_journal::SyncState::Pulling,
            thegn_core::util::now_ms(),
        ) {
            let reason =
                format!("provider sync journal could not mark pull in progress: {error:#}");
            return finish_provider_sync(dir, path, &entry.env_name, Err(reason));
        }
        let result = download(&provider, &id, &workdir, Some(&baseline));
        return finish_provider_sync(dir, path, &entry.env_name, result);
    };

    let result = match provider_sync_target(cfg, &env_name) {
        Some((provider, id, workdir)) => {
            if let Ok(Some(entry)) = thegn_svc::sync_journal::load_at(dir, path)
                && journal_pull_is_fresh(&entry, thegn_core::util::now_ms())
            {
                if let Ok(mut registry) = provider_sync_registry().lock()
                    && let Some(record) = registry.get_mut(path)
                {
                    record.in_flight = false;
                }
                return Err(
                    "provider sync pull is already running in another thegn process".into(),
                );
            }
            if let Err(error) = thegn_svc::sync_journal::set_state_at(
                dir,
                path,
                thegn_svc::sync_journal::SyncState::Pulling,
                thegn_core::util::now_ms(),
            ) {
                tracing::warn!(
                    target: "thegn::lifecycle",
                    worktree = %path,
                    %error,
                    "provider sync journal could not mark pull in progress"
                );
            }
            download(&provider, &id, &workdir, baseline.as_deref())
        }
        _ => Err(format!(
            "provider sync target unavailable for environment {env_name}"
        )),
    };
    finish_provider_sync(dir, path, &env_name, result)
}

#[cfg(test)]
mod sync_journal_wire_tests {
    use super::*;
    use std::collections::BTreeMap;
    use thegn_core::config::{Config, EnvConfig};
    use thegn_core::testenv::IsolatedState;
    use thegn_svc::provider::SyncBaseline;
    use thegn_svc::sync_journal::{self, SyncJournalEntry, SyncState};

    fn config_with_env(name: &str) -> Config {
        Config {
            env: BTreeMap::from([(name.to_owned(), EnvConfig::default())]),
            ..Config::default()
        }
    }

    fn entry(path: &str, env_name: &str, state: SyncState, updated_at_ms: i64) -> SyncJournalEntry {
        let baseline: SyncBaseline = BTreeMap::from([("src/lib.rs".to_owned(), [0xabu8; 32])]);
        let mut entry = journal_entry(
            path,
            env_name,
            "sprites",
            "sandbox-a",
            "/workspace",
            &baseline,
            100,
        );
        entry.state = state;
        entry.updated_at_ms = updated_at_ms;
        entry
    }

    #[test]
    fn journal_entry_round_trips_the_baseline() {
        let _state = IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let baseline: SyncBaseline = BTreeMap::from([
            ("a.txt".to_owned(), [0xabu8; 32]),
            ("b.txt".to_owned(), [0x01u8; 32]),
        ]);
        let original = journal_entry(
            "/worktrees/a",
            "env-a",
            "sprites",
            "sandbox-a",
            "/workspace",
            &baseline,
            42,
        );

        sync_journal::record_at(dir.path(), &original).unwrap();
        let loaded = sync_journal::load_at(dir.path(), &original.worktree)
            .unwrap()
            .unwrap();

        assert_eq!(loaded, original);
        assert_eq!(
            sync_journal::baseline_from_hex(&loaded.baseline).unwrap(),
            baseline
        );
    }

    #[test]
    fn journal_target_mismatch_names_the_changed_field() {
        let _state = IsolatedState::new();
        let original = entry("/worktrees/a", "env-a", SyncState::Uploaded, 100);

        let mut changed = original.clone();
        changed.provider = "daytona".to_owned();
        assert!(
            journal_target_matches(&changed, "sprites", "sandbox-a", "/workspace")
                .unwrap_err()
                .contains("provider")
        );

        let mut changed = original.clone();
        changed.sandbox = "sandbox-b".to_owned();
        assert!(
            journal_target_matches(&changed, "sprites", "sandbox-a", "/workspace")
                .unwrap_err()
                .contains("sandbox")
        );

        let mut changed = original;
        changed.workdir = "/other".to_owned();
        assert!(
            journal_target_matches(&changed, "sprites", "sandbox-a", "/workspace")
                .unwrap_err()
                .contains("workdir")
        );
    }

    #[test]
    fn fresh_pulling_entry_from_another_process_refuses() {
        let _state = IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktree").to_string_lossy().into_owned();
        let original = entry(
            &path,
            "missing-env",
            SyncState::Pulling,
            thegn_core::util::now_ms(),
        );
        sync_journal::record_at(dir.path(), &original).unwrap();

        let error = deprovision_sync_in(dir.path(), &Config::default(), &path).unwrap_err();
        assert_eq!(
            error,
            "provider sync pull is already running in another thegn process"
        );
        assert_eq!(
            sync_journal::load_at(dir.path(), &path).unwrap(),
            Some(original)
        );
    }

    #[test]
    fn stale_pulling_entry_is_retried() {
        let _state = IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktree").to_string_lossy().into_owned();
        let original = entry(
            &path,
            "env-a",
            SyncState::Pulling,
            thegn_core::util::now_ms() - SYNC_PULL_FRESH_MS - 1,
        );
        sync_journal::record_at(dir.path(), &original).unwrap();

        let error = deprovision_sync_in(dir.path(), &config_with_env("env-a"), &path).unwrap_err();
        assert!(error.contains("target is unavailable"), "{error}");
        assert!(!error.contains("already running"), "{error}");
        assert_eq!(
            sync_journal::load_at(dir.path(), &path).unwrap(),
            Some(original)
        );
    }

    #[test]
    fn deprovision_without_registry_or_journal_is_ok() {
        let _state = IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktree").to_string_lossy().into_owned();

        assert_eq!(
            deprovision_sync_in(dir.path(), &Config::default(), &path),
            Ok(())
        );
    }

    #[test]
    fn deprovision_with_unreadable_journal_is_an_error_naming_the_file() {
        let _state = IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktree").to_string_lossy().into_owned();
        sync_journal::record_at(dir.path(), &entry(&path, "env-a", SyncState::Uploaded, 100))
            .unwrap();
        let file = sync_journal::entry_path(dir.path(), &path);
        let root = thegn_core::fsperm::open_private_dir(dir.path()).unwrap();
        let file_name = file.file_name().unwrap().to_str().unwrap();
        root.write_owner_only_atomic(file_name, b"garbage").unwrap();

        let error = deprovision_sync_in(dir.path(), &Config::default(), &path).unwrap_err();
        assert!(error.contains(&file.display().to_string()), "{error}");
        assert!(error.contains("remove"), "{error}");
    }

    #[test]
    fn deprovision_with_journal_for_a_removed_env_keeps_the_entry() {
        let _state = IsolatedState::new();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktree").to_string_lossy().into_owned();
        let original = entry(&path, "removed-env", SyncState::Uploaded, 100);
        sync_journal::record_at(dir.path(), &original).unwrap();

        let error = deprovision_sync_in(dir.path(), &Config::default(), &path).unwrap_err();
        assert!(error.contains("removed-env"), "{error}");
        assert_eq!(
            sync_journal::load_at(dir.path(), &path).unwrap(),
            Some(original)
        );
    }

    #[test]
    fn forget_removes_the_journal_entry() {
        let _state = IsolatedState::new();
        let path = tempfile::tempdir()
            .unwrap()
            .path()
            .to_string_lossy()
            .into_owned();
        let original = entry(&path, "env-a", SyncState::Uploaded, 100);
        let dir = sync_journal::dir();
        sync_journal::record_at(&dir, &original).unwrap();
        register_provider_sync(&path, "env-a", None);

        forget_provider_sync(&path);

        assert_eq!(sync_journal::load_at(&dir, &path).unwrap(), None);
    }
}

/// Launch inputs only the agent-spawn path supplies.
///
/// Everything the compositor launches resolves its command from `[[agents]]` by
/// name, which cannot express "run this agent *on this task*". A supervisor
/// spawning a fleet needs exactly that, so it renders the command itself — via
/// [`thegn_core::agent_task`], which owns the prompt templating and the shell
/// quoting — and hands the result down. Empty by default, so every existing
/// caller composes exactly as it did before.
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchExtras<'a> {
    /// Replaces the `[[agents]]` command resolution wholesale. Already
    /// prompt-substituted and shell-quoted by the caller.
    pub cmd_override: Option<&'a str>,
    /// Exported as `THEGN_PROMPT` beside `THEGN_WORKTREE`/`THEGN_BRANCH`, so a
    /// wrapper, a hook, or the agent itself can read the task it was given.
    pub prompt: Option<&'a str>,
    /// Skip recording `choice` as the worktree's remembered agent. Set by
    /// preset application: a preset "only launches panes" (its commands MUST
    /// NOT claim the worktree's agent, even when one resolves to an agent
    /// entry). Default `false` — every existing caller records as before.
    pub suppress_agent_record: bool,
    /// A `[[pipeline.stages]]` name whose `env` / `permissions` overrides
    /// apply to this launch (the model is already in `cmd_override`).
    pub stage: Option<&'a str>,
    /// Per-launch permission grants layered over the configured agent/stage.
    pub permissions: &'a [String],
    /// Permit the per-launch grants to widen the configured baseline.
    pub widen_permissions: bool,
}

/// Why a settled sandbox must NOT compose an argv, if it can't reach its
/// target (defense in depth behind the `prepare_sandbox_env_with_selection`
/// halt — THE-2044):
/// - a spec whose provider placement has no remote exec transport (wrapping
///   would yield a bare `/bin/sh -lc …` on the HOST);
/// - no spec at all while the outcome still claims a remote/provider env
///   (`is_remote` or a persisted provider `location`) — the host-shell arm
///   would silently run a provider/remote env on the host.
pub(crate) fn host_argv_refusal(sb: &SandboxOutcome) -> Option<String> {
    match &sb.spec {
        Some(spec) => spec.placement.transport_refusal(),
        None if sb.is_remote || sb.location.is_some() => {
            Some("refusing to run a provider/remote env on the host".to_string())
        }
        None => None,
    }
}

/// The [`SandboxHalt`] for a [`host_argv_refusal`]. `compose_spec` never
/// re-resolves the env (no new env-resolution site), so the env name is the
/// placement family — the provider kind for a provider spec.
fn host_argv_halt(sb: &SandboxOutcome, reason: String) -> anyhow::Error {
    let (env_name, placement) = sb.spec.as_ref().map_or_else(
        || ("remote".to_string(), sb.backend_label.clone()),
        |s| (s.placement.kind(), s.placement.label()),
    );
    SandboxHalt {
        env_name,
        placement,
        reason,
        ask: false,
        dormant: None,
        sealed_home: false,
    }
    .into()
}

/// Pure composition of the final [`LaunchSpec`] from a settled sandbox: argv
/// (sandbox-wrapped, or a bare login shell on the host fallback), cwd, env,
/// plus the effective backend label and any fallback warnings.
#[allow(clippy::too_many_arguments)]
pub fn compose_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    loc: &GitLoc,
    sb: &SandboxOutcome,
    extras: LaunchExtras<'_>,
) -> anyhow::Result<LaunchSpec> {
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
    let provider_session = (sb.backend_label == "devcontainer")
        .then(|| crate::devcontainer_provider::session_for(worktree))
        .flatten();
    if sb.backend_label == "devcontainer" && sb.spec.is_none() && provider_session.is_none() {
        return Err(DevcontainerLaunchRefused {
            reason: "selected provider session is no longer available".into(),
        }
        .into());
    }
    let in_oci = sb.spec.is_some() || provider_session.is_some();
    let cmd = if let Some(over) = extras.cmd_override {
        // An agent launched on a task: the caller already rendered the command.
        over.to_string()
    } else if choice == "clean-shell" {
        // Watchdog fallback: a plain rc-free shell. Ignores any `[sandbox] shell`
        // override on purpose — the override is part of what may be hanging.
        clean_shell_inner()
    } else if choice == "shell" && !sb_shell.is_empty() {
        shell_inner_override(&sb_shell)
    } else if choice == "shell" {
        shell_inner(in_oci)
    } else {
        resolve_command(cfg, choice).map_err(|why| anyhow::anyhow!("{why}"))?
    };
    // A provider pane lands at $HOME; prefix `cd <workdir>` so direnv/devShell load.
    let placement = sb.spec.as_ref().map(|s| &s.placement);
    let cmd = crate::provider_workdir::with_workdir_cd(choice, placement, cmd);
    let cmd = if sb.is_remote {
        format!(
            "{}{cmd}",
            crate::remote_enqueue_auth::source_prefix(worktree)
        )
    } else {
        cmd
    };
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
    if let Some(p) = extras.prompt.filter(|p| !p.is_empty()) {
        env.push(("THEGN_PROMPT".to_string(), p.to_string()));
    }
    // The binary that launched this pane, so a headless worker can call back
    // into the SAME build that dispatched it. A bare `thegn` on the pane PATH is
    // whatever the operator last installed: here it was a copy five schema
    // versions stale that did not carry the `dispatch` subcommand at all, so
    // every pipeline worker's `thegn dispatch report <row>` — the one call that
    // closes its row — failed with `unrecognized subcommand`. That is
    // indistinguishable from a crashed worker and is how rows pile up unclosable.
    // `self_exe_path`, not `current_exe`: after a rebuild-in-place the latter
    // reads back `<path> (deleted)`, and a worker inheriting THAT gets a
    // THEGN_BIN whose every call fails with "No such file or directory" — the
    // same unclosable-row signature as the stale-copy case above. It must stay
    // an on-disk path the worker can resolve itself (never `/proc/self/exe`,
    // which would name the worker's own binary).
    if let Some(exe) = thegn_core::util::self_exe_path() {
        env.push(("THEGN_BIN".to_string(), exe.to_string_lossy().into_owned()));
    }
    // Match the build's parallelism to the pane's OWN ceiling. `CARGO_BUILD_JOBS`
    // is per-invocation, so without this every worktree claims the whole machine
    // independently and N of them multiply — the amplification behind ~67
    // concurrent compilers on a 24-core box. Advisory: the dev shell only applies
    // it when unset, so an explicit value on the command line still wins.
    let cap_limits = thegn_core::sandbox::SandboxLimits::from(&cfg.sandbox.limits);
    if let Some(jobs) = thegn_core::sandbox_cpucap::cargo_jobs_for(&cap_limits) {
        env.push(("CARGO_BUILD_JOBS".to_string(), jobs.to_string()));
    }
    // A provider exec is a host-side CLI process. Pass only the same explicit
    // local-env values admitted to `devcontainer up`; `pane_pty` supplies the
    // safe runtime base environment independently.
    if let Some(session) = &provider_session {
        env.extend(session.exec_env().iter().cloned());
    }
    // Local bwrap gets its passthrough env (tokens, API keys) via the pane's
    // process env, not world-readable `--setenv` argv (enter_argv skips those).
    if let Some(spec) = &sb.spec
        && spec.backend == sandbox::Backend::Bwrap
        && spec.placement.is_local()
    {
        env.extend(spec.env.iter().cloned());
    }
    // Opt-in model-proxy routing (`route_via_proxy`): probe-before-inject so a
    // down proxy can never strand the agent on a dead loopback endpoint. A
    // provider session is a host-side CLI process that re-reads raw repo JSON;
    // do not expose proxy credentials or agent env to that parser. The provider
    // handle already carries the only local-env values it is allowed to see.
    if provider_session.is_none() {
        match crate::model_proxy_daemon::agent_proxy_env(cfg, choice, worktree) {
            crate::model_proxy_daemon::ProxyEnvDecision::Inject(vars) => env.extend(vars),
            crate::model_proxy_daemon::ProxyEnvDecision::Skipped(why) => {
                tracing::warn!(agent = %choice, %why, "route_via_proxy skipped — launching with direct-provider env");
            }
            crate::model_proxy_daemon::ProxyEnvDecision::NotRequested => {}
        }
    }
    // `[[agents]].env` (+ the stage's overlay, key by key): the operator's own
    // per-entry environment — an account's `CLAUDE_CONFIG_DIR`, a pi home —
    // applied LAST so it wins over the composed identity env. Secrets expand
    // here (`env:`/`file:`); an unresolvable value is dropped, never exported
    // as its literal ref. Keep it out of a provider CLI's host environment:
    // that process parses raw repo JSON and must not gain a new localEnv read.
    if provider_session.is_none()
        && let Ok(eff) = thegn_core::agent_task::effective_agent_with(
            cfg,
            choice,
            extras.stage,
            extras.permissions,
            extras.widen_permissions,
        )
    {
        env.extend(eff.expanded_env());
    }
    let mut warnings = sb.warnings.clone();
    let mut degraded = sb.degraded_from_provider;
    let argv = match (&sb.spec, &provider_session) {
        (None, Some(session)) => match session.exec_argv(&cmd) {
            Ok(argv) => thegn_core::sandbox_cpucap::wrap_provider_pane_argv(
                argv,
                &thegn_core::sandbox::SandboxLimits::from(&cfg.sandbox.limits),
                thegn_core::sandbox_cpucap::detect_cpu_cap(),
            ),
            Err(error) => {
                tracing::warn!(target: "thegn::config_trust", "devcontainer session rejected: {error}");
                // The provider was selected before the native OCI candidate was
                // resolved, so there is no equivalent containment spec to enter
                // here. Returning a typed refusal keeps the pane spawn site
                // from turning invalidated target authority into a host login
                // shell whose rc files could execute repository code.
                return Err(DevcontainerLaunchRefused {
                    reason: error.to_string(),
                }
                .into());
            }
        },
        (Some(spec), _) => {
            if let Some(reason) = host_argv_refusal(sb) {
                return Err(host_argv_halt(sb, reason));
            }
            let entered = sandbox::enter_argv_guarded(spec, &cmd).map_err(|error| {
                let hint = error.remediation_hint();
                anyhow::Error::new(error).context(hint)
            })?;
            if let Some(warning) = entered.omitted_secrets_warning() {
                thegn_core::msg::warn(&warning);
                warnings.push(warning);
            }
            entered.into_spawned()
        }
        (None, None) => {
            // Never the host for an env that claims to be remote/provider.
            if let Some(reason) = host_argv_refusal(sb) {
                return Err(host_argv_halt(sb, reason));
            }
            // Host fallback: a login shell so PATH/env expand — still CAPPED. There
            // is no sandbox spec here (no container runtime, or one turned off), but
            // capping is not sandboxing: this pane runs the same builds as any other
            // and needs the same ceiling. Without the wrap it escaped `thegn.slice`
            // entirely, which is how the aggregate cap came to govern nothing.
            thegn_core::sandbox_cpucap::wrap_uncontained_pane_argv(vec![
                thegn_core::util::shell(),
                "-lc".to_string(),
                cmd,
            ])
        }
    };
    // The label must describe the argv, not the resolver's intent. For a LOCAL
    // placement the argv is authoritative, so reconcile against it: a resolver
    // that returned a container backend while composing a bare host shell is a
    // false containment claim, and this is where it stops being one. A remote
    // placement keeps the resolver's label — its runtime lives behind a
    // transport whose argv shape can't be read from here (see `sandbox_truth`).
    let local =
        sb.spec.as_ref().is_none_or(|s| s.placement.is_local()) && provider_session.is_none();
    let remote = sb.spec.as_ref().is_some_and(|s| !s.placement.is_local());
    let truth = local.then(|| thegn_core::sandbox_truth::reconcile(&sb.backend_label, &argv));
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
    tracing::debug!(
        class = sb.admission.class().as_str(),
        enforcement = sb.admission.enforcement().as_str(),
        "sandbox admission"
    );
    Ok(LaunchSpec {
        argv,
        cwd,
        env,
        backend,
        warnings,
        degraded,
        remote,
    })
}

/// Compose the [`LaunchSpec`] for running `choice` in `worktree`. Records the
/// choice (and any sandbox backend) in the DB, mirroring the zellij path's
/// side effects so the dashboard/`--resume` keep working. Errors when an
/// explicit sandbox choice cannot be honored (no silent host fallback).
///
/// `branch` is the worktree's branch (for the pane env + title); `None` falls
/// back to the worktree basename.
///
/// THE-84: the last two production callers (the sandbox-chain pre-warm and
/// the `sandbox-argv` verb) moved to record-suppressed builders, so this is
/// test-only today — the tripwire forces re-auditing if a production caller
/// reappears (it must then decide about `suppress_agent_record`).
#[cfg_attr(not(test), expect(dead_code))]
pub fn launch_spec(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    // `daemon_persistent = false`: the convenience path serves the tool drawer,
    // one-shot CLI shells, and tests — in-process panes that keep the bwrap
    // `--die-with-parent` guard. Daemon-routed center tabs go through
    // `launch_spec_center` / `launch_spec_synced_with` / `terminal_launch_spec`,
    // which resolve the flag from the live daemon route.
    launch_spec_full(
        cfg,
        worktree,
        branch,
        choice,
        false,
        LaunchExtras::default(),
    )
}

/// [`launch_spec`] for a **daemon-routed center pane resolved ON the loop** —
/// a split (`spawn_worktree_shell_pane`) or the startup watchdog's clean-shell
/// fallback. Identical to `launch_spec` except that it marks the sandbox spec
/// pane-daemon-owned when the daemon route is active, so a local bwrap pane
/// drops `--die-with-parent`.
///
/// That guard is `prctl(PR_SET_PDEATHSIG)`, keyed to the *thread* that forked
/// bwrap, not the process — on a daemon-owned pane it reaps a shell that is
/// supposed to survive, which is exactly the "switched away, came back, my
/// terminal started over" failure. `launch_spec` (⇒ `daemon_persistent =
/// false`) is correct only for panes that really do die with the compositor.
///
/// The off-loop sibling is [`crate::direnv_warm::launch_spec_synced_with`]; both
/// resolve the flag from the one source of truth,
/// [`crate::handlers::startup::daemon_active`].
pub fn launch_spec_center(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
) -> anyhow::Result<LaunchSpec> {
    launch_spec_center_with(cfg, worktree, branch, choice, LaunchExtras::default())
}

/// [`launch_spec_center`] with caller-supplied [`LaunchExtras`]. The
/// split/new-pane shell path passes `suppress_agent_record: true` — a shell
/// pane is not a choice of agent, and recording it would clobber the wizard /
/// launch-menu / `--bind`-owned `worktrees.agent` row on every pane open.
pub fn launch_spec_center_with(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    extras: LaunchExtras<'_>,
) -> anyhow::Result<LaunchSpec> {
    let daemon_persistent = crate::handlers::startup::daemon_active(cfg);
    launch_spec_full(cfg, worktree, branch, choice, daemon_persistent, extras)
}

/// The sandbox-chain pre-warm resolution (run.rs `prewarm_sandbox_chain`):
/// daemon-routed like any center pane, and NEVER recorded — a warm is not a
/// choice of agent (THE-84: it must not clobber `worktrees.agent`).
pub(crate) fn prewarm_spec(cfg: &Config, worktree: &str) -> anyhow::Result<LaunchSpec> {
    launch_spec_center_with(
        cfg,
        worktree,
        None,
        "shell",
        LaunchExtras {
            suppress_agent_record: true,
            ..Default::default()
        },
    )
}

/// Automatic sibling/tab prewarm may prepare a contained target or attach an
/// already-running daemon session, but it must never synthesize a host pane.
/// Focused materialize/launch paths deliberately do not call this policy: an
/// explicit host choice remains an explicit user action.
pub(crate) fn reject_host_prewarm(
    specs: &mut Result<Vec<(u32, LaunchSpec)>, crate::handlers::provision::SpecError>,
) {
    if specs.as_ref().is_ok_and(|resolved| {
        resolved
            .iter()
            .any(|(_, spec)| spec.backend == "host" && !spec.remote)
    }) {
        *specs = Err(crate::handlers::provision::SpecError::PrewarmSkipped);
    }
}

/// Like [`launch_spec`] but with the full set of launch knobs.
///
/// `daemon_persistent` marks the resolved sandbox spec as pane-daemon-owned so
/// a local bwrap pane drops `--die-with-parent` and survives UI detach instead
/// of being reaped with its forking thread. Set `true` for daemon-routed center
/// tabs, `false` for ephemeral in-process panes (drawer/CLI).
/// `extras` carries the agent-spawn path's command override + prompt; every
/// other caller passes [`LaunchExtras::default`].
#[allow(clippy::too_many_arguments)]
pub fn launch_spec_full(
    cfg: &Config,
    worktree: &str,
    branch: Option<&str>,
    choice: &str,
    daemon_persistent: bool,
    extras: LaunchExtras<'_>,
) -> anyhow::Result<LaunchSpec> {
    // One state-DB handle for the whole resolution (each open re-runs the
    // pragmas). The location read hands back the handle it opened, so the
    // state reads below reuse it instead of opening SQLite a second time —
    // this runs on the event loop for a drawer/menu launch.
    let (mut loc, db) = match GitLoc::for_worktree_op_shared(Path::new(worktree), "pane launch") {
        Ok((loc, db)) => (loc, db),
        Err(_) if !remote_placement_configured(cfg) => {
            (GitLoc::Local(PathBuf::from(worktree)), None)
        }
        Err(error) => return Err(anyhow::anyhow!(error)),
    };

    // Resolved once: the host env fold near the end needs the entry's env KEYS
    // (see there). The headless allow-list (`[[agents]].permissions`, or the
    // stage's) is NOT applied here: it rides the launch command itself through
    // the harness's command-scoped mechanism (`EffectiveAgent::permission_args`,
    // THE-440), so launch preparation never reads or writes anything under the
    // repository-controlled worktree.
    let eff_agent = thegn_core::agent_task::effective_agent_with(
        cfg,
        choice,
        extras.stage,
        extras.permissions,
        extras.widen_permissions,
    )
    .ok();

    // No handle ⇒ the state DB did not exist yet (nothing recorded, so the
    // worktree is local) or the tolerated location refusal above: open it, as
    // the state reads below always did, creating it when absent.
    let db = match db {
        Some(db) => Some(db),
        None => match Db::open() {
            Ok(db) => Some(db),
            Err(error) => {
                launch_state_gate(
                    cfg,
                    worktree,
                    choice,
                    "the state database could not be opened, so this worktree's saved sandbox override, selected environment and credential-overlay gate cannot be checked",
                    &worktree_isolation_read_failure(worktree, &error),
                )?;
                None
            }
        },
    }; // best-effort for shells: choice recording and repo-root lookups fall back to local git below

    // Record the choice for the dashboard / `--resume` (keyed by worktree path).
    // Two launches are deliberately NOT recorded as the worktree's remembered
    // agent: the transient `clean-shell` watchdog fallback (the user may fix
    // their dotfiles), and tool drawers (yazi/lazygit/editor/diff) — those are
    // overlays, not the worktree's agent, and are auto-prewarmed on every switch,
    // so recording them would clobber the real choice on every worktree.
    let saved_backend = match db.as_ref() {
        Some(db) => {
            if !extras.suppress_agent_record
                && choice != "clean-shell"
                && cfg.tool_command(choice).is_none()
            {
                let _ = db.set_worktree_agent(worktree, choice); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }
            match thegn_core::db::retry_transient(|| db.worktree_sandbox(worktree)) {
                Ok(saved) => saved,
                Err(error) => {
                    launch_state_gate(
                        cfg,
                        worktree,
                        choice,
                        "this worktree's saved sandbox override could not be read",
                        &error,
                    )?;
                    None
                }
            }
        }
        None => None,
    };

    // The local repo root drives the per-repo sandbox overlay + slug. Prefer the
    // DB (carries remote worktrees with no local cwd), else climb from the path.
    let recorded_root = match db
        .as_ref()
        .map(|db| thegn_core::db::retry_transient(|| db.repo_root_for(worktree)))
    {
        Some(Ok(root)) => root,
        Some(Err(error)) => {
            launch_state_gate(
                cfg,
                worktree,
                choice,
                "this worktree's repository root could not be read",
                &error,
            )?;
            None
        }
        None => None,
    };
    let repo_root: PathBuf = recorded_root
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| repo::main_worktree(Path::new(worktree)))
        .unwrap_or_else(|| PathBuf::from(worktree));

    // The selected execution environment: the worktree's own `env_name`, else
    // its workspace's. `resolve_env` falls through to the repo/global default.
    let selected_env = match db.as_ref() {
        Some(db) => match thegn_core::db::retry_transient(|| {
            db.effective_env_checked(worktree, &repo_root.to_string_lossy())
        }) {
            Ok(selected) => selected,
            Err(error) => {
                launch_state_gate(
                    cfg,
                    worktree,
                    choice,
                    "this worktree's selected execution environment could not be read",
                    &worktree_isolation_read_failure(worktree, &error),
                )?;
                None
            }
        },
        None => None,
    };

    // THE-2054: a provider row stored under an older transport (e.g. the
    // vendor `sprite exec` CLI before `exec = auto` ignored it) is healed to the
    // env's current control prefix before anything launches through it.
    if let Some(db) = db.as_ref()
        && let Some(fresh) = crate::provision_gate::heal_provider_prefix(
            db,
            cfg,
            &repo_root,
            worktree,
            selected_env.clone(),
        )
    {
        loc = fresh;
    }

    // A recorded per-worktree backend is a deliberate override (only the wizard
    // writes it, when divergent); honour it as explicit so it sticks across
    // restarts even against a non-"auto" config. Empty/NULL → auto (re-resolve).
    let saved_is_override =
        thegn_core::launch_backend::saved_pick_is_override(saved_backend.as_deref());
    let mut outcome = prepare_sandbox_env_with_selection(
        cfg,
        &repo_root,
        worktree,
        &loc,
        saved_backend.as_deref(),
        saved_is_override,
        &thegn_core::sandbox_pin::EnvSelection {
            name: selected_env.clone(),
            profile: None,
        },
        db.as_ref(),
    )?;
    if let Some(target) = outcome.route_ssh_target.as_ref() {
        crate::remote_enqueue_auth::reconcile_ssh_credential(cfg, target, &repo_root, worktree)?;
    }
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
            // A sandbox whose repository cannot be provisioned ends the open
            // here (THE-2014) instead of falling into the unprovisioned halt.
            let env_name = cfg
                .resolve_env(
                    &repo_root,
                    &loc,
                    Path::new(worktree),
                    &thegn_core::sandbox_pin::EnvSelection {
                        name: selected_env.clone(),
                        profile: None,
                    },
                )
                .name;
            provision_provider_repo(cfg, &repo_root, worktree, &env_name, &fresh, branch)?;
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
    // THE-515: when the trusted overlay that would pin this repo's agent
    // accounts / env bundle (HOME) is refused, refuse an AGENT launch instead
    // of running it with credentials that block did not choose. Shells (and
    // any non-agent choice) always launch: the refused block's bundle is then
    // simply not applied, so a registry problem can never lock the user out
    // of a terminal.
    if !loc.is_remote()
        && let Some(db) = db.as_ref()
    {
        match thegn_core::db::retry_transient(|| repo_slug_checked(db, &repo_root)) {
            Ok(Some(slug)) => credential_overlay_gate(cfg, db, &slug, choice)?,
            Ok(None) => {}
            Err(error) => launch_state_gate(
                cfg,
                worktree,
                choice,
                "the workspace slug for the credential-overlay gate could not be read",
                &error,
            )?,
        }
    }
    let resolved = (!loc.is_remote())
        .then_some(db.as_ref())
        .flatten()
        .map(|db| {
            let slug = repo_slug(db, &repo_root);
            // At launch (off the event loop) so secret resolvers may run.
            bundle::compose_at_launch(cfg, db, worktree, slug.as_deref(), Some(choice))
        })
        .unwrap_or_default();
    bundle_read_gate(cfg, worktree, choice, &resolved.unreadable)?;
    // Credential/HOME dirs the child writes into must exist before launch.
    for dir in &resolved.ensure_dirs {
        if let Err(error) = std::fs::create_dir_all(dir) {
            dotfiles_launch_gate(
                cfg,
                worktree,
                choice,
                anyhow::Error::new(error).context(format!("cannot create {dir}")),
            )?;
        }
    }
    // Tier-2 dotfiles: materialize each active bundle's dotfile tree into its
    // managed HOME (idempotent, off the event loop — launch_spec is blocking).
    if !loc.is_remote() {
        for (spec, home) in bundle::dotfile_targets(cfg, &resolved) {
            if let Err(error) = bundle::materialize_dotfiles(&spec, &home, &|pid| {
                crate::platform::pid_alive(i64::from(pid))
            }) {
                dotfiles_launch_gate(
                    cfg,
                    worktree,
                    choice,
                    anyhow::Error::new(error)
                        .context(format!("bundle dotfiles into {}", home.display())),
                )?;
            }
        }
    }
    if let Some(spec) = outcome.spec.as_mut() {
        resolved.merge_into_spec(spec);
        // Profile credential firewall (H): mount the active profile's git/gh/gpg
        // config dirs path-preservingly so the container sees the profile
        // identity its rerooted GIT_CONFIG_GLOBAL/GH_CONFIG_DIR env points at.
        // No-op on the default profile. SKIPPED for a sealed launch
        // (`seal_home`): these are the profile's gh config and gnupg dirs, i.e.
        // exactly the credentials the sealed `$HOME` exists to hide (THE-215).
        // (The agent's own provider auth home, `provider_home_mounts` below, is
        // intentionally mounted: the harness cannot run without its own login.)
        let cred_mounts = if spec.seal_home.is_some() {
            Vec::new()
        } else {
            thegn_core::profile::sandbox_cred_mounts()
        };
        for (host, ro) in cred_mounts {
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
    // itself. Only an already-existing cache is read; a cold cache is left to
    // explicit target-side setup. Local worktrees only (remote panes run where
    // the host store isn't mounted). See [`devenv`].
    let devshell = (!loc.is_remote() && !outcome.is_remote)
        .then(|| {
            if db.is_none() && !cfg.env.is_empty() {
                tracing::warn!(
                    target: "thegn::sandbox",
                    worktree,
                    "devshell injection skipped: the state database could not be opened"
                );
                return false;
            }
            let approvals = db
                .as_ref()
                .map(|db| {
                    crate::handlers::repo_trust::approvals_for(db, &repo_root.to_string_lossy())
                })
                .unwrap_or_else(thegn_core::config_resolve::Approvals::deny_all);
            match crate::handlers::repo_trust::effective_environment_for_worktree(
                cfg,
                db.as_ref(),
                &repo_root,
                Path::new(worktree),
                None,
                &approvals,
            ) {
                Ok(environment) => environment.sandbox.inject_devshell,
                Err(error) => {
                    crate::launch_notice::post(
                        worktree,
                        &format!(
                            "devshell injection skipped: the environment could not be resolved: {error:#}"
                        ),
                    );
                    tracing::warn!(
                        target: "thegn::sandbox",
                        worktree,
                        error = %format!("{error:#}"),
                        "devshell injection skipped: the environment could not be resolved"
                    );
                    false
                }
            }
        })
        .filter(|enabled| *enabled)
        .and_then(|_| devenv::cached(&repo_root));
    // Toolchain activation is one shared, cache-first composition for host,
    // sandbox, provider, and daemon launches. The provider call performs no
    // child-process work here: a cold approved environment schedules a bounded
    // background refresh and this launch safely receives shims/base instead.
    let activation = crate::mise_provider::activation_for_launch(
        cfg,
        worktree,
        &repo_root,
        &loc,
        &resolved,
        devshell.as_ref(),
        db.as_ref(),
    );

    // Mark the resolved sandbox as pane-daemon-owned when this pane is
    // daemon-routed, so `enter_argv` drops the bwrap `--die-with-parent` guard
    // (harmless no-op for non-bwrap backends). The daemon reaps its own
    // sessions, so the guard would only reap a shell that is meant to persist.
    if let Some(spec) = outcome.spec.as_mut() {
        spec.daemon_persistent = daemon_persistent;
        // `compose_spec` snapshots init_script/env_overrides into argv, so the
        // activation layer must be applied before it builds the wrapper.
        apply_activation_sandbox(spec, &activation);
    }

    // `[[agents]].env` (+ the stage overlay) is applied LAST inside
    // `compose_spec` precisely so it beats the composed identity env — and on the
    // SANDBOX path it does, because the spec env is folded in before it. The host
    // path folded the bundle + build env in AFTER the call, silently reversing
    // that precedence for exactly the entries that most need it: a `pipeline-*`
    // agent pointing `CODEX_HOME` at its own config home got the inherited
    // `~/.codex` folded back over it by the bundle's legacy per-provider account
    // carve (`bundle::compose_at_launch`, which falls back to the harness's
    // `effective_config_dir` when no account is managed). The headless
    // sandbox/approval settings then never loaded, so every worker started fine
    // and died unable to write a file — a silent worker death, not a config error.
    // Reserve the entry's keys so the same env wins on both paths.
    let agent_env_keys = eff_agent.as_ref().map(|eff| &eff.env);
    let agent_env = eff_agent
        .as_ref()
        .map(|eff| eff.expanded_env())
        .unwrap_or_default();

    // The sandbox path reverses the same precedence by a different mechanism:
    // `env_overrides` are emitted as `export KEY='…'` lines inside the wrap
    // script (`sandbox::wrap_script`), which runs AFTER the pane's process env
    // is set, so they too land on top of the entry env. This is the one point
    // where every contributor to that map — the bundle, the build cache, the
    // devShell, the ssh shim — has finished folding, so strip the reserved keys
    // here rather than teaching each of them the rule.
    if let Some(sandbox_spec) = outcome.spec.as_mut() {
        reserve_sandbox_overrides(&mut sandbox_spec.env_overrides, agent_env_keys);
        // Reserving the key is only half of it. The bundle does not merely SET a
        // credential home, it also carves that path read-write into the sandbox
        // (`bundle::fold_cred_dir`) — because under a read-only `$HOME` the
        // harness cannot otherwise write its own runtime state. Dropping the
        // override without redirecting the carve leaves the entry's home
        // unmounted, and codex dies before its first turn with
        // `failed to initialize in-process app-server client: Read-only file
        // system`. Carve whichever home the entry actually names.
        for (host, dir) in provider_home_mounts(eff_agent.as_ref()) {
            if !sandbox_spec.mounts.iter().any(|m| m.dest == dir) {
                let _ = std::fs::create_dir_all(&dir); // best-effort: dir prep: the mount reports the real failure
                sandbox_spec.mounts.push(sandbox::Mount {
                    dest: dir,
                    host,
                    ro: false,
                    cache: false,
                });
            }
        }
        let cache = crate::build_cache::apply_sandbox_compiler_cache(
            sandbox_spec,
            cfg,
            &repo_root,
            &agent_env,
        );
        crate::build_cache::record_sandbox_cache_decision(
            cfg,
            &repo_root,
            sandbox_spec.backend,
            &cache,
        );
        if cfg.sandbox.compiler_cache == thegn_core::config::SandboxCompilerCache::Auto
            && !cache.active
        {
            outcome.warnings.push(cache.reason);
        }
    }

    let mut spec = compose_spec(cfg, worktree, branch, choice, &loc, &outcome, extras)?;
    if let Some(stage) = extras.stage
        && let Some(sandbox_spec) = outcome.spec.as_ref()
    {
        let homes: Vec<PathBuf> = provider_home_mounts(eff_agent.as_ref())
            .into_iter()
            .map(|(_, dest)| PathBuf::from(dest))
            .collect();
        let common = crate::stage_worker_hold::stage_git_common_dir(&sandbox_spec.worktree);
        let callback_ok =
            thegn_core::util::self_exe_path().is_some() && !thegn_core::util::self_exe_is_stale();
        if let Some(hold) = crate::stage_worker_hold::stage_worker_hold(
            stage,
            &crate::stage_worker_hold::StageHoldFacts {
                backend: sandbox_spec.backend,
                local: sandbox_spec.placement.is_local(),
                worktree: &sandbox_spec.worktree,
                mounts: &sandbox_spec.mounts,
                git_common_dir: common.as_deref(),
                provider_homes: &homes,
                callback_ok,
            },
        ) {
            return Err(anyhow::Error::new(hold));
        }
    }
    // On the bare-host path (no sandbox spec and no provider session) the bundle
    // identity + build env ride the pane env (layered on the curated base in
    // `spawn_with_env`). A provider CLI is also host-side, but its environment
    // must remain limited to the provider handle's safe runtime + allowlist.
    let provider_active = outcome.backend_label == "devcontainer"
        && crate::devcontainer_provider::session_for(worktree).is_some();
    if outcome.spec.is_none() && !provider_active {
        extend_reserving(&mut spec.env, resolved.env_pairs(), agent_env_keys);
        extend_reserving(&mut spec.env, build_env, agent_env_keys);
    }
    // Apply the same composed activation plan to both launch forms. Sandbox
    // paths become an init export against the target's own base PATH; host
    // paths are materialized as one ordinary PATH value.
    if outcome.spec.is_none() && !provider_active {
        apply_activation_host(&mut spec, &activation);
        // Automatic route: after activation, so the shim directory stays first.
        crate::cmd::job_shim::inject_pane_env(cfg, &repo_root, &mut spec.env);
    }
    // Record what this launch ACTUALLY enters, for every surface that displays
    // containment. `spec.backend` is argv-derived (see `compose_spec`), so this
    // column can never carry a sandbox the pane does not have. Deliberately a
    // different column from `sandbox_backend`, which stays the user's pick and
    // drives the next resolution. Best-effort: the DB is a cache, and failing to
    // record a label must never take down a launch.
    if let Some(db) = db.as_ref() {
        let _ = db.set_worktree_observed(worktree, &spec.backend); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    Ok(spec)
}

fn apply_activation_sandbox(
    spec: &mut sandbox::SandboxSpec,
    plan: &thegn_core::toolchain_activation::ActivationPlan,
) {
    if let Some(path) = plan.path() {
        let line = format!("export PATH={}:$PATH\n", thegn_core::util::sh_quote(&path));
        spec.init_script = Some(match spec.init_script.take() {
            Some(existing) => format!("{line}{existing}"),
            None => line,
        });
    }
    for (key, value) in plan.env_pairs() {
        spec.env_overrides.entry(key).or_insert(value);
    }
}

/// Append `extra` to a pane env, skipping every key the agent entry declares in
/// its own `[[agents]].env` overlay. `spec.env` is applied in order, so a later
/// pair wins — dropping the key is what lets the entry's earlier value stand.
///
/// A key the entry declares but whose `env:`/`file:` ref did not resolve is
/// reserved too: `expanded_env` drops such a value deliberately ("never exported
/// as its literal ref"), and quietly substituting an unrelated credential home
/// for it is the same silent-wrong-value failure the reservation exists to stop.
fn extend_reserving(
    env: &mut Vec<(String, String)>,
    extra: Vec<(String, String)>,
    reserved: Option<&std::collections::BTreeMap<String, String>>,
) {
    env.extend(
        extra
            .into_iter()
            .filter(|(k, _)| !reserved.is_some_and(|r| r.contains_key(k))),
    );
}

/// The `(host, dest)` credential-home mounts an agent entry's own env implies:
/// one per provider whose relocation var (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`, …)
/// the entry sets to an absolute path. Path-preserving and read-write, matching
/// `bundle::fold_cred_dir` — this replaces the carve the bundle would have made
/// for the default home, which reserving the key suppresses.
///
/// Values are taken post-expansion, so an `env:`/`file:` ref that did not
/// resolve yields no mount (there is no path to carve).
fn provider_home_mounts(
    eff: Option<&thegn_core::agent_task::EffectiveAgent>,
) -> Vec<(String, String)> {
    let Some(eff) = eff else { return vec![] };
    let homes: Vec<&str> = thegn_core::account::providers()
        .iter()
        .map(|p| p.home_env)
        .filter(|e| !e.is_empty())
        .collect();
    eff.expanded_env()
        .into_iter()
        .filter(|(k, v)| homes.contains(&k.as_str()) && std::path::Path::new(v).is_absolute())
        .map(|(_, v)| {
            let p = thegn_core::util::expand_tilde(&v);
            (p.clone(), p)
        })
        .collect()
}

/// Drop every sandbox env override the agent entry declares for itself, so the
/// entry env `compose_spec` applies to the pane process is not re-exported over
/// inside the wrap script. See the call site for why this is the right chokepoint.
fn reserve_sandbox_overrides(
    overrides: &mut std::collections::HashMap<String, String>,
    reserved: Option<&std::collections::BTreeMap<String, String>>,
) {
    let Some(reserved) = reserved else { return };
    overrides.retain(|k, _| !reserved.contains_key(k));
}

fn apply_activation_host(
    spec: &mut LaunchSpec,
    plan: &thegn_core::toolchain_activation::ActivationPlan,
) {
    if let Some(path) = plan.path() {
        let base = std::env::var("PATH").unwrap_or_default();
        spec.env.retain(|(key, _)| key != "PATH");
        let path = if base.is_empty() {
            path
        } else {
            format!("{path}:{base}")
        };
        spec.env.push(("PATH".into(), path));
    }
    for (key, value) in plan.env_pairs() {
        if !spec.env.iter().any(|(existing, _)| existing == &key) {
            spec.env.push((key, value));
        }
    }
}

// Compatibility helper for the focused legacy unit test. Production launches
// use `apply_activation_host` above so bundle, devshell, and provider values
// share one composition path.
#[cfg(test)]
fn inject_devshell_host(spec: &mut LaunchSpec, dev: &devenv::Devshell) {
    if let Some(path) = &dev.path {
        let base = std::env::var("PATH").unwrap_or_default();
        let merged = if base.is_empty() {
            path.clone()
        } else {
            format!("{path}:{base}")
        };
        spec.env.retain(|(key, _)| key != "PATH");
        spec.env.push(("PATH".into(), merged));
    }
    for (key, value) in &dev.vars {
        if !spec.env.iter().any(|(existing, _)| existing == key) {
            spec.env.push((key.clone(), value.clone()));
        }
    }
}

/// Refuse an agent launch whose credential-bearing trusted overlay
/// (`[project.<key>] accounts` / `env_bundle`) is refused (THE-515). A refused
/// block without credentials only loses its settings; it does not block.
pub(crate) fn credential_overlay_gate(
    cfg: &Config,
    db: &Db,
    slug: &str,
    choice: &str,
) -> anyhow::Result<()> {
    // Every configured `[[agents]]`/`[[tools]]` entry, not only the ones whose
    // program maps to a known account provider: `env_bundle` redirects HOME for
    // ANY of them, which is exactly the credential the refused block pinned. A
    // choice that is not a configured entry (a shell) always launches.
    if !is_configured_entry(cfg, choice) {
        return Ok(());
    }
    if let thegn_core::workspace_overlay::WorkspaceOverlay::Refused(refusal) =
        cfg.workspace_overlay_for_tab_slug(db, slug)
        && thegn_core::workspace_overlay::refusal_carries_credentials(&cfg.workspace, &refusal)
    {
        anyhow::bail!("agent launch refused: {refusal}");
    }
    Ok(())
}

pub(crate) fn dotfiles_launch_gate(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    failure: anyhow::Error,
) -> anyhow::Result<()> {
    dotfiles_launch_gate_with_notice(cfg, worktree, choice, failure, crate::launch_notice::post)
}

pub(crate) fn dotfiles_launch_gate_with_notice(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    failure: anyhow::Error,
    notify: impl FnOnce(&str, &str),
) -> anyhow::Result<()> {
    if is_configured_entry(cfg, choice) {
        let detail = format!("{failure:#}");
        return Err(failure.context(format!(
            "refusing to launch `{choice}`: its bundle HOME could not be prepared: {detail}"
        )));
    }
    let text = format!("{failure:#}; the shell launches without it");
    thegn_core::msg::warn(&text);
    notify(worktree, &text);
    Ok(())
}

fn is_configured_entry(cfg: &Config, choice: &str) -> bool {
    cfg.agents
        .iter()
        .chain(cfg.tools.iter())
        .any(|entry| entry.name == choice)
}

/// Gate a launch whose env-bundle or account selection could not be read
/// (THE-509h): a configured agent is refused, a shell launches with a visible
/// warning (see `launch_state_gate`).
fn bundle_read_gate(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    unreadable: &[String],
) -> anyhow::Result<()> {
    bundle_read_gate_with(
        cfg,
        worktree,
        choice,
        unreadable,
        crate::launch_notice::post,
    )
}

fn bundle_read_gate_with(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    unreadable: &[String],
    notify: impl FnOnce(&str, &str),
) -> anyhow::Result<()> {
    if unreadable.is_empty() {
        return Ok(());
    }
    launch_state_gate_with_notice(
        cfg,
        worktree,
        choice,
        "this worktree's env-bundle or coding-agent account selection could not be read",
        &anyhow::anyhow!("{}", unreadable.join("; ")),
        notify,
    )
}

pub(crate) fn launch_state_gate(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    what: &str,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    launch_state_gate_with_notice(
        cfg,
        worktree,
        choice,
        what,
        error,
        crate::launch_notice::post,
    )
}

pub(crate) fn launch_state_gate_with_notice(
    cfg: &Config,
    worktree: &str,
    choice: &str,
    what: &str,
    error: &anyhow::Error,
    notify: impl FnOnce(&str, &str),
) -> anyhow::Result<()> {
    if let Some(failure) = error
        .chain()
        .find_map(|source| source.downcast_ref::<WorktreeIsolationReadFailure>())
    {
        return Err(anyhow::Error::new(failure.halt.clone()));
    }
    if is_configured_entry(cfg, choice) {
        anyhow::bail!("refusing to launch `{choice}`: {what}: {error:#}");
    }
    let text = format!("{what}: {error:#}; the shell launches without it");
    thegn_core::msg::warn(&text);
    notify(worktree, &text);
    Ok(())
}

/// The persisted slug for a repo root (for per-workspace account defaults), or
/// `None` if the DB has no slug yet.
pub(crate) fn repo_slug(db: &Db, repo_root: &Path) -> Option<String> {
    repo_slug_checked(db, repo_root).ok().flatten()
}

pub(crate) fn repo_slug_checked(db: &Db, repo_root: &Path) -> anyhow::Result<Option<String>> {
    let Some(file_name) = repo_root.file_name() else {
        return Ok(None);
    };
    let base = file_name.to_string_lossy().into_owned();
    db.slug_for_repo(&repo_root.to_string_lossy(), &base)
        .map(Some)
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "agent_cleanup_fixture.rs"]
pub(crate) mod cleanup_fixture;

#[cfg(test)]
#[path = "agent_dotfiles_tests.rs"]
mod dotfiles_tests;

#[cfg(test)]
#[path = "agent_vpn_tests.rs"]
mod vpn_tests;

#[cfg(test)]
#[path = "agent_projection_tests.rs"]
mod projection_tests;

#[cfg(test)]
#[path = "agent_provision_tests.rs"]
mod provision_tests;
