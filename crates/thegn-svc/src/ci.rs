//! CI/CD provider seam (AV group). The cross-provider sibling of
//! [`crate::forge`]: a [`CiProvider`] trait normalizing run history / job-step
//! drilldown / logs / trigger·rerun·cancel onto thegn-core's
//! [`thegn_core::ci`] model, with per-provider impls that degrade
//! native→CLI→error just like the forge's `GithubNative`→`GithubCli` ladder
//! ("a gap is slower or unavailable, never broken").
//!
//! Phase A ships GitHub Actions (via the `gh` CLI, reusing the user's `gh`
//! auth) and GitLab CI (via `glab` + the GitLab API), routed as a
//! `Box<dyn CiProvider>` ([`CiClient`]).
//!
//! Every subprocess call is blocking; callers invoke these from a
//! `spawn_blocking` task (the host's hydration seam), exactly as the `gh`
//! backend is driven.

use thegn_core::ci::{
    CiCaps, CiError, CiJob, CiLog, CiRun, CiState, CiStep, CiSystem, CiWorkflow, RerunScope,
    classify_stderr,
};
use thegn_core::config::{CiConfig, CiProviderKind};
use thegn_core::remote::GitLoc;
use thegn_core::secretref::BareAs;

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

/// Provider-side bounds are deliberately finite even before the host applies
/// the user's cache policy.  This protects the blocking provider lane from a
/// job that emits an unbounded log (the configured, usually smaller, bounds
/// are applied again by the cache-facing read paths).
const PROVIDER_LOG_MAX_LINES: usize = 2_000;
const PROVIDER_LOG_MAX_BYTES: usize = 1024 * 1024;
const GITLAB_JOBS_PAGE_SIZE: usize = 100;
const GITLAB_JOBS_MAX_PAGES: usize = 50;
const GITLAB_JOBS_MAX_ROWS: usize = GITLAB_JOBS_PAGE_SIZE * GITLAB_JOBS_MAX_PAGES;
const GITLAB_JOBS_MAX_BYTES: usize = 4 * 1024 * 1024;
const CI_CLI_TIMEOUT: Duration = Duration::from_secs(30);
const CI_LOG_TIMEOUT: Duration = Duration::from_secs(60);
const CI_CLI_MAX_STDOUT: usize = 8 * 1024 * 1024;
const CI_CLI_MAX_STDERR: usize = 64 * 1024;
const PROVIDER_JSON_MAX_DEPTH: usize = 32;
const PROVIDER_JSON_MAX_NODES: usize = 262_144;
const PROVIDER_JSON_MAX_CONTAINER_ITEMS: usize = 10_000;
const PROVIDER_JSON_MAX_STRING_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_FIELD_BYTES: usize = 4096;
const FALLBACK_READER_GRACE: Duration = Duration::from_millis(500);

/// A CI/CD backend for one provider. Read methods first; mutations are
/// capability-gated via [`Self::caps`] so a provider can decline what it can't do.
///
/// A blocking seam (provider-seams spec): every implementation is a
/// subprocess (`gh`, `glab`) or a REST call it drives synchronously, so
/// callers run it on a blocking thread and no runtime handle is needed.
/// Object-safe: `provider_for` hands back a `Box<dyn CiProvider>`.
pub trait CiProvider: thegn_core::seam::Probe + Send + Sync {
    /// Which CI system this is.
    fn system(&self) -> CiSystem;

    /// Recent runs (newest first), optionally filtered to `branch`.
    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError>;

    /// One run with its jobs (and steps, where the provider exposes them).
    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError>;

    /// A job's log text ("why did it fail"). `run_id` is needed by providers
    /// whose job ids aren't globally addressable (GitLab); GitHub ignores it.
    fn logs(&self, loc: &GitLoc, run_id: &str, job_id: &str) -> Result<CiLog, CiError>;

    /// Dispatchable workflow definitions (drives the trigger prompt).
    fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError>;

    /// Trigger a workflow with `inputs` (`workflow_dispatch`). Phase B.
    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError>;

    /// Re-run a run (all jobs or only the failed ones). Phase B.
    fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError>;

    /// Cancel an in-flight run. Phase B.
    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError>;

    fn caps(&self) -> CiCaps;
}

/// Provider run history plus rows rejected because their target identifiers
/// were malformed. Consumers preserve this count when presenting a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunList {
    pub runs: Vec<CiRun>,
    pub discarded_rows: usize,
}

/// A targeted run response plus the number of malformed jobs omitted from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiRunDetail {
    pub run: CiRun,
    pub discarded_jobs: usize,
}

// === provider selection ====================================================

/// Pick the concrete CI provider for a worktree from `[ci]` config, resolving
/// `"auto"` by sniffing the git remote then falling back to detected CI files.
/// `None` when CI is disabled, undetected, or the resolved system's kind is
/// `reserved` in `CiProviderKind` (Drone/Woodpecker/Jenkins/Argo) — the
/// caller shows a note. The reserved set is the config enum's, not a second
/// list here: `kind_coverage` below pins that every non-reserved kind
/// constructs a client.
pub fn provider_for(loc: &GitLoc, cfg: &CiConfig) -> Option<CiClient> {
    provider_for_cancellable(loc, cfg, Arc::new(AtomicBool::new(false)))
}

/// Pick a CI provider whose subprocesses observe the caller-owned cancellation flag.
pub fn provider_for_cancellable(
    loc: &GitLoc,
    cfg: &CiConfig,
    cancel: Arc<AtomicBool>,
) -> Option<CiClient> {
    if cfg.provider == CiProviderKind::None {
        return None;
    }
    let origin = origin_url(loc);
    let system = resolve_system_from_origin(loc, cfg, origin.as_deref())?;
    if system == CiSystem::GithubActions {
        // Keep the exact origin snapshot used for provider selection. The
        // authenticated gh call must not re-read a possibly changed remote.
        let remote = origin.as_deref().and_then(parse_github_remote)?;
        return Some(Box::new(GithubCi {
            remote: Some(remote),
            cancel,
        }));
    }
    client_for_system_with_config_and_cancel(system, cfg, cancel)
}

/// The system → client factory (pure; `provider_for` adds remote/file sniffing).
pub fn client_for_system(system: CiSystem) -> Option<CiClient> {
    client_for_system_with_config(system, &CiConfig::default())
}

/// System → client factory carrying provider configuration and resolving any
/// configured token once, at construction. The value is injected into the
/// child environment (never argv) for every `glab` call.
fn client_for_system_with_config(system: CiSystem, cfg: &CiConfig) -> Option<CiClient> {
    client_for_system_with_config_and_cancel(system, cfg, Arc::new(AtomicBool::new(false)))
}

fn client_for_system_with_config_and_cancel(
    system: CiSystem,
    cfg: &CiConfig,
    cancel: Arc<AtomicBool>,
) -> Option<CiClient> {
    match system {
        CiSystem::GithubActions => Some(Box::new(GithubCi {
            remote: None,
            cancel,
        })),
        CiSystem::GitlabCi => Some(Box::new(GitlabCi::from_config(cfg, cancel))),
        CiSystem::Drone | CiSystem::Woodpecker | CiSystem::Jenkins | CiSystem::Argo => None,
    }
}

/// The kind → system map for explicit (non-`auto`) selections.
pub fn system_for_kind(kind: CiProviderKind) -> Option<CiSystem> {
    match kind {
        CiProviderKind::None | CiProviderKind::Auto => None,
        CiProviderKind::Github => Some(CiSystem::GithubActions),
        CiProviderKind::Gitlab => Some(CiSystem::GitlabCi),
        CiProviderKind::Drone => Some(CiSystem::Drone),
        CiProviderKind::Woodpecker => Some(CiSystem::Woodpecker),
        CiProviderKind::Jenkins => Some(CiSystem::Jenkins),
        CiProviderKind::Argo => Some(CiSystem::Argo),
    }
}

/// Resolve the active [`CiSystem`] for a worktree (pure once the remote URL is
/// in hand). `provider == auto` sniffs the origin host, else honours the config.
pub fn resolve_system(loc: &GitLoc, cfg: &CiConfig) -> Option<CiSystem> {
    if cfg.provider == CiProviderKind::None {
        return None;
    }
    let origin = origin_url(loc);
    resolve_system_from_origin(loc, cfg, origin.as_deref())
}

fn resolve_system_from_origin(
    loc: &GitLoc,
    cfg: &CiConfig,
    origin: Option<&str>,
) -> Option<CiSystem> {
    match cfg.provider {
        CiProviderKind::None => None,
        CiProviderKind::Auto => {
            if let Some(sys) = origin.and_then(system_from_remote_host) {
                return Some(sys);
            }
            // Fall back to detected CI-config files in the worktree.
            if !loc.is_remote()
                && let Some(cfg) =
                    thegn_core::ci::detect_ci_configs(std::path::Path::new(&loc.path())).first()
            {
                return Some(cfg.system);
            }
            None
        }
        explicit => system_for_kind(explicit),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubRemote {
    host: String,
    repository: String,
}

/// GitHub CLI supports enterprise instances, but CI operations here require
/// the unambiguous owner/repository path shape. The shared remote parser
/// rejects credentials, URL decorations, encoded delimiters and traversal.
/// Is `hostname` GitHub's apex, or a subdomain of it?
///
/// Used where GitHub must be *recognised* from a remote alone — you cannot infer
/// that `ghe.example.test` is GitHub, so auto-detection accepts only the real
/// thing. See [`impersonates_github`] for the different question a
/// target-builder asks.
fn is_github_host(hostname: &str) -> bool {
    hostname == "github.com" || hostname.ends_with(".github.com")
}

fn is_gitlab_host(hostname: &str) -> bool {
    hostname == "gitlab.com" || hostname.ends_with(".gitlab.com")
}

/// Does `hostname` dress itself up as github.com while being some other host?
///
/// This is deliberately NOT [`is_github_host`]'s question. Once the operator has
/// said "this is GitHub", an **enterprise** instance has an arbitrary hostname
/// (`ghe.example.test`), so a membership test would reject every self-hosted
/// install. What must still be refused is a host built to read like the real one:
/// `github.com.evil.test` carries `github.com` as its leading labels and belongs
/// to `evil.test`. An unrelated name is fine; a name wearing github.com's is not.
fn impersonates_github(hostname: &str) -> bool {
    hostname.starts_with("github.com.")
}

fn parse_github_remote(raw: &str) -> Option<GithubRemote> {
    let remote = parse_gitlab_remote(raw)?;
    if remote.project.split('/').count() != 2 {
        return None;
    }
    // Checking only the path shape let any authority through, so a `--repo`
    // target could be built for a lookalike and an authenticated mutation sent
    // to it. Enterprise hosts stay allowed; impersonations do not.
    if impersonates_github(remote.host.split(':').next()?) {
        return None;
    }
    Some(GithubRemote {
        host: remote.host,
        repository: remote.project,
    })
}

fn github_targeted_argv(remote: &GithubRemote, args: &[&str]) -> Vec<String> {
    let mut targeted: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let delimiter = targeted.iter().position(|arg| arg == "--");
    let at = delimiter.unwrap_or(targeted.len());
    let target = format!("{}/{}", remote.host, remote.repository);
    targeted.splice(at..at, ["--repo".to_string(), target]);
    targeted
}

/// Map a git remote URL's host to a CI system (pure, tested).
///
/// Host matching lives in [`is_github_host`] / [`is_gitlab_host`] — one rule, two
/// callers, so the lookalike-host bug cannot be fixed in one place and survive in
/// the other.
pub fn system_from_remote_host(url: &str) -> Option<CiSystem> {
    let host = parse_gitlab_remote(url)?.host;
    let hostname = host.split(':').next()?;
    if is_github_host(hostname) {
        Some(CiSystem::GithubActions)
    } else if is_gitlab_host(hostname) {
        Some(CiSystem::GitlabCi)
    } else {
        None
    }
}

fn origin_url(loc: &GitLoc) -> Option<String> {
    loc.origin_url()
}

/// The selected CI backend. Object-safe, so routing is a `Box<dyn>` and a
/// new provider is one `impl CiProvider` + one factory arm.
pub type CiClient = Box<dyn CiProvider>;

// === helpers ===============================================================

fn run_cli(cmd: Command, cancel: &AtomicBool) -> Result<String, CiError> {
    run_cli_within(
        cmd,
        crate::issue::gh_run::Limits {
            timeout: CI_CLI_TIMEOUT,
            max_stdout: CI_CLI_MAX_STDOUT,
            max_stderr: CI_CLI_MAX_STDERR,
        },
        cancel,
    )
}

fn run_cli_within(
    cmd: Command,
    limits: crate::issue::gh_run::Limits,
    cancel: &AtomicBool,
) -> Result<String, CiError> {
    use crate::issue::gh_run::run_blocking;
    if !crate::plugin::proc::bounded_group_termination_supported() {
        let max_stdout = limits.max_stdout;
        let (stdout, status, stderr) = run_fallback_bounded(cmd, limits, cancel, move |reader| {
            read_capped(reader, max_stdout)
        })?;
        return if status.success() {
            String::from_utf8(stdout)
                .map_err(|_| CiError::Other("provider output was not UTF-8".into()))
        } else {
            Err(classify_stderr(&stderr))
        };
    }

    run_blocking(cmd, limits, cancel).map_err(ci_run_error)
}

fn ci_run_error(error: crate::issue::gh_run::RunError) -> CiError {
    use crate::issue::gh_run::RunError;
    match error {
        RunError::NotInstalled => CiError::NotInstalled,
        RunError::Exit { stderr, .. } => classify_stderr(&stderr),
        RunError::Timeout => CiError::Other("CI provider command timed out".into()),
        RunError::Truncated => CiError::Other("provider output byte budget exhausted".into()),
        RunError::Io(message) => CiError::Other(format!("CI provider command failed: {message}")),
        RunError::Cancelled => CiError::Other("CI provider command cancelled".into()),
    }
}

/// Run the bounded std-only fallback used where process-tree termination is
/// unavailable. Reader threads own the pipes, while this worker owns the child
/// and polls a single deadline and cancellation flag.
fn run_fallback_bounded<T, F>(
    mut cmd: Command,
    limits: crate::issue::gh_run::Limits,
    cancel: &AtomicBool,
    consume: F,
) -> Result<(T, std::process::ExitStatus, String), CiError>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn Read) -> Result<T, CiError> + Send + 'static,
{
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::Instant;

    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(CiError::Other("CI provider command cancelled".into()));
    }
    let deadline = Instant::now() + limits.timeout;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                CiError::NotInstalled
            } else {
                CiError::Other(format!("CI provider command failed: {error}"))
            }
        })?;
    let Some(stdout) = child.stdout.take() else {
        fallback_kill_reap(&mut child, "missing stdout pipe");
        return Err(CiError::Other("provider stdout was not piped".into()));
    };
    let Some(stderr) = child.stderr.take() else {
        fallback_kill_reap(&mut child, "missing stderr pipe");
        return Err(CiError::Other("provider stderr was not piped".into()));
    };

    let (out_tx, out_rx) = mpsc::channel();
    let output_reader = std::thread::Builder::new()
        .spawn(move || {
            let mut stdout = stdout;
            let result = consume(&mut stdout);
            if let Err(error) = out_tx.send(result) {
                tracing::debug!(target: "thegn::ci", %error, "failed to return provider stdout result");
            }
        })
        .map_err(|error| {
            fallback_kill_reap(&mut child, "stdout reader setup failed");
            CiError::Other(format!("provider stdout reader setup failed: {error}"))
        })?;
    let (err_tx, err_rx) = mpsc::channel();
    let max_stderr = limits.max_stderr;
    let error_reader = match std::thread::Builder::new().spawn(move || {
        let result = read_stderr_checked(stderr, max_stderr);
        if let Err(error) = err_tx.send(result) {
            tracing::debug!(target: "thegn::ci", %error, "failed to return provider stderr result");
        }
    }) {
        Ok(reader) => reader,
        Err(error) => {
            fallback_kill_reap(&mut child, "stderr reader setup failed");
            match out_rx.recv_timeout(FALLBACK_READER_GRACE) {
                Ok(Ok(_)) => {}
                Ok(Err(reader_error)) => {
                    tracing::debug!(target: "thegn::ci", %reader_error, "stdout reader failed during fallback setup cleanup")
                }
                Err(receive_error) => {
                    tracing::debug!(target: "thegn::ci", %receive_error, "stdout reader cleanup grace expired")
                }
            }
            drop(output_reader);
            return Err(CiError::Other(format!(
                "provider stderr reader setup failed: {error}"
            )));
        }
    };

    let mut output = None;
    let mut stderr = None;
    let mut status = None;
    let mut exited_at = None;
    let mut terminal_error = None;
    loop {
        match out_rx.try_recv() {
            Ok(Ok(value)) => output = Some(value),
            Ok(Err(error)) => terminal_error = Some(error),
            Err(TryRecvError::Disconnected) => {
                terminal_error
                    .get_or_insert_with(|| CiError::Other("provider stdout reader stopped".into()));
            }
            Err(TryRecvError::Empty) => {}
        }
        match err_rx.try_recv() {
            Ok(Ok(value)) => stderr = Some(value),
            Ok(Err(error)) => {
                terminal_error.get_or_insert(error);
            }
            Err(TryRecvError::Disconnected) => {
                terminal_error
                    .get_or_insert_with(|| CiError::Other("provider stderr reader stopped".into()));
            }
            Err(TryRecvError::Empty) => {}
        }
        if terminal_error.is_some() || cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    exited_at = Some(Instant::now());
                }
                Ok(None) => {}
                Err(error) => {
                    terminal_error = Some(CiError::Other(format!(
                        "provider process wait failed: {error}"
                    )));
                    break;
                }
            }
        }
        if status.is_some() && output.is_some() && stderr.is_some() {
            break;
        }
        let now = Instant::now();
        if now >= deadline || exited_at.is_some_and(|at| now >= at + FALLBACK_READER_GRACE) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let cancelled = cancel.load(std::sync::atomic::Ordering::Relaxed);
    let timed_out = status.is_none() && terminal_error.is_none() && !cancelled;
    if status.is_none() {
        match fallback_terminate(&mut child) {
            Ok(exit) => status = Some(exit),
            Err(error) => terminal_error = Some(error),
        }
    }

    let grace_deadline = Instant::now() + FALLBACK_READER_GRACE;
    while (output.is_none() || stderr.is_none()) && Instant::now() < grace_deadline {
        if output.is_none() {
            match out_rx.try_recv() {
                Ok(Ok(value)) => output = Some(value),
                Ok(Err(error)) => terminal_error = Some(error),
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
            }
        }
        if stderr.is_none() {
            match err_rx.try_recv() {
                Ok(Ok(value)) => stderr = Some(value),
                Ok(Err(error)) => terminal_error = Some(error),
                Err(TryRecvError::Disconnected | TryRecvError::Empty) => {}
            }
        }
        if output.is_none() || stderr.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    drop(output_reader);
    drop(error_reader);
    if cancelled {
        return Err(CiError::Other("CI provider command cancelled".into()));
    }
    if timed_out {
        return Err(CiError::Other("CI provider command timed out".into()));
    }
    if let Some(error) = terminal_error {
        return Err(error);
    }
    let output =
        output.ok_or_else(|| CiError::Other("provider stdout reader grace expired".into()))?;
    let stderr =
        stderr.ok_or_else(|| CiError::Other("provider stderr reader grace expired".into()))?;
    let status =
        status.ok_or_else(|| CiError::Other("provider process has no terminal status".into()))?;
    Ok((output, status, stderr))
}

