//! Running a headless CLI agent to completion, off the event loop.
//!
//! The single place thegn spawns a fixing agent. Background queues decide *that*
//! an agent should run and *what* to tell it ([`thegn_core::agent_task`] renders
//! the prompt and resolves the command); this module owns the process mechanics,
//! which are the same regardless of what is being fixed:
//!
//! * cwd is the work's **own worktree** — never the canonical checkout;
//! * a login shell, so an npm-global `claude` is on PATH with the user's creds,
//!   exactly like an interactive agent pane;
//! * its own process group/job, so completion is defined over the agent's whole
//!   tree and the deadline reaps it (`bounded`, unix only);
//! * stdout/stderr are drained on the calling thread by `thegn_proc`'s poll
//!   reader (a chatty agent must not deadlock or SIGPIPE on a full pipe), with
//!   only a bounded tail retained — this runs off the compositor;
//! * the inherited git environment scrubbed, so the agent's `git` operates on
//!   its cwd rather than an inherited `GIT_DIR`/`GIT_INDEX_FILE`.
//!
//! Keeping it in one module is what stops a second queue from re-deriving the
//! quoting contract and re-stubbing the Windows path.

use thegn_core::agent_task::{TaskKind, TaskVars};

#[cfg(test)]
thread_local! {
    static FORCE_HEADLESS_UNSUPPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Explain why the shared runner cannot execute on this platform. Callers use
/// this before changing queue state or spending an attempt.
pub(crate) fn headless_unsupported() -> Option<String> {
    #[cfg(test)]
    if FORCE_HEADLESS_UNSUPPORTED.with(std::cell::Cell::get) {
        return Some("headless agent handoff is not supported on this platform (THE-235); held, no attempt spent".into());
    }
    #[cfg(not(unix))]
    {
        return Some("headless agent handoff is not supported on this platform (THE-235); held, no attempt spent".into());
    }
    #[cfg(unix)]
    None
}

#[cfg(test)]
pub(crate) fn force_headless_unsupported() -> impl Drop {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FORCE_HEADLESS_UNSUPPORTED.with(|value| value.set(false));
        }
    }
    FORCE_HEADLESS_UNSUPPORTED.with(|value| value.set(true));
    Reset
}

#[cfg(any(unix, test))]
const MAX_RENDERED_COMMAND_BYTES: usize = 100 * 1024;

#[cfg(any(unix, test))]
fn rendered_command_exceeds_limit(command: &str) -> bool {
    command.len() > MAX_RENDERED_COMMAND_BYTES
}

#[cfg(any(unix, test))]
fn fit_command(
    template: &str,
    prompt: &str,
    vars: &TaskVars,
) -> Result<Option<(String, usize)>, thegn_core::agent_task::TemplateError> {
    let command = thegn_core::agent_task::substitute_command(template, prompt, vars)?;
    if !rendered_command_exceeds_limit(&command) {
        return Ok(Some((command, prompt.len())));
    }
    if !template.contains("{prompt}") {
        return Ok(None);
    }

    let mut prompt_bytes = prompt.len() / 2;
    loop {
        let bounded_prompt = thegn_core::agent_task::bound_text(prompt, prompt_bytes);
        let command = thegn_core::agent_task::substitute_command(template, &bounded_prompt, vars)?;
        if !rendered_command_exceeds_limit(&command) {
            return Ok(Some((command, bounded_prompt.len())));
        }
        if prompt_bytes == 0 {
            return Ok(None);
        }
        prompt_bytes /= 2;
    }
}

/// One dispatch: where to run, what to say, and how long to allow.
#[cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "The Windows stub only reads kind; THE-235 uses these fields on Unix."
    )
)]
pub(crate) struct AgentTaskRun<'a> {
    pub kind: TaskKind,
    /// Absolute path of the worktree the agent works in (its cwd).
    pub worktree: &'a str,
    /// The rendered prompt — prose, handed over through one configured channel.
    pub prompt: &'a str,
    /// The command template, already resolved (`agent_command`, or an
    /// `[[agents]]` entry's headless form). Placeholders are bare.
    pub command_template: &'a str,
    /// Variables the command template may reference, minus `{prompt}`.
    pub vars: &'a TaskVars,
    /// Deadline for this invocation, in seconds. 0 is not "unbounded": it maps
    /// to a finite ceiling ([`bounded::AGENT_CEILING`]).
    pub timeout_secs: u64,
    /// When `Some`, run the agent command INSIDE this resolved sandbox (the
    /// queue's opt-in isolation floor). `None` keeps the default host + shared
    /// slice posture. The floor decision itself is made by [`agent_floor_gate`]
    /// before this run — a fail-closed miss never reaches here.
    pub sandbox: Option<thegn_core::sandbox::SandboxSpec>,
    /// Run with a credential-free environment. Every autonomous handoff
    /// (autopilot and queue agents) must not inherit the launcher's credentials.
    /// Interactive panes do not use this runner.
    pub credential_free: bool,
}

/// How a queue's opt-in agent floor resolves for one dispatch — the attribution
/// split. A fail-closed miss (or an unbuildable sandbox under a demanded floor)
/// is [`InfraHold`](AgentDispatch::InfraHold): the queue entry is held, never the
/// branch/PR marked failed (the merge-guard doctrine). A runnable spec is also
/// provisioned and preflighted before this gate returns; VPN-requiring specs are
/// held until queued dispatch can attach their tunnel.
pub(crate) enum AgentDispatch {
    /// Run the task; `Some(spec)` runs it inside that sandbox, `None` on the host.
    Run(Option<thegn_core::sandbox::SandboxSpec>),
    /// Run, but the floor was missed under `degrade` — carry the warning to log.
    RunDegraded(Option<thegn_core::sandbox::SandboxSpec>, String),
    /// Do not run: an infrastructure failure. Hold the entry; never blame the code.
    InfraHold(String),
}

/// What happened when a headless agent was dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentRunVerdict {
    /// The agent ran to an exit; `None` means it was killed by a signal.
    Exited(Option<i32>),
    /// The agent ran but was cut short (timeout, cancellation, unsettled group, or reap failure).
    /// It consumed its attempt.
    Interrupted(String),
    /// No agent process ever did any work. This infrastructure failure must not consume an attempt.
    NotStarted(String),
}

impl AgentRunVerdict {
    pub(crate) fn exited_zero(&self) -> bool {
        matches!(self, Self::Exited(Some(0)))
    }

    pub(crate) fn not_started(&self) -> Option<&str> {
        match self {
            Self::NotStarted(reason) => Some(reason),
            Self::Exited(_) | Self::Interrupted(_) => None,
        }
    }
}

/// Give each queue agent a thegn-owned OCI name derived from its full worktree
/// path, so same-basename worktrees cannot collide.
fn scope_agent_spec(
    mut spec: thegn_core::sandbox::SandboxSpec,
    worktree: &str,
) -> thegn_core::sandbox::SandboxSpec {
    spec.name = thegn_core::sandbox::container_name(&format!("agent-{worktree}"));
    credential_free_spec(spec)
}

const RESET_CREDENTIAL_HELPER: [(&str, &str); 3] = [
    ("GIT_CONFIG_COUNT", "1"),
    ("GIT_CONFIG_KEY_0", "credential.helper"),
    ("GIT_CONFIG_VALUE_0", ""),
];

/// Remove credential-bearing mounts and environment from an autonomous agent's
/// resolved sandbox. Interactive pane specs do not pass through this helper.
pub(crate) fn credential_free_spec(
    spec: thegn_core::sandbox::SandboxSpec,
) -> thegn_core::sandbox::SandboxSpec {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    credential_free_spec_at(spec, home.as_deref())
}

fn credential_free_spec_at(
    mut spec: thegn_core::sandbox::SandboxSpec,
    home: Option<&std::path::Path>,
) -> thegn_core::sandbox::SandboxSpec {
    spec.env
        .retain(|(key, _)| !credential_shaped(key) && !credential_locator(key));
    for (key, value) in RESET_CREDENTIAL_HELPER {
        spec.env.retain(|(existing, _)| existing != key);
        spec.env.push((key.to_string(), value.to_string()));
    }
    if let Some(home) = home {
        let identity_dests = thegn_core::ssh_creds::identity_mounts_at(home, &[])
            .into_iter()
            .map(|mount| mount.dest)
            .collect::<Vec<_>>();
        strip_credential_mounts(&mut spec.mounts, home, &identity_dests);
    }
    spec
}

fn strip_credential_mounts(
    mounts: &mut Vec<thegn_core::sandbox::Mount>,
    home: &std::path::Path,
    identity_dests: &[String],
) {
    let credential_dirs = [
        home.join(".ssh"),
        home.join(".gnupg"),
        home.join(".config/gh"),
    ];
    mounts.retain(|mount| {
        let dest = std::path::Path::new(&mount.dest);
        !identity_dests.contains(&mount.dest)
            && !credential_dirs
                .iter()
                .any(|credential_dir| dest.starts_with(credential_dir))
    });
}

