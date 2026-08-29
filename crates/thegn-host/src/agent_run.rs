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
//! * its own process group/job, so the watchdog reaps the agent's whole tree;
//! * stdout/stderr drained on threads (a chatty agent must not deadlock on a
//!   full pipe buffer), capped, and discarded — this runs off the compositor;
//! * the inherited git environment scrubbed, so the agent's `git` operates on
//!   its cwd rather than an inherited `GIT_DIR`/`GIT_INDEX_FILE`.
//!
//! Keeping it in one module is what stops a second queue from re-deriving the
//! quoting contract and re-stubbing the Windows path.

// Only the unix runner spawns/waits; the Windows stub needs none of it.
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::time::{Duration, Instant};

use thegn_core::agent_task::{TaskKind, TaskVars};

/// One dispatch: where to run, what to say, and how long to allow.
pub(crate) struct AgentTaskRun<'a> {
    pub kind: TaskKind,
    /// Absolute path of the worktree the agent works in (its cwd).
    pub worktree: &'a str,
    /// The rendered prompt — prose, handed over as an argument and in the env.
    pub prompt: &'a str,
    /// The command template, already resolved (`agent_command`, or an
    /// `[[agents]]` entry's headless form). Placeholders are bare.
    pub command_template: &'a str,
    /// Variables the command template may reference, minus `{prompt}`.
    pub vars: &'a TaskVars,
    /// Watchdog for this invocation, in seconds. 0 disables it.
    pub timeout_secs: u64,
    /// When `Some`, run the agent command INSIDE this resolved sandbox (the
    /// queue's opt-in isolation floor). `None` keeps the default host + shared
    /// slice posture. The floor decision itself is made by [`agent_floor_gate`]
    /// before this run — a fail-closed miss never reaches here.
    pub sandbox: Option<thegn_core::sandbox::SandboxSpec>,
}

/// How a queue's opt-in agent floor resolves for one dispatch — the attribution
/// split. A fail-closed miss (or an unbuildable sandbox under a demanded floor)
/// is [`InfraHold`](AgentDispatch::InfraHold): the queue entry is held, never the
/// branch/PR marked failed (the merge-guard doctrine).
pub(crate) enum AgentDispatch {
    /// Run the task; `Some(spec)` runs it inside that sandbox, `None` on the host.
    Run(Option<thegn_core::sandbox::SandboxSpec>),
    /// Run, but the floor was missed under `degrade` — carry the warning to log.
    RunDegraded(Option<thegn_core::sandbox::SandboxSpec>, String),
    /// Do not run: an infrastructure failure. Hold the entry; never blame the code.
    InfraHold(String),
}

/// Resolve a queue agent task's sandbox + floor into a dispatch decision. With
/// the opt-in off this is always `Run(None)` (host + slice, unchanged). With it
/// on, the worktree's sandbox is resolved and its honest class compared against
/// the demanded floor via the pure [`thegn_core::sandbox_floor::agent_task_gate`].
pub(crate) fn agent_floor_gate(
    full: &thegn_core::config::Config,
    worktree: &str,
    sandbox_on: bool,
    floor: thegn_core::config::IsolationFloor,
    on_miss: thegn_core::config::OnFloorMiss,
) -> AgentDispatch {
    use thegn_core::capabilities::IsolationClass;
    use thegn_core::sandbox_floor::{AgentGate, agent_task_gate};
    if !sandbox_on {
        return AgentDispatch::Run(None);
    }
    let loc = thegn_core::remote::GitLoc::Local(std::path::PathBuf::from(worktree));
    let name = std::path::Path::new(worktree)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("agent-task");
    // `None` ⇒ the sandbox couldn't be established (disabled, or the chain
    // resolved to the host) — a broken boundary under a demanded floor.
    let spec = thegn_core::sandbox::resolve(&full.sandbox, &loc, name);
    let resolved = spec.as_ref().map(|s| s.capabilities().isolation);
    let best = resolved.unwrap_or(IsolationClass::HostProcess);
    match agent_task_gate(true, floor, on_miss, resolved, best) {
        AgentGate::Run => AgentDispatch::Run(spec),
        AgentGate::RunDegraded(w) => AgentDispatch::RunDegraded(spec, w),
        AgentGate::InfraHold(r) => AgentDispatch::InfraHold(r),
    }
}

