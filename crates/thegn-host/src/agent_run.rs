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
//! * stdout/stderr drained to EOF on threads (a chatty agent must not deadlock
//!   or SIGPIPE on a full pipe), only a bounded tail retained — this runs off
//!   the compositor;
//! * the inherited git environment scrubbed, so the agent's `git` operates on
//!   its cwd rather than an inherited `GIT_DIR`/`GIT_INDEX_FILE`.
//!
//! Keeping it in one module is what stops a second queue from re-deriving the
//! quoting contract and re-stubbing the Windows path.

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
    /// Deadline for this invocation, in seconds. 0 is not "unbounded": it maps
    /// to a finite ceiling ([`bounded::AGENT_CEILING`]).
    pub timeout_secs: u64,
    /// When `Some`, run the agent command INSIDE this resolved sandbox (the
    /// queue's opt-in isolation floor). `None` keeps the default host + shared
    /// slice posture. The floor decision itself is made by [`agent_floor_gate`]
    /// before this run — a fail-closed miss never reaches here.
    pub sandbox: Option<thegn_core::sandbox::SandboxSpec>,
    /// Run with a credential-free environment. Autopilot issue content is
    /// untrusted, so its worker must not inherit the launcher's credentials.
    /// Queue agents retain the historical credentialed behavior explicitly.
    pub credential_free: bool,
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
    // THE-215: a sealed profile whose sandbox could not be established must not
    // run the agent on the host — no spec skips the home gate below, so refuse.
    if spec.is_none() && full.sandbox.profile.hides_home() {
        return AgentDispatch::InfraHold(format!(
            "profile `{}` requires a sandbox that hides $HOME, but none could be established \
             for {worktree}",
            full.sandbox.profile.as_str()
        ));
    }
    // THE-215: a sealed launch that would expose the host `$HOME` is an
    // infrastructure failure under a demanded floor, same as a missed floor.
    if let Some(miss) = spec.as_ref().and_then(thegn_core::sandbox_floor::home_gate) {
        return AgentDispatch::InfraHold(miss);
    }
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
pub(crate) fn run(task: &AgentTaskRun<'_>) -> bool {
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
        Some(spec) => match thegn_core::sandbox::enter_argv(spec, &command) {
            Ok(argv) => argv,
            Err(error) => {
                tracing::error!("{error}; refusing queued agent sandbox launch");
                return false;
            }
        },
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
    if task.credential_free {
        cmd.env_clear();
        for (key, value) in credential_free_env(std::env::vars()) {
            cmd.env(key, value);
        }
        // Re-add the task context after clearing the inherited environment.
        cmd.env("THEGN_TASK_KIND", task.kind.as_str())
            .env("THEGN_TASK_PROMPT", task.prompt)
            .env("THEGN_WORKTREE", task.worktree);
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
        let t = &result.stderr.tail;
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
    result.outcome.success()
}

/// Bounded, drained, fully reaped execution of one agent process group (unix).
///
/// Generalises the notification-sound approach in
/// [`crate::platform::sound_process`]: the leader is observed with `WNOWAIT`
/// and stays an unreaped zombie (pinning its pid/pgid) until its whole group
/// has quiesced or been killed, so a group signal can never hit a recycled
/// group. On top of that, both pipes are read to EOF for the life of the run,
/// retaining only a bounded tail.
#[cfg(unix)]
pub(crate) mod bounded {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::platform::GroupHandle;
    use crate::platform::sound_process::{POLL_INTERVAL, live_members, next_backoff};

    /// Finite stand-in for `timeout_secs = 0` ("no watchdog"): an unbounded
    /// agent run would hold a queue claim forever.
    pub(crate) const AGENT_CEILING: Duration = Duration::from_secs(6 * 60 * 60);
    /// No configured deadline may exceed this (also keeps `Instant` math safe).
    const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    /// After the LEADER exits, how long the rest of its group may keep running
    /// before it is terminated: a leftover sccache server / watcher / fsmonitor
    /// must not hold the queue claim for the whole timeout. Capped by the
    /// overall deadline. (Non-Linux `live_members` falls back to
    /// `!group.is_empty()`, so there the full window always elapses first.)
    pub(crate) const QUIESCE_AFTER_LEADER: Duration = Duration::from_secs(15);
    /// SIGTERM-to-SIGKILL grace for the group at the deadline.
    const TERM_GRACE: Duration = Duration::from_secs(2);
    /// How long to wait for the group (and the leader) to settle after SIGKILL.
    const KILL_SETTLE: Duration = Duration::from_secs(1);
    /// After the group is gone, how long readers get to hit EOF on their own.
    const READER_GRACE: Duration = Duration::from_millis(500);
    /// Reader wake interval: only bounds how late the stop flag is noticed.
    const READER_POLL_MS: i32 = 100;
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
        /// The direct child exited (its status is advisory) and the group was
        /// quiesced or killed.
        Exited(std::process::ExitStatus),
        /// The deadline passed with the leader still running; group killed.
        TimedOut,
        /// Cancelled (shutdown) mid-run; group killed.
        Cancelled,
        /// Group members survived SIGKILL (e.g. uninterruptible); the leader is
        /// handed to a reaper thread.
        Unsettled,
        Spawn(std::io::Error),
        Reap(std::io::Error),
    }

    impl AgentRunOutcome {
        pub(crate) fn success(&self) -> bool {
            matches!(self, Self::Exited(s) if s.success())
        }
        /// Not a verdict on the agent's work: the run was cut short, or could
        /// not be supervised. Callers must re-check the world, not blame code.
        pub(crate) fn is_infrastructure(&self) -> bool {
            !matches!(self, Self::Exited(_))
        }
    }

    /// Bounded tail of one stream plus truncation metadata.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct Capture {
        pub tail: Vec<u8>,
        /// Every byte read, retained or not.
        pub total: u64,
        pub truncated: bool,
    }

    #[derive(Debug)]
    pub(crate) struct AgentRun {
        pub outcome: AgentRunOutcome,
        pub stdout: Capture,
        pub stderr: Capture,
    }

    impl Capture {
        fn push(&mut self, chunk: &[u8]) {
            self.total += chunk.len() as u64;
            self.tail.extend_from_slice(chunk);
            self.shrink_to(RETAIN_PER_STREAM);
        }
        fn shrink_to(&mut self, max: usize) {
            if self.tail.len() > max {
                let drop = self.tail.len() - max;
                self.tail.drain(..drop);
                self.truncated = true;
            }
        }
    }

    /// Enforce the aggregate cap by trimming the older bytes of the larger tail.
    fn fit_aggregate(out: &mut Capture, err: &mut Capture) {
        while out.tail.len() + err.tail.len() > RETAIN_AGGREGATE {
            let excess = out.tail.len() + err.tail.len() - RETAIN_AGGREGATE;
            let big = if out.tail.len() >= err.tail.len() {
                &mut *out
            } else {
                &mut *err
            };
            let target = big.tail.len().saturating_sub(excess);
            big.shrink_to(target);
        }
    }

    /// Read `src` until EOF (or `stop`), discarding everything but a tail.
    /// Polls so the stop flag still ends it when a descendant that escaped the
    /// group holds the write end open.
    fn drain<R: Read + AsRawFd>(mut src: R, stop: &AtomicBool) -> Capture {
        let mut cap = Capture::default();
        let mut buf = vec![0u8; 64 * 1024];
        let fd = src.as_raw_fd();
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd on an fd owned by `src` for this call.
            let rc = unsafe { libc::poll(&mut pfd, 1, READER_POLL_MS) };
            if rc < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if rc == 0 {
                continue;
            }
            match src.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => cap.push(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        cap
    }

    fn spawn_reader<R: Read + AsRawFd + Send + 'static>(
        src: Option<R>,
        stop: &Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Capture> {
        let stop = stop.clone();
        std::thread::spawn(move || src.map(|s| drain(s, &stop)).unwrap_or_default())
    }

    /// SIGTERM, then SIGKILL after a grace; true when no live member remains.
    fn terminate_group(group: &GroupHandle, pgid: i32) -> bool {
        group.terminate();
        if wait_empty(group, pgid, Instant::now() + TERM_GRACE) {
            return true;
        }
        group.kill();
        wait_empty(group, pgid, Instant::now() + KILL_SETTLE)
    }

    fn wait_empty(group: &GroupHandle, pgid: i32, until: Instant) -> bool {
        let mut backoff = POLL_INTERVAL;
        while live_members(group, pgid) {
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(backoff.min(remaining));
            backoff = next_backoff(backoff, remaining);
        }
        true
    }

    enum Stop {
        LeaderExited,
        Timeout,
        Cancelled,
        Supervise(std::io::Error),
    }

    /// Spawn `cmd` in its own group and run it to a bounded, fully reaped end.
    /// Wall clock is at most `timeout` plus the cleanup bound: the group
    /// TERM/KILL phases (`TERM_GRACE + KILL_SETTLE`), the leader reap
    /// (`KILL_SETTLE`), `READER_GRACE`, and /proc scan polling; realistically
    /// about 5s worst case.
    pub(crate) fn run_bounded(
        cmd: &mut Command,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> AgentRun {
        run_bounded_with(cmd, timeout, QUIESCE_AFTER_LEADER, cancel)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "every wait follows a confirmed exit or a group kill"
    )]
    fn run_bounded_with(
        cmd: &mut Command,
        timeout: Duration,
        quiesce: Duration,
        cancel: &AtomicBool,
    ) -> AgentRun {
        let (mut child, group) = match crate::platform::spawn_grouped(cmd) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(target: "thegn::agent", error = %e, "agent failed to spawn");
                return AgentRun {
                    outcome: AgentRunOutcome::Spawn(e),
                    stdout: Capture::default(),
                    stderr: Capture::default(),
                };
            }
        };
        let pgid = child.id() as i32;
        let stop_readers = Arc::new(AtomicBool::new(false));
        let out_h = spawn_reader(child.stdout.take(), &stop_readers);
        let err_h = spawn_reader(child.stderr.take(), &stop_readers);

        let exec_deadline = Instant::now() + timeout;
        let mut backoff = POLL_INTERVAL;
        let stop = loop {
            if cancel.load(Ordering::Acquire) {
                break Stop::Cancelled;
            }
            match crate::platform::gate_child_exited(&mut child) {
                Ok(true) => break Stop::LeaderExited,
                Ok(false) => {
                    let remaining = exec_deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break Stop::Timeout;
                    }
                    std::thread::sleep(backoff.min(remaining));
                    backoff = next_backoff(backoff, remaining);
                }
                Err(e) => break Stop::Supervise(e),
            }
        };

        // The leader is unreaped (if exited): its pid pins the pgid for the
        // signals below. Drain the group, or terminate it.
        let mut settled = true;
        let mut cancelled_late = false;
        match &stop {
            Stop::LeaderExited => {
                let mut backoff = POLL_INTERVAL;
                let drain_deadline = exec_deadline.min(Instant::now() + quiesce);
                while live_members(&group, pgid) {
                    let remaining = drain_deadline.saturating_duration_since(Instant::now());
                    if cancel.load(Ordering::Acquire) {
                        cancelled_late = true;
                    }
                    if cancelled_late || remaining.is_zero() {
                        tracing::warn!(
                            target: "thegn::agent",
                            "agent exited but its descendants outlived the run; terminating the group"
                        );
                        settled = terminate_group(&group, pgid);
                        break;
                    }
                    std::thread::sleep(backoff.min(remaining));
                    backoff = next_backoff(backoff, remaining);
                }
            }
            // Waiting on the leader failed (ECHILD etc.): the pgid is no longer
            // pinned, so do NOT signal the group; just report the outcome.
            Stop::Supervise(_) => {}
            Stop::Timeout | Stop::Cancelled => settled = terminate_group(&group, pgid),
        }

        // Reap the leader last. After a group SIGKILL it exits promptly; if it
        // somehow does not, a reaper thread owns it so it can never be a zombie.
        let reap_by = Instant::now() + KILL_SETTLE;
        let mut exited = false;
        while Instant::now() < reap_by {
            match crate::platform::gate_child_exited(&mut child) {
                Ok(true) => {
                    exited = true;
                    break;
                }
                Ok(false) => std::thread::sleep(POLL_INTERVAL),
                Err(_) => break,
            }
        }
        let status = if exited {
            Some(child.wait())
        } else {
            // best-effort: the reaper thread's only job is the eventual wait
            let _ = std::thread::spawn(move || child.wait());
            None
        };

        // Readers end at EOF; past the grace the stop flag ends them even when
        // an escaped descendant still holds a pipe.
        let grace_end = Instant::now() + READER_GRACE;
        while !(out_h.is_finished() && err_h.is_finished()) && Instant::now() < grace_end {
            std::thread::sleep(Duration::from_millis(2));
        }
        stop_readers.store(true, Ordering::Release);
        let mut stdout = out_h.join().unwrap_or_default();
        let mut stderr = err_h.join().unwrap_or_default();
        fit_aggregate(&mut stdout, &mut stderr);

        let outcome = match (status, stop) {
            (_, Stop::Supervise(e)) => AgentRunOutcome::Reap(e),
            (None, _) => AgentRunOutcome::Unsettled,
            (Some(Err(e)), _) => AgentRunOutcome::Reap(e),
            (Some(Ok(_)), Stop::Cancelled) => AgentRunOutcome::Cancelled,
            (Some(Ok(_)), Stop::LeaderExited) if cancelled_late => AgentRunOutcome::Cancelled,
            (Some(Ok(_)), Stop::Timeout) if settled => AgentRunOutcome::TimedOut,
            (Some(Ok(_)), Stop::Timeout) => AgentRunOutcome::Unsettled,
            (Some(Ok(s)), Stop::LeaderExited) => AgentRunOutcome::Exited(s),
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
            assert_eq!(r.stdout.tail.len(), RETAIN_PER_STREAM);
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
            assert!(r.stdout.tail.len() <= RETAIN_PER_STREAM);
            assert!(r.stderr.tail.len() <= RETAIN_PER_STREAM);
            assert!(r.stdout.tail.len() + r.stderr.tail.len() <= RETAIN_AGGREGATE);
            assert!(r.stdout.truncated && r.stderr.truncated);
        }

        #[test]
        fn small_output_is_retained_untruncated() {
            let r = run("echo hi; echo oops >&2", Duration::from_secs(10));
            assert_eq!(r.stdout.tail, b"hi\n");
            assert_eq!(r.stderr.tail, b"oops\n");
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
        fn aggregate_cap_trims_the_larger_tail_and_flags_truncation() {
            let mut a = Capture {
                tail: vec![1; RETAIN_PER_STREAM],
                ..Capture::default()
            };
            let mut b = Capture {
                tail: vec![2; RETAIN_PER_STREAM],
                ..Capture::default()
            };
            fit_aggregate(&mut a, &mut b);
            assert!(a.tail.len() + b.tail.len() <= RETAIN_AGGREGATE);
            assert!(a.truncated || b.truncated);
        }

        #[test]
        fn spawn_failure_is_typed() {
            let mut c = Command::new("/nonexistent/thegn-agent");
            let r = run_bounded(&mut c, Duration::from_secs(1), &AtomicBool::new(false));
            assert!(matches!(r.outcome, AgentRunOutcome::Spawn(_)));
        }
    }
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

/// Environment passed to an untrusted issue worker. Keep the same harmless
/// infrastructure allowlist used by fresh panes, then apply the stricter hook
/// boundary's credential-name filter as defense in depth. In particular this
/// removes token/key/secret/password/auth/socket/agent variables even when a
/// future infrastructure allowlist grows.
fn credential_free_env<I>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    thegn_core::util::filter_host_env(vars, &[])
        .into_iter()
        .filter(|(key, _)| !credential_shaped(key))
        .collect()
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
            credential_free: false,
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
}