/// Provision a resolved dispatch target before a queue can claim its work.
/// VPN attachment is not yet supported for queued agents, so a requested VPN
/// fails closed instead of entering a sandbox without its required network.
fn provision_dispatch(
    dispatch: AgentDispatch,
    provision: &dyn Fn(&thegn_core::sandbox::SandboxSpec) -> Result<Option<String>, String>,
) -> AgentDispatch {
    let (spec, degraded_warning) = match dispatch {
        AgentDispatch::Run(None) => return AgentDispatch::Run(None),
        AgentDispatch::RunDegraded(None, warning) => {
            return AgentDispatch::RunDegraded(None, warning);
        }
        AgentDispatch::InfraHold(reason) => return AgentDispatch::InfraHold(reason),
        AgentDispatch::Run(Some(spec)) => (spec, None),
        AgentDispatch::RunDegraded(Some(spec), warning) => (spec, Some(warning)),
    };

    if spec.vpn.is_some() {
        return AgentDispatch::InfraHold(format!(
            "sandbox `{}` requests a VPN, which queued agent dispatch cannot attach yet (THE-304); held",
            spec.name
        ));
    }
    let provisioning_warning = match provision(&spec) {
        Ok(warning) => warning,
        Err(error) => {
            return AgentDispatch::InfraHold(format!(
                "agent sandbox `{}` could not be provisioned: {error}",
                spec.name
            ));
        }
    };
    match (degraded_warning, provisioning_warning) {
        (Some(warning), Some(provisioning_warning)) => {
            AgentDispatch::RunDegraded(Some(spec), format!("{warning}; {provisioning_warning}"))
        }
        (Some(warning), None) => AgentDispatch::RunDegraded(Some(spec), warning),
        (None, Some(warning)) => AgentDispatch::RunDegraded(Some(spec), warning),
        (None, None) => AgentDispatch::Run(Some(spec)),
    }
}

fn compose_unverified_warning(
    spec: &thegn_core::sandbox::SandboxSpec,
    report: &thegn_core::sandbox_compose::EnsureReport,
) -> Option<String> {
    report.compose_unverified.as_ref().map(|reason| {
        format!(
            "sandbox {}: Compose config could not be inspected ({reason}); protected host binds were not checked; isolation reported as {}",
            spec.backend.label(),
            thegn_core::sandbox_compose::compose_reported_isolation(
                spec.capabilities().isolation,
                report
            )
            .as_str()
        )
    })
}

/// Resolve a queue agent task's sandbox + floor into a dispatch decision. With
/// the opt-in off, the `[sandbox]` floor still applies: a fail-closed floor
/// holds the queue entry, explicit degrade labels `RunDegraded(None, ..)`, and
/// an off floor permits `Run(None)` (host + slice).
/// With the opt-in on, the worktree's sandbox is resolved and its honest class
/// compared against at least the `[sandbox]` floor in force (default:
/// shared-kernel, refusing on a miss), via the pure
/// [`thegn_core::sandbox_floor::queue_agent_gate`]. The queue floor can raise
/// that boundary but cannot weaken it.
/// Before returning a runnable dispatch, this gate ensures the sandbox and
/// proves it can execute a noninteractive preflight command.
pub(crate) fn agent_floor_gate(
    full: &thegn_core::config::Config,
    worktree: &str,
    sandbox_on: bool,
    floor: thegn_core::config::IsolationFloor,
    on_miss: thegn_core::config::OnFloorMiss,
) -> AgentDispatch {
    if !sandbox_on {
        return uncontained_agent_gate(&full.sandbox);
    }
    let effective = effective_agent_sandbox(full, worktree);
    agent_floor_gate_with_effective(full, worktree, floor, on_miss, effective)
}

fn uncontained_agent_gate(sb: &thegn_core::config::SandboxConfig) -> AgentDispatch {
    use thegn_core::config::OnFloorMiss;

    let floor = thegn_core::sandbox_floor::floor_in_force(sb, false);
    if floor.required_rank().is_none() {
        return AgentDispatch::Run(None);
    }
    match thegn_core::sandbox_floor::miss_policy_in_force(sb) {
        OnFloorMiss::Fail => AgentDispatch::InfraHold(format!(
            "queue agent held: agent_sandbox = false would run it on the host, but the [sandbox] \
             isolation floor in force is `{}` and refuses a miss — set the queue's \
             agent_sandbox = true to run it contained, or [sandbox] isolation_floor = \"off\" \
             to allow uncontained queue agents",
            floor.as_str()
        )),
        OnFloorMiss::Degrade => AgentDispatch::RunDegraded(
            None,
            format!(
                "queue agent running uncontained on the host under the explicit [sandbox] isolation floor `{}` (on_floor_miss = \"degrade\")",
                floor.as_str()
            ),
        ),
    }
}

fn effective_agent_sandbox(
    full: &thegn_core::config::Config,
    worktree: &str,
) -> Result<thegn_core::config::SandboxConfig, String> {
    let worktree_path = std::path::Path::new(worktree);
    let repo_root = crate::merge_ops::repo_root_of(worktree_path)
        .ok_or_else(|| format!("could not resolve the repository root for {worktree}"))?;
    effective_agent_sandbox_at(full, &repo_root, worktree_path)
}

fn effective_agent_sandbox_at(
    full: &thegn_core::config::Config,
    repo_root: &std::path::Path,
    worktree: &std::path::Path,
) -> Result<thegn_core::config::SandboxConfig, String> {
    if let Some(parse_error) = thegn_core::config::repo_overlay_parse_error(repo_root) {
        return Err(format!(
            "{} failed to parse; refusing to run the queued agent without the repo's sandbox policy",
            parse_error.path.display()
        ));
    }
    let loc = thegn_core::remote::GitLoc::Local(worktree.to_path_buf());
    let env = full.resolve_env(repo_root, &loc, worktree, None);
    if env.unresolved_selection || env.is_remote() {
        return Err(format!(
            "queued agents run only on the local host; the effective env `{}` is remote/unresolved",
            env.name
        ));
    }
    Ok(env.sandbox)
}

fn agent_floor_gate_with_effective(
    full: &thegn_core::config::Config,
    worktree: &str,
    floor: thegn_core::config::IsolationFloor,
    on_miss: thegn_core::config::OnFloorMiss,
    effective: Result<thegn_core::config::SandboxConfig, String>,
) -> AgentDispatch {
    use thegn_core::sandbox_floor::{AgentGate, queue_agent_gate};
    let effective = match effective {
        Ok(effective) => effective,
        Err(reason) => return AgentDispatch::InfraHold(reason),
    };
    let effective = credential_free_sandbox(effective);
    let loc = thegn_core::remote::GitLoc::Local(std::path::PathBuf::from(worktree));
    let name = std::path::Path::new(worktree)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("agent-task");
    // `None` ⇒ the sandbox couldn't be established (disabled, or the chain
    // resolved to the host) — a broken boundary under a demanded floor.
    let spec = thegn_core::sandbox::resolve(&effective, &loc, name)
        .map(|spec| scope_agent_spec(spec, worktree));
    // THE-215: a sealed profile whose sandbox could not be established must not
    // run the agent on the host — no spec skips the home gate below, so refuse.
    if spec.is_none() && (full.sandbox.profile.hides_home() || effective.profile.hides_home()) {
        let profile = if full.sandbox.profile.hides_home() {
            full.sandbox.profile
        } else {
            effective.profile
        };
        return AgentDispatch::InfraHold(format!(
            "profile `{}` requires a sandbox that hides $HOME, but none could be established \
             for {worktree}",
            profile.as_str()
        ));
    }
    let Some(spec) = spec else {
        return AgentDispatch::InfraHold(format!(
            "no sandbox could be established for the queued agent in {worktree} (backend `{}`); autonomous agents never fall back to the host — start a container runtime (see `thegn doctor`), or set the queue's agent_sandbox = false with [sandbox] isolation_floor = \"off\" to run it on the host credential-free",
            effective.backend.as_str()
        ));
    };
    // THE-215: a sealed launch that would expose the host `$HOME` is an
    // infrastructure failure under a demanded floor, same as a missed floor.
    if let Some(miss) = thegn_core::sandbox_floor::home_gate(&spec) {
        return AgentDispatch::InfraHold(miss);
    }
    let best = spec.capabilities().isolation;
    let resolved = Some(best);
    let global_gate = queue_agent_gate(&full.sandbox, floor, on_miss, resolved, best);
    let effective_gate = queue_agent_gate(&effective, floor, on_miss, resolved, best);
    let dispatch = match stricter_agent_gate(global_gate, effective_gate) {
        AgentGate::Run => AgentDispatch::Run(Some(spec)),
        AgentGate::RunDegraded(w) => AgentDispatch::RunDegraded(Some(spec), w),
        AgentGate::InfraHold(r) => AgentDispatch::InfraHold(r),
    };
    provision_dispatch(dispatch, &|spec| {
        let report =
            thegn_core::sandbox::ensure_attested(spec, &crate::daemon::compose_guard(&full.daemon))
                .map_err(|error| format!("{error:#}"))?;
        let warning = compose_unverified_warning(spec, &report);
        thegn_core::sandbox_preflight::preflight_exec(spec)?;
        Ok(warning)
    })
}

fn stricter_agent_gate(
    global: thegn_core::sandbox_floor::AgentGate,
    effective: thegn_core::sandbox_floor::AgentGate,
) -> thegn_core::sandbox_floor::AgentGate {
    use thegn_core::sandbox_floor::AgentGate;
    let severity = |gate: &AgentGate| match gate {
        AgentGate::Run => 0,
        AgentGate::RunDegraded(_) => 1,
        AgentGate::InfraHold(_) => 2,
    };
    if severity(&effective) > severity(&global) {
        effective
    } else {
        global
    }
}