/// Run the agent to completion and report whether it exited zero.
///
/// **The exit code is advisory.** Callers decide by re-checking the world (the
/// merge queue re-attempts the fold), because an agent can exit non-zero having
/// committed a good fix, or exit zero having done nothing.
#[cfg(unix)]
#[expect(clippy::disallowed_methods)]
pub(crate) fn run(task: &AgentTaskRun<'_>) -> bool {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use thegn_core::util;

    let command = match thegn_core::agent_task::substitute_command(
        task.command_template,
        task.prompt,
        task.vars,
    ) {
        Ok(c) => c,
        Err(e) => {
            // Validation runs at config time; reaching here means a template
            // slipped through, so say so rather than spawning nonsense.
            tracing::warn!(
                target: "thegn::agent",
                kind = %task.kind,
                error = %e,
                "agent command template is invalid; not dispatching"
            );
            return false;
        }
    };

    // Join the shared aggregate slice, like the fold gate and every interactive
    // pane. A queue handoff runs a coding agent unattended — it must not be the
    // one thing on the box with no ceiling. When the queue opted into the sandbox
    // (floor already cleared by `agent_floor_gate`), run the command INSIDE the
    // resolved sandbox first, then join the slice on top.
    let inner_argv = match &task.sandbox {
        Some(spec) => thegn_core::sandbox::enter_argv(spec, &command),
        None => vec![util::shell(), "-lc".to_string(), command.clone()],
    };
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(inner_argv);

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(task.worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("THEGN_TASK_KIND", task.kind.as_str())
        .env("THEGN_TASK_PROMPT", task.prompt)
        .env("THEGN_WORKTREE", task.worktree);
    for (k, v) in legacy_env(task) {
        cmd.env(k, v);
    }
    // Defense in depth: the agent's git must target its cwd, not an inherited
    // GIT_DIR/GIT_INDEX_FILE (mirrors task.rs::build_capped_command).
    for var in util::GIT_ENV_VARS {
        cmd.env_remove(var);
    }

    // Own group/job so the watchdog reaps the agent's whole tree.
    let (mut child, group) = match crate::platform::spawn_grouped(&mut cmd) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "thegn::agent", error = %e, "agent failed to spawn");
            return false;
        }
    };

    // Watchdog: kill the process group if the agent overruns its deadline.
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog = (task.timeout_secs > 0).then(|| {
        let done = done.clone();
        let timed_out = timed_out.clone();
        // Clone (don't move) the group into the thread: on Windows the job is
        // kill-on-close, so the spawner's handle must outlive the child.
        let group = group.clone();
        let deadline = Duration::from_secs(task.timeout_secs);
        std::thread::spawn(move || {
            let end = Instant::now() + deadline;
            while Instant::now() < end {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if !done.load(Ordering::Relaxed) {
                timed_out.store(true, Ordering::Relaxed);
                group.terminate();
            }
        })
    });

    // Drain the pipes so a chatty agent can't deadlock on a full buffer.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        if let Some(o) = stdout {
            let mut b = Vec::new();
            let _ = o.take(1 << 20).read_to_end(&mut b); // best-effort: drain: bounded read of auxiliary output; failure loses the buffer, not the outcome
        }
    });
    let err_h = std::thread::spawn(move || {
        if let Some(e) = stderr {
            let mut b = Vec::new();
            let _ = e.take(1 << 20).read_to_end(&mut b); // best-effort: drain: bounded read of auxiliary output; failure loses the buffer, not the outcome
        }
    });

    let status = child.wait();
    done.store(true, Ordering::Relaxed);
    if let Some(w) = watchdog {
        let _ = w.join(); // best-effort: thread join: a panicked helper loses its output, not the caller
    }
    let _ = out_h.join(); // best-effort: thread join: a panicked helper loses its output, not the caller
    let _ = err_h.join(); // best-effort: thread join: a panicked helper loses its output, not the caller

    if timed_out.load(Ordering::Relaxed) {
        tracing::warn!(target: "thegn::agent", kind = %task.kind, "agent timed out");
        return false;
    }
    status.map(|s| s.success()).unwrap_or(false)
}

/// Back-compat env for the merge kinds. `THEGN_MERGE_PROMPT` / `THEGN_MERGE_TARGET`
/// / `THEGN_BRANCH` predate the generalized `THEGN_TASK_*` contract and are
/// shipped surface someone may script against, so they are kept (deprecated) for
/// the two kinds that already emitted them.
#[cfg(any(unix, test))]
fn legacy_env(task: &AgentTaskRun<'_>) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Some(b) = task.vars.get("branch") {
        out.push(("THEGN_BRANCH", b.to_string()));
    }
    if matches!(task.kind, TaskKind::MergeConflict | TaskKind::GateFailure) {
        out.push(("THEGN_MERGE_PROMPT", task.prompt.to_string()));
        if let Some(t) = task.vars.get("target") {
            out.push(("THEGN_MERGE_TARGET", t.to_string()));
        }
    }
    out
}

/// Windows stub: the command template is composed with POSIX `sh_quote` and run
/// through `$SHELL -lc`, neither of which maps onto pwsh/cmd. Port the quoting
/// before enabling this path on Windows.
#[cfg(not(unix))]
pub(crate) fn run(task: &AgentTaskRun<'_>) -> bool {
    tracing::warn!(
        target: "thegn::agent",
        kind = %task.kind,
        "headless agent runs are not yet supported on Windows"
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn merge_kinds_keep_their_legacy_env_vars() {
        let v = vars();
        for kind in [TaskKind::MergeConflict, TaskKind::GateFailure] {
            let env = legacy_env(&task(kind, &v));
            assert_eq!(
                env.iter()
                    .find(|(k, _)| *k == "THEGN_MERGE_PROMPT")
                    .map(|(_, v)| v.as_str()),
                Some("fix it"),
                "{kind} lost THEGN_MERGE_PROMPT"
            );
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
}