fn fallback_kill_reap(child: &mut std::process::Child, reason: &str) {
    if let Err(error) = child.kill() {
        tracing::debug!(target: "thegn::ci", %error, reason, "failed to kill fallback provider child");
    }
    let deadline = std::time::Instant::now() + FALLBACK_READER_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Ok(None) => {
                tracing::debug!(target: "thegn::ci", reason, "fallback provider child reap grace expired");
                return;
            }
            Err(error) => {
                tracing::debug!(target: "thegn::ci", %error, reason, "failed to reap fallback provider child");
                return;
            }
        }
    }
}

fn fallback_terminate(
    child: &mut std::process::Child,
) -> Result<std::process::ExitStatus, CiError> {
    if let Err(error) = child.kill() {
        tracing::debug!(target: "thegn::ci", %error, "failed to kill fallback provider child");
    }
    let deadline = std::time::Instant::now() + FALLBACK_READER_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Ok(None) => return Err(CiError::Other("provider process reap grace expired".into())),
            Err(error) => {
                return Err(CiError::Other(format!(
                    "provider process wait failed: {error}"
                )));
            }
        }
    }
}

fn read_stderr_checked<R: Read>(mut reader: R, max: usize) -> Result<String, CiError> {
    let mut bytes = Vec::with_capacity(max.min(64 * 1024));
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|error| CiError::Other(format!("provider stderr read failed: {error}")))?;
        if n == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        let retained = n.min(max.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..retained]);
    }
}

/// Read no more than `max` bytes, probing one additional byte only to detect
/// that the producer exceeded the limit.
fn read_capped<R: Read>(mut reader: R, max: usize) -> Result<Vec<u8>, CiError> {
    let mut output = Vec::with_capacity(max.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = max.saturating_sub(output.len());
        let requested = chunk.len().min(remaining.saturating_add(1));
        let n = reader
            .read(&mut chunk[..requested])
            .map_err(|error| CiError::Other(error.to_string()))?;
        if n == 0 {
            return Ok(output);
        }
        if n > remaining {
            return Err(CiError::Other(
                "provider output byte budget exhausted".into(),
            ));
        }
        output.extend_from_slice(&chunk[..n]);
    }
}

/// Run a command while bounding stdout before it is buffered in memory.
fn run_cli_capped(cmd: Command, max: usize, cancel: &AtomicBool) -> Result<String, CiError> {
    use crate::plugin::proc::bounded_group_termination_supported;
    if bounded_group_termination_supported() {
        return run_cli_within(
            cmd,
            crate::issue::gh_run::Limits {
                timeout: CI_CLI_TIMEOUT,
                max_stdout: max,
                max_stderr: CI_CLI_MAX_STDERR,
            },
            cancel,
        );
    }
    let limits = crate::issue::gh_run::Limits {
        timeout: CI_CLI_TIMEOUT,
        max_stdout: max,
        max_stderr: CI_CLI_MAX_STDERR,
    };
    let (output, status, stderr) =
        run_fallback_bounded(cmd, limits, cancel, move |reader| read_capped(reader, max))?;
    if !status.success() {
        return Err(classify_stderr(&stderr));
    }
    String::from_utf8(output).map_err(|_| CiError::Other("provider output was not UTF-8".into()))
}

/// Run a log-producing command while retaining only its newest complete lines.
///
/// `Command::output` is unsuitable for CI logs: it buffers the complete child
/// stdout before the caller can impose a cap.  Drain stdout in a fixed-size
/// reader instead and keep a bounded deque; stderr is drained concurrently so
/// a noisy failed provider cannot deadlock the child.  The status and error
/// classification remain identical to [`run_cli`].
fn run_bounded_log(cmd: Command, cancel: &AtomicBool) -> Result<CiLog, CiError> {
    run_bounded_log_within(
        cmd,
        crate::issue::gh_run::Limits {
            timeout: CI_LOG_TIMEOUT,
            // Streaming consumers own stdout retention; the streaming runner
            // does not use this stdout limit, but Limits keeps it explicit.
            max_stdout: PROVIDER_LOG_MAX_BYTES,
            max_stderr: CI_CLI_MAX_STDERR,
        },
        cancel,
    )
}

fn run_bounded_log_within(
    cmd: Command,
    limits: crate::issue::gh_run::Limits,
    cancel: &AtomicBool,
) -> Result<CiLog, CiError> {
    use crate::issue::gh_run::run_blocking_streaming;
    use crate::plugin::proc::bounded_group_termination_supported;

    if bounded_group_termination_supported() {
        let (log_result, _status) =
            run_blocking_streaming(cmd, limits, cancel, |reader: &mut dyn Read| {
                collect_log(reader)
            })
            .map_err(ci_run_error)?;
        let (text, truncated) = log_result?;
        return Ok(CiLog { text, truncated });
    }

    let (log_result, status, stderr) =
        run_fallback_bounded(cmd, limits, cancel, |reader| collect_log(reader))?;
    if !status.success() {
        return Err(classify_stderr(&stderr));
    }
    let (text, truncated) = log_result?;
    Ok(CiLog { text, truncated })
}

/// Read at most a fixed number of bytes per chunk and discard old complete
/// lines as new output arrives.  An over-sized line is discarded rather than
/// allowing one pathological line to defeat the byte ceiling.
fn collect_log<R: Read>(mut reader: R) -> Result<(String, bool), CiError> {
    let mut bytes = [0u8; 8192];
    let mut line = Vec::new();
    let mut lines: VecDeque<String> = VecDeque::new();
    let mut kept_bytes = 0usize;
    let mut truncated = false;
    let mut discard_line = false;
    let mut discard_line_last_marker = None;
    let mut in_pem = false;
    let mut marker_tail = Vec::with_capacity(b"-----BEGIN ".len());

    loop {
        let n = reader
            .read(&mut bytes)
            .map_err(|e| CiError::Other(e.to_string()))?;
        if n == 0 {
            if !line.is_empty() && !discard_line {
                push_log_line(
                    &mut lines,
                    &mut kept_bytes,
                    &line,
                    &mut truncated,
                    &mut in_pem,
                );
            }
            break;
        }
        for &byte in &bytes[..n] {
            marker_tail.push(byte);
            if marker_tail.len() > b"-----BEGIN ".len() {
                marker_tail.remove(0);
            }
            if marker_tail.ends_with(b"-----BEGIN ") {
                in_pem = true;
                discard_line_last_marker = Some(true);
            } else if marker_tail.ends_with(b"-----END ") {
                in_pem = false;
                discard_line_last_marker = Some(false);
            }
            if discard_line {
                if byte == b'\n' {
                    discard_line = false;
                    if let Some(begin) = discard_line_last_marker {
                        in_pem = begin;
                    }
                    discard_line_last_marker = None;
                    marker_tail.clear();
                    truncated = true;
                }
                continue;
            }
            line.push(byte);
            if line.len() > PROVIDER_LOG_MAX_BYTES {
                line.clear();
                discard_line = true;
                truncated = true;
                if byte == b'\n' {
                    discard_line = false;
                    if let Some(begin) = discard_line_last_marker {
                        in_pem = begin;
                    }
                    discard_line_last_marker = None;
                    marker_tail.clear();
                }
            } else if byte == b'\n' {
                push_log_line(
                    &mut lines,
                    &mut kept_bytes,
                    &line,
                    &mut truncated,
                    &mut in_pem,
                );
                line.clear();
                discard_line_last_marker = None;
                marker_tail.clear();
            }
        }
    }

    Ok((lines.into_iter().collect(), truncated))
}

fn push_log_line(
    lines: &mut VecDeque<String>,
    kept_bytes: &mut usize,
    line: &[u8],
    truncated: &mut bool,
    in_pem: &mut bool,
) {
    let Ok(line) = std::str::from_utf8(line) else {
        *truncated = true;
        return;
    };
    let line = thegn_core::ci_log::redact_stream_line(line.as_bytes(), in_pem);
    let len = line.len();
    if len > PROVIDER_LOG_MAX_BYTES {
        *truncated = true;
        return;
    }
    lines.push_back(line);
    *kept_bytes += len;
    while lines.len() > PROVIDER_LOG_MAX_LINES || *kept_bytes > PROVIDER_LOG_MAX_BYTES {
        if let Some(old) = lines.pop_front() {
            *kept_bytes -= old.len();
            *truncated = true;
        }
    }
}

fn nonempty(s: &serde_json::Value, key: &str) -> Option<String> {
    s.get(key)
        .and_then(|v| v.as_str())
        .filter(|value| value.len() <= MAX_PROVIDER_FIELD_BYTES)
        .map(str::to_string)
        .filter(|v| !v.is_empty())
}