/// Run the agent to completion and preserve whether it started, exited, or was interrupted.
///
/// **The exit code is advisory.** Callers decide by re-checking the world (the
/// merge queue re-attempts the fold), because an agent can exit non-zero having
/// committed a good fix, or exit zero having done nothing.
#[cfg(unix)]
pub(crate) fn run_outcome(task: &AgentTaskRun<'_>) -> AgentRunVerdict {
    use std::process::{Command, Stdio};
    use thegn_core::util;

    let (command, kept_prompt_bytes) = match fit_command(
        task.command_template,
        task.prompt,
        task.vars,
    ) {
        Ok(Some(fit)) => fit,
        Ok(None) => {
            tracing::warn!(
                target: "thegn::agent",
                kind = %task.kind,
                prompt_bytes = task.prompt.len(),
                max_command_bytes = MAX_RENDERED_COMMAND_BYTES,
                "agent command exceeds the safe argument limit even without the prompt; not dispatching"
            );
            return AgentRunVerdict::NotStarted(
                "agent command exceeds the safe argument limit even without the prompt".into(),
            );
        }
        Err(e) => {
            // Validation runs at config time; reaching here means a template
            // slipped through, so say so rather than spawning nonsense.
            tracing::warn!(
                target: "thegn::agent",
                kind = %task.kind,
                error = %e,
                "agent command template is invalid; not dispatching"
            );
            return AgentRunVerdict::NotStarted(
                "agent command template is invalid; not dispatching".into(),
            );
        }
    };
    if kept_prompt_bytes < task.prompt.len() {
        tracing::warn!(
            target: "thegn::agent",
            kind = %task.kind,
            original_prompt_bytes = task.prompt.len(),
            kept_prompt_bytes,
            rendered_command_bytes = command.len(),
            "agent prompt was bounded to fit the rendered command; task data omitted"
        );
    }

    // Join the shared aggregate slice, like the fold gate and every interactive
    // pane. A queue handoff runs a coding agent unattended — it must not be the
    // one thing on the box with no ceiling. When the queue opted into the sandbox
    // (floor already cleared by `agent_floor_gate`), run the command INSIDE the
    // resolved sandbox first, then join the slice on top.
    let (inner_argv, _secret_env_file) = match &task.sandbox {
        Some(spec) => match thegn_core::sandbox::enter_argv_guarded(spec, &command) {
            Ok(guarded) => guarded.into_parts(),
            Err(error) => {
                tracing::error!("{error}; refusing queued agent sandbox launch");
                return AgentRunVerdict::NotStarted(format!(
                    "{error}; refusing queued agent sandbox launch"
                ));
            }
        },
        None => (
            vec![util::shell(), "-lc".to_string(), command.clone()],
            None,
        ),
    };
    // Keep a secret env-file until the bounded runner has waited for the child.
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(inner_argv);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(task.worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("THEGN_TASK_KIND", task.kind.as_str())
        .env("THEGN_WORKTREE", task.worktree);
    if let Some((key, value)) = prompt_env(task) {
        cmd.env(key, value);
    }
    if task.credential_free {
        cmd.env_clear();
        for (key, value) in credential_free_env(std::env::vars()) {
            cmd.env(key, value);
        }
        for (key, value) in RESET_CREDENTIAL_HELPER {
            cmd.env(key, value);
        }
        // Re-add the task context after clearing the inherited environment.
        cmd.env("THEGN_TASK_KIND", task.kind.as_str())
            .env("THEGN_WORKTREE", task.worktree);
        if let Some((key, value)) = prompt_env(task) {
            cmd.env(key, value);
        }
    }
    for (k, v) in legacy_env(task) {
        cmd.env(k, v);
    }
    // Defense in depth: the agent's git must target its cwd, not an inherited
    // GIT_DIR/GIT_INDEX_FILE (mirrors task.rs::build_capped_command).
    for var in util::GIT_ENV_VARS {
        cmd.env_remove(var);
    }

    // Own group/job; bounded, drained, and reaped by `bounded::run_bounded`.
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let result = bounded::run_bounded(
        &mut cmd,
        bounded::effective_timeout(task.timeout_secs),
        &NEVER,
    );
    let stderr_tail = {
        let t = &result.stderr.bytes;
        let from = t.len().saturating_sub(2048);
        String::from_utf8_lossy(&t[from..]).into_owned()
    };
    let outcome = &result.outcome;
    let error = match outcome {
        bounded::AgentRunOutcome::Spawn(e) | bounded::AgentRunOutcome::Reap(e) => {
            Some(e.to_string())
        }
        _ => None,
    };
    match outcome {
        bounded::AgentRunOutcome::TimedOut
        | bounded::AgentRunOutcome::Unsettled
        | bounded::AgentRunOutcome::Spawn(_)
        | bounded::AgentRunOutcome::Reap(_) => tracing::warn!(
            target: "thegn::agent",
            kind = %task.kind,
            outcome = ?outcome,
            error = error.as_deref().unwrap_or(""),
            infrastructure = outcome.is_infrastructure(),
            stdout_bytes = result.stdout.total,
            stdout_truncated = result.stdout.truncated,
            stderr_bytes = result.stderr.total,
            stderr_truncated = result.stderr.truncated,
            stderr_tail = %stderr_tail,
            "agent run did not complete cleanly"
        ),
        _ => tracing::debug!(
            target: "thegn::agent",
            kind = %task.kind,
            outcome = ?outcome,
            error = error.as_deref().unwrap_or(""),
            infrastructure = outcome.is_infrastructure(),
            stdout_bytes = result.stdout.total,
            stdout_truncated = result.stdout.truncated,
            stderr_bytes = result.stderr.total,
            stderr_truncated = result.stderr.truncated,
            stderr_tail = %stderr_tail,
            "agent run finished"
        ),
    }
    bounded::verdict(&result.outcome, task.sandbox.is_some())
}

/// Run the agent to completion and report whether it exited zero.
///
/// **The exit code is advisory.** Callers decide by re-checking the world (the
/// merge queue re-attempts the fold), because an agent can exit non-zero having
/// committed a good fix, or exit zero having done nothing.
pub(crate) fn run(task: &AgentTaskRun<'_>) -> bool {
    run_outcome(task).exited_zero()
}

/// Bounded, drained, fully reaped execution of one agent process group (unix).
///
/// `thegn_proc` owns process-group supervision, pipe polling, output retention,
/// and reaping. This adapter keeps the agent-specific limits and outcome model.
#[cfg(unix)]
pub(crate) mod bounded {
    use std::process::Command;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use thegn_proc::{Capture, End, GroupState, Leader, Limits, Run, Stream};

    /// Finite stand-in for `timeout_secs = 0` ("no watchdog"): an unbounded
    /// agent run would hold a queue claim forever.
    pub(crate) const AGENT_CEILING: Duration = Duration::from_secs(6 * 60 * 60);
    /// No configured deadline may exceed this (also keeps `Instant` math safe).
    const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    /// After the leader exits, how long the rest of its group may keep running.
    pub(crate) const QUIESCE_AFTER_LEADER: Duration = Duration::from_secs(15);
    const TERM_GRACE: Duration = Duration::from_secs(2);
    const KILL_SETTLE: Duration = Duration::from_secs(1);
    const READER_GRACE: Duration = Duration::from_millis(500);
    /// Retained output per stream (the tail) and across both streams.
    pub(crate) const RETAIN_PER_STREAM: usize = 32 * 1024;
    pub(crate) const RETAIN_AGGREGATE: usize = 48 * 1024;

    /// Map the configured seconds to the enforced deadline: 0 gets the ceiling.
    pub(crate) fn effective_timeout(secs: u64) -> Duration {
        if secs == 0 {
            AGENT_CEILING
        } else {
            Duration::from_secs(secs).min(MAX_TIMEOUT)
        }
    }

    #[derive(Debug)]
    pub(crate) enum AgentRunOutcome {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
        Unsettled,
        Spawn(std::io::Error),
        Reap(std::io::Error),
    }

    impl AgentRunOutcome {
        pub(crate) fn success(&self) -> bool {
            matches!(self, Self::Exited(status) if status.success())
        }

        /// Not a verdict on the agent's work: the run was cut short, or could
        /// not be supervised. Callers must re-check the world, not blame code.
        pub(crate) fn is_infrastructure(&self) -> bool {
            !matches!(self, Self::Exited(_))
        }
    }

    pub(crate) fn verdict(outcome: &AgentRunOutcome, sandboxed: bool) -> super::AgentRunVerdict {
        use super::AgentRunVerdict;

        match outcome {
            AgentRunOutcome::Spawn(error) => {
                AgentRunVerdict::NotStarted(format!("agent failed to spawn: {error}"))
            }
            AgentRunOutcome::Exited(status) => match status.code() {
                Some(code @ (126 | 127)) => AgentRunVerdict::NotStarted(format!(
                    "agent command could not be executed (exit {code})"
                )),
                Some(125) if sandboxed => AgentRunVerdict::NotStarted(
                    "sandbox engine refused the agent entry (exit 125)".into(),
                ),
                code => AgentRunVerdict::Exited(code),
            },
            AgentRunOutcome::TimedOut => AgentRunVerdict::Interrupted("timed out".into()),
            AgentRunOutcome::Cancelled => AgentRunVerdict::Interrupted("cancelled".into()),
            AgentRunOutcome::Unsettled => {
                AgentRunVerdict::Interrupted("unsettled process group".into())
            }
            AgentRunOutcome::Reap(error) => {
                AgentRunVerdict::Interrupted(format!("could not reap: {error}"))
            }
        }
    }

    #[derive(Debug)]
    pub(crate) struct AgentRun {
        pub outcome: AgentRunOutcome,
        pub stdout: Capture,
        pub stderr: Capture,
    }

    /// Spawn `cmd` in its own group and run it to a bounded, fully reaped end.
    pub(crate) fn run_bounded(
        cmd: &mut Command,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> AgentRun {
        run_bounded_with(cmd, timeout, QUIESCE_AFTER_LEADER, cancel)
    }

    fn run_bounded_with(
        cmd: &mut Command,
        timeout: Duration,
        quiesce: Duration,
        cancel: &AtomicBool,
    ) -> AgentRun {
        let limits = Limits {
            quiesce_after_leader: Some(quiesce),
            term_grace: TERM_GRACE,
            kill_settle: KILL_SETTLE,
            reader_grace: READER_GRACE,
            stdout: Stream::Tail {
                cap: RETAIN_PER_STREAM,
            },
            stderr: Stream::Tail {
                cap: RETAIN_PER_STREAM,
            },
            aggregate_tail_cap: Some(RETAIN_AGGREGATE),
            ..Limits::silent(timeout)
        };
        let run = thegn_proc::run(cmd, &limits, cancel);
        if let End::Spawn(error) = &run.end {
            tracing::warn!(target: "thegn::agent", error = %error, "agent failed to spawn");
        }
        if run.leader_exited_first
            && matches!(run.group, GroupState::Terminated | GroupState::Survivors)
        {
            tracing::warn!(
                target: "thegn::agent",
                "agent exited but its descendants outlived the run; terminating the group"
            );
        }
        classify(run)
    }

    /// Preserve the agent queue's historical distinction between an execution
    /// result and a run that could not be fully supervised.
    fn classify(run: Run) -> AgentRun {
        let Run {
            end,
            group,
            leader,
            stdout,
            stderr,
            ..
        } = run;
        let outcome = match (end, group, leader) {
            (End::Spawn(error), _, _) => AgentRunOutcome::Spawn(error),
            (End::Supervise(error), _, _) => AgentRunOutcome::Reap(error),
            (_, _, Leader::Detached | Leader::NotSpawned) => AgentRunOutcome::Unsettled,
            (_, _, Leader::ReapFailed(error)) => AgentRunOutcome::Reap(error),
            (End::Cancelled, _, Leader::Reaped(_)) => AgentRunOutcome::Cancelled,
            (End::TimedOut, GroupState::Terminated | GroupState::Drained, Leader::Reaped(_)) => {
                AgentRunOutcome::TimedOut
            }
            (End::TimedOut, GroupState::Survivors | GroupState::Unknown, Leader::Reaped(_)) => {
                AgentRunOutcome::Unsettled
            }
            (End::LeaderExited, _, Leader::Reaped(status)) => AgentRunOutcome::Exited(status),
            // Tail streams cannot trigger OutputLimit; treat it as infrastructure, never success.
            (End::OutputLimit(_), _, Leader::Reaped(_)) => AgentRunOutcome::Unsettled,
        };
        AgentRun {
            outcome,
            stdout,
            stderr,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::process::ExitStatusExt;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::Instant;

        fn sh(script: &str) -> Command {
            let mut c = Command::new("sh");
            c.args(["-c", script])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            c
        }

        fn alive(pid: i32) -> bool {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(s) => s
                    .rsplit_once(')')
                    .and_then(|(_, r)| r.split_whitespace().next().map(|st| st != "Z"))
                    .unwrap_or(false),
                // No /proc: fall back to signal 0.
                Err(_) if !std::path::Path::new("/proc/self").exists() => {
                    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
                }
                Err(_) => false,
            }
        }

        fn pid_from(path: &std::path::Path) -> i32 {
            for _ in 0..200 {
                if let Ok(s) = std::fs::read_to_string(path)
                    && let Ok(p) = s.trim().parse()
                {
                    return p;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("pid file never written");
        }

        fn run(script: &str, timeout: Duration) -> AgentRun {
            run_bounded(&mut sh(script), timeout, &AtomicBool::new(false))
        }

        #[test]
        fn verdict_classifies_not_started_and_interrupted_runs() {
            let status = |code| std::process::ExitStatus::from_raw(code << 8);
            for code in [126, 127] {
                assert!(matches!(
                    verdict(&AgentRunOutcome::Exited(status(code)), false),
                    super::super::AgentRunVerdict::NotStarted(_)
                ));
                assert!(matches!(
                    verdict(&AgentRunOutcome::Exited(status(code)), true),
                    super::super::AgentRunVerdict::NotStarted(_)
                ));
            }
            assert!(matches!(
                verdict(&AgentRunOutcome::Exited(status(125)), true),
                super::super::AgentRunVerdict::NotStarted(_)
            ));
            assert_eq!(
                verdict(&AgentRunOutcome::Exited(status(125)), false),
                super::super::AgentRunVerdict::Exited(Some(125))
            );
            assert_eq!(
                verdict(&AgentRunOutcome::Exited(status(0)), false),
                super::super::AgentRunVerdict::Exited(Some(0))
            );
            assert_eq!(
                verdict(&AgentRunOutcome::Exited(status(1)), false),
                super::super::AgentRunVerdict::Exited(Some(1))
            );
            assert!(matches!(
                verdict(&AgentRunOutcome::TimedOut, false),
                super::super::AgentRunVerdict::Interrupted(_)
            ));
            assert!(matches!(
                verdict(&AgentRunOutcome::Cancelled, false),
                super::super::AgentRunVerdict::Interrupted(_)
            ));
            assert!(matches!(
                verdict(&AgentRunOutcome::Unsettled, false),
                super::super::AgentRunVerdict::Interrupted(_)
            ));
            assert!(matches!(
                verdict(&AgentRunOutcome::Reap(std::io::Error::other("x")), false),
                super::super::AgentRunVerdict::Interrupted(_)
            ));
            assert!(matches!(
                verdict(&AgentRunOutcome::Spawn(std::io::Error::other("x")), false),
                super::super::AgentRunVerdict::NotStarted(_)
            ));
        }

        const MIB2: usize = 2 << 20;

        #[test]
        fn stdout_flood_past_the_cap_does_not_block_or_sigpipe() {
            // A SIGPIPE'd writer would make the pipeline fail.
            let r = run(
                &format!("head -c {MIB2} /dev/zero | tr '\\0' x"),
                Duration::from_secs(20),
            );
            assert!(r.outcome.success(), "{:?}", r.outcome);
            assert_eq!(r.stdout.total, MIB2 as u64);
            assert_eq!(r.stdout.bytes.len(), RETAIN_PER_STREAM);
            assert!(r.stdout.truncated);
        }

        #[test]
        fn stderr_flood_past_the_cap_does_not_block() {
            let r = run(
                &format!("head -c {MIB2} /dev/zero | tr '\\0' e >&2"),
                Duration::from_secs(20),
            );
            assert!(r.outcome.success(), "{:?}", r.outcome);
            assert_eq!(r.stderr.total, MIB2 as u64);
            assert!(r.stderr.truncated);
        }

        #[test]
        fn simultaneous_floods_respect_per_stream_and_aggregate_caps() {
            let r = run(
                &format!(
                    "(head -c {MIB2} /dev/zero | tr '\\0' o) & head -c {MIB2} /dev/zero | tr '\\0' e >&2; wait"
                ),
                Duration::from_secs(30),
            );
            assert!(r.outcome.success(), "{:?}", r.outcome);
            assert_eq!(r.stdout.total, MIB2 as u64);
            assert_eq!(r.stderr.total, MIB2 as u64);
            assert!(r.stdout.bytes.len() <= RETAIN_PER_STREAM);
            assert!(r.stderr.bytes.len() <= RETAIN_PER_STREAM);
            assert!(r.stdout.bytes.len() + r.stderr.bytes.len() <= RETAIN_AGGREGATE);
            assert!(r.stdout.truncated && r.stderr.truncated);
        }

        #[test]
        fn small_output_is_retained_untruncated() {
            let r = run("echo hi; echo oops >&2", Duration::from_secs(10));
            assert_eq!(r.stdout.bytes, b"hi\n");
            assert_eq!(r.stderr.bytes, b"oops\n");
            assert!(!r.stdout.truncated && !r.stderr.truncated);
        }

        #[test]
        fn early_exit_with_a_background_descendant_holding_the_pipes_is_bounded() {
            let dir = tempfile::tempdir().unwrap();
            let pidf = dir.path().join("pid");
            let started = Instant::now();
            let r = run_bounded_with(
                &mut sh(&format!("sleep 300 & echo $! > {}; exit 0", pidf.display())),
                Duration::from_secs(60),
                Duration::from_secs(1),
                &AtomicBool::new(false),
            );
            assert!(matches!(r.outcome, AgentRunOutcome::Exited(s) if s.success()));
            assert!(started.elapsed() < Duration::from_secs(1) + Duration::from_secs(6));
            assert!(!alive(pid_from(&pidf)), "descendant survived the run");
        }

        #[test]
        fn leftover_background_child_is_killed_after_the_quiesce_window() {
            let dir = tempfile::tempdir().unwrap();
            let pidf = dir.path().join("pid");
            let started = Instant::now();
            let r = run(
                &format!("sleep 300 & echo $! > {}; exit 0", pidf.display()),
                Duration::from_secs(120),
            );
            assert!(matches!(r.outcome, AgentRunOutcome::Exited(s) if s.success()));
            let took = started.elapsed();
            assert!(
                took >= QUIESCE_AFTER_LEADER - Duration::from_secs(1),
                "{took:?}"
            );
            assert!(
                took < QUIESCE_AFTER_LEADER + Duration::from_secs(6),
                "{took:?}"
            );
            assert!(
                !alive(pid_from(&pidf)),
                "sleeper survived and was not reaped"
            );
        }

        #[test]
        fn term_resistant_descendant_is_killed_and_nothing_lingers() {
            let dir = tempfile::tempdir().unwrap();
            let pidf = dir.path().join("pid");
            let started = Instant::now();
            let r = run_bounded_with(
                &mut sh(&format!(
                    "(trap '' TERM; while :; do sleep 1; done) & echo $! > {}; exit 0",
                    pidf.display()
                )),
                Duration::from_secs(60),
                Duration::from_secs(1),
                &AtomicBool::new(false),
            );
            assert!(matches!(r.outcome, AgentRunOutcome::Exited(_)));
            assert!(started.elapsed() < Duration::from_secs(1) + Duration::from_secs(8));
            assert!(!alive(pid_from(&pidf)));
        }

        #[test]
        fn deadline_with_a_running_leader_is_a_typed_infra_timeout() {
            let started = Instant::now();
            let r = run("sleep 300", Duration::from_millis(300));
            assert!(
                matches!(r.outcome, AgentRunOutcome::TimedOut),
                "{:?}",
                r.outcome
            );
            assert!(r.outcome.is_infrastructure() && !r.outcome.success());
            assert!(started.elapsed() < Duration::from_secs(8));
        }

        #[test]
        fn flooding_writer_that_outlives_the_deadline_is_reaped() {
            let r = run(
                "yes | head -c 100000000; sleep 300",
                Duration::from_millis(500),
            );
            assert!(
                matches!(r.outcome, AgentRunOutcome::TimedOut),
                "{:?}",
                r.outcome
            );
        }

        #[test]
        fn cancellation_during_a_run_kills_the_group() {
            let cancel = Arc::new(AtomicBool::new(false));
            let c2 = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                c2.store(true, Ordering::Release);
            });
            let started = Instant::now();
            let r = run_bounded(
                &mut sh("sleep 300 & wait"),
                Duration::from_secs(60),
                &cancel,
            );
            assert!(
                matches!(r.outcome, AgentRunOutcome::Cancelled),
                "{:?}",
                r.outcome
            );
            assert!(started.elapsed() < Duration::from_secs(8));
        }

        #[test]
        fn zero_timeout_maps_to_a_finite_ceiling() {
            assert_eq!(effective_timeout(0), AGENT_CEILING);
            assert_eq!(effective_timeout(5), Duration::from_secs(5));
            assert_eq!(effective_timeout(u64::MAX), MAX_TIMEOUT);
        }

        #[test]
        fn classify_preserves_the_agent_outcome_mapping() {
            use thegn_proc::{Containment, End, GroupState, Leader, Run, Which};

            #[derive(Clone, Copy)]
            enum Want {
                Spawn,
                Reap,
                Unsettled,
                Cancelled,
                TimedOut,
                Success,
                Failure,
            }

            fn run(end: End, group: GroupState, leader: Leader) -> Run {
                Run {
                    end,
                    group,
                    leader,
                    leader_exited_first: false,
                    stdout: Capture::default(),
                    stderr: Capture::default(),
                    containment: Containment::ProcessGroup,
                }
            }

            let success = std::process::ExitStatus::from_raw(0);
            let failure = std::process::ExitStatus::from_raw(256);
            let cases = [
                (
                    run(
                        End::Spawn(std::io::Error::other("spawn")),
                        GroupState::Unknown,
                        Leader::NotSpawned,
                    ),
                    Want::Spawn,
                ),
                (
                    run(
                        End::Supervise(std::io::Error::other("supervise")),
                        GroupState::Unknown,
                        Leader::Detached,
                    ),
                    Want::Reap,
                ),
                (
                    run(End::LeaderExited, GroupState::Unknown, Leader::Detached),
                    Want::Unsettled,
                ),
                (
                    run(End::LeaderExited, GroupState::Unknown, Leader::NotSpawned),
                    Want::Unsettled,
                ),
                (
                    run(
                        End::LeaderExited,
                        GroupState::Unknown,
                        Leader::ReapFailed(std::io::Error::other("reap")),
                    ),
                    Want::Reap,
                ),
                (
                    run(
                        End::Cancelled,
                        GroupState::Terminated,
                        Leader::Reaped(failure),
                    ),
                    Want::Cancelled,
                ),
                (
                    run(
                        End::TimedOut,
                        GroupState::Terminated,
                        Leader::Reaped(failure),
                    ),
                    Want::TimedOut,
                ),
                (
                    run(End::TimedOut, GroupState::Drained, Leader::Reaped(failure)),
                    Want::TimedOut,
                ),
                (
                    run(
                        End::TimedOut,
                        GroupState::Survivors,
                        Leader::Reaped(failure),
                    ),
                    Want::Unsettled,
                ),
                (
                    run(End::TimedOut, GroupState::Unknown, Leader::Reaped(failure)),
                    Want::Unsettled,
                ),
                (
                    run(
                        End::LeaderExited,
                        GroupState::Terminated,
                        Leader::Reaped(success),
                    ),
                    Want::Success,
                ),
                (
                    run(
                        End::LeaderExited,
                        GroupState::Drained,
                        Leader::Reaped(failure),
                    ),
                    Want::Failure,
                ),
                (
                    run(
                        End::OutputLimit(Which::Stdout),
                        GroupState::Terminated,
                        Leader::Reaped(failure),
                    ),
                    Want::Unsettled,
                ),
            ];

            for (run, expected) in cases {
                let actual = classify(run).outcome;
                match expected {
                    Want::Spawn => assert!(matches!(&actual, AgentRunOutcome::Spawn(_))),
                    Want::Reap => assert!(matches!(&actual, AgentRunOutcome::Reap(_))),
                    Want::Unsettled => assert!(matches!(&actual, AgentRunOutcome::Unsettled)),
                    Want::Cancelled => assert!(matches!(&actual, AgentRunOutcome::Cancelled)),
                    Want::TimedOut => assert!(matches!(&actual, AgentRunOutcome::TimedOut)),
                    Want::Success => assert!(
                        matches!(&actual, AgentRunOutcome::Exited(status) if status.success())
                    ),
                    Want::Failure => assert!(
                        matches!(&actual, AgentRunOutcome::Exited(status) if !status.success())
                    ),
                }
                if matches!(expected, Want::Unsettled) {
                    assert!(actual.is_infrastructure());
                }
            }
        }

        #[test]
        fn spawn_failure_is_typed() {
            let mut c = Command::new("/nonexistent/thegn-agent");
            let r = run_bounded(&mut c, Duration::from_secs(1), &AtomicBool::new(false));
            assert!(matches!(r.outcome, AgentRunOutcome::Spawn(_)));
        }
    }
}

/// Back-compat env for merge kinds. The legacy prompt alias is emitted only
/// when the command template does not consume `{prompt}`.
#[cfg(any(unix, test))]
fn legacy_env(task: &AgentTaskRun<'_>) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(b) = task.vars.get("branch") {
        out.push(("THEGN_BRANCH", b.to_string()));
    }
    if matches!(task.kind, TaskKind::MergeConflict | TaskKind::GateFailure) {
        if prompt_env(task).is_some() {
            out.push(("THEGN_MERGE_PROMPT", task.prompt.to_string()));
        }
        if let Some(t) = task.vars.get("target") {
            out.push(("THEGN_MERGE_TARGET", t.to_string()));
        }
    }
    out
}

/// Keep rendered prose out of argv-backed command templates. When a command
/// does not consume `{prompt}`, expose it once under the generalized task key.
#[cfg(any(unix, test))]
fn prompt_env<'a>(task: &'a AgentTaskRun<'_>) -> Option<(&'static str, &'a str)> {
    if thegn_core::agent_task::template_vars(task.command_template)
        .is_ok_and(|vars| vars.iter().any(|name| name == "prompt"))
    {
        return None;
    }
    Some(("THEGN_TASK_PROMPT", task.prompt))
}

