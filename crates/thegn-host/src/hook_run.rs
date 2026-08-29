//! Off-loop execution of worktree lifecycle hooks.
//!
//! This module is deliberately the only host seam that starts hook processes.
//! Policy and trust decisions live in `thegn_core::hooks`; this file turns a
//! normalized entry into a short-lived, isolated background job.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::hooks::{HookContext, HookSpec};

/// The terminal state of one hook process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRunState {
    Succeeded,
    Failed,
    TimedOut,
}

/// Captured result for one normalized hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    pub command: String,
    pub state: HookRunState,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub log_path: PathBuf,
}

impl HookRunResult {
    pub fn succeeded(&self) -> bool {
        self.state == HookRunState::Succeeded
    }

    pub fn summary(&self) -> String {
        match self.state {
            HookRunState::Succeeded => format!("hook succeeded: {}", self.command),
            HookRunState::Failed => format!("hook failed (exit {:?}): {}", self.code, self.command),
            HookRunState::TimedOut => format!("hook timed out: {}", self.command),
        }
    }

    /// A bounded, line-oriented tail suitable for notifications. Hook output
    /// is untrusted command output, so redact common credential-shaped lines
    /// before putting it in the durable inbox/toast path.
    pub fn failure_tail(&self) -> String {
        failure_tail(&format!("{}\n{}", self.stdout, self.stderr))
    }
}

/// Run one hook. Callers must invoke this from a worker, never from the
/// compositor loop: the CPU wrapper probes the host and the child wait blocks.
#[expect(clippy::disallowed_methods)]
pub fn run(spec: &HookSpec, context: &HookContext, cwd: &Path) -> HookRunResult {
    let log_path = log_path(&context.worktree, context.event);
    let mut child = match spawn(spec, context, cwd) {
        Ok(child) => child,
        Err(error) => {
            let stderr = format!("failed to start hook: {error}");
            append_log(&log_path, context, spec, HookRunState::Failed, "", &stderr);
            return HookRunResult {
                command: spec.command.clone(),
                state: HookRunState::Failed,
                code: None,
                stdout: String::new(),
                stderr,
                log_path,
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = stdout.map(read_pipe);
    let err_thread = stderr.map(read_pipe);
    let deadline = Instant::now() + Duration::from_secs(spec.timeout_secs);
    let mut timed_out = false;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                kill_process_group(&mut child);
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break None,
        }
    };
    let stdout = join_pipe(out_thread);
    let stderr = join_pipe(err_thread);
    let state = if timed_out {
        HookRunState::TimedOut
    } else if status
        .as_ref()
        .is_some_and(std::process::ExitStatus::success)
    {
        HookRunState::Succeeded
    } else {
        HookRunState::Failed
    };
    append_log(&log_path, context, spec, state, &stdout, &stderr);
    HookRunResult {
        command: spec.command.clone(),
        state,
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        log_path,
    }
}

fn spawn(spec: &HookSpec, context: &HookContext, cwd: &Path) -> std::io::Result<Child> {
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(vec![
        "sh".into(),
        "-lc".into(),
        spec.command.clone(),
    ]);
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(thegn_core::util::filter_host_env(std::env::vars(), &[]))
        .envs(context.environment());
    crate::platform::prepare_hook_process_group(&mut command);
    command.spawn()
}

fn read_pipe<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = pipe.read_to_end(&mut output);
        String::from_utf8_lossy(&output).into_owned()
    })
}

fn join_pipe(pipe: Option<std::thread::JoinHandle<String>>) -> String {
    pipe.and_then(|thread| thread.join().ok())
        .unwrap_or_default()
}

fn kill_process_group(child: &mut Child) {
    crate::platform::kill_hook_process_group(child);
}

static LOG_INDICES: OnceLock<Mutex<std::collections::HashMap<(String, String), u64>>> =
    OnceLock::new();