/// Allocation-free structural scan before serde constructs a provider tree.
fn preflight_provider_json(input: &[u8]) -> Result<(), &'static str> {
    if input.len() > CI_CLI_MAX_STDOUT {
        return Err("json byte limit");
    }
    let mut stack: Vec<(u8, usize, bool)> = Vec::new();
    let mut nodes = 0usize;
    let mut i = 0usize;
    while i < input.len() {
        match input[i] {
            b' ' | b'\n' | b'\r' | b'\t' | b':' => i += 1,
            b',' => {
                if let Some((_, commas, has_item)) = stack.last_mut()
                    && *has_item
                {
                    *commas = commas.saturating_add(1);
                    *has_item = false;
                }
                i += 1;
            }
            b'{' | b'[' => {
                nodes = nodes.saturating_add(1);
                if nodes > PROVIDER_JSON_MAX_NODES {
                    return Err("json node limit");
                }
                if let Some((_, _, has_item)) = stack.last_mut() {
                    *has_item = true;
                }
                stack.push((input[i], 0, false));
                if stack.len() > PROVIDER_JSON_MAX_DEPTH {
                    return Err("json depth limit");
                }
                i += 1;
            }
            b'}' | b']' => {
                let expected = if input[i] == b'}' { b'{' } else { b'[' };
                let Some((kind, commas, has_item)) = stack.pop() else {
                    return Err("json nesting");
                };
                if kind != expected {
                    return Err("json nesting");
                }
                if commas.saturating_add(usize::from(has_item)) > PROVIDER_JSON_MAX_CONTAINER_ITEMS
                {
                    return Err("json container limit");
                }
                i += 1;
            }
            b'"' => {
                if let Some((_, _, has_item)) = stack.last_mut() {
                    *has_item = true;
                }
                nodes = nodes.saturating_add(1);
                if nodes > PROVIDER_JSON_MAX_NODES {
                    return Err("json node limit");
                }
                i += 1;
                let mut decoded = 0usize;
                let mut closed = false;
                while i < input.len() {
                    match input[i] {
                        b'"' => {
                            i += 1;
                            closed = true;
                            break;
                        }
                        b'\\' => {
                            i += 1;
                            let Some(escape) = input.get(i).copied() else {
                                return Err("json string");
                            };
                            if escape == b'u' {
                                if i + 4 >= input.len()
                                    || !input[i + 1..i + 5].iter().all(|b| b.is_ascii_hexdigit())
                                {
                                    return Err("json escape");
                                }
                                decoded = decoded.saturating_add(3);
                                i += 5;
                            } else if matches!(
                                escape,
                                b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                            ) {
                                decoded = decoded.saturating_add(1);
                                i += 1;
                            } else {
                                return Err("json escape");
                            }
                        }
                        b if b < 0x20 => return Err("json control"),
                        _ => {
                            decoded = decoded.saturating_add(1);
                            i += 1;
                        }
                    }
                    if decoded > PROVIDER_JSON_MAX_STRING_BYTES {
                        return Err("json string limit");
                    }
                }
                if !closed {
                    return Err("json string");
                }
            }
            _ => {
                if let Some((_, _, has_item)) = stack.last_mut() {
                    *has_item = true;
                }
                nodes = nodes.saturating_add(1);
                if nodes > PROVIDER_JSON_MAX_NODES {
                    return Err("json node limit");
                }
                while i < input.len()
                    && !matches!(input[i], b' ' | b'\n' | b'\r' | b'\t' | b',' | b']' | b'}')
                {
                    i += 1;
                }
            }
        }
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err("json nesting")
    }
}

fn decode_provider_json<T: serde::de::DeserializeOwned>(
    json: &str,
    what: &str,
) -> Result<T, String> {
    preflight_provider_json(json.as_bytes())
        .map_err(|reason| format!("invalid {what} response: {reason}"))?;
    serde_json::from_str(json).map_err(|error| format!("invalid {what} response: {error}"))
}

fn cap_provider_rows(mut runs: Vec<CiRun>, limit: usize) -> (Vec<CiRun>, usize) {
    let keep = limit.max(1);
    let discarded = runs.len().saturating_sub(keep);
    runs.truncate(keep);
    (runs, discarded)
}

/// Parse provider JSON identifiers as unsigned JSON integers. Provider IDs do
/// not pass through a string grammar: serde's number representation rejects
/// fractions, exponents, negatives and values outside u64.
fn json_id(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_u64)
        .filter(|id| *id > 0)
        .map(|id| id.to_string())
}

/// Canonical textual IDs supplied by CLI/API callers. Keep this pure so every
/// ingress and provider boundary applies the same contract.
pub fn validate_ci_id(value: &str) -> Result<&str, CiError> {
    let valid = !value.is_empty()
        && value.len() <= 20
        && value.as_bytes()[0] != b'0'
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0);
    if valid {
        Ok(value)
    } else {
        Err(CiError::Other(format!("invalid CI identifier {value:?}")))
    }
}

/// Workflow names and repository-relative workflow paths accepted by `gh`.
/// Spaces are valid in names; path selectors additionally allow `/`. Leading
/// option syntax, controls, URL syntax and dot segments are never selectors.
pub fn validate_workflow_selector(value: &str) -> Result<&str, CiError> {
    let valid = !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.starts_with('-')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !value.chars().any(char::is_control)
        && !value.contains('\\');
    if valid {
        Ok(value)
    } else {
        Err(CiError::Other(format!(
            "invalid workflow selector {value:?}"
        )))
    }
}

/// Validate a Git branch ref using the forbidden forms from git-check-ref-format.
/// Query-reserved characters such as `&`, `=`, `%` and `#` remain valid data.
pub fn validate_branch_ref(value: &str) -> Result<&str, CiError> {
    let bytes = value.as_bytes();
    let bad = value.is_empty()
        || value == "@"
        || value.starts_with('-')
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("//")
        || value.contains("@{")
        || value
            .split('/')
            .any(|part| part.starts_with('.') || part.ends_with('.') || part.ends_with(".lock"))
        || bytes
            .iter()
            .any(|b| *b <= 0x20 || *b == 0x7f || b"~^:?*[\\".contains(b));
    if bad {
        Err(CiError::Other(format!("invalid branch ref {value:?}")))
    } else {
        Ok(value)
    }
}

fn github_workflow_argv(
    workflow: &str,
    inputs: &[(String, String)],
) -> Result<Vec<String>, CiError> {
    validate_workflow_selector(workflow)?;
    let mut args = vec!["workflow".into(), "run".into()];
    for (key, value) in inputs {
        args.push("-f".into());
        args.push(format!("{key}={value}"));
    }
    args.extend(["--".into(), workflow.into()]);
    Ok(args)
}

fn github_run_detail_argv(run_id: &str) -> Result<Vec<String>, CiError> {
    validate_ci_id(run_id)?;
    Ok(vec![
        "run".into(),
        "view".into(),
        "--json".into(),
        GH_DETAIL_FIELDS.into(),
        "--".into(),
        run_id.into(),
    ])
}

fn github_rerun_argv(run_id: &str, scope: RerunScope) -> Result<Vec<String>, CiError> {
    validate_ci_id(run_id)?;
    let mut args = vec!["run".into(), "rerun".into()];
    if scope == RerunScope::Failed {
        args.push("--failed".into());
    }
    args.extend(["--".into(), run_id.into()]);
    Ok(args)
}

fn github_cancel_argv(run_id: &str) -> Result<Vec<String>, CiError> {
    validate_ci_id(run_id)?;
    Ok(vec![
        "run".into(),
        "cancel".into(),
        "--".into(),
        run_id.into(),
    ])
}

fn gitlab_pipelines_endpoint(
    project_segment: &str,
    branch: Option<&str>,
    limit: usize,
) -> Result<String, CiError> {
    let mut endpoint = format!(
        "projects/{project_segment}/pipelines?per_page={}",
        limit.max(1)
    );
    if let Some(branch) = branch {
        validate_branch_ref(branch)?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("ref", branch)
            .finish();
        endpoint.push('&');
        endpoint.push_str(&query);
    }
    Ok(endpoint)
}

fn gitlab_run_endpoint(project: &str, run_id: &str, action: &str) -> Result<String, CiError> {
    validate_ci_id(run_id)?;
    if !matches!(action, "" | "jobs" | "retry" | "cancel") {
        return Err(CiError::Other(format!(
            "invalid GitLab pipeline operation {action:?}"
        )));
    }
    let suffix = if action.is_empty() {
        String::new()
    } else {
        format!("/{action}")
    };
    Ok(format!("projects/{project}/pipelines/{run_id}{suffix}"))
}

fn gitlab_job_trace_endpoint(project: &str, job_id: &str) -> Result<String, CiError> {
    validate_ci_id(job_id)?;
    Ok(format!("projects/{project}/jobs/{job_id}/trace"))
}

fn parse_provider_row<T>(
    rows: &[serde_json::Value],
    mut parse: impl FnMut(&serde_json::Value) -> Option<T>,
) -> (Vec<T>, usize) {
    let mut valid = Vec::with_capacity(rows.len());
    let mut discarded = 0;
    for row in rows {
        if let Some(value) = parse(row) {
            valid.push(value);
        } else {
            discarded += 1;
        }
    }
    (valid, discarded)
}

// === GitHub Actions (gh CLI) ==============================================

const GH_RUN_FIELDS: &str = "databaseId,name,displayTitle,headBranch,headSha,event,\
                             status,conclusion,number,createdAt,updatedAt,url,workflowName";
const GH_DETAIL_FIELDS: &str = "databaseId,name,displayTitle,headBranch,headSha,event,\
                                status,conclusion,number,createdAt,updatedAt,url,workflowName,jobs";

/// GitHub Actions via the `gh` CLI — reuses the user's existing `gh` auth
/// (keyring, enterprise hosts) instead of threading a token.
pub struct GithubCi {
    // provider_for binds the same origin snapshot used to select GitHub. The
    // unbound form is retained for the public system factory and resolves once
    // when the operation starts.
    remote: Option<GithubRemote>,
    cancel: Arc<AtomicBool>,
}

impl GithubCi {
    fn remote(&self, loc: &GitLoc) -> Result<GithubRemote, CiError> {
        if let Some(remote) = &self.remote {
            return Ok(remote.clone());
        }
        let raw = origin_url(loc).ok_or(CiError::NotConfigured)?;
        parse_github_remote(&raw)
            .ok_or_else(|| CiError::Other("unsupported or ambiguous GitHub remote".into()))
    }

    fn command(&self, loc: &GitLoc, args: &[&str]) -> Result<Command, CiError> {
        let remote = self.remote(loc)?;
        let argv = github_targeted_argv(&remote, args);
        let argv: Vec<_> = argv.iter().map(String::as_str).collect();
        let mut command = loc.gh_command(&argv);
        // `--repo HOST/OWNER/REPO` is an explicit target on every CI
        // subcommand. Pin the environment equivalents too so inherited
        // GH_REPO/GH_HOST cannot retarget a command on any gh version.
        command.env("GH_REPO", format!("{}/{}", remote.host, remote.repository));
        command.env("GH_HOST", &remote.host);
        Ok(command)
    }
}

impl thegn_core::seam::Probe for GithubCi {
    fn probe(&self) -> thegn_core::seam::ProbeReport {
        thegn_core::seam::ProbeReport::new(
            "ci",
            "github",
            crate::seam::registry::binary_availability("gh"),
        )
        .with_caps(&self.caps())
        .note("GitHub Actions via `gh` (reuses `gh auth`)")
    }
}

impl CiProvider for GithubCi {
    fn system(&self) -> CiSystem {
        CiSystem::GithubActions
    }
    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError> {
        let limit_s = limit.max(1).to_string();
        let mut args = vec!["run", "list", "--limit", &limit_s, "--json", GH_RUN_FIELDS];
        if let Some(b) = branch {
            validate_branch_ref(b)?;
            args.push("--branch");
            args.push(b);
        }
        let json = run_cli(self.command(loc, &args)?, &self.cancel)?;
        let (runs, discarded_rows) = parse_gh_runs_with_discarded(&json).map_err(CiError::Other)?;
        let (runs, overflow) = cap_provider_rows(runs, limit);
        let discarded_rows = discarded_rows.saturating_add(overflow);
        if discarded_rows > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows, "discarded malformed or excess GitHub run rows");
        }
        Ok(CiRunList {
            runs,
            discarded_rows,
        })
    }

    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError> {
        let args = github_run_detail_argv(run_id)?;
        let argv: Vec<_> = args.iter().map(String::as_str).collect();
        let json = run_cli(self.command(loc, &argv)?, &self.cancel)?;
        let value: serde_json::Value =
            decode_provider_json(&json, "provider run").map_err(CiError::Other)?;
        let observed = value
            .get("databaseId")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let run = gh_run_from_value(&value)
            .ok_or_else(|| CiError::Other(format!("invalid provider run identifier {observed}")))?;
        let jobs = value
            .get("jobs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                CiError::Other(format!("malformed GitHub job rows in run {}", run.id))
            })?;
        let (parsed_jobs, discarded_jobs) = parse_provider_row(jobs, gh_job_from_value);
        if discarded_jobs > 0 {
            return Err(CiError::Other(format!(
                "malformed GitHub job/step rows in run {} ({discarded_jobs} row(s))",
                run.id
            )));
        }
        let mut run = run;
        run.jobs = parsed_jobs;
        validate_ci_id(&run.id)?;
        if run.id != run_id {
            return Err(CiError::Other(format!(
                "provider returned run id {:?} for requested {run_id:?}",
                run.id
            )));
        }
        Ok(CiRunDetail {
            run,
            discarded_jobs,
        })
    }

    fn logs(&self, loc: &GitLoc, _run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        // `gh run view --job <id> --log` (job ids are globally addressable).
        validate_ci_id(_run_id)?;
        validate_ci_id(job_id)?;
        run_bounded_log(
            self.command(loc, &["run", "view", "--job", job_id, "--log"])?,
            &self.cancel,
        )
    }

    fn workflows(&self, loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        let json = run_cli(
            self.command(loc, &["workflow", "list", "--json", "id,name,path,state"])?,
            &self.cancel,
        )?;
        parse_gh_workflows(&json).map_err(CiError::Other)
    }

    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        let args = github_workflow_argv(workflow, inputs)?;
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(self.command(loc, &argv)?, &self.cancel).map(|_| ())
    }

    fn rerun(&self, loc: &GitLoc, run_id: &str, scope: RerunScope) -> Result<(), CiError> {
        let args = github_rerun_argv(run_id, scope)?;
        let argv: Vec<_> = args.iter().map(String::as_str).collect();
        run_cli(self.command(loc, &argv)?, &self.cancel).map(|_| ())
    }

    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        let args = github_cancel_argv(run_id)?;
        let argv: Vec<_> = args.iter().map(String::as_str).collect();
        run_cli(self.command(loc, &argv)?, &self.cancel).map(|_| ())
    }

    fn caps(&self) -> CiCaps {
        CiCaps {
            logs: true,
            steps: true,
            trigger: true,
            rerun: true,
            rerun_failed: true,
            cancel: true,
        }
    }
}