/// Environment passed to an untrusted issue worker. Keep the same harmless
/// infrastructure allowlist used by fresh panes, then remove credential names
/// and locators. XDG_RUNTIME_DIR stays so the outer systemd-run --user scope and
/// rootless podman can reach the user manager; /run/user is not mounted into the
/// sandbox by default. GIT_CONFIG_GLOBAL stays for commit identity; the runner
/// resets git's credential helpers separately.
fn credential_free_env<I>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let resolver_markers = ["THEGN_SANDBOX".to_string()];
    thegn_core::util::filter_host_env(vars, &resolver_markers)
        .into_iter()
        .filter(|(key, _)| !credential_shaped(key) && !credential_locator(key))
        .collect()
}

fn credential_free_sandbox(
    mut sb: thegn_core::config::SandboxConfig,
) -> thegn_core::config::SandboxConfig {
    sb.env_passthrough
        .retain(|name| !credential_shaped(name) && !credential_locator(name));
    sb
}

fn credential_locator(key: &str) -> bool {
    matches!(
        key,
        "GH_CONFIG_DIR" | "GNUPGHOME" | "GIT_SSH_COMMAND" | "GPG_TTY" | "DBUS_SESSION_BUS_ADDRESS"
    ) || (key.starts_with("THEGN_") && key != "THEGN_SANDBOX")
}

