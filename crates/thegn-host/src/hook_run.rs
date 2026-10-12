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

/// Callers must invoke this from a worker, never from the compositor loop: the
/// CPU wrapper probes the host and the child wait blocks.
#[expect(clippy::disallowed_methods)]
fn run_with_log_root(
    spec: &HookSpec,
    context: &HookContext,
    cwd: &Path,
    log_root: &Path,
) -> HookRunResult {
    let allocation = log_path(log_root, &context.worktree, context.event);
    let (log_path, allocation_error) = match allocation {
        Ok(path) => (path, None),
        Err(error) => (PathBuf::new(), Some(error.to_string())),
    };
    let (mut child, group) = match spawn(spec, context, cwd) {
        Ok(child) => child,
        Err(error) => {
            let stderr = format!("failed to start hook: {error}");
            let log_error = allocation_error.or_else(|| {
                write_log(&log_path, context, HookRunState::Failed, "", &stderr)
                    .err()
                    .map(|error| error.to_string())
            });
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
    let log_error = allocation_error.or_else(|| {
        write_log(&log_path, context, state, &stdout, &stderr)
            .err()
            .map(|error| error.to_string())
    });
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

const HOOK_LOG_KEEP_NEWEST: usize = 20;
const HOOK_LOG_MAX_AGE_SECS: u64 = 14 * 86_400;
const HOOK_LOG_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const HOOK_LOG_PRUNE_INTERVAL_SECS: i64 = 60 * 60;
const HOOK_LOG_ALLOCATION_ATTEMPTS: usize = 1000;
static LAST_LOG_PRUNE: OnceLock<Mutex<std::collections::HashMap<PathBuf, i64>>> = OnceLock::new();
static LAST_LOG_INDEX: OnceLock<Mutex<std::collections::HashMap<(PathBuf, String), u64>>> =
    OnceLock::new();

pub(crate) fn log_root() -> PathBuf {
    let state_home = thegn_core::util::xdg_state_home();
    #[cfg(test)]
    assert_ne!(
        state_home,
        thegn_core::util::home().join(".local/state"),
        "THE-744: hook test resolved the real XDG state home"
    );
    state_home.join("thegn").join("hooks")
}

/// Prune direct hook-log children on the background disk lane. The hourly
/// throttle ensures idle rounds do not repeatedly enumerate the state tree.
pub(crate) fn prune_logs(root: &Path, now_secs: i64) {
    let mut last_run = LAST_LOG_PRUNE
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last_run
        .get(root)
        .is_some_and(|last| now_secs.saturating_sub(*last) < HOOK_LOG_PRUNE_INTERVAL_SECS)
    {
        return;
    }
    last_run.insert(root.to_path_buf(), now_secs);
    drop(last_run);

    prune_logs_with(
        root,
        now_secs,
        HOOK_LOG_KEEP_NEWEST,
        HOOK_LOG_MAX_AGE_SECS,
        HOOK_LOG_MAX_TOTAL_BYTES,
    );
}

fn prune_logs_with(
    root: &Path,
    now_secs: i64,
    keep_newest: usize,
    max_age_secs: u64,
    max_total_bytes: u64,
) {
    let mut emptied_candidates = std::collections::HashSet::new();
    let Ok(root_meta) = std::fs::symlink_metadata(root) else {
        return;
    };
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return;
    }
    let Ok(slugs) = std::fs::read_dir(root) else {
        return;
    };
    for slug in slugs.flatten() {
        let path = slug.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        let mut logs = Vec::new();
        for entry in entries.flatten() {
            let file_path = entry.path();
            let Ok(file_meta) = std::fs::symlink_metadata(&file_path) else {
                continue;
            };
            if !file_meta.is_file()
                || file_meta.file_type().is_symlink()
                || file_path.extension().is_none_or(|ext| ext != "log")
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let mtime_secs = match file_meta.modified() {
                Ok(time) => match time.duration_since(std::time::UNIX_EPOCH) {
                    Ok(age) => age.as_secs(),
                    Err(_) => 0,
                },
                Err(_) => 0,
            };
            logs.push(thegn_core::disk_reclaim::HookLog { name, mtime_secs });
        }
        let remove = thegn_core::disk_reclaim::plan_hook_log_prune(
            &logs,
            keep_newest,
            max_age_secs,
            now_secs.max(0) as u64,
        );
        for name in remove {
            let file_path = path.join(name);
            match std::fs::remove_file(&file_path) {
                Ok(()) => {
                    emptied_candidates.insert(path.clone());
                }
                Err(error) => {
                    tracing::debug!(%error, "could not remove expired hook log");
                }
            }
        }
    }

    let retained = collect_direct_logs(root);
    for path in plan_total_byte_prune(&retained, max_total_bytes) {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    emptied_candidates.insert(parent.to_path_buf());
                }
            }
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "could not remove hook log over total byte cap");
            }
        }
    }

    for path in emptied_candidates {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let is_empty = match std::fs::read_dir(&path) {
            Ok(mut entries) => entries.next().is_none(),
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "could not inspect hook log directory");
                continue;
            }
        };
        if is_empty && let Err(error) = std::fs::remove_dir(&path) {
            tracing::debug!(%error, path = %path.display(), "could not remove empty hook log directory");
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RetainedHookLog {
    path: PathBuf,
    slug: std::ffi::OsString,
    name: std::ffi::OsString,
    mtime_secs: u64,
    bytes: u64,
}

fn collect_direct_logs(root: &Path) -> Vec<RetainedHookLog> {
    let mut logs = Vec::new();
    let Ok(slugs) = std::fs::read_dir(root) else {
        return logs;
    };
    for slug_entry in slugs.flatten() {
        let slug_path = slug_entry.path();
        let Ok(slug_meta) = std::fs::symlink_metadata(&slug_path) else {
            continue;
        };
        if !slug_meta.is_dir() || slug_meta.file_type().is_symlink() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&slug_path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "log") {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            let (Some(slug), Some(name)) = (slug_path.file_name(), path.file_name()) else {
                continue;
            };
            let slug = slug.to_owned();
            let name = name.to_owned();
            let mtime_secs = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |age| age.as_secs());
            logs.push(RetainedHookLog {
                path,
                slug,
                name,
                mtime_secs,
                bytes: meta.len(),
            });
        }
    }
    logs
}