/// Parse `gh run list --json …` (an array) into runs.
pub fn parse_gh_runs(json: &str) -> Result<Vec<CiRun>, String> {
    parse_gh_runs_with_discarded(json).map(|parsed| parsed.0)
}

pub fn parse_gh_runs_with_discarded(json: &str) -> Result<(Vec<CiRun>, usize), String> {
    let rows = decode_provider_json::<Vec<serde_json::Value>>(json, "GitHub runs")?;
    Ok(parse_provider_row(&rows, gh_run_from_value))
}

/// Parse `gh run view <id> --json …` (a single object, with `jobs`).
pub fn parse_gh_run_detail(json: &str) -> Option<CiRun> {
    let v: serde_json::Value = decode_provider_json(json, "GitHub run").ok()?;
    if !v.is_object() {
        return None;
    }
    let run = gh_run_from_value(&v)?;
    let jobs = v.get("jobs")?.as_array()?;
    if jobs.iter().any(|job| gh_job_from_value(job).is_none()) {
        return None;
    }
    Some(run)
}

fn gh_run_from_value(v: &serde_json::Value) -> Option<CiRun> {
    let id = json_id(v, "databaseId")?;
    let status = nonempty(v, "status")?;
    let conclusion = nonempty(v, "conclusion");
    warn_unknown_github_state("run", &status, conclusion.as_deref());
    let completed = status.eq_ignore_ascii_case("completed");
    Some(CiRun {
        id,
        name: nonempty(v, "workflowName").or_else(|| nonempty(v, "name"))?,
        title: nonempty(v, "displayTitle").unwrap_or_default(),
        event: nonempty(v, "event").unwrap_or_default(),
        branch: nonempty(v, "headBranch").unwrap_or_default(),
        sha: nonempty(v, "headSha").unwrap_or_default(),
        state: CiState::from_github(&status, conclusion.as_deref()),
        status_raw: status,
        conclusion_raw: conclusion,
        url: nonempty(v, "url").unwrap_or_default(),
        run_number: v.get("number").and_then(serde_json::Value::as_u64),
        started_at: nonempty(v, "createdAt"),
        finished_at: completed.then(|| nonempty(v, "updatedAt")).flatten(),
        jobs: gh_jobs_with_discarded(v.get("jobs")).0,
    })
}

fn gh_jobs_with_discarded(value: Option<&serde_json::Value>) -> (Vec<CiJob>, usize) {
    value
        .and_then(serde_json::Value::as_array)
        .map(|rows| parse_provider_row(rows, gh_job_from_value))
        .unwrap_or_default()
}

fn gh_job_from_value(v: &serde_json::Value) -> Option<CiJob> {
    let id = json_id(v, "databaseId")?;
    let status = nonempty(v, "status")?;
    let conclusion = nonempty(v, "conclusion");
    warn_unknown_github_state("job", &status, conclusion.as_deref());
    Some(CiJob {
        id,
        name: nonempty(v, "name").unwrap_or_default(),
        state: CiState::from_github(&status, conclusion.as_deref()),
        url: nonempty(v, "url"),
        started_at: nonempty(v, "startedAt"),
        finished_at: nonempty(v, "completedAt"),
        steps: v
            .get("steps")?
            .as_array()?
            .iter()
            .map(gh_step_from_value)
            .collect::<Option<Vec<_>>>()?,
    })
}

fn gh_step_from_value(v: &serde_json::Value) -> Option<CiStep> {
    let status = nonempty(v, "status")?;
    let conclusion = nonempty(v, "conclusion");
    warn_unknown_github_state("step", &status, conclusion.as_deref());
    Some(CiStep {
        name: nonempty(v, "name")?,
        number: v.get("number").and_then(serde_json::Value::as_u64),
        state: CiState::from_github(&status, conclusion.as_deref()),
        started_at: nonempty(v, "startedAt"),
        finished_at: nonempty(v, "completedAt"),
    })
}

fn warn_unknown_github_state(kind: &str, status: &str, conclusion: Option<&str>) {
    let known_status = match kind {
        "step" => matches!(
            status.to_ascii_lowercase().as_str(),
            "queued" | "in_progress" | "completed"
        ),
        _ => matches!(
            status.to_ascii_lowercase().as_str(),
            "queued" | "waiting" | "requested" | "pending" | "in_progress" | "completed"
        ),
    };
    let known_conclusion = conclusion.is_none_or(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "success"
                | "failure"
                | "timed_out"
                | "cancelled"
                | "canceled"
                | "skipped"
                | "neutral"
                | "action_required"
                | "stale"
                | "startup_failure"
        )
    });
    if !known_status || !known_conclusion {
        tracing::warn!(target: "thegn::ci", unknown_states = 1, kind, status, ?conclusion, "unknown GitHub CI state; using fallback mapping");
    }
}

/// Parse `gh workflow list --json id,name,path,state`.
pub fn parse_gh_workflows(json: &str) -> Result<Vec<CiWorkflow>, String> {
    let arr = decode_provider_json::<Vec<serde_json::Value>>(json, "GitHub workflows")?;
    arr.iter()
        .map(|v| {
            Some(CiWorkflow {
                id: json_id(v, "id")?,
                name: nonempty(v, "name")?,
                path: nonempty(v, "path")?,
                // `gh workflow list` doesn't expose the trigger set; treat
                // active workflows as dispatchable (trigger degrades with a
                // readable error if a given one isn't). Input prompting +
                // accurate dispatchability come in Phase B.
                dispatchable: nonempty(v, "state")
                    .map(|s| s.eq_ignore_ascii_case("active"))
                    .unwrap_or(true),
                inputs: Vec::new(),
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "invalid GitHub workflow row".to_string())
}

// === GitLab CI (glab + GitLab API) ========================================

/// Credential selection for GitLab CLI execution.
#[derive(Clone, Default, PartialEq, Eq)]
enum GitlabCredential {
    #[default]
    CliManaged,
    Configured {
        token: String,
        explicit: bool,
    },
    Unavailable,
}

impl std::fmt::Debug for GitlabCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CliManaged => f.write_str("CliManaged"),
            Self::Configured { .. } => f.write_str("Configured(<redacted>)"),
            Self::Unavailable => f.write_str("Unavailable"),
        }
    }
}

impl GitlabCredential {
    fn from_resolution(raw: &str, resolution: crate::secret::SecretResolution) -> Self {
        match resolution {
            crate::secret::SecretResolution::Absent => Self::CliManaged,
            crate::secret::SecretResolution::Resolved(token) => Self::Configured {
                token,
                explicit: raw != thegn_core::config::GitLabCiConfig::default().token,
            },
            crate::secret::SecretResolution::Failed
                if raw == thegn_core::config::GitLabCiConfig::default().token =>
            {
                // Compatibility: the shipped env:GITLAB_TOKEN default has
                // historically allowed glab's own login when the env is absent.
                Self::CliManaged
            }
            crate::secret::SecretResolution::Failed => Self::Unavailable,
        }
    }
}

/// GitLab CI via `glab api`. The shipped `env:GITLAB_TOKEN` default remains
/// explicitly CLI-managed when unavailable for compatibility with `glab auth
/// login`; an absent field is also CLI-managed. Any other configured reference
/// that fails resolution is unavailable and fails closed. Explicitly configured
/// resolved tokens are supplied through `GITLAB_TOKEN` only for local commands;
/// the shipped ambient default remains CLI-managed for remote commands. GitLab
/// has no per-job "steps", so [`CiJob::steps`] stays empty.
#[derive(Default)]
pub struct GitlabCi {
    credential: GitlabCredential,
    host: Option<String>,
    cancel: Arc<AtomicBool>,
}

impl thegn_core::seam::Probe for GitlabCi {
    fn probe(&self) -> thegn_core::seam::ProbeReport {
        thegn_core::seam::ProbeReport::new(
            "ci",
            "gitlab",
            crate::seam::registry::binary_availability("glab"),
        )
        .with_caps(&self.caps())
        .note("GitLab CI via `glab api`")
    }
}

impl GitlabCi {
    fn from_config(cfg: &CiConfig, cancel: Arc<AtomicBool>) -> Self {
        Self {
            credential: GitlabCredential::from_resolution(
                &cfg.gitlab.token,
                crate::secret::resolve_outcome(&cfg.gitlab.token, BareAs::Literal, "ci:gitlab"),
            ),
            host: (!cfg.gitlab.host.trim().is_empty()).then(|| cfg.gitlab.host.trim().to_string()),
            cancel,
        }
    }

    /// Construct a `glab` command with credentials in the child environment,
    /// never in argv. Keeping this at one chokepoint prevents a new operation
    /// from accidentally falling back to an unaudited/config-ignored path.
    fn command(
        &self,
        loc: &GitLoc,
        remote: &GitlabRemote,
        args: &[&str],
    ) -> Result<Command, CiError> {
        if matches!(&self.credential, GitlabCredential::Unavailable) {
            return Err(CiError::Other(
                "configured [ci.gitlab] token reference could not be resolved".into(),
            ));
        }
        if matches!(
            &self.credential,
            GitlabCredential::Configured { explicit: true, .. }
        ) && loc.is_remote()
        {
            return Err(CiError::Other(
                "configured GitLab token cannot be forwarded to a remote/provider worktree".into(),
            ));
        }
        let mut cmd = loc.cli_command("glab", args);
        if let GitlabCredential::Configured { token, explicit } = &self.credential
            && (!loc.is_remote() || *explicit)
        {
            cmd.env("GITLAB_TOKEN", token);
        }
        cmd.env("GITLAB_HOST", &remote.host);
        Ok(cmd)
    }

    /// Resolve origin once for an operation. The resulting host and project
    /// are reused for both the credential-bearing command and its endpoint.
    fn remote(&self, loc: &GitLoc) -> Result<GitlabRemote, CiError> {
        let remote = snapshot_gitlab_remote(|| origin_url(loc))?;
        if let Some(configured_host) = &self.host
            && normalize_gitlab_host(configured_host).as_deref() != Some(remote.host.as_str())
        {
            return Err(CiError::Other(
                "GitLab remote host does not match configured host".into(),
            ));
        }
        Ok(remote)
    }

    fn project_seg(remote: &GitlabRemote) -> String {
        encode_project_segment(&remote.project)
    }
}