fn credential_shaped(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PRIVATE_KEY",
        "_KEY",
        "_API_KEY",
        "_ACCESS_KEY",
        "_AUTH",
        "_CREDENTIAL",
        "_SOCK",
        "_AGENT",
    ]
    .iter()
    .any(|suffix| upper.ends_with(suffix))
        || upper == "GPG_AGENT_INFO"
}

/// Windows stub: the command template is composed with POSIX `sh_quote` and run
/// through `$SHELL -lc`, neither of which maps onto pwsh/cmd. Port the quoting
/// before enabling this path on Windows.
#[cfg(not(unix))]
pub(crate) fn run_outcome(task: &AgentTaskRun<'_>) -> AgentRunVerdict {
    tracing::warn!(
        target: "thegn::agent",
        kind = %task.kind,
        "headless agent runs are not yet supported on Windows"
    );
    AgentRunVerdict::NotStarted(
        "headless agent runs are not yet supported on this platform (THE-235)".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn vars() -> TaskVars {
        TaskVars::new()
            .set("branch", "tg/fix")
            .set("target", "main")
    }

    fn task<'a>(kind: TaskKind, vars: &'a TaskVars) -> AgentTaskRun<'a> {
        AgentTaskRun {
            kind,
            worktree: "/w/fix",
            prompt: "fix it",
            command_template: "claude -p {prompt}",
            vars,
            timeout_secs: 0,
            sandbox: None,
            credential_free: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_outcome_reports_a_missing_command_as_not_started() {
        let worktree = tempfile::tempdir().unwrap();
        let vars = vars();
        let missing = AgentTaskRun {
            worktree: worktree.path().to_str().unwrap(),
            command_template: "thegn-no-such-agent-b62",
            ..task(TaskKind::MergeConflict, &vars)
        };
        assert!(matches!(
            run_outcome(&missing),
            AgentRunVerdict::NotStarted(_)
        ));

        let nonzero = AgentTaskRun {
            command_template: "exit 3",
            ..missing
        };
        assert_eq!(run_outcome(&nonzero), AgentRunVerdict::Exited(Some(3)));
        let successful = AgentTaskRun {
            command_template: "true",
            ..nonzero
        };
        assert_eq!(run_outcome(&successful), AgentRunVerdict::Exited(Some(0)));
        assert!(run(&successful));
    }

    fn sandbox_spec() -> thegn_core::sandbox::SandboxSpec {
        thegn_core::sandbox::SandboxSpec {
            backend: thegn_core::sandbox::Backend::Bwrap,
            placement: thegn_core::placement::Placement::Local,
            image: None,
            worktree: std::path::PathBuf::from("/repo"),
            mounts: Vec::new(),
            env: Vec::new(),
            env_overrides: std::collections::HashMap::new(),
            env_block: Vec::new(),
            network: thegn_core::config::Network::Nat,
            network_allow: Vec::new(),
            network_block: Vec::new(),
            read_only_root: false,
            no_new_privileges: false,
            pids_limit: None,
            drop_capabilities: Vec::new(),
            add_capabilities: Vec::new(),
            file_access: thegn_core::config::FileAccess::Worktree,
            ports: Vec::new(),
            gpu: None,
            limits: thegn_core::sandbox::SandboxLimits::default(),
            volumes: Vec::new(),
            compose: None,
            build: None,
            init_script: None,
            devenv: false,
            devenv_path: None,
            name: "agent-preflight-test".into(),
            vpn: None,
            oci_host: None,
            oci_runtime: None,
            daemon_persistent: false,
            seal_home: None,
            volume_owners: Vec::new(),
        }
    }

    #[test]
    fn queue_agent_sandbox_honours_a_repo_network_none_overlay() {
        let _state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".thegn.toml"),
            "[sandbox]\nnetwork = \"none\"\n",
        )
        .unwrap();
        let full: thegn_core::config::Config = toml::from_str(
            r#"
                [sandbox]
                enabled = true
                backend = "none"
                network = "nat"
            "#,
        )
        .unwrap();

        let effective = effective_agent_sandbox_at(&full, repo.path(), repo.path()).unwrap();
        assert_eq!(effective.network, thegn_core::config::Network::None);
        let dispatch = agent_floor_gate_with_effective(
            &full,
            repo.path().to_str().unwrap(),
            thegn_core::config::IsolationFloor::Off,
            thegn_core::config::OnFloorMiss::Degrade,
            Ok(effective),
        );
        assert!(matches!(
            dispatch,
            AgentDispatch::InfraHold(reason) if reason.contains("never fall back to the host")
        ));
    }

    #[test]
    fn remote_global_default_env_holds_the_queue_agent() {
        let _state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        // The global default selects `x`; the repo hint is redundant here.
        std::fs::write(repo.path().join(".thegn.toml"), "env = \"x\"\n").unwrap();
        let full: thegn_core::config::Config = toml::from_str(
            r#"
                [sandbox]
                default_env = "x"
                [env.x]
                placement = "ssh"
            "#,
        )
        .unwrap();

        let effective = effective_agent_sandbox_at(&full, repo.path(), repo.path());
        let dispatch = agent_floor_gate_with_effective(
            &full,
            repo.path().to_str().unwrap(),
            thegn_core::config::IsolationFloor::Off,
            thegn_core::config::OnFloorMiss::Degrade,
            effective,
        );
        assert!(
            matches!(dispatch, AgentDispatch::InfraHold(reason) if reason.contains("effective env `x` is remote/unresolved"))
        );
    }

    #[test]
    fn global_sealed_profile_holds_when_effective_env_disables_backend() {
        let _state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let full: thegn_core::config::Config = toml::from_str(
            r#"
                [sandbox]
                enabled = true
                backend = "none"
                profile = "sealed"
                isolation_floor = "off"
                default_env = "x"
                [env.x.sandbox]
                backend = "none"
                profile = "open"
            "#,
        )
        .unwrap();
        let effective = effective_agent_sandbox_at(&full, repo.path(), repo.path()).unwrap();
        assert_eq!(effective.profile, thegn_core::config::SandboxProfile::Open);
        assert_eq!(effective.backend, thegn_core::config::SandboxBackend::None);

        let dispatch = agent_floor_gate_with_effective(
            &full,
            repo.path().to_str().unwrap(),
            thegn_core::config::IsolationFloor::Off,
            thegn_core::config::OnFloorMiss::Degrade,
            Ok(effective),
        );
        assert!(
            matches!(dispatch, AgentDispatch::InfraHold(reason) if reason.contains("profile `sealed` requires a sandbox that hides $HOME"))
        );
    }

    #[test]
    fn malformed_repo_overlay_holds_the_queue_agent() {
        let _state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let overlay = repo.path().join(".thegn.toml");
        std::fs::write(&overlay, "[sandbox\nnetwork = \"none\"\n").unwrap();
        let full: thegn_core::config::Config = toml::from_str(
            r#"
                [sandbox]
                enabled = true
                network = "nat"
            "#,
        )
        .unwrap();

        let effective = effective_agent_sandbox_at(&full, repo.path(), repo.path());
        assert!(
            matches!(&effective, Err(reason) if reason.contains("refusing to run the queued agent") && reason.contains(".thegn.toml"))
        );
        let dispatch = agent_floor_gate_with_effective(
            &full,
            repo.path().to_str().unwrap(),
            thegn_core::config::IsolationFloor::Off,
            thegn_core::config::OnFloorMiss::Degrade,
            effective,
        );
        assert!(
            matches!(dispatch, AgentDispatch::InfraHold(reason) if reason.contains(".thegn.toml"))
        );
    }

    #[test]
    fn stricter_of_global_and_effective_floor_wins() {
        use thegn_core::sandbox_floor::AgentGate;

        assert!(matches!(
            stricter_agent_gate(AgentGate::Run, AgentGate::InfraHold("effective".into())),
            AgentGate::InfraHold(reason) if reason == "effective"
        ));
        assert!(matches!(
            stricter_agent_gate(AgentGate::InfraHold("global".into()), AgentGate::Run),
            AgentGate::InfraHold(reason) if reason == "global"
        ));
        assert!(matches!(
            stricter_agent_gate(
                AgentGate::RunDegraded("global".into()),
                AgentGate::RunDegraded("effective".into())
            ),
            AgentGate::RunDegraded(reason) if reason == "global"
        ));
    }

    #[test]
    fn uncontained_queue_agent_is_held_under_the_default_floor() {
        assert!(matches!(
            uncontained_agent_gate(&thegn_core::config::SandboxConfig::default()),
            AgentDispatch::InfraHold(reason)
                if reason.contains("shared-kernel")
                    && reason.contains("isolation_floor = \"off\"")
        ));
        assert!(matches!(
            agent_floor_gate(
                &thegn_core::config::Config::default(),
                "/w/fix",
                false,
                thegn_core::config::IsolationFloor::Off,
                thegn_core::config::OnFloorMiss::Degrade,
            ),
            AgentDispatch::InfraHold(_)
        ));
    }

    #[test]
    fn uncontained_queue_agent_degrades_under_an_explicit_degrade_floor() {
        let sandbox = thegn_core::config::SandboxConfig {
            isolation_floor: Some(thegn_core::config::IsolationFloor::SharedKernel),
            on_floor_miss: thegn_core::config::OnFloorMiss::Degrade,
            ..thegn_core::config::SandboxConfig::default()
        };
        assert!(matches!(
            uncontained_agent_gate(&sandbox),
            AgentDispatch::RunDegraded(None, warning) if warning.contains("uncontained")
        ));
        let full = thegn_core::config::Config {
            sandbox,
            ..thegn_core::config::Config::default()
        };
        assert!(matches!(
            agent_floor_gate(
                &full,
                "/w/fix",
                false,
                thegn_core::config::IsolationFloor::Off,
                thegn_core::config::OnFloorMiss::Fail,
            ),
            AgentDispatch::RunDegraded(None, warning) if warning.contains("uncontained")
        ));
    }

    #[test]
    fn uncontained_queue_agent_runs_when_the_floor_is_off() {
        let explicit_off = thegn_core::config::SandboxConfig {
            isolation_floor: Some(thegn_core::config::IsolationFloor::Off),
            ..thegn_core::config::SandboxConfig::default()
        };
        assert!(matches!(
            uncontained_agent_gate(&explicit_off),
            AgentDispatch::Run(None)
        ));
        let full = thegn_core::config::Config {
            sandbox: explicit_off,
            ..thegn_core::config::Config::default()
        };
        assert!(matches!(
            agent_floor_gate(
                &full,
                "/w/fix",
                false,
                thegn_core::config::IsolationFloor::SharedKernel,
                thegn_core::config::OnFloorMiss::Fail,
            ),
            AgentDispatch::Run(None)
        ));
        let disabled = thegn_core::config::SandboxConfig {
            enabled: false,
            ..thegn_core::config::SandboxConfig::default()
        };
        assert!(matches!(
            uncontained_agent_gate(&disabled),
            AgentDispatch::Run(None)
        ));
        let full = thegn_core::config::Config {
            sandbox: disabled,
            ..thegn_core::config::Config::default()
        };
        assert!(matches!(
            agent_floor_gate(
                &full,
                "/w/fix",
                false,
                thegn_core::config::IsolationFloor::SharedKernel,
                thegn_core::config::OnFloorMiss::Fail,
            ),
            AgentDispatch::Run(None)
        ));
        let host_backend = thegn_core::config::SandboxConfig {
            backend: thegn_core::config::SandboxBackend::None,
            ..thegn_core::config::SandboxConfig::default()
        };
        assert!(matches!(
            uncontained_agent_gate(&host_backend),
            AgentDispatch::Run(None)
        ));
    }

    #[test]
    fn queue_agent_spec_uses_thegn_owned_container_name() {
        let _state = crate::testenv::IsolatedState::new();
        let spec = scope_agent_spec(sandbox_spec(), "/wt/api");
        assert!(spec.name.starts_with(thegn_core::sandbox::CONTAINER_PREFIX));
        assert_eq!(
            spec.name,
            thegn_core::sandbox::container_name("agent-/wt/api")
        );
    }

    #[test]
    fn queue_agent_specs_keep_same_basename_worktrees_distinct_and_live() {
        let _state = crate::testenv::IsolatedState::new();
        let a = scope_agent_spec(sandbox_spec(), "/wt/a/api");
        let b = scope_agent_spec(sandbox_spec(), "/wt/b/api");
        assert_ne!(a.name, b.name);
        assert!(thegn_core::sandbox::identify_orphans(&["/wt/a/api".into()], &[a.name]).is_empty());
    }

    #[test]
    fn provisioning_leaves_host_dispatch_unchanged() {
        let calls = Cell::new(0);
        let dispatch = provision_dispatch(AgentDispatch::Run(None), &|_| {
            calls.set(calls.get() + 1);
            Ok(None)
        });
        assert!(matches!(dispatch, AgentDispatch::Run(None)));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn provisioning_failure_holds_the_named_sandbox() {
        let spec = sandbox_spec();
        let name = spec.name.clone();
        let dispatch = provision_dispatch(AgentDispatch::Run(Some(spec)), &|_| {
            Err("exec probe failed: boom".into())
        });
        match dispatch {
            AgentDispatch::InfraHold(reason) => {
                assert!(reason.contains(&name));
                assert!(reason.contains("boom"));
            }
            _ => panic!("provisioning failure must hold dispatch"),
        }
    }

    #[test]
    fn successful_provisioning_preserves_degraded_warning() {
        let spec = sandbox_spec();
        let calls = Cell::new(0);
        let dispatch =
            provision_dispatch(AgentDispatch::RunDegraded(Some(spec), "w".into()), &|_| {
                calls.set(calls.get() + 1);
                Ok(None)
            });
        assert!(matches!(dispatch, AgentDispatch::RunDegraded(Some(_), warning) if warning == "w"));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn vpn_request_holds_without_provisioning() {
        let mut spec = sandbox_spec();
        let vpn_config = thegn_core::config::VpnConfig {
            provider: thegn_core::config::VpnProviderKind::Tailscale,
            ..Default::default()
        };
        spec.vpn = thegn_core::sandbox::build_vpn_spec(
            &vpn_config,
            &spec.name,
            thegn_core::config::SandboxProfile::Hardened,
        );
        assert!(spec.vpn.is_some());
        let calls = Cell::new(0);
        let dispatch = provision_dispatch(AgentDispatch::Run(Some(spec)), &|_| {
            calls.set(calls.get() + 1);
            Ok(None)
        });
        assert!(matches!(dispatch, AgentDispatch::InfraHold(reason) if reason.contains("VPN")));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn merge_kinds_keep_their_legacy_env_vars() {
        let v = vars();
        for kind in [TaskKind::MergeConflict, TaskKind::GateFailure] {
            let env = legacy_env(&task(kind, &v));
            assert!(env.iter().all(|(k, _)| *k != "THEGN_MERGE_PROMPT"));
            assert_eq!(prompt_env(&task(kind, &v)), None);
            assert_eq!(
                env.iter()
                    .find(|(k, _)| *k == "THEGN_MERGE_TARGET")
                    .map(|(_, v)| v.as_str()),
                Some("main")
            );
            assert_eq!(
                env.iter()
                    .find(|(k, _)| *k == "THEGN_BRANCH")
                    .map(|(_, v)| v.as_str()),
                Some("tg/fix")
            );
        }
    }

    #[test]
    fn a_task_without_a_target_omits_the_target_var() {
        let v = TaskVars::new().set("branch", "tg/fix");
        let env = legacy_env(&task(TaskKind::MergeConflict, &v));
        assert!(env.iter().all(|(k, _)| *k != "THEGN_MERGE_TARGET"));
    }

    #[test]
    fn prompt_uses_command_or_environment_and_keeps_legacy_merge_alias() {
        let v = vars();
        let command_task = task(TaskKind::MergeConflict, &v);
        assert_eq!(prompt_env(&command_task), None);
        assert!(
            legacy_env(&command_task)
                .iter()
                .all(|(key, _)| *key != "THEGN_MERGE_PROMPT")
        );

        let mut env_task = task(TaskKind::MergeConflict, &v);
        env_task.command_template = "agent --noninteractive";
        assert_eq!(prompt_env(&env_task), Some(("THEGN_TASK_PROMPT", "fix it")));
        assert_eq!(
            legacy_env(&env_task)
                .iter()
                .find(|(key, _)| *key == "THEGN_MERGE_PROMPT")
                .map(|(_, value)| value.as_str()),
            Some("fix it")
        );
    }

    #[test]
    fn quote_heavy_prompt_is_bounded_to_fit_rendered_command_limit() {
        let prompt = "'".repeat(40 * 1024);
        let (command, kept_prompt_bytes) =
            fit_command("agent -p {prompt}", &prompt, &TaskVars::new())
                .unwrap()
                .unwrap();
        assert!(command.len() <= MAX_RENDERED_COMMAND_BYTES);
        assert!(command.contains("task data omitted"));
        assert!(kept_prompt_bytes < prompt.len());
    }

    #[test]
    fn fitting_short_prompt_preserves_exact_substitution() {
        let prompt = "short prompt";
        assert_eq!(
            fit_command("agent -p {prompt}", prompt, &TaskVars::new()).unwrap(),
            Some((
                thegn_core::agent_task::substitute_command(
                    "agent -p {prompt}",
                    prompt,
                    &TaskVars::new()
                )
                .unwrap(),
                prompt.len()
            ))
        );
    }

    #[test]
    fn oversized_non_prompt_variable_cannot_be_fitted() {
        let vars = TaskVars::new().set("branch", "a".repeat(200 * 1024));
        assert!(
            fit_command("agent --branch {branch} -p {prompt}", "small", &vars)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn credential_free_environment_drops_credential_shaped_variables() {
        let env = credential_free_env([
            ("PATH".into(), "/bin".into()),
            ("GH_TOKEN".into(), "secret".into()),
            ("GITHUB_TOKEN".into(), "secret".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "secret".into()),
            ("SSH_AUTH_SOCK".into(), "/run/agent.sock".into()),
            ("LANG".into(), "C".into()),
        ]);
        assert_eq!(
            env,
            vec![("PATH".into(), "/bin".into()), ("LANG".into(), "C".into())]
        );
    }

    #[test]
    fn credential_free_environment_drops_credential_locators() {
        let env = credential_free_env([
            ("PATH".into(), "/bin".into()),
            ("GH_CONFIG_DIR".into(), "/home/u/.config/gh".into()),
            ("GNUPGHOME".into(), "/home/u/.gnupg".into()),
            ("GIT_SSH_COMMAND".into(), "ssh -i /key".into()),
            ("GPG_TTY".into(), "/dev/pts/1".into()),
            (
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/run/user/1/bus".into(),
            ),
            ("THEGN_DIR".into(), "/home/u/.thegn".into()),
            ("THEGN_PROFILE".into(), "personal".into()),
            ("THEGN_SANDBOX".into(), "1".into()),
            ("XDG_RUNTIME_DIR".into(), "/run/user/1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/tmp/identity".into()),
            ("LANG".into(), "C".into()),
        ]);
        assert_eq!(
            env,
            vec![
                ("PATH".into(), "/bin".into()),
                ("THEGN_SANDBOX".into(), "1".into()),
                ("XDG_RUNTIME_DIR".into(), "/run/user/1".into()),
                ("GIT_CONFIG_GLOBAL".into(), "/tmp/identity".into()),
                ("LANG".into(), "C".into()),
            ]
        );
    }

    #[test]
    fn credential_free_spec_strips_identity_and_credential_dir_mounts() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path();
        let mount = |dest: std::path::PathBuf| thegn_core::sandbox::Mount {
            host: dest.to_string_lossy().into_owned(),
            dest: dest.to_string_lossy().into_owned(),
            ro: true,
            cache: false,
        };
        let identity_dest = "/identity/outside-home".to_string();
        let mut mounts = vec![
            mount(home_path.join(".ssh/id_ed25519")),
            mount(home_path.join(".gnupg")),
            mount(home_path.join(".config/gh")),
            mount(home_path.join(".config/ghx")),
            mount(std::path::PathBuf::from("/worktree")),
            thegn_core::sandbox::Mount {
                host: "/identity/key".into(),
                dest: identity_dest.clone(),
                ro: true,
                cache: false,
            },
        ];
        strip_credential_mounts(&mut mounts, home_path, &[identity_dest]);
        let mut remaining = mounts
            .into_iter()
            .map(|mount| mount.dest)
            .collect::<Vec<_>>();
        remaining.sort();
        let mut expected = vec![
            home_path.join(".config/ghx").to_string_lossy().into_owned(),
            "/worktree".to_string(),
        ];
        expected.sort();
        assert_eq!(remaining, expected);
    }

    #[test]
    fn queue_agent_spec_is_credential_free() {
        let state = crate::testenv::IsolatedState::new();
        let mut spec = sandbox_spec();
        spec.env = vec![
            ("GNUPGHOME".into(), "/gnupg".into()),
            ("GH_CONFIG_DIR".into(), "/gh".into()),
            ("TERM".into(), "xterm".into()),
            ("THEGN_SANDBOX".into(), "1".into()),
        ];
        let ssh_mount = state.root().join(".ssh");
        spec.mounts.push(thegn_core::sandbox::Mount {
            host: ssh_mount.to_string_lossy().into_owned(),
            dest: ssh_mount.to_string_lossy().into_owned(),
            ro: true,
            cache: false,
        });
        let spec = scope_agent_spec(spec, "/wt/api");
        assert!(
            !spec
                .env
                .iter()
                .any(|(key, _)| { matches!(key.as_str(), "GNUPGHOME" | "GH_CONFIG_DIR") })
        );
        assert!(
            spec.env
                .iter()
                .any(|(key, value)| key == "TERM" && value == "xterm")
        );
        assert!(
            spec.env
                .iter()
                .any(|(key, value)| key == "THEGN_SANDBOX" && value == "1")
        );
        for (key, value) in RESET_CREDENTIAL_HELPER {
            assert!(
                spec.env
                    .iter()
                    .any(|(name, existing)| name == key && existing == value)
            );
        }
        assert!(spec.mounts.is_empty());
    }

    #[test]
    fn credential_free_launcher_keeps_xdg_runtime_dir() {
        let env = credential_free_env([("XDG_RUNTIME_DIR".into(), "/run/user/7".into())]);
        assert_eq!(env, vec![("XDG_RUNTIME_DIR".into(), "/run/user/7".into())]);
    }

    #[test]
    fn provisioning_warning_degrades_dispatch_and_merges_existing_warning() {
        let dispatch = provision_dispatch(AgentDispatch::Run(Some(sandbox_spec())), &|_| {
            Ok(Some("w2".into()))
        });
        assert!(
            matches!(dispatch, AgentDispatch::RunDegraded(Some(_), warning) if warning == "w2")
        );

        let dispatch = provision_dispatch(
            AgentDispatch::RunDegraded(Some(sandbox_spec()), "w".into()),
            &|_| Ok(Some("w2".into())),
        );
        assert!(matches!(
            dispatch,
            AgentDispatch::RunDegraded(Some(_), warning) if warning == "w; w2"
        ));

        let dispatch = provision_dispatch(AgentDispatch::Run(Some(sandbox_spec())), &|_| Ok(None));
        assert!(matches!(dispatch, AgentDispatch::Run(Some(_))));
    }

    #[test]
    fn compose_unverified_warning_names_reason_and_reported_isolation() {
        let spec = sandbox_spec();
        let warning = compose_unverified_warning(
            &spec,
            &thegn_core::sandbox_compose::EnsureReport {
                compose_unverified: Some("timed out".into()),
            },
        )
        .unwrap();
        assert!(warning.contains("timed out"));
        assert!(warning.contains("host-process"));
        assert!(
            compose_unverified_warning(
                &spec,
                &thegn_core::sandbox_compose::EnsureReport::default()
            )
            .is_none()
        );
    }

    #[test]
    fn queue_agent_provisioning_does_not_use_guardless_ensure() {
        let production = include_str!("agent_run.rs")
            .split_once("\nmod tests {")
            .map(|(production, _)| production)
            .unwrap();
        assert!(!production.contains(concat!("sandbox::", "ensure(")));
    }

    #[test]
    fn queue_agent_without_a_sandbox_is_held_not_run_on_the_host() {
        let _state = crate::testenv::IsolatedState::new();
        let repo = tempfile::tempdir().unwrap();
        let full: thegn_core::config::Config = toml::from_str(
            r#"
                [sandbox]
                enabled = true
                backend = "none"
                isolation_floor = "off"
            "#,
        )
        .unwrap();
        let effective = effective_agent_sandbox_at(&full, repo.path(), repo.path()).unwrap();
        let dispatch = agent_floor_gate_with_effective(
            &full,
            repo.path().to_str().unwrap(),
            thegn_core::config::IsolationFloor::Off,
            thegn_core::config::OnFloorMiss::Degrade,
            Ok(effective),
        );
        assert!(matches!(
            dispatch,
            AgentDispatch::InfraHold(reason) if reason.contains("never fall back to the host")
        ));
    }

    #[test]
    fn credential_free_sandbox_drops_credential_passthrough() {
        let env =
            credential_free_sandbox(thegn_core::config::SandboxConfig::default()).env_passthrough;
        for name in [
            "SSH_AUTH_SOCK",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "ANTHROPIC_API_KEY",
            "GPG_AGENT_INFO",
        ] {
            assert!(
                !env.iter().any(|candidate| candidate == name),
                "retained {name}"
            );
        }
        for name in ["TERM", "COLORTERM", "GIT_CONFIG_GLOBAL"] {
            assert!(
                env.iter().any(|candidate| candidate == name),
                "dropped {name}"
            );
        }
        for name in ["GNUPGHOME", "GPG_TTY", "GH_CONFIG_DIR"] {
            assert!(
                !env.iter().any(|candidate| candidate == name),
                "retained {name}"
            );
        }
    }

    #[test]
    fn credential_free_sandbox_drops_credential_locators_from_passthrough() {
        let env =
            credential_free_sandbox(thegn_core::config::SandboxConfig::default()).env_passthrough;
        for name in ["GNUPGHOME", "GPG_TTY", "GH_CONFIG_DIR", "GIT_SSH_COMMAND"] {
            assert!(
                !env.iter().any(|candidate| candidate == name),
                "retained {name}"
            );
        }
        for name in ["TERM", "COLORTERM", "GIT_CONFIG_GLOBAL"] {
            assert!(
                env.iter().any(|candidate| candidate == name),
                "dropped {name}"
            );
        }
    }

    #[test]
    fn credential_free_environment_drops_the_gpg_agent_locator() {
        let env = credential_free_env([
            ("GPG_AGENT_INFO".into(), "/run/agent".into()),
            ("PATH".into(), "/bin".into()),
            ("XDG_RUNTIME_DIR".into(), "/run/user/1".into()),
            (
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/run/dbus".into(),
            ),
        ]);
        assert_eq!(
            env,
            vec![
                ("PATH".into(), "/bin".into()),
                ("XDG_RUNTIME_DIR".into(), "/run/user/1".into()),
            ]
        );
    }

    #[test]
    fn queue_handoffs_run_credential_free() {
        let sources = [
            include_str!("merge_driver.rs"),
            include_str!("pr_driver.rs"),
            include_str!("ci_autofix.rs"),
            include_str!("review_handoff_headless.rs"),
            include_str!("review_task_handoff.rs"),
        ];
        for source in sources {
            let production = source
                .split_once("\nmod tests {")
                .or_else(|| source.split_once("\n#[cfg(test)]"))
                .map(|(production, _)| production)
                .unwrap_or(source);
            assert!(production.contains("credential_free: true"));
            assert!(!production.contains("credential_free: false"));
        }
    }
}