fn log_path(worktree: &str, event: thegn_core::hooks::HookEvent) -> PathBuf {
    let worktree_slug = thegn_core::util::slugify(worktree);
    let slug = if worktree_slug.is_empty() {
        "worktree".to_string()
    } else {
        format!(
            "{}-{}",
            worktree_slug,
            thegn_core::util::short_hash(worktree, 6)
        )
    };
    let event_name = event.as_str().to_string();
    let next = LOG_INDICES
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("hook log index mutex poisoned")
        .entry((slug.clone(), event_name.clone()))
        .or_insert_with(|| {
            let dir = thegn_core::util::xdg_state_home()
                .join("thegn")
                .join("hooks")
                .join(&slug);
            std::fs::read_dir(dir)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter_map(|name| {
                    name.strip_prefix(&format!("{event_name}-"))
                        .and_then(|n| n.strip_suffix(".log"))
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .max()
                .unwrap_or(0)
        });
    *next += 1;
    thegn_core::util::xdg_state_home()
        .join("thegn")
        .join("hooks")
        .join(slug)
        .join(format!("{event_name}-{next}.log"))
}

const FAILURE_TAIL_BYTES: usize = 4096;

fn failure_tail(output: &str) -> String {
    let mut tail = if output.len() > FAILURE_TAIL_BYTES {
        String::from_utf8_lossy(&output.as_bytes()[output.len() - FAILURE_TAIL_BYTES..])
            .into_owned()
    } else {
        output.to_string()
    };
    if let Some(first_newline) = tail.find('\n')
        && output.len() > FAILURE_TAIL_BYTES
    {
        tail.replace_range(..=first_newline, "…\n");
    }
    tail.lines()
        .map(|line| {
            let upper = line.to_ascii_uppercase();
            if ["TOKEN", "SECRET", "PASSWORD", "PRIVATE_KEY"]
                .iter()
                .any(|marker| upper.contains(marker))
            {
                "[redacted hook output]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn append_log(
    path: &Path,
    context: &HookContext,
    spec: &HookSpec,
    state: HookRunState,
    stdout: &str,
    stderr: &str,
) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = format!(
        "event={} state={state:?} command={:?}\nstdout:\n{}\nstderr:\n{}\n",
        context.event.as_str(),
        spec.command,
        stdout,
        stderr
    );
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Run a list in declaration order, stopping only when the configured failure
/// policy blocks the selected execution mode.
pub fn run_all(
    specs: &[HookSpec],
    context: &HookContext,
    cwd: &Path,
    mode: thegn_core::hooks::HookExecutionMode,
) -> Vec<HookRunResult> {
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        let result = run(spec, context, cwd);
        let failed = !result.succeeded();
        results.push(result);
        if failed && spec.blocks_failure(mode) {
            break;
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::hooks::{HookEvent, HookFailure, HookScope};

    fn spec(command: &str, timeout_secs: u64) -> HookSpec {
        HookSpec {
            command: command.into(),
            wait: false,
            timeout_secs,
            on_failure: HookFailure::Warn,
            scope: HookScope::Global,
        }
    }

    fn context() -> HookContext {
        HookContext {
            event: HookEvent::PostCreate,
            repo_root: "/repo".into(),
            worktree: "/worktree".into(),
            branch: "feature".into(),
            workspace: "workspace".into(),
        }
    }

    #[test]
    fn command_uses_curated_context_environment() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &spec("printf '%s' \"$THEGN_EVENT:$THEGN_BRANCH:$GH_TOKEN\"", 2),
            &context(),
            dir.path(),
        );
        assert_eq!(result.state, HookRunState::Succeeded);
        assert_eq!(result.stdout, "post_create:feature:");
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(&spec("sleep 2", 1), &context(), dir.path());
        assert_eq!(result.state, HookRunState::TimedOut);
    }

    #[test]
    fn blocking_failure_stops_the_remaining_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = spec("exit 7", 2);
        first.on_failure = HookFailure::Block;
        let results = run_all(
            &[first, spec("printf unreachable", 2)],
            &context(),
            dir.path(),
            thegn_core::hooks::HookExecutionMode::User,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].state, HookRunState::Failed);
    }

    #[test]
    fn failure_tail_is_bounded_and_redacts_credential_shaped_output() {
        let result = HookRunResult {
            command: "false".into(),
            state: HookRunState::Failed,
            code: Some(1),
            stdout: format!("{}\nAPI_TOKEN=do-not-show", "x".repeat(5000)),
            stderr: String::new(),
            log_path: PathBuf::new(),
        };
        let tail = result.failure_tail();
        assert!(tail.len() <= FAILURE_TAIL_BYTES + 4);
        assert!(!tail.contains("do-not-show"));
        assert!(tail.contains("redacted hook output"));
    }
}