impl CiProvider for GitlabCi {
    fn system(&self) -> CiSystem {
        CiSystem::GitlabCi
    }
    fn runs(&self, loc: &GitLoc, branch: Option<&str>, limit: usize) -> Result<CiRunList, CiError> {
        let remote = self.remote(loc)?;
        let project_segment = Self::project_seg(&remote);
        let endpoint = gitlab_pipelines_endpoint(&project_segment, branch, limit)?;
        let json = run_cli(
            self.command(loc, &remote, &["api", &endpoint])?,
            &self.cancel,
        )?;
        let (runs, discarded) =
            parse_gitlab_pipelines_with_discarded(&json).map_err(CiError::Other)?;
        let (runs, overflow) = cap_provider_rows(runs, limit);
        let discarded = discarded.saturating_add(overflow);
        if discarded > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = discarded, "discarded malformed or excess GitLab pipeline rows");
        }
        Ok(CiRunList {
            runs,
            discarded_rows: discarded,
        })
    }

    fn run_detail(&self, loc: &GitLoc, run_id: &str) -> Result<CiRunDetail, CiError> {
        validate_ci_id(run_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        // Pipeline header + its jobs (two calls; the jobs carry the states).
        let run_endpoint = gitlab_run_endpoint(&proj, run_id, "")?;
        let jobs_endpoint = gitlab_run_endpoint(&proj, run_id, "jobs")?;
        let pipe_json = run_cli(
            self.command(loc, &remote, &["api", &run_endpoint])?,
            &self.cancel,
        )?;
        let value: serde_json::Value =
            decode_provider_json(&pipe_json, "provider pipeline").map_err(CiError::Other)?;
        let observed = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let mut run = gitlab_pipeline_from_value(&value)
            .filter(|_| nonempty(&value, "status").is_some())
            .ok_or_else(|| {
                CiError::Other(format!("invalid provider pipeline identifier {observed}"))
            })?;
        validate_ci_id(&run.id)?;
        if run.id != run_id {
            return Err(CiError::Other(format!(
                "provider returned run id {:?} for requested {run_id:?}",
                run.id
            )));
        }
        let mut jobs = Vec::new();
        let mut ids = std::collections::HashSet::new();
        let mut discarded_jobs = 0usize;
        let mut bytes = 0usize;
        let mut complete = false;
        for page in 1..=GITLAB_JOBS_MAX_PAGES {
            let endpoint = format!("{jobs_endpoint}?per_page={GITLAB_JOBS_PAGE_SIZE}&page={page}");
            let remaining = GITLAB_JOBS_MAX_BYTES.saturating_sub(bytes);
            let jobs_json = run_cli_capped(
                self.command(loc, &remote, &["api", &endpoint])?,
                remaining,
                &self.cancel,
            )?;
            bytes = bytes.saturating_add(jobs_json.len());
            let page_rows =
                decode_provider_json::<Vec<serde_json::Value>>(&jobs_json, "GitLab jobs")
                    .map_err(CiError::Other)?;
            let page_length = page_rows.len();
            let (parsed, discarded) = parse_provider_row(&page_rows, gitlab_job_from_value);
            for job in parsed {
                if !ids.insert(job.id.clone()) {
                    return Err(CiError::Other(format!(
                        "duplicate GitLab job id {:?}",
                        job.id
                    )));
                }
                jobs.push(job);
            }
            discarded_jobs += discarded;
            if jobs.len() + discarded_jobs > GITLAB_JOBS_MAX_ROWS {
                return Err(CiError::Other(
                    "GitLab jobs pagination row budget exhausted".into(),
                ));
            }
            if page_length < GITLAB_JOBS_PAGE_SIZE {
                complete = true;
                break;
            }
        }
        if !complete {
            return Err(CiError::Other(
                "GitLab jobs pagination page budget exhausted".into(),
            ));
        }
        if discarded_jobs > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = discarded_jobs, "discarded malformed GitLab job rows");
        }
        run.jobs = jobs;
        Ok(CiRunDetail {
            run,
            discarded_jobs,
        })
    }

    fn logs(&self, loc: &GitLoc, _run_id: &str, job_id: &str) -> Result<CiLog, CiError> {
        validate_ci_id(_run_id)?;
        validate_ci_id(job_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        let endpoint = gitlab_job_trace_endpoint(&proj, job_id)?;
        run_bounded_log(
            self.command(loc, &remote, &["api", &endpoint])?,
            &self.cancel,
        )
    }

    fn workflows(&self, _loc: &GitLoc) -> Result<Vec<CiWorkflow>, CiError> {
        // GitLab has one pipeline definition (`.gitlab-ci.yml`), not a set of
        // dispatchable workflows; manual-trigger support is Phase B.
        Ok(Vec::new())
    }

    fn trigger(
        &self,
        loc: &GitLoc,
        workflow: &str,
        inputs: &[(String, String)],
    ) -> Result<(), CiError> {
        validate_workflow_selector(workflow)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        let mut args = vec!["api".to_string(), "-X".into(), "POST".into()];
        args.push(format!("projects/{proj}/pipeline"));
        for (k, v) in inputs {
            args.push("-f".into());
            args.push(format!("{k}={v}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        run_cli(self.command(loc, &remote, &argv)?, &self.cancel).map(|_| ())
    }

    fn rerun(&self, loc: &GitLoc, run_id: &str, _scope: RerunScope) -> Result<(), CiError> {
        validate_ci_id(run_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        // GitLab: `retry` re-runs failed jobs; a fresh full run isn't a single
        // call, so both scopes map to retry (it's the closest primitive).
        let endpoint = gitlab_run_endpoint(&proj, run_id, "retry")?;
        run_cli(
            self.command(loc, &remote, &["api", "-X", "POST", &endpoint])?,
            &self.cancel,
        )
        .map(|_| ())
    }

    fn cancel(&self, loc: &GitLoc, run_id: &str) -> Result<(), CiError> {
        validate_ci_id(run_id)?;
        let remote = self.remote(loc)?;
        let proj = Self::project_seg(&remote);
        let endpoint = gitlab_run_endpoint(&proj, run_id, "cancel")?;
        run_cli(
            self.command(loc, &remote, &["api", "-X", "POST", &endpoint])?,
            &self.cancel,
        )
        .map(|_| ())
    }

    fn caps(&self) -> CiCaps {
        CiCaps {
            logs: true,
            steps: false,
            trigger: true,
            rerun: true,
            // Pipeline `retry` has no failed-only scope — see `rerun` above.
            rerun_failed: false,
            cancel: true,
        }
    }
}

/// Extract a GitLab project path (`group/sub/repo`, keeping subgroups) from a
/// git remote URL. Pure, tested.
pub fn gitlab_project_path(url: &str) -> Option<String> {
    parse_gitlab_remote(url).map(|remote| remote.project)
}

#[derive(Debug)]
struct GitlabRemote {
    host: String,
    project: String,
}

fn parse_gitlab_remote_checked(raw: &str) -> Result<GitlabRemote, CiError> {
    parse_gitlab_remote(raw)
        .ok_or_else(|| CiError::Other("invalid GitLab remote authority or project path".into()))
}

fn snapshot_gitlab_remote(
    read_origin: impl FnOnce() -> Option<String>,
) -> Result<GitlabRemote, CiError> {
    let raw = read_origin().ok_or(CiError::NotConfigured)?;
    parse_gitlab_remote_checked(&raw)
}

fn normalize_gitlab_host(raw: &str) -> Option<String> {
    if raw.contains("://") {
        let url = url::Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let host = url.host_str()?;
        return Some(match url.port() {
            Some(port) => format!("{host}:{port}").to_ascii_lowercase(),
            None => host.to_ascii_lowercase(),
        });
    }
    let host = raw.trim().trim_matches('/').to_ascii_lowercase();
    (!host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':')))
    .then_some(host)
}

/// Accept only ordinary SSH/HTTP(S) git remotes with an unambiguous host and
/// plain project path. Encoded paths and URL decorations are rejected before
/// constructing an authenticated `glab api` request.
fn parse_gitlab_remote(raw: &str) -> Option<GitlabRemote> {
    let raw = raw.trim();
    let (host, path) = if raw.contains("://") {
        if !raw_url_path_is_unambiguous(raw) {
            return None;
        }
        let parsed = url::Url::parse(raw).ok()?;
        if !matches!(parsed.scheme(), "https" | "http" | "ssh")
            || parsed.host_str().is_none()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || (parsed.scheme() != "ssh" && !parsed.username().is_empty())
            || (parsed.scheme() == "ssh"
                && !parsed.username().is_empty()
                && parsed.username() != "git")
            || parsed.password().is_some()
        {
            return None;
        }
        let host = match parsed.port() {
            Some(port) => format!("{}:{port}", parsed.host_str()?),
            None => parsed.host_str()?.to_string(),
        };
        (
            host.to_ascii_lowercase(),
            parsed.path().trim_start_matches('/').to_string(),
        )
    } else {
        // SCP-like SSH: optional conventional username, a DNS/IPv4 host, and
        // a colon before the path. IPv6 must use a real ssh:// URL.
        let (authority, path) = raw.split_once(':')?;
        let (username, host) = authority
            .rsplit_once('@')
            .map_or((None, authority), |(username, host)| (Some(username), host));
        if username.is_some_and(|user| {
            user.is_empty()
                || !user
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }) {
            return None;
        }
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        {
            return None;
        }
        (host.to_ascii_lowercase(), path.to_string())
    };
    let path = path.strip_suffix(".git").unwrap_or(&path);
    let segments: Vec<_> = path.split('/').collect();
    if segments.len() < 2
        || segments.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || part.starts_with('.')
                || part
                    .bytes()
                    .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'.' | b'_' | b'-'))
        })
    {
        return None;
    }
    Some(GitlabRemote {
        host,
        project: segments.join("/"),
    })
}

/// Check path text before `url::Url` can normalize dot segments or decode its
/// interpretation of delimiters. Encoded path bytes are rejected entirely:
/// project components have a deliberately narrow literal grammar.
fn raw_url_path_is_unambiguous(raw: &str) -> bool {
    let Some((_, authority_and_path)) = raw.split_once("://") else {
        return false;
    };
    let authority_end = authority_and_path
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_path.len());
    let suffix = &authority_and_path[authority_end..];
    let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
    let path = &suffix[..path_end];
    if !path
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return false;
    }
    path.split('/')
        .filter(|component| !component.is_empty())
        .all(|component| component != "." && component != "..")
}

