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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thegn_core::agent_task::{TaskKind, TaskVars};

/// The shell that runs a rendered agent command.
///
/// The command template is composed with POSIX `sh_quote`, so it needs a POSIX
/// shell — not "the user's shell". On unix those coincide. On Windows
/// [`thegn_core::util::shell`] resolves pwsh → powershell → COMSPEC, none of
/// which understands `-lc` or POSIX quoting, which is why this path used to be
/// stubbed out entirely.
///
/// It does not need to be: Git for Windows ships `sh.exe`, and
/// [`thegn_core::util::posix_shell`] finds it regardless of `PATH`. The POSIX
/// script then runs unchanged rather than needing a second spelling — the same
/// argument the daemon's session scripts and the merge-queue gate already make.
///
/// `None` means there is genuinely no POSIX shell, and the caller declines
/// rather than spawning something that would mangle the command.
fn agent_shell() -> Option<String> {
    if cfg!(unix) {
        Some(thegn_core::util::shell())
    } else {
        thegn_core::util::posix_shell()
    }
}

/// One dispatch: where to run, what to say, and how long to allow.
///
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
}

/// Run the agent to completion and report whether it exited zero.
///
/// **The exit code is advisory.** Callers decide by re-checking the world (the
/// merge queue re-attempts the fold), because an agent can exit non-zero having
/// committed a good fix, or exit zero having done nothing.
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

    // Two independent fixes that must COMPOSE, not replace each other:
    //
    //  - the shell has to be a POSIX one. The command template is rendered with
    //    `sh_quote`, and on Windows `util::shell()` resolves pwsh/COMSPEC, which
    //    understands neither `-lc` nor POSIX quoting. That is why this whole path
    //    used to be stubbed out there; `agent_shell` picks the `sh.exe` Git for
    //    Windows ships.
    //  - the run has to join the shared aggregate CPU slice, like the fold gate
    //    and every interactive pane. A queue handoff runs a coding agent
    //    unattended, so it must not be the one thing on the box with no ceiling.
    //
    // Taking either side alone would silently undo the other: main's version
    // re-introduces `util::shell()` (breaking Windows again), and mine drops the
    // cap (leaving an unattended agent uncapped).
    let Some(shell) = agent_shell() else {
        tracing::warn!(
            target: "thegn::agent",
            kind = %task.kind,
            "no POSIX shell to run the agent command (install Git for Windows); not dispatching"
        );
        return false;
    };
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(vec![
        shell,
        "-lc".to_string(),
        command.clone(),
    ]);

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
            let _ = o.take(1 << 20).read_to_end(&mut b);
        }
    });
    let err_h = std::thread::spawn(move || {
        if let Some(e) = stderr {
            let mut b = Vec::new();
            let _ = e.take(1 << 20).read_to_end(&mut b);
        }
    });

    let status = child.wait();
    done.store(true, Ordering::Relaxed);
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let _ = out_h.join();
    let _ = err_h.join();

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

/// Behavioural tests for the runner itself — it really spawns, so these are the
/// only thing that can catch the class of bug that kept this path stubbed on
/// Windows for so long: a shell that cannot parse the command it is handed.
#[cfg(test)]
mod run_tests {
    use super::*;

