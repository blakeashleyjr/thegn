//! Off-loop execution of worktree lifecycle hooks.
//!
//! This module is deliberately the only host seam that starts hook processes.
//! Policy and trust decisions live in `thegn_core::hooks`; this file turns a
//! normalized entry into a short-lived, isolated background job.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
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
    /// Failure to persist the diagnostic log is reported separately from the
    /// hook process result so it never masks the primary outcome.
    pub log_error: Option<String>,
}

impl HookRunResult {
    pub fn succeeded(&self) -> bool {
        self.state == HookRunState::Succeeded
    }

    pub fn summary(&self) -> String {
        let primary = match self.state {
            // Do not echo the shell command: it is user/repository supplied
            // data and may contain a literal secret. The detailed result keeps
            // it for internal callers, while notifications stay safe.
            HookRunState::Succeeded => "hook succeeded".to_string(),
            HookRunState::Failed => format!("hook failed (exit {:?})", self.code),
            HookRunState::TimedOut => "hook timed out".to_string(),
        };
        match &self.log_error {
            Some(error) => format!("{primary}; hook log unavailable: {error}"),
            None => primary,
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
    let (mut child, group) = match spawn(spec, context, cwd) {
        Ok(child) => child,
        Err(error) => {
            let stderr = format!("failed to start hook: {error}");
            let log_error = append_log(&log_path, context, HookRunState::Failed, "", &stderr)
                .err()
                .map(|error| error.to_string());
            return HookRunResult {
                command: spec.command.clone(),
                state: HookRunState::Failed,
                code: None,
                stdout: String::new(),
                stderr,
                log_path,
                log_error,
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_thread = stdout.map(read_pipe);
    let err_thread = stderr.map(read_pipe);
    // Do not construct an absolute Instant from untrusted config: a valid
    // `u64` timeout can exceed the representable range of `Instant` and panic
    // here. Elapsed-time comparison keeps the timeout bounded without an
    // overflow edge.
    let started = Instant::now();
    let timeout = Duration::from_secs(spec.timeout_secs);
    let mut timed_out = false;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                timed_out = true;
                group.kill();
                // Reap the direct child after the group termination request.
                break child.wait().ok();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                // A wait error must not leave the child (or its descendants)
                // running after the lifecycle operation has lost ownership of
                // it. Kill the group and reap the direct child before leaving.
                group.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // A successful direct shell may leave a background descendant holding one
    // of these pipe writers open. Give both readers one shared, bounded grace
    // period, then snapshot what they captured and let any straggling reader
    // finish detached. Hook completion must not inherit a descendant's lifetime.
    let pipe_deadline = Instant::now() + PIPE_DRAIN_GRACE;
    let stdout = join_pipe(out_thread, pipe_deadline);
    let stderr = join_pipe(err_thread, pipe_deadline);
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
    let log_error = append_log(&log_path, context, state, &stdout, &stderr)
        .err()
        .map(|error| error.to_string());
    HookRunResult {
        command: spec.command.clone(),
        state,
        code: status.and_then(|s| s.code()),
        stdout,
        stderr,
        log_path,
        log_error,
    }
}

fn spawn(
    spec: &HookSpec,
    context: &HookContext,
    cwd: &Path,
) -> std::io::Result<(Child, crate::platform::GroupHandle)> {
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
        .envs(hook_base_env(std::env::vars()))
        .envs(context.environment());
    // The platform seam creates a real process group on Unix and assigns the
    // child to a kill-on-close Job Object on Windows. Keeping the handle alive
    // through wait is what makes timeout cleanup cover grandchildren too.
    crate::platform::spawn_grouped(&mut command)
}

/// Build the non-context portion of a hook environment. The general pane
/// allowlist admits `THEGN_*` for internal pane markers, but repository hooks
/// are an untrusted boundary: inherited values must not shadow the five
/// context variables installed below or carry a credential-shaped name.
fn hook_base_env<I>(vars: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    thegn_core::util::filter_host_env(vars, &[])
        .into_iter()
        .filter(|(key, _)| !key.starts_with("THEGN_") && !credential_shaped(key))
        .collect()
}

fn credential_shaped(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PRIVATE_KEY",
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

const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(250);
const PIPE_DRAIN_TIMEOUT_MARKER: &str = "\n[hook output pipe remained open]";

#[derive(Default)]
struct PipeCapture {
    output: Vec<u8>,
    truncated: bool,
}

impl PipeCapture {
    fn push(&mut self, chunk: &[u8]) {
        if self.output.len() < MAX_CAPTURE_BYTES {
            let keep = chunk.len().min(MAX_CAPTURE_BYTES - self.output.len());
            self.output.extend_from_slice(&chunk[..keep]);
            self.truncated |= keep < chunk.len();
        } else {
            self.truncated = true;
        }
    }

    fn text(&self, drain_timed_out: bool) -> String {
        let mut text = String::from_utf8_lossy(&self.output).into_owned();
        if self.truncated {
            text.push_str("\n[hook output truncated]");
        }
        if drain_timed_out {
            text.push_str(PIPE_DRAIN_TIMEOUT_MARKER);
        }
        text
    }
}

struct PipeReader {
    captured: Arc<Mutex<PipeCapture>>,
    finished: std::sync::mpsc::Receiver<()>,
}

fn read_pipe<R: Read + Send + 'static>(mut pipe: R) -> PipeReader {
    let captured = Arc::new(Mutex::new(PipeCapture::default()));
    let worker_capture = Arc::clone(&captured);
    let (finished_tx, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    worker_capture
                        .lock()
                        .expect("hook pipe capture mutex poisoned")
                        .push(&chunk[..count]);
                }
                Err(_) => break,
            }
        }
        let _ = finished_tx.send(()); // best-effort: the runner may have timed out its drain
    });
    PipeReader { captured, finished }
}

fn join_pipe(pipe: Option<PipeReader>, deadline: Instant) -> String {
    let Some(pipe) = pipe else {
        return String::new();
    };
    let finished = pipe
        .finished
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .is_ok();
    pipe.captured
        .lock()
        .expect("hook pipe capture mutex poisoned")
        .text(!finished)
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
    let mut indices = LOG_INDICES
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .expect("hook log index mutex poisoned");
    let next = indices
        .get(&(slug.clone(), event_name.clone()))
        .copied()
        .unwrap_or_else(|| {
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
    let next = next + 1;
    indices.insert((slug.clone(), event_name.clone()), next);
    drop(indices);
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
            if [
                "TOKEN",
                "SECRET",
                "PASSWORD",
                "PRIVATE_KEY",
                "API_KEY",
                "ACCESS_KEY",
                "_AUTH",
                "BEARER ",
                "CREDENTIAL",
            ]
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
    state: HookRunState,
    stdout: &str,
    stderr: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::platform::restrict_dir_owner_only_checked(parent)?;
    }
    let line = format!(
        "event={} state={state:?} command=[redacted]\nstdout:\n{}\nstderr:\n{}\n",
        context.event.as_str(),
        redact_output(stdout),
        redact_output(stderr),
    );
    use std::io::Write;
    let mut file = crate::platform::append_private_file(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn redact_output(output: &str) -> String {
    output
        .lines()
        .map(|line| {
            let upper = line.to_ascii_uppercase();
            if [
                "TOKEN",
                "SECRET",
                "PASSWORD",
                "PRIVATE_KEY",
                "API_KEY",
                "ACCESS_KEY",
                "_AUTH",
                "BEARER ",
                "CREDENTIAL",
            ]
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
    fn hook_base_env_excludes_inherited_context_and_credentials() {
        let env = hook_base_env([
            ("PATH".into(), "/bin".into()),
            ("THEGN_INBOX_SECRET".into(), "secret".into()),
            ("THEGN_API_KEY".into(), "secret".into()),
            ("GH_TOKEN".into(), "secret".into()),
            ("SSH_AUTH_SOCK".into(), "/tmp/agent.sock".into()),
            ("THEGN_SAFE_MARKER".into(), "must-not-inherit".into()),
        ]);
        assert_eq!(env, vec![("PATH".to_string(), "/bin".to_string())]);
        assert!(credential_shaped("SERVICE_API_KEY"));
        assert!(credential_shaped("OIDC_AUTH"));
    }

    #[test]
    fn timeout_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(&spec("sleep 2", 1), &context(), dir.path());
        assert_eq!(result.state, HookRunState::TimedOut);
    }

    #[test]
    fn maximum_timeout_value_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(&spec("true", u64::MAX), &context(), dir.path());
        assert_eq!(result.state, HookRunState::Succeeded);
    }

    #[test]
    fn background_descendant_cannot_hold_hook_completion_open() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = run(&spec("printf ready; sleep 2 &", 5), &context(), dir.path());
        assert_eq!(result.state, HookRunState::Succeeded);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "inherited pipe delayed completion for {:?}",
            started.elapsed()
        );
        assert!(result.stdout.starts_with("ready"));
        assert!(result.stdout.contains(PIPE_DRAIN_TIMEOUT_MARKER));
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
            log_error: None,
        };
        let tail = result.failure_tail();
        assert!(tail.len() <= FAILURE_TAIL_BYTES + 4);
        assert!(!tail.contains("do-not-show"));
        assert!(tail.contains("redacted hook output"));
    }

    #[test]
    fn hook_output_is_capped_while_the_pipe_is_drained() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(&spec("yes x | head -c 2000000", 2), &context(), dir.path());
        assert_eq!(result.state, HookRunState::Succeeded);
        assert!(result.stdout.len() <= MAX_CAPTURE_BYTES + "\n[hook output truncated]".len());
        assert!(result.stdout.contains("[hook output truncated]"));
    }

    #[test]
    fn hook_log_redacts_command_output_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks").join("post-create.log");
        append_log(
            &path,
            &context(),
            HookRunState::Failed,
            "ACCESS_KEY=do-not-write",
            "Bearer do-not-write",
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("command=[redacted]"));
        assert!(!contents.contains("do-not-write"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn hook_log_write_failure_is_returned_without_hiding_hook_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-file");
        std::fs::create_dir(&path).unwrap();
        let error = append_log(&path, &context(), HookRunState::Failed, "output", "error")
            .expect_err("a directory cannot be opened as an append log");
        assert!(!error.to_string().is_empty());

        let result = HookRunResult {
            command: "false".into(),
            state: HookRunState::Failed,
            code: Some(1),
            stdout: String::new(),
            stderr: String::new(),
            log_path: path,
            log_error: Some(error.to_string()),
        };
        assert!(!result.succeeded());
        assert!(result.summary().contains("hook failed"));
        assert!(result.summary().contains("hook log unavailable"));
    }
}