fn encode_project_segment(project: &str) -> String {
    let mut encoded = String::new();
    for byte in project.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Parse `GET projects/:id/pipelines` (array) into runs.
pub fn parse_gitlab_pipelines(json: &str) -> Result<Vec<CiRun>, String> {
    parse_gitlab_pipelines_with_discarded(json).map(|parsed| parsed.0)
}

pub fn parse_gitlab_pipelines_with_discarded(json: &str) -> Result<(Vec<CiRun>, usize), String> {
    let rows = decode_provider_json::<Vec<serde_json::Value>>(json, "GitLab pipelines")?;
    Ok(parse_provider_row(&rows, gitlab_pipeline_from_value))
}

/// Parse `GET projects/:id/pipelines/:id` (single object) into a run header.
pub fn parse_gitlab_pipeline_detail(json: &str) -> Option<CiRun> {
    let v: serde_json::Value = decode_provider_json(json, "GitLab pipeline").ok()?;
    if !v.is_object() {
        return None;
    }
    let run = gitlab_pipeline_from_value(&v)?;
    nonempty(&v, "status")?;
    Some(run)
}

fn gitlab_pipeline_from_value(v: &serde_json::Value) -> Option<CiRun> {
    let id = json_id(v, "id")?;
    let status = nonempty(v, "status")?;
    warn_unknown_gitlab_state(&status);
    let terminal = CiState::from_gitlab(&status).is_terminal();
    Some(CiRun {
        id: id.clone(),
        name: nonempty(v, "name").unwrap_or_else(|| format!("pipeline #{id}")),
        title: nonempty(v, "ref").unwrap_or_default(),
        event: nonempty(v, "source").unwrap_or_default(),
        branch: nonempty(v, "ref").unwrap_or_default(),
        sha: nonempty(v, "sha").unwrap_or_default(),
        state: CiState::from_gitlab(&status),
        status_raw: status,
        conclusion_raw: None,
        url: nonempty(v, "web_url").unwrap_or_default(),
        run_number: v.get("iid").and_then(serde_json::Value::as_u64),
        started_at: nonempty(v, "created_at"),
        finished_at: terminal.then(|| nonempty(v, "updated_at")).flatten(),
        jobs: Vec::new(),
    })
}

/// Parse `GET projects/:id/pipelines/:id/jobs` (array) into jobs.
pub fn parse_gitlab_jobs(json: &str) -> Result<Vec<CiJob>, String> {
    parse_gitlab_jobs_with_discarded(json).map(|parsed| parsed.0)
}

pub fn parse_gitlab_jobs_with_discarded(json: &str) -> Result<(Vec<CiJob>, usize), String> {
    let arr = decode_provider_json::<Vec<serde_json::Value>>(json, "GitLab jobs")?;
    Ok(parse_provider_row(&arr, gitlab_job_from_value))
}

fn gitlab_job_from_value(v: &serde_json::Value) -> Option<CiJob> {
    let id = json_id(v, "id")?;
    let status = nonempty(v, "status")?;
    warn_unknown_gitlab_state(&status);
    Some(CiJob {
        id,
        name: nonempty(v, "name").unwrap_or_default(),
        state: CiState::from_gitlab(&status),
        url: nonempty(v, "web_url"),
        started_at: nonempty(v, "started_at"),
        finished_at: nonempty(v, "finished_at"),
        steps: Vec::new(),
    })
}

fn warn_unknown_gitlab_state(status: &str) {
    if !matches!(
        status.to_ascii_lowercase().as_str(),
        "created"
            | "waiting_for_resource"
            | "preparing"
            | "pending"
            | "running"
            | "success"
            | "failed"
            | "canceled"
            | "cancelled"
            | "skipped"
            | "manual"
            | "scheduled"
            | "canceling"
            | "waiting_for_callback"
    ) {
        tracing::warn!(target: "thegn::ci", unknown_states = 1, status, "unknown GitLab CI state; using fallback mapping");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_available() -> bool {
        Command::new("sh").arg("-c").arg("sleep 0").output().is_ok()
    }

    fn bounded_shell_available() -> bool {
        crate::plugin::proc::bounded_group_termination_supported()
            && Command::new("sh").arg("-c").arg("exit 0").output().is_ok()
    }

    fn test_limits(timeout: Duration, max_stdout: usize) -> crate::issue::gh_run::Limits {
        crate::issue::gh_run::Limits {
            timeout,
            max_stdout,
            max_stderr: 1024,
        }
    }

    fn test_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn bounded_cli_times_out_a_hung_provider() {
        if !bounded_shell_available() {
            return;
        }
        let start = std::time::Instant::now();
        let result = run_cli_within(
            {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "sleep 30"]);
                cmd
            },
            test_limits(Duration::from_millis(200), 1024),
            &test_cancel(),
        );
        assert!(
            matches!(result, Err(CiError::Other(message)) if message == "CI provider command timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn fallback_runner_times_out_a_hung_provider() {
        if !shell_available() {
            return;
        }
        let start = std::time::Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let result = run_fallback_bounded(
            cmd,
            test_limits(Duration::from_millis(200), 1024),
            &test_cancel(),
            |reader| read_capped(reader, 1024),
        );
        assert!(
            matches!(result, Err(CiError::Other(message)) if message == "CI provider command timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn fallback_runner_observes_cancellation() {
        if !shell_available() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                signal.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 30"]);
            let result = run_fallback_bounded(
                cmd,
                test_limits(Duration::from_secs(10), 1024),
                &cancel,
                |reader| read_capped(reader, 1024),
            );
            assert!(
                matches!(result, Err(CiError::Other(message)) if message == "CI provider command cancelled")
            );
        });
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn fallback_runner_bounds_a_descendant_holding_stdout() {
        if !shell_available() {
            return;
        }
        let start = std::time::Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo ok"]);
        let result = run_fallback_bounded(
            cmd,
            test_limits(Duration::from_secs(10), 1024),
            &test_cancel(),
            |reader| read_capped(reader, 1024),
        );
        assert!(
            matches!(result, Err(CiError::Other(message)) if message == "provider stdout reader grace expired")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn provider_json_preflight_rejects_structure_and_field_overflow() {
        let too_many = format!(
            "[{}]",
            vec!["0"; PROVIDER_JSON_MAX_CONTAINER_ITEMS + 1].join(",")
        );
        assert!(parse_gh_runs_with_discarded(&too_many).is_err());

        let object = format!(
            "{{{}}}",
            (0..52)
                .map(|index| format!("\"k{index}\":0"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let many_nodes = format!("[{}]", vec![object; 2500].join(","));
        assert!(parse_gh_runs_with_discarded(&many_nodes).is_err());

        let nested = format!(
            "{}0{}",
            "[".repeat(PROVIDER_JSON_MAX_DEPTH + 1),
            "]".repeat(PROVIDER_JSON_MAX_DEPTH + 1)
        );
        assert!(parse_gh_runs_with_discarded(&nested).is_err());

        let oversized = "x".repeat(PROVIDER_JSON_MAX_STRING_BYTES + 1);
        let row = format!(
            r#"[{{"databaseId":1,"workflowName":"CI","status":"completed","displayTitle":"{oversized}"}}]"#
        );
        assert!(parse_gh_runs_with_discarded(&row).is_err());
    }

    #[test]
    fn oversized_retained_status_discards_provider_row() {
        let status = "s".repeat(MAX_PROVIDER_FIELD_BYTES + 1);
        let payload = format!(r#"[{{"databaseId":1,"workflowName":"CI","status":"{status}"}}]"#);
        let (runs, discarded) = parse_gh_runs_with_discarded(&payload).unwrap();
        assert!(runs.is_empty());
        assert_eq!(discarded, 1);
    }

    #[test]
    fn provider_run_rows_are_capped_to_requested_limit() {
        let payload = r#"[
          {"databaseId":1,"workflowName":"one","status":"completed"},
          {"databaseId":2,"workflowName":"two","status":"completed"},
          {"databaseId":3,"workflowName":"three","status":"completed"}
        ]"#;
        let (runs, _) = parse_gh_runs_with_discarded(payload).unwrap();
        let (runs, overflow) = cap_provider_rows(runs, 2);
        assert_eq!(runs.len(), 2);
        assert_eq!(overflow, 1);
    }

    #[test]
    fn bounded_cli_observes_external_cancellation() {
        if !bounded_shell_available() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                signal.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 30"]);
            let result = run_cli_within(cmd, test_limits(Duration::from_secs(10), 1024), &cancel);
            assert!(
                matches!(result, Err(CiError::Other(message)) if message == "CI provider command cancelled")
            );
        });
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bounded_cli_reaps_pipe_holding_descendants() {
        if !bounded_shell_available() {
            return;
        }
        let start = std::time::Instant::now();
        let result = run_cli_within(
            {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "sleep 30 & echo ok"]);
                cmd
            },
            test_limits(Duration::from_secs(2), 1024),
            &test_cancel(),
        );
        assert!(result.is_ok());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bounded_log_times_out_a_hung_provider() {
        if !bounded_shell_available() {
            return;
        }
        let start = std::time::Instant::now();
        let result = run_bounded_log_within(
            {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "sleep 30"]);
                cmd
            },
            test_limits(Duration::from_millis(200), 1024),
            &test_cancel(),
        );
        assert!(
            matches!(result, Err(CiError::Other(message)) if message == "CI provider command timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bounded_log_observes_external_cancellation() {
        if !bounded_shell_available() {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancel);
        let start = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                signal.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 30"]);
            let result =
                run_bounded_log_within(cmd, test_limits(Duration::from_secs(10), 1024), &cancel);
            assert!(
                matches!(result, Err(CiError::Other(message)) if message == "CI provider command cancelled")
            );
        });
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bounded_log_reaps_pipe_holding_descendants() {
        if !bounded_shell_available() {
            return;
        }
        let start = std::time::Instant::now();
        let result = run_bounded_log_within(
            {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "sleep 30 & printf 'a\\nb\\n'"]);
                cmd
            },
            test_limits(Duration::from_secs(2), 1024),
            &test_cancel(),
        );
        let log = result.expect("bounded log should finish after killing the descendant");
        assert_eq!(log.text, "a\nb\n");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn bounded_log_classifies_nonzero_exit_like_bounded_cli() {
        if !bounded_shell_available() {
            return;
        }
        let script = "printf 'fatal: authentication required\\n' >&2; exit 1";
        let mut cli = Command::new("sh");
        cli.args(["-c", script]);
        let cli_error = run_cli_within(
            cli,
            test_limits(Duration::from_secs(2), 1024),
            &test_cancel(),
        )
        .expect_err("provider command should fail");
        let mut log = Command::new("sh");
        log.args(["-c", script]);
        let log_error = run_bounded_log_within(
            log,
            test_limits(Duration::from_secs(2), 1024),
            &test_cancel(),
        )
        .expect_err("provider log command should fail");
        assert_eq!(log_error, cli_error);
    }

    #[test]
    fn bounded_cli_rejects_stdout_over_its_byte_budget() {
        if !bounded_shell_available() {
            return;
        }
        let result = run_cli_within(
            {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "while :; do printf x; done"]);
                cmd
            },
            test_limits(Duration::from_secs(5), 1024),
            &test_cancel(),
        );
        assert!(
            matches!(result, Err(CiError::Other(message)) if message == "provider output byte budget exhausted")
        );
    }

    #[test]
    fn bounded_cli_preserves_spawn_io_error_detail() {
        if !crate::plugin::proc::bounded_group_termination_supported() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let result = run_cli_within(
            Command::new(temp.path()),
            test_limits(Duration::from_secs(1), 1024),
            &test_cancel(),
        );
        assert!(matches!(result, Err(CiError::Other(message)) if message
                .strip_prefix("CI provider command failed: ")
                .is_some_and(|detail| !detail.is_empty())));
    }

    #[test]
    fn collector_redacts_pem_before_evicting_its_begin_line() {
        let mut input = String::from("-----BEGIN PRIVATE KEY-----\n");
        for _ in 0..PROVIDER_LOG_MAX_LINES + 1 {
            input.push_str("ordinary line\n");
        }
        input.push_str("SYNTHETIC_KEY_BODY\n-----END PRIVATE KEY-----\n");
        for _ in 0..PROVIDER_LOG_MAX_LINES - 2 {
            input.push_str("ordinary line\n");
        }
        let (tail, truncated) = collect_log(input.as_bytes()).unwrap();
        assert!(truncated);
        assert!(!tail.contains("SYNTHETIC_KEY_BODY"));
        assert!(tail.contains("ordinary line"));
    }

    #[test]
    fn collector_redacts_prefixed_pem_before_evicting_its_begin_line() {
        let mut input =
            String::from("build\tstep\t2024-01-01T00:00:00Z -----BEGIN OPENSSH PRIVATE KEY-----\n");
        for _ in 0..PROVIDER_LOG_MAX_LINES + 1 {
            input.push_str("ordinary line\n");
        }
        input.push_str(
            "SYNTHETIC_KEY_BODY\nbuild\tstep\t2024-01-01T00:00:01Z -----END OPENSSH PRIVATE KEY-----\n",
        );
        for _ in 0..PROVIDER_LOG_MAX_LINES - 2 {
            input.push_str("ordinary line\n");
        }
        let (tail, truncated) = collect_log(input.as_bytes()).unwrap();
        assert!(truncated);
        assert!(!tail.contains("SYNTHETIC_KEY_BODY"));
        assert!(tail.contains("ordinary line"));
    }

    #[test]
    fn oversized_prefixed_pem_begin_still_latches_redaction_state() {
        let mut input = vec![b'x'; PROVIDER_LOG_MAX_BYTES + 32];
        input.extend_from_slice(b"build\tstep\t2024-01-01T00:00:00Z -----BEGIN PRIVATE KEY-----\n");
        input.extend_from_slice(b"SYNTHETIC_OVERSIZED_KEY_BODY\n");
        input.extend_from_slice(b"-----END PRIVATE KEY-----\nafter\n");
        let (text, truncated) = collect_log(input.as_slice()).unwrap();
        assert!(truncated);
        assert!(!text.contains("SYNTHETIC_OVERSIZED_KEY_BODY"));
        assert!(text.contains("after"));
    }

    #[test]
    fn oversized_line_keeps_the_last_pem_marker_state() {
        let mut input = vec![b'x'; PROVIDER_LOG_MAX_BYTES + 32];
        input.extend_from_slice(b"-----END A----- -----BEGIN B-----\n");
        input.extend_from_slice(b"SYNTHETIC_KEY_AFTER_BEGIN\n");
        input.extend_from_slice(b"-----END B-----\nafter\n");
        let (text, truncated) = collect_log(input.as_slice()).unwrap();
        assert!(truncated);
        assert!(!text.contains("SYNTHETIC_KEY_AFTER_BEGIN"));
        assert!(text.contains("after"));
    }

    #[test]
    fn remote_host_to_system() {
        assert_eq!(
            system_from_remote_host("git@github.com:o/r.git"),
            Some(CiSystem::GithubActions)
        );
        assert_eq!(
            system_from_remote_host("https://gitlab.com/g/s/r.git"),
            Some(CiSystem::GitlabCi)
        );
        assert_eq!(
            system_from_remote_host("https://example.test/github.com/g/r.git"),
            None
        );
        assert_eq!(
            system_from_remote_host("https://github.com.evil.test/g/r.git"),
            None
        );
        assert_eq!(system_from_remote_host("git@bitbucket.org:o/r.git"), None);
    }

    #[test]
    fn gitlab_path_parsing() {
        assert_eq!(
            gitlab_project_path("git@gitlab.com:group/sub/repo.git").as_deref(),
            Some("group/sub/repo")
        );
        assert_eq!(
            gitlab_project_path("https://gitlab.example.com/group/repo").as_deref(),
            Some("group/repo")
        );
        assert_eq!(
            gitlab_project_path("ssh://git@gitlab.example.com:2222/group/repo.git").as_deref(),
            Some("group/repo")
        );
        let remote = parse_gitlab_remote("https://gitlab.example.com:8443/group/sub/repo.git")
            .expect("parsed GitLab authority and project");
        assert_eq!(remote.host, "gitlab.example.com:8443");
        assert_eq!(remote.project, "group/sub/repo");
        // single-segment (no group) → None (GitLab projects always have a group)
        assert_eq!(gitlab_project_path("https://gitlab.com/repo.git"), None);
        for invalid in [
            "https://gitlab.com/group/repo?next=other",
            "https://gitlab.com/group/repo#fragment",
            "https://user@gitlab.com/group/repo",
            "https://user:token@gitlab.com/group/repo",
            "https://gitlab.com/group%2Frepo/sub",
            "https://gitlab.com/group\\repo",
            // URL parsing normalizes these dot segments unless the raw path is
            // checked before parsing; remotes must not silently retarget.
            "https://gitlab.com/../group/repo",
            "https://gitlab.com/%2e%2e/group/repo",
            "git@gitlab.com:group/../repo.git",
            "git@gitlab.com:group//repo.git",
            "git@gitlab.com:group/repo#fragment",
        ] {
            assert_eq!(gitlab_project_path(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn gitlab_remote_errors_never_echo_credentials_or_remote_text() {
        let secret_remote = "https://user:fixture-token@gitlab.example.test/group/repo";
        let error = parse_gitlab_remote_checked(secret_remote)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("fixture-token"));
        assert!(!error.contains("user"));
        assert_eq!(error, "invalid GitLab remote authority or project path");
    }

    #[test]
    fn gitlab_operation_takes_one_remote_snapshot_for_host_and_project() {
        let mut reads = 0;
        let remote = snapshot_gitlab_remote(|| {
            reads += 1;
            Some("https://gitlab.example.test/group/repo".into())
        })
        .unwrap();
        assert_eq!(reads, 1);

        let endpoint =
            gitlab_pipelines_endpoint(&GitlabCi::project_seg(&remote), None, 10).unwrap();
        let client = GitlabCi {
            credential: GitlabCredential::Configured {
                token: "fixture-token".into(),
                explicit: true,
            },
            host: None,
            ..GitlabCi::default()
        };
        let loc = GitLoc::for_worktree(std::path::Path::new("."));
        let command = client.command(&loc, &remote, &["api", &endpoint]).unwrap();
        assert_eq!(endpoint, "projects/group%2Frepo/pipelines?per_page=10");
        assert_eq!(
            command
                .get_envs()
                .find(|(key, _)| *key == std::ffi::OsStr::new("GITLAB_HOST"))
                .and_then(|(_, value)| value)
                .map(|value| value.to_string_lossy().into_owned())
                .as_deref(),
            Some("gitlab.example.test")
        );
    }

    #[test]
    fn ci_ids_have_one_bounded_canonical_spelling() {
        for valid in ["1", "9", "12345678901234567890"] {
            assert_eq!(validate_ci_id(valid), Ok(valid));
        }
        for invalid in [
            "",
            "0",
            "01",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1.0",
            "1e5",
            "1/2",
            "1..2",
            "1%2f3",
            "1#x",
            "1\n",
            "1\t",
            "--help",
            "18446744073709551616",
            "123456789012345678901",
        ] {
            let err = validate_ci_id(invalid).unwrap_err();
            // The message quotes the input with `{:?}`, which ESCAPES control
            // characters — deliberately, so a rejected id carrying a newline or a
            // tab cannot inject them into a log line or a terminal. So assert the
            // escaped spelling; asserting the raw bytes would demand the unsafe
            // behaviour for the `"1\n"` and `"1\t"` cases.
            assert!(
                err.to_string().contains(&format!("{invalid:?}")),
                "{invalid:?}: {err}"
            );
        }
    }

    #[test]
    fn provider_json_ids_are_numeric_and_bad_list_rows_are_counted() {
        let payload = r#"[
          {"databaseId":1,"status":"completed","workflowName":"CI"},
          {"databaseId":"2","status":"completed"},
          {"databaseId":1.0,"status":"completed"},
          {"databaseId":-1,"status":"completed"},
          {"databaseId":18446744073709551616,"status":"completed"}
        ]"#;
        let (runs, discarded) = parse_gh_runs_with_discarded(payload).unwrap();
        assert_eq!(
            runs.iter().map(|run| run.id.as_str()).collect::<Vec<_>>(),
            ["1"]
        );
        assert_eq!(discarded, 4);

        let jobs = r#"[{"id":7,"status":"success"},{"id":"8"},{"id":1e5}]"#;
        let (jobs, discarded) = parse_gitlab_jobs_with_discarded(jobs).unwrap();
        assert_eq!(
            jobs.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
            ["7"]
        );
        assert_eq!(discarded, 2);
        let rows: serde_json::Value =
            serde_json::from_str(r#"[{"databaseId":7,"status":"completed","steps":[]},{"databaseId":"8"},{"databaseId":1e5}]"#)
                .unwrap();
        let (parsed, discarded) = gh_jobs_with_discarded(Some(&rows));
        assert_eq!(parsed.len(), 1);
        assert_eq!(discarded, 2);
    }

    #[test]
    fn unknown_gitlab_status_is_kept_and_mapped_to_unknown() {
        let (runs, discarded) =
            parse_gitlab_pipelines_with_discarded(r#"[{"id":9,"status":"canceling"}]"#).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, CiState::Pending);
        assert_eq!(discarded, 0);
    }

    #[test]
    fn unknown_github_run_and_job_states_are_kept() {
        let (runs, discarded) = parse_gh_runs_with_discarded(
            r#"[{"databaseId":9,"workflowName":"CI","status":"future_state","conclusion":"future_result"}]"#,
        )
        .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, CiState::Fail);
        assert_eq!(discarded, 0);
        let (jobs, discarded) = gh_jobs_with_discarded(Some(&serde_json::json!([
            {"databaseId": 10, "name": "job", "status": "future_state", "steps": []}
        ])));
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].state, CiState::Pending);
        assert_eq!(discarded, 0);
    }

    #[test]
    fn gitlab_detail_paginates_and_rejects_incomplete_or_duplicate_pages() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let glab = bin.join("glab");
        std::fs::write(
            &glab,
            r##"#!/bin/sh
endpoint=
for arg in "$@"; do endpoint=$arg; done
case "$endpoint" in
  *"/pipelines/123/jobs?"*|*"/pipelines/124/jobs?"*|*"/pipelines/125/jobs?"*|*"/pipelines/126/jobs?"*)
    case "$endpoint" in *pipelines/123/jobs?*) run=123 ;; *pipelines/124/jobs?*) run=124 ;; *pipelines/125/jobs?*) run=125 ;; *) run=126 ;; esac
    page=${endpoint##*page=}
    if [ "$run" = 126 ]; then printf '{}\n'; exit 0; fi
    if [ "$page" = 1 ] || [ "$run" = 125 ]; then
      printf '['
      i=1
      base=$(( (page - 1) * 100 ))
      while [ "$i" -le 100 ]; do
        [ "$i" -gt 1 ] && printf ','
        id=$((base + i))
        printf '{"id":%s,"name":"job-%s","status":"success"}' "$id" "$id"
        i=$((i + 1))
      done
      printf ']\n'
    elif [ "$run" = 123 ]; then
      printf '[{"id":101,"name":"last","status":"failed"}]\n'
    elif [ "$run" = 124 ]; then
      printf '[{"id":1,"name":"duplicate","status":"success"}]\n'
    else
      printf '[]\n'
    fi ;;
  *"/pipelines/"*)
    run=${endpoint##*/}
    printf '{"id":%s,"status":"success"}\n' "$run" ;;
  *) printf '[]\n' ;;