fn plan_total_byte_prune(logs: &[RetainedHookLog], max_total_bytes: u64) -> Vec<PathBuf> {
    let mut oldest_first = logs.to_vec();
    oldest_first.sort_by(|a, b| {
        a.mtime_secs
            .cmp(&b.mtime_secs)
            .then(a.slug.cmp(&b.slug))
            .then(a.name.cmp(&b.name))
    });
    let mut total_bytes = logs.iter().fold(0u128, |total, log| {
        total.saturating_add(u128::from(log.bytes))
    });
    let mut remove = Vec::new();
    for log in oldest_first {
        if total_bytes <= u128::from(max_total_bytes) {
            break;
        }
        total_bytes = total_bytes.saturating_sub(u128::from(log.bytes));
        remove.push(log.path);
    }
    remove
}

fn log_path(
    log_root: &Path,
    worktree: &str,
    event: thegn_core::hooks::HookEvent,
) -> std::io::Result<PathBuf> {
    let label: String = thegn_core::util::slugify(worktree)
        .chars()
        .take(48)
        .collect();
    let label = if label.is_empty() { "worktree" } else { &label };
    let digest = thegn_core::identity::digest_parts(
        b"thegn/hook-log-worktree",
        &[thegn_core::identity::BytePart::new(worktree.as_bytes())],
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string()))?;
    let suffix: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let slug = format!("{label}-{suffix}");
    let dir = log_root.join(slug);
    std::fs::create_dir_all(&dir)?;
    crate::platform::restrict_dir_owner_only_checked(&dir)?;

    let event_name = event.as_str().to_string();
    let key = (dir.clone(), event_name.clone());
    let mut indices = LAST_LOG_INDEX
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut index = match indices.get(&key).copied() {
        Some(last) => last.checked_add(1),
        None => {
            let mut max_index = 0u64;
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if let Some(found) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_prefix(&format!("{event_name}-")))
                    .and_then(|name| name.strip_suffix(".log"))
                    .and_then(|name| name.parse::<u64>().ok())
                {
                    max_index = max_index.max(found);
                }
            }
            max_index.checked_add(1)
        }
    }
    .ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "hook log index space exhausted",
        )
    })?;

    for _ in 0..HOOK_LOG_ALLOCATION_ATTEMPTS {
        let path = dir.join(format!("{event_name}-{index}.log"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                // Keep this reservation even if finalization later fails: reusing a path
                // already returned in a HookRunResult could identify a different run.
                drop(file);
                indices.insert(key, index);
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                index = index.checked_add(1).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "hook log index space exhausted",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("could not allocate hook log after {HOOK_LOG_ALLOCATION_ATTEMPTS} attempts"),
    ))
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

fn write_log(
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
    thegn_core::fsperm::write_owner_only_atomic(path, line.as_bytes())
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
    run_all_with_log_root(specs, context, cwd, mode, &log_root())
}

fn run_all_with_log_root(
    specs: &[HookSpec],
    context: &HookContext,
    cwd: &Path,
    mode: thegn_core::hooks::HookExecutionMode,
    log_root: &Path,
) -> Vec<HookRunResult> {
    let mut results = Vec::with_capacity(specs.len());
    for spec in specs {
        let result = run_with_log_root(spec, context, cwd, log_root);
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

    #[test]
    fn log_root_panics_when_xdg_state_home_is_the_real_default() {
        let real_state_home = thegn_core::util::home()
            .join(".local/state")
            .to_string_lossy()
            .into_owned();
        let _env = crate::testenv::EnvVarGuard::set(&[("XDG_STATE_HOME", &real_state_home)]);
        assert!(std::panic::catch_unwind(log_root).is_err());
    }

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

    fn run_isolated(spec: &HookSpec, context: &HookContext, cwd: &Path) -> HookRunResult {
        run_with_log_root(spec, context, cwd, cwd)
    }

    #[test]
    fn command_uses_curated_context_environment() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_isolated(
            &spec("printf '%s' \"$THEGN_EVENT:$THEGN_BRANCH:$GH_TOKEN\"", 2),
            &context(),
            dir.path(),
        );
        assert_eq!(result.state, HookRunState::Succeeded);
        assert_eq!(result.stdout, "post_create:feature:");
    }

    #[test]
    fn hook_log_uses_injected_root() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut context = context();
        context.worktree = format!("/isolated-hook-log-{}", std::process::id());

        let result = run_with_log_root(
            &spec("printf 'logged'", 2),
            &context,
            cwd.path(),
            dir.path(),
        );

        assert!(result.succeeded());
        assert!(result.log_error.is_none());
        assert!(result.log_path.starts_with(dir.path()));
        let contents = std::fs::read_to_string(&result.log_path).unwrap();
        assert!(contents.contains("stdout:\nlogged"));
    }

    #[test]
    fn hook_log_allocation_retries_files_created_by_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = format!("/isolated-hook-log-race-{}", std::process::id());
        let first = log_path(dir.path(), &worktree, HookEvent::PostCreate).unwrap();
        assert!(first.ends_with("post_create-1.log"));
        let second_process_path = first.with_file_name("post_create-2.log");
        std::fs::write(&second_process_path, b"other process owns this run").unwrap();

        let next = log_path(dir.path(), &worktree, HookEvent::PostCreate).unwrap();
        assert_ne!(next, second_process_path);
        assert!(next.ends_with("post_create-3.log"));
    }

    #[test]
    fn hook_log_allocation_does_not_reuse_pruned_path() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = format!("/isolated-hook-log-prune-{}", std::process::id());
        let first = log_path(dir.path(), &worktree, HookEvent::PostCreate).unwrap();
        std::fs::remove_file(&first).unwrap();

        let next = log_path(dir.path(), &worktree, HookEvent::PostCreate).unwrap();
        assert_ne!(next, first);
        assert!(next.ends_with("post_create-2.log"));
    }

    #[test]
    fn hook_log_directory_component_is_bounded_for_deep_worktree_strings() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = "w".repeat(4 * 1024);
        let path = log_path(dir.path(), &worktree, HookEvent::PostCreate).unwrap();
        let component = path.parent().unwrap().file_name().unwrap();
        assert!(component.to_string_lossy().len() <= 255);

        let overlong_worktree = "w".repeat(thegn_core::identity::MAX_IDENTITY_COMPONENT_BYTES + 1);
        assert!(log_path(dir.path(), &overlong_worktree, HookEvent::PostCreate).is_err());
    }

    #[test]
    fn prune_logs_limits_direct_regular_log_files_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let slug = dir.path().join("worktree-hash");
        std::fs::create_dir_all(slug.join("nested")).unwrap();
        for index in 0..21 {
            std::fs::write(slug.join(format!("post_create-{index}.log")), b"log").unwrap();
        }
        std::fs::write(slug.join("keep.txt"), b"not a log").unwrap();
        std::fs::write(slug.join("nested/old.log"), b"nested").unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 1;
        prune_logs(dir.path(), now);

        let remaining_logs = std::fs::read_dir(&slug)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "log"))
            .count();
        assert_eq!(remaining_logs, 20);
        assert!(slug.join("keep.txt").exists());
        assert!(slug.join("nested/old.log").exists());
    }

    #[test]
    fn prune_logs_applies_total_byte_cap_oldest_first_across_slug_directories() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        let logs = [
            (alpha.join("a.log"), 100),
            (beta.join("b.log"), 101),
            (alpha.join("c.log"), 102),
            (beta.join("d.log"), 103),
            (alpha.join("e.log"), 104),
            (beta.join("f.log"), 105),
        ];
        for (path, modified_secs) in &logs {
            std::fs::write(path, b"0123456789").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(*modified_secs))
                .unwrap();
        }

        prune_logs_with(dir.path(), 200, 20, 14 * 86_400, 35);

        for (path, modified_secs) in &logs {
            assert_eq!(
                path.exists(),
                *modified_secs >= 103,
                "unexpected retained status for {}",
                path.display()
            );
        }
        let total_bytes: u64 = collect_direct_logs(dir.path())
            .iter()
            .map(|log| log.bytes)
            .sum();
        assert!(total_bytes <= 35);
    }

    #[test]
    fn prune_logs_removes_only_slug_directories_emptied_by_this_pass() {
        let dir = tempfile::tempdir().unwrap();
        let emptied = dir.path().join("emptied");
        let already_empty = dir.path().join("already-empty");
        std::fs::create_dir_all(&emptied).unwrap();
        std::fs::create_dir_all(&already_empty).unwrap();
        std::fs::write(emptied.join("old.log"), b"log").unwrap();

        prune_logs_with(dir.path(), 200, 20, 14 * 86_400, 0);

        assert!(!emptied.exists());
        assert!(already_empty.is_dir());
    }

    #[test]
    fn total_byte_prune_breaks_mtime_ties_by_slug_then_name() {
        let logs = vec![
            RetainedHookLog {
                path: PathBuf::from("beta/z.log"),
                slug: "beta".into(),
                name: "z.log".into(),
                mtime_secs: 10,
                bytes: 10,
            },
            RetainedHookLog {
                path: PathBuf::from("alpha/b.log"),
                slug: "alpha".into(),
                name: "b.log".into(),
                mtime_secs: 10,
                bytes: 10,
            },
            RetainedHookLog {
                path: PathBuf::from("alpha/a.log"),
                slug: "alpha".into(),
                name: "a.log".into(),
                mtime_secs: 10,
                bytes: 10,
            },
        ];

        assert_eq!(
            plan_total_byte_prune(&logs, 20),
            vec![PathBuf::from("alpha/a.log")]
        );
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
        let result = run_isolated(&spec("sleep 2", 1), &context(), dir.path());
        assert_eq!(result.state, HookRunState::TimedOut);
    }

    #[test]
    fn maximum_timeout_value_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_isolated(&spec("true", u64::MAX), &context(), dir.path());
        assert_eq!(result.state, HookRunState::Succeeded);
    }

    #[test]
    fn background_descendant_cannot_hold_hook_completion_open() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_isolated(&spec("sleep 2 &", 5), &context(), dir.path());
        assert_eq!(result.state, HookRunState::Succeeded);
        // The marker is emitted only when the bounded drain wins the race
        // against EOF. Its presence proves completion was reported while the
        // descendant still held the inherited writer, without depending on
        // how quickly this test process is scheduled on a loaded machine.
        assert!(
            result.stdout.contains(PIPE_DRAIN_TIMEOUT_MARKER),
            "expected the bounded drain path, got stdout {:?}",
            result.stdout
        );
    }

    #[test]
    fn blocking_failure_stops_the_remaining_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = spec("exit 7", 2);
        first.on_failure = HookFailure::Block;
        let results = run_all_with_log_root(
            &[first, spec("printf unreachable", 2)],
            &context(),
            dir.path(),
            thegn_core::hooks::HookExecutionMode::User,
            dir.path(),
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
        let result = run_isolated(&spec("yes x | head -c 2000000", 2), &context(), dir.path());
        assert_eq!(result.state, HookRunState::Succeeded);
        assert!(result.stdout.len() <= MAX_CAPTURE_BYTES + "\n[hook output truncated]".len());
        assert!(result.stdout.contains("[hook output truncated]"));
    }

    #[test]
    fn hook_log_redacts_command_output_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks").join("post-create.log");
        write_log(
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
        crate::platform::test_assert_owner_only_permissions(&path, path.parent().unwrap());
    }

    #[test]
    fn hook_log_write_failure_is_returned_without_hiding_hook_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-file");
        std::fs::create_dir(&path).unwrap();
        let error = write_log(&path, &context(), HookRunState::Failed, "output", "error")
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

    #[test]
    fn hook_log_atomic_write_replaces_stale_reserved_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path(), "/repo/worktree", HookEvent::PostCreate).unwrap();
        std::fs::write(&path, b"stale").unwrap();

        write_log(&path, &context(), HookRunState::Succeeded, "fresh", "").unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert!(!contents.contains("stale"));
        assert!(contents.contains("stdout:\nfresh"));
    }

    #[test]
    fn hook_log_atomic_write_does_not_write_through_reserved_hard_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_path(dir.path(), "/repo/worktree", HookEvent::PostCreate).unwrap();
        let other = path.with_file_name("other.log");
        std::fs::hard_link(&path, &other).unwrap();

        write_log(&path, &context(), HookRunState::Succeeded, "fresh", "").unwrap();

        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("stdout:\nfresh")
        );
        assert!(std::fs::read(&other).unwrap().is_empty());
        crate::platform::test_assert_owner_only_permissions(&path, path.parent().unwrap());
    }
}