    /// A throwaway worktree, since the runner's cwd contract is part of what is
    /// under test.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "thegn-agent-run-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d
    }

    fn vars() -> TaskVars {
        TaskVars::new().set("branch", "tg/fix")
    }

    /// Run `template` in `dir` and report success.
    fn run_in(dir: &std::path::Path, template: &str, timeout_secs: u64, v: &TaskVars) -> bool {
        run(&AgentTaskRun {
            kind: TaskKind::MergeConflict,
            worktree: &dir.to_string_lossy(),
            prompt: "fix it",
            command_template: template,
            vars: v,
            timeout_secs,
        })
    }

    #[test]
    fn a_posix_shell_is_resolvable_on_this_platform() {
        // The whole port rests on this. On Windows it is Git for Windows'
        // `sh.exe`; if it ever stops resolving, every test below fails for a
        // reason that has nothing to do with what it is testing.
        assert!(
            agent_shell().is_some(),
            "no POSIX shell resolved — agent dispatch would decline everywhere"
        );
    }

    #[test]
    fn the_agent_runs_and_its_exit_code_is_reported() {
        let d = scratch("exit");
        assert!(
            run_in(&d, "exit 0", 30, &vars()),
            "zero exit must report true"
        );
        assert!(
            !run_in(&d, "exit 3", 30, &vars()),
            "non-zero exit must report false"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_agent_runs_in_its_own_worktree() {
        // The cwd contract: never the canonical checkout. A `git` the agent runs
        // has to land in the work's own tree.
        let d = scratch("cwd");
        assert!(run_in(&d, "pwd > where.txt", 30, &vars()));
        let got = std::fs::read_to_string(d.join("where.txt")).expect("agent wrote in its cwd");
        assert!(!got.trim().is_empty(), "pwd produced nothing");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_task_env_contract_reaches_the_agent() {
        let d = scratch("env");
        assert!(run_in(
            &d,
            "printf '%s|%s|%s' \"$THEGN_TASK_KIND\" \"$THEGN_TASK_PROMPT\" \"$THEGN_BRANCH\" > env.txt",
            30,
            &vars()
        ));
        let got = std::fs::read_to_string(d.join("env.txt")).expect("env.txt");
        assert_eq!(
            got,
            format!("{}|fix it|tg/fix", TaskKind::MergeConflict.as_str()),
            "the documented env contract did not reach the agent"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_inherited_git_dir_is_scrubbed_before_the_agent_sees_it() {
        // Defense in depth: an inherited GIT_DIR would silently point the
        // agent's `git` at some OTHER repository — the `core.worktree`
        // pollution class this scrub exists for.
        let d = scratch("gitscrub");
        // SAFETY: single-threaded within this test; removed immediately after.
        unsafe { std::env::set_var("GIT_DIR", "/somewhere/else/.git") };
        let ok = run_in(&d, "printf '[%s]' \"$GIT_DIR\" > git.txt", 30, &vars());
        // SAFETY: same.
        unsafe { std::env::remove_var("GIT_DIR") };
        assert!(ok);
        assert_eq!(
            std::fs::read_to_string(d.join("git.txt")).expect("git.txt"),
            "[]",
            "GIT_DIR leaked into the agent"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_invalid_template_declines_instead_of_spawning() {
        let d = scratch("badtpl");
        // An unknown placeholder must not reach a shell as literal text.
        assert!(
            !run_in(&d, "claude -p {nope}", 30, &vars()),
            "an invalid template must not dispatch"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_prompt_is_quoted_rather_than_interpreted() {
        // The rendered command goes through a shell, so a prompt containing
        // shell metacharacters must arrive as TEXT. Unquoted, `; touch pwned`
        // would run as its own command — the injection this quoting prevents.
        let d = scratch("quote");
        let v = TaskVars::new();
        let ok = run(&AgentTaskRun {
            kind: TaskKind::MergeConflict,
            worktree: &d.to_string_lossy(),
            prompt: "boom; touch pwned",
            command_template: "printf '%s' {prompt} > got.txt",
            vars: &v,
            timeout_secs: 30,
        });
        assert!(ok, "the command should still run");
        assert!(
            !d.join("pwned").exists(),
            "the prompt was interpreted as shell, not passed as text"
        );
        assert_eq!(
            std::fs::read_to_string(d.join("got.txt")).expect("got.txt"),
            "boom; touch pwned"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_watchdog_kills_an_overrunning_agent() {
        // A hung agent must not pin a queue forever. `false` here is the
        // timeout, not the command's own status.
        let d = scratch("timeout");
        let start = std::time::Instant::now();
        assert!(
            !run_in(&d, "sleep 30", 1, &vars()),
            "an overrunning agent must report failure"
        );
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "the watchdog did not fire: took {:?}",
            start.elapsed()
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