esac
"##,
        )
        .unwrap();
        if thegn_core::fsperm::make_executable_for_test(&glab).is_err() {
            return;
        }
        let loc = GitLoc::Local(temp.path().to_path_buf());
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "https://gitlab.com/group/repo.git",
            ],
        ] {
            assert!(
                thegn_core::util::git_cmd(temp.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _env = thegn_core::testenv::EnvGuard::mutate_pairs(&[("PATH", Some(&path))]);
        let client = GitlabCi::default();
        let detail = client.run_detail(&loc, "123").unwrap();
        assert_eq!(detail.run.jobs.len(), 101);
        assert_eq!(detail.run.jobs[100].state, CiState::Fail);
        assert!(
            client
                .run_detail(&loc, "124")
                .unwrap_err()
                .to_string()
                .contains("duplicate GitLab job id")
        );
        assert!(
            client
                .run_detail(&loc, "125")
                .unwrap_err()
                .to_string()
                .contains("page budget exhausted")
        );
        assert!(
            client
                .run_detail(&loc, "126")
                .unwrap_err()
                .to_string()
                .contains("invalid GitLab jobs response")
        );
    }

    #[test]
    fn workflow_argv_and_gitlab_query_preserve_target_identity() {
        let inputs = vec![("environment".into(), "staging".into())];
        assert_eq!(
            github_workflow_argv("Release candidate (manual).yml", &inputs).unwrap(),
            [
                "workflow",
                "run",
                "-f",
                "environment=staging",
                "--",
                "Release candidate (manual).yml"
            ]
        );
        for invalid in ["--help", "../workflow.yml", "a//b", "bad\nname"] {
            assert!(validate_workflow_selector(invalid).is_err(), "{invalid:?}");
        }
        assert_eq!(
            gitlab_pipelines_endpoint("group%2Fsub%2Frepo", Some("feature&status=success#x%"), 30)
                .unwrap(),
            "projects/group%2Fsub%2Frepo/pipelines?per_page=30&ref=feature%26status%3Dsuccess%23x%25"
        );
        assert!(gitlab_pipelines_endpoint("group%2Frepo", Some("bad ref"), 30).is_err());
        // Match git-check-ref-format's rule that a ref component cannot start
        // with a dot. This guards validation independently of query encoding.
        for invalid in [".hidden", "feature/.hidden"] {
            assert!(validate_branch_ref(invalid).is_err(), "{invalid:?}");
        }
        for invalid in [
            "feature/..hidden",
            "feature/trailing./part",
            "feature/name.lock",
            "feature/@{upstream}",
            "feature/name\\part",
            "feature/name\u{7f}part",
            "@",
        ] {
            assert!(validate_branch_ref(invalid).is_err(), "{invalid:?}");
        }
        for valid in [
            "feature&status=success",
            "release#candidate",
            "percent%name",
        ] {
            assert_eq!(validate_branch_ref(valid), Ok(valid));
        }
        assert_eq!(
            github_run_detail_argv("123").unwrap(),
            ["run", "view", "--json", GH_DETAIL_FIELDS, "--", "123"]
        );
        assert_eq!(
            github_rerun_argv("123", RerunScope::Failed).unwrap(),
            ["run", "rerun", "--failed", "--", "123"]
        );
        assert_eq!(
            github_cancel_argv("123").unwrap(),
            ["run", "cancel", "--", "123"]
        );
    }

    #[test]
    fn github_origin_parsing_and_target_argv_preserve_authority() {
        for (raw, host) in [
            ("git@github.com:owner/repo.git", "github.com"),
            (
                "ssh://git@ghe.example.test:2222/owner/repo.git",
                "ghe.example.test:2222",
            ),
            ("https://ghe.example.test/owner/repo", "ghe.example.test"),
        ] {
            let remote = parse_github_remote(raw).expect("valid GitHub origin");
            assert_eq!(remote.host, host);
            assert_eq!(remote.repository, "owner/repo");
            let target = format!("{host}/owner/repo");
            assert_eq!(
                github_targeted_argv(&remote, &["run", "cancel", "--", "123"]),
                [
                    "run".to_string(),
                    "cancel".to_string(),
                    "--repo".to_string(),
                    target,
                    "--".to_string(),
                    "123".to_string()
                ]
            );
        }
        for raw in [
            "https://github.com/owner/group/repo",
            "https://github.com/owner/../repo",
            "https://github.com/owner/repo?other=repo",
            "https://user:token@github.com/owner/repo",
            "https://github.com/owner%2Frepo",
            "https://github.com.evil.test/owner/repo",
        ] {
            assert!(parse_github_remote(raw).is_none(), "{raw:?}");
        }
    }

    #[test]
    fn github_provider_argv_and_environment_ignore_ambient_target() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let capture = temp.path().join("argv.txt");
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            r##"#!/bin/sh
printf '%s\t' "$@" >> "$CI_CAPTURE"
printf 'GH_REPO=%s\tGH_HOST=%s\n' "$GH_REPO" "$GH_HOST" >> "$CI_CAPTURE"
case " $* " in
  *" --job "*) printf 'log output\n' ;;
  *" view "*" 124 "*) printf '%s\n' '{"databaseId":124,"workflowName":"CI","status":"completed","jobs":[{"databaseId":456,"status":"completed","steps":[{}]}]}' ;;
  *" view "*) printf '%s\n' '{"databaseId":123,"workflowName":"CI","status":"completed","jobs":[]}' ;;
  *) printf '[]\n' ;;
esac
"##,
        )
        .unwrap();
        // The fake `gh` is a POSIX shell script, so this fixture is Unix-only —
        // but express that through the cross-platform seam and a RUNTIME skip
        // rather than `#[cfg(unix)]`. `ci.rs` is not on
        // `test/platform-cfg-svc-ratchet.txt` and that list is shrink-only, so a
        // platform `#[cfg]` here fails the build; this also keeps the test
        // type-checked on Windows. Same idiom as `thegn_core::github`'s fixture.
        if thegn_core::fsperm::make_executable_for_test(&gh).is_err() {
            return;
        }

        let loc = GitLoc::Local(temp.path().to_path_buf());
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
            vec![
                "config",
                "--local",
                "remote.origin.gh-resolved",
                "ambient/wrong",
            ],
        ] {
            assert!(
                // `git_cmd` scrubs GIT_ENV_VARS; this test can run inside a live
                // thegn worktree whose inherited GIT_DIR would otherwise redirect
                // these commands out of the temp repo.
                thegn_core::util::git_cmd(temp.path())
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let config_dir = temp.path().join("gh-config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let capture_text = capture.to_string_lossy().into_owned();
        let config_text = config_dir.to_string_lossy().into_owned();
        let _env = thegn_core::testenv::EnvGuard::mutate_pairs(&[
            ("PATH", Some(&path)),
            ("GH_REPO", Some("ambient/override")),
            ("GH_HOST", Some("ambient.example.test")),
            ("GH_CONFIG_DIR", Some(&config_text)),
            ("CI_CAPTURE", Some(&capture_text)),
        ]);

        let remote = parse_github_remote("https://github.com/owner/repo.git").unwrap();
        let client = GithubCi {
            remote: Some(remote),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        client
            .runs(&loc, Some("feature&status=success"), 7)
            .unwrap();
        client.run_detail(&loc, "123").unwrap();
        assert!(
            client
                .run_detail(&loc, "124")
                .unwrap_err()
                .to_string()
                .contains("malformed GitHub job/step rows")
        );
        client.logs(&loc, "123", "456").unwrap();
        client.workflows(&loc).unwrap();
        client.trigger(&loc, "release/manual.yml", &[]).unwrap();
        client.rerun(&loc, "123", RerunScope::Failed).unwrap();
        client.cancel(&loc, "123").unwrap();

        let records = std::fs::read_to_string(&capture).unwrap();
        let records: Vec<String> = records.lines().map(str::to_string).collect();
        let expected = vec![
            format!("run\tlist\t--limit\t7\t--json\t{GH_RUN_FIELDS}\t--branch\tfeature&status=success\t--repo\tgithub.com/owner/repo\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com"),
            format!("run\tview\t--json\t{GH_DETAIL_FIELDS}\t--repo\tgithub.com/owner/repo\t--\t123\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com"),
            format!("run\tview\t--json\t{GH_DETAIL_FIELDS}\t--repo\tgithub.com/owner/repo\t--\t124\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com"),
            "run\tview\t--job\t456\t--log\t--repo\tgithub.com/owner/repo\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com".into(),
            "workflow\tlist\t--json\tid,name,path,state\t--repo\tgithub.com/owner/repo\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com".into(),
            "workflow\trun\t--repo\tgithub.com/owner/repo\t--\trelease/manual.yml\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com".into(),
            "run\trerun\t--failed\t--repo\tgithub.com/owner/repo\t--\t123\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com".into(),
            "run\tcancel\t--repo\tgithub.com/owner/repo\t--\t123\tGH_REPO=github.com/owner/repo\tGH_HOST=github.com".into(),
        ];
        assert_eq!(records, expected);

        let before = std::fs::metadata(&capture).unwrap().len();
        assert!(client.runs(&loc, Some("bad ref"), 7).is_err());
        assert!(client.rerun(&loc, "--help", RerunScope::All).is_err());
        assert!(client.cancel(&loc, "01").is_err());
        assert!(client.logs(&loc, "123", "1e5").is_err());
        assert!(client.trigger(&loc, "--help", &[]).is_err());
        assert_eq!(std::fs::metadata(&capture).unwrap().len(), before);

        let unsupported = tempfile::tempdir().unwrap();
        assert!(
            thegn_core::util::git_cmd(unsupported.path())
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            thegn_core::util::git_cmd(unsupported.path())
                .args([
                    "remote",
                    "add",
                    "origin",
                    "https://github.com.evil.test/owner/repo"
                ])
                .status()
                .unwrap()
                .success()
        );
        let config = CiConfig {
            provider: CiProviderKind::Github,
            ..CiConfig::default()
        };
        let unsupported_loc = GitLoc::Local(unsupported.path().to_path_buf());
        assert!(provider_for(&unsupported_loc, &config).is_none());
        assert_eq!(std::fs::metadata(&capture).unwrap().len(), before);
    }

    #[test]
    fn rejected_identifiers_never_reach_request_construction() {
        let mut authenticated_requests = 0;
        for id in ["--help", "01", "1e5", "1#x", "1%2f2"] {
            if github_cancel_argv(id).is_ok() {
                authenticated_requests += 1;
            }
            assert!(github_cancel_argv(id).is_err(), "{id:?}");
            if gitlab_run_endpoint("g%2Fr", id, "retry").is_ok() {
                authenticated_requests += 1;
            }
            assert!(gitlab_run_endpoint("g%2Fr", id, "retry").is_err(), "{id:?}");
        }
        if gitlab_job_trace_endpoint("g%2Fr", "--help").is_ok() {
            authenticated_requests += 1;
        }
        assert!(gitlab_job_trace_endpoint("g%2Fr", "--help").is_err());
        if github_workflow_argv("--help", &[]).is_ok() {
            authenticated_requests += 1;
        }
        assert!(github_workflow_argv("--help", &[]).is_err());
        assert_eq!(authenticated_requests, 0);

        assert_eq!(
            gitlab_run_endpoint("g%2Fr", "123", "retry").unwrap(),
            "projects/g%2Fr/pipelines/123/retry"
        );
        assert_eq!(
            gitlab_job_trace_endpoint("g%2Fr", "456").unwrap(),
            "projects/g%2Fr/jobs/456/trace"
        );
    }

    #[test]
    fn provider_entry_points_reject_bad_identifiers_before_subprocesses() {
        let loc = GitLoc::for_worktree(std::path::Path::new("/does/not/exist"));
        let github = GithubCi {
            remote: None,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let gitlab = GitlabCi::default();

        for error in [
            github.run_detail(&loc, "--help").unwrap_err(),
            github.logs(&loc, "1", "1e5").unwrap_err(),
            github.rerun(&loc, "1/2", RerunScope::All).unwrap_err(),
            github.cancel(&loc, "01").unwrap_err(),
            gitlab.run_detail(&loc, "--help").unwrap_err(),
            gitlab.logs(&loc, "1", "1#x").unwrap_err(),
            gitlab.rerun(&loc, "1.0", RerunScope::All).unwrap_err(),
            gitlab.cancel(&loc, "18446744073709551616").unwrap_err(),
        ] {
            assert!(
                error.to_string().contains("invalid CI identifier"),
                "provider entry point reached another failure before rejecting the id: {error}"
            );
        }

        assert!(
            github
                .trigger(&loc, "--help", &[])
                .unwrap_err()
                .to_string()
                .contains("invalid workflow selector")
        );
        assert!(
            gitlab
                .trigger(&loc, "../workflow.yml", &[])
                .unwrap_err()
                .to_string()
                .contains("invalid workflow selector")
        );
    }

    #[test]
    fn gitlab_config_token_is_in_child_environment_never_argv() {
        let mut cfg = CiConfig::default();
        cfg.gitlab.token = "fixture-secret-token".into();
        cfg.gitlab.host = "gitlab.example.test".into();
        let client = GitlabCi::from_config(&cfg, Arc::new(AtomicBool::new(false)));
        let loc = GitLoc::for_worktree(std::path::Path::new("."));
        let remote = GitlabRemote {
            host: "gitlab.example.test".into(),
            project: "g/r".into(),
        };
        let command = client
            .command(&loc, &remote, &["api", "projects/g%2Fr/pipelines"])
            .unwrap();
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(!args.iter().any(|arg| arg.contains("fixture-secret-token")));
        let env = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            env.get("GITLAB_TOKEN").and_then(Option::as_deref),
            Some("fixture-secret-token")
        );
        assert_eq!(
            env.get("GITLAB_HOST").and_then(Option::as_deref),
            Some("gitlab.example.test")
        );
    }

    #[test]
    fn gitlab_configured_token_refuses_remote_and_provider_transports() {
        let mut cfg = CiConfig::default();
        cfg.gitlab.token = "fixture-secret-token".into();
        let configured = GitlabCi::from_config(&cfg, Arc::new(AtomicBool::new(false)));
        let remote = GitlabRemote {
            host: "gitlab.example.test".into(),
            project: "g/r".into(),
        };
        let provider_loc = GitLoc::Provider {
            control_prefix: vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "x".into(),
                "--".into(),
            ],
            path: "/workspace".into(),
        };
        let error = configured
            .command(&provider_loc, &remote, &["api", "projects/g%2Fr/pipelines"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot be forwarded"));
        assert!(!error.contains("fixture-secret-token"));

        let cli_managed = GitlabCi {
            credential: GitlabCredential::CliManaged,
            host: None,
            ..GitlabCi::default()
        };
        assert!(
            cli_managed
                .command(&provider_loc, &remote, &["api", "projects/g%2Fr/pipelines"])
                .is_ok()
        );
    }

    #[test]
    fn shipped_default_resolved_token_uses_remote_cli_login() {
        let default = thegn_core::config::GitLabCiConfig::default().token;
        let credential = GitlabCredential::from_resolution(
            &default,
            crate::secret::SecretResolution::Resolved("ambient-token".into()),
        );
        let client = GitlabCi {
            credential,
            host: None,
            ..GitlabCi::default()
        };
        let provider_loc = GitLoc::Provider {
            control_prefix: vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "x".into(),
                "--".into(),
            ],
            path: "/workspace".into(),
        };
        let remote = GitlabRemote {
            host: "gitlab.example.test".into(),
            project: "g/r".into(),
        };
        let command = client
            .command(&provider_loc, &remote, &["api", "projects/g%2Fr/pipelines"])
            .unwrap();
        assert!(
            !command
                .get_envs()
                .any(|(key, _)| key == std::ffi::OsStr::new("GITLAB_TOKEN"))
        );
    }

    #[test]
    fn explicit_gitlab_secret_resolution_failures_stop_before_command_creation() {
        assert!(std::env::var_os("THE541_SURELY_UNSET_VAR").is_none());
        let missing_file = std::env::temp_dir().join(format!(
            "the541-ci-token-that-does-not-exist-{}",
            std::process::id()
        ));
        assert!(!missing_file.exists());
        let missing_file_ref = format!("file:{}", missing_file.display());
        let loc = GitLoc::for_worktree(std::path::Path::new("."));
        let remote = GitlabRemote {
            host: "gitlab.example.test".into(),
            project: "g/r".into(),
        };
        for reference in [
            "keyring:the541-fixture".to_string(),
            missing_file_ref,
            "env:THE541_SURELY_UNSET_VAR".to_string(),
        ] {
            let mut cfg = CiConfig::default();
            cfg.gitlab.token = reference;
            let client = GitlabCi::from_config(&cfg, Arc::new(AtomicBool::new(false)));
            assert!(matches!(
                client.command(&loc, &remote, &["api", "projects/g%2Fr/pipelines"]),
                Err(CiError::Other(message))
                    if message == "configured [ci.gitlab] token reference could not be resolved"
            ));
        }
    }

    #[test]
    fn gitlab_absent_and_legacy_default_credentials_are_cli_managed() {
        assert_eq!(
            GitlabCredential::from_resolution("", crate::secret::SecretResolution::Absent),
            GitlabCredential::CliManaged
        );
        assert_eq!(
            GitlabCredential::from_resolution(
                &thegn_core::config::GitLabCiConfig::default().token,
                crate::secret::SecretResolution::Failed
            ),
            GitlabCredential::CliManaged
        );
        assert_eq!(
            GitlabCredential::from_resolution(
                "env:THE541_SURELY_UNSET_VAR",
                crate::secret::SecretResolution::Failed
            ),
            GitlabCredential::Unavailable
        );
        let mut cfg = CiConfig::default();
        cfg.gitlab.token.clear();
        let client = GitlabCi::from_config(&cfg, Arc::new(AtomicBool::new(false)));
        let loc = GitLoc::for_worktree(std::path::Path::new("."));
        let remote = GitlabRemote {
            host: "gitlab.example.test".into(),
            project: "g/r".into(),
        };
        let command = client.command(&loc, &remote, &["api"]).unwrap();
        assert!(
            !command
                .get_envs()
                .any(|(key, _)| key == std::ffi::OsStr::new("GITLAB_TOKEN"))
        );
    }

    #[test]
    fn gitlab_credential_debug_redacts_configured_token() {
        let credential = GitlabCredential::Configured {
            token: "fixture-secret-token".into(),
            explicit: true,
        };
        assert_eq!(format!("{credential:?}"), "Configured(<redacted>)");
    }

    #[test]
    fn capped_reader_rejects_first_byte_over_limit_and_accepts_exact_limit() {
        assert!(read_capped(std::io::repeat(b'x').take(5), 4).is_err());
        assert_eq!(
            read_capped(std::io::repeat(b'x').take(4), 4).unwrap(),
            b"xxxx"
        );
    }

    #[test]
    fn pem_begin_marker_split_across_single_byte_reads_is_redacted() {
        struct OneByte<R>(R);
        impl<R: Read> Read for OneByte<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let len = buf.len().min(1);
                self.0.read(&mut buf[..len])
            }
        }

        let input =
            b"before\n-----BEGIN PRIVATE KEY-----\nsecret-body\n-----END PRIVATE KEY-----\nafter\n";
        let (text, _) = collect_log(OneByte(&input[..])).unwrap();
        assert!(!text.contains("secret-body"));
        assert!(text.contains("before"));
        assert!(text.contains("after"));
        assert!(text.contains(thegn_core::ci_log::REDACTION_MARKER));
    }

    #[test]
    fn prefixed_pem_markers_split_across_single_byte_reads_are_redacted() {
        struct OneByte<R>(R);
        impl<R: Read> Read for OneByte<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let len = buf.len().min(1);
                self.0.read(&mut buf[..len])
            }
        }

        let input = b"build\tstep\t2024-01-01T00:00:00.0000000Z -----BEGIN OPENSSH PRIVATE KEY-----\nSYNTHETICKEYBODY\nbuild\tstep\t2024-01-01T00:00:01.0000000Z -----END OPENSSH PRIVATE KEY-----\nafter\n";
        let (text, _) = collect_log(OneByte(&input[..])).unwrap();
        assert!(!text.contains("SYNTHETICKEYBODY"));
        assert!(text.contains("after"));
        assert!(text.contains(thegn_core::ci_log::REDACTION_MARKER));
    }

    #[test]
    fn parse_gh_run_list() {
        let json = r#"[
          {"databaseId":123,"name":"CI","workflowName":"CI","displayTitle":"fix: thing",
           "headBranch":"main","headSha":"abc123","event":"push","status":"completed",
           "conclusion":"failure","number":42,"createdAt":"2026-06-25T10:00:00Z",
           "updatedAt":"2026-06-25T10:05:00Z","url":"https://gh/run/123"},
          {"databaseId":124,"workflowName":"CI","displayTitle":"wip","headBranch":"main",
           "headSha":"def","event":"push","status":"in_progress","conclusion":"",
           "number":43,"createdAt":"2026-06-25T11:00:00Z","updatedAt":"2026-06-25T11:01:00Z",
           "url":"https://gh/run/124"}
        ]"#;
        let runs = parse_gh_runs(json).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "123");
        assert_eq!(runs[0].name, "CI");
        assert_eq!(runs[0].state, CiState::Fail);
        assert_eq!(runs[0].finished_at.as_deref(), Some("2026-06-25T10:05:00Z"));
        assert_eq!(runs[0].run_number, Some(42));
        // in-flight run: no finished_at, Running
        assert_eq!(runs[1].state, CiState::Running);
        assert_eq!(runs[1].finished_at, None);
        assert!(parse_gh_runs("not json").is_err());
        assert!(parse_gh_runs("{}").is_err());
        assert_eq!(parse_gh_runs_with_discarded("[]").unwrap(), (Vec::new(), 0));
    }

    #[test]
    fn parse_gh_detail_with_jobs_and_steps() {
        let json = r#"{
          "databaseId":123,"workflowName":"CI","displayTitle":"fix","headBranch":"main",
          "headSha":"abc","event":"push","status":"completed","conclusion":"failure",
          "number":42,"createdAt":"2026-06-25T10:00:00Z","updatedAt":"2026-06-25T10:05:00Z",
          "url":"https://gh/run/123",
          "jobs":[
            {"databaseId":1,"name":"build","status":"completed","conclusion":"success",
             "startedAt":"2026-06-25T10:00:10Z","completedAt":"2026-06-25T10:02:00Z",
             "url":"https://gh/job/1",
             "steps":[{"name":"Checkout","number":1,"status":"completed","conclusion":"success"}]},
            {"databaseId":2,"name":"test","status":"completed","conclusion":"failure",
             "startedAt":"2026-06-25T10:02:00Z","completedAt":"2026-06-25T10:05:00Z",
             "url":"https://gh/job/2","steps":[]}
          ]
        }"#;
        let run = parse_gh_run_detail(json).unwrap();
        assert_eq!(run.id, "123");
        assert_eq!(run.jobs.len(), 2);
        assert_eq!(run.jobs[0].name, "build");
        assert_eq!(run.jobs[0].state, CiState::Pass);
        assert_eq!(run.jobs[0].steps.len(), 1);
        assert_eq!(run.jobs[0].steps[0].state, CiState::Pass);
        assert_eq!(run.jobs[1].state, CiState::Fail);
        assert!(parse_gh_run_detail("[]").is_none());
        assert!(parse_gh_run_detail(r#"{"databaseId":1,"status":"completed"}"#).is_none());
        assert!(parse_gh_run_detail(r#"{"databaseId":1,"status":"completed","jobs":[{"databaseId":2,"status":"completed","steps":[{}]}]}"#).is_none());
    }

    #[test]
    fn parse_gh_workflow_list() {
        let json = r#"[
          {"id":1,"name":"CI","path":".github/workflows/ci.yml","state":"active"},
          {"id":2,"name":"Stale","path":".github/workflows/stale.yml","state":"disabled_manually"}
        ]"#;
        let wfs = parse_gh_workflows(json).unwrap();
        assert_eq!(wfs.len(), 2);
        assert_eq!(wfs[0].id, "1");
        assert!(wfs[0].dispatchable);
        assert!(!wfs[1].dispatchable);
    }

    #[test]
    fn parse_gitlab_pipeline_list_and_jobs() {
        let pipelines = r#"[
          {"id":1001,"iid":7,"ref":"main","sha":"abc","status":"failed","source":"push",
           "web_url":"https://gl/p/1001","created_at":"2026-06-25T10:00:00Z",
           "updated_at":"2026-06-25T10:06:00Z"},
          {"id":1002,"iid":8,"ref":"main","sha":"def","status":"running","source":"push",
           "web_url":"https://gl/p/1002","created_at":"2026-06-25T11:00:00Z",
           "updated_at":"2026-06-25T11:01:00Z"}
        ]"#;
        let runs = parse_gitlab_pipelines(pipelines).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "1001");
        assert_eq!(runs[0].state, CiState::Fail);
        assert_eq!(runs[0].name, "pipeline #1001");
        assert_eq!(runs[0].run_number, Some(7));
        assert_eq!(runs[0].finished_at.as_deref(), Some("2026-06-25T10:06:00Z"));
        // running pipeline → not terminal → no finished_at
        assert_eq!(runs[1].state, CiState::Running);
        assert_eq!(runs[1].finished_at, None);

        let detail = r#"{"id":1001,"iid":7,"ref":"main","sha":"abc","status":"failed",
          "source":"push","web_url":"https://gl/p/1001","created_at":"2026-06-25T10:00:00Z",
          "updated_at":"2026-06-25T10:06:00Z"}"#;
        assert_eq!(parse_gitlab_pipeline_detail(detail).unwrap().id, "1001");

        let jobs = r#"[
          {"id":5001,"name":"build","stage":"build","status":"success",
           "started_at":"2026-06-25T10:00:10Z","finished_at":"2026-06-25T10:02:00Z",
           "web_url":"https://gl/j/5001"},
          {"id":5002,"name":"test","stage":"test","status":"failed",
           "started_at":"2026-06-25T10:02:00Z","finished_at":"2026-06-25T10:06:00Z",
           "web_url":"https://gl/j/5002"}
        ]"#;
        let jl = parse_gitlab_jobs(jobs).unwrap();
        assert_eq!(jl.len(), 2);
        assert_eq!(jl[0].name, "build");
        assert_eq!(jl[0].state, CiState::Pass);
        assert!(jl[0].steps.is_empty());
        assert_eq!(jl[1].state, CiState::Fail);
        assert!(parse_gitlab_jobs_with_discarded("nope").is_err());
        assert!(parse_gitlab_jobs_with_discarded("[]").unwrap().0.is_empty());
    }

    #[test]
    fn caps_are_set() {
        assert!(
            GithubCi {
                remote: None,
                cancel: Arc::new(AtomicBool::new(false))
            }
            .caps()
            .steps
        );
        assert!(!GitlabCi::default().caps().steps);
        assert!(
            GithubCi {
                remote: None,
                cancel: Arc::new(AtomicBool::new(false))
            }
            .caps()
            .trigger
        );
        assert!(GitlabCi::default().caps().cancel);
        // GitLab's pipeline `retry` can't scope to failed jobs; offering the
        // distinction anyway would silently retry everything.
        assert!(
            GithubCi {
                remote: None,
                cancel: Arc::new(AtomicBool::new(false))
            }
            .caps()
            .rerun_failed
        );
        assert!(!GitlabCi::default().caps().rerun_failed);
    }
}

#[cfg(test)]
mod kind_coverage_tests {
    use super::*;

    /// Every `CiProviderKind` value either builds a client or is `reserved`
    /// — the provider-seams rule, pinned for this seam. `auto`/`none` are
    /// selectors rather than providers and resolve per worktree, so they are
    /// exercised by `resolve_system` tests instead.
    #[test]
    fn every_ci_kind_is_implemented_or_reserved() {
        crate::seam::kind_coverage(|k: CiProviderKind| match k {
            CiProviderKind::Auto | CiProviderKind::None => Some(None),
            explicit => system_for_kind(explicit)
                .and_then(client_for_system)
                .map(Some),
        });
    }
}
