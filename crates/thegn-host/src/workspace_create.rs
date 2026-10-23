//! The Alt+W "new workspace" flow's non-loop halves: input classification
//! (path / URL / new-project), clone-by-URL, and the create+switch completion
//! shared by the typed prompt, the fuzzy picker, and the clone-finished drain
//! arm. Extracted from `run.rs` (god-file ratchet).

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::store::WorkspaceStore;

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::menu::{self, MenuOverlay};
use crate::panes::Panes;
use crate::run::{
    DrawerRuntime, ResidentWorkspace, SidebarState, WorkspacePool, persist_session_layout,
    refresh_tab_model, remap_cold_workspace_ids,
};

const MAX_PROGRESS_SEGMENT: usize = 512;
const CLONE_TOTAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CLONE_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const CLONE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const CLONE_TERMINATE_GRACE: Duration = Duration::from_millis(300);
const CLONE_POLL_INTERVAL: Duration = Duration::from_millis(50);
static CLONE_GENERATION: AtomicU64 = AtomicU64::new(0);

struct CloneJob {
    generation: u64,
    url: String,
    dest: std::path::PathBuf,
    cfg: thegn_core::config::Config,
    tx: tokio::sync::mpsc::UnboundedSender<CloneEvent>,
    waker: termwiz::terminal::TerminalWaker,
}

#[derive(Default)]
struct CloneSupervisorState {
    running: bool,
    pending: Option<CloneJob>,
}

static CLONE_SUPERVISOR: OnceLock<Mutex<CloneSupervisorState>> = OnceLock::new();
static CLONE_SUPERVISOR_CHANGED: OnceLock<Condvar> = OnceLock::new();
#[cfg(test)]
static CLONE_GENERATION_TEST_LOCK: Mutex<()> = Mutex::new(());

fn clone_supervisor_state() -> &'static Mutex<CloneSupervisorState> {
    CLONE_SUPERVISOR.get_or_init(|| Mutex::new(CloneSupervisorState::default()))
}

fn clone_supervisor_changed() -> &'static Condvar {
    CLONE_SUPERVISOR_CHANGED.get_or_init(Condvar::new)
}

fn wait_for_clone_supervisor(
    state: &Mutex<CloneSupervisorState>,
    changed: &Condvar,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while state.running {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let (next, result) = changed
            .wait_timeout(state, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = next;
        if result.timed_out() && state.running {
            return false;
        }
    }
    true
}

pub(crate) fn shutdown_clones(timeout: Duration) -> bool {
    CLONE_GENERATION.fetch_add(1, Ordering::SeqCst);
    {
        let mut state = clone_supervisor_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = None;
    }
    wait_for_clone_supervisor(
        clone_supervisor_state(),
        clone_supervisor_changed(),
        timeout,
    )
}

fn next_clone_generation() -> u64 {
    CLONE_GENERATION
        .fetch_add(1, Ordering::SeqCst)
        .wrapping_add(1)
}

fn clone_event_is_current(event_generation: u64, current_generation: u64) -> bool {
    event_generation == current_generation
}

fn send_clone_event_best_effort(
    tx: &tokio::sync::mpsc::UnboundedSender<CloneEvent>,
    event: CloneEvent,
) {
    if tx.send(event).is_err() {
        // The picker/loop may have gone away while the clone was running.
    }
}

fn wake_clone_loop_best_effort(waker: &termwiz::terminal::TerminalWaker) {
    if waker.wake().is_err() {
        // A wake pulse is only a nudge; the loop may already be shutting down.
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneStep {
    Continue,
    Terminate,
    Kill,
}

#[derive(Clone, Copy)]
struct CloneLimits {
    total: Duration,
    connect: Duration,
    idle: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloneStopReason {
    Cancelled,
    ConnectTimeout,
    IdleTimeout,
    TotalTimeout,
}

impl CloneStopReason {
    fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "git clone cancelled",
            Self::ConnectTimeout => "git clone timed out (no response from remote)",
            Self::IdleTimeout => "git clone timed out (stalled)",
            Self::TotalTimeout => "git clone timed out",
        }
    }
}

fn clone_stop_reason(
    now: Instant,
    started: Instant,
    limits: CloneLimits,
    last_output: Option<Instant>,
    cancelled: bool,
) -> Option<CloneStopReason> {
    if cancelled {
        Some(CloneStopReason::Cancelled)
    } else if now.saturating_duration_since(started) >= limits.total {
        Some(CloneStopReason::TotalTimeout)
    } else if let Some(last_output) = last_output {
        if now.saturating_duration_since(last_output) >= limits.idle {
            Some(CloneStopReason::IdleTimeout)
        } else {
            None
        }
    } else if now.saturating_duration_since(started) >= limits.connect {
        Some(CloneStopReason::ConnectTimeout)
    } else {
        None
    }
}

fn clone_step(
    now: Instant,
    started: Instant,
    limits: CloneLimits,
    last_output: Option<Instant>,
    cancelled: bool,
    term_sent_at: Option<Instant>,
) -> CloneStep {
    if let Some(sent) = term_sent_at {
        if now.duration_since(sent) >= CLONE_TERMINATE_GRACE {
            CloneStep::Kill
        } else {
            CloneStep::Continue
        }
    } else if clone_stop_reason(now, started, limits, last_output, cancelled).is_some() {
        CloneStep::Terminate
    } else {
        CloneStep::Continue
    }
}

struct ProgressThrottle {
    min_interval: Duration,
    last_emit: Option<Instant>,
    pending: Option<String>,
}

impl ProgressThrottle {
    fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_emit: None,
            pending: None,
        }
    }

    fn offer(&mut self, line: String, now: Instant) -> Option<String> {
        self.pending = Some(line);
        let ready = self
            .last_emit
            .is_none_or(|last| now.duration_since(last) >= self.min_interval);
        if ready {
            self.last_emit = Some(now);
            self.pending.take()
        } else {
            None
        }
    }

    fn flush(&mut self) -> Option<String> {
        self.pending.take()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedWorkspaceUrl {
    clone_url: String,
    canonical_origin: String,
    label: String,
    destination_name: String,
}

fn looks_like_git_url(input: &str) -> bool {
    input.starts_with("http://")
        || input.starts_with("https://")
        || input.starts_with("ssh://")
        || input.starts_with("git://")
        || input.starts_with("git@")
}

fn parse_workspace_url(input: &str) -> Result<ParsedWorkspaceUrl> {
    anyhow::ensure!(input.len() <= 2048, "git URL is too long");
    anyhow::ensure!(
        !input.chars().any(char::is_control) && !input.contains('\\'),
        "git URL contains a control character or platform separator"
    );
    let raw_path = if input.starts_with("git@") && !input.contains("://") {
        input
            .split_once(':')
            .context("invalid scp-style git URL")?
            .1
    } else {
        input
            .split_once("://")
            .context("unsupported git URL")?
            .1
            .split_once('/')
            .map(|(_, path)| path.split(['?', '#']).next().unwrap_or(""))
            .unwrap_or("")
    };
    let raw_path_lower = raw_path.to_ascii_lowercase();
    anyhow::ensure!(
        !raw_path.starts_with('/')
            && ![
                "%2f", "%5c", "%2e", "%25", "%00", "%01", "%02", "%03", "%04", "%05", "%06", "%07",
                "%08", "%09", "%0a", "%0b", "%0c", "%0d", "%0e", "%0f", "%10", "%11", "%12", "%13",
                "%14", "%15", "%16", "%17", "%18", "%19", "%1a", "%1b", "%1c", "%1d", "%1e", "%1f",
                "%7f"
            ]
            .iter()
            .any(|part| raw_path_lower.contains(part))
            && raw_path.trim_end_matches('/').split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.starts_with('/')
                    && !(part.len() == 2
                        && part.as_bytes()[0].is_ascii_alphabetic()
                        && part.ends_with(':'))
            }),
        "ambiguous or unsafe git URL path"
    );
    let (clone_url, canonical_origin, tail) = if input.starts_with("git@") && !input.contains("://")
    {
        let (authority, path) = input.split_once(':').context("invalid scp-style git URL")?;
        let host = authority
            .strip_prefix("git@")
            .context("invalid scp-style git URL")?;
        anyhow::ensure!(
            !host.is_empty() && !host.chars().any(|c| matches!(c, '/' | '\\' | '@')),
            "invalid git host"
        );
        (
            input.to_string(),
            format!(
                "ssh://{}/{}",
                host.to_lowercase(),
                path.trim_end_matches('/')
            ),
            path.rsplit('/').next().unwrap_or("").to_owned(),
        )
    } else {
        let parsed = url::Url::parse(input).context("unsupported git URL")?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https" | "ssh" | "git"),
            "unsupported git URL scheme"
        );
        anyhow::ensure!(parsed.host_str().is_some(), "git URL has no host");
        anyhow::ensure!(
            parsed.password().is_none(),
            "embedded URL credentials are not supported; use a git credential helper"
        );
        anyhow::ensure!(
            parsed.scheme() == "ssh" || parsed.username().is_empty(),
            "embedded URL credentials are not supported; use a git credential helper"
        );
        anyhow::ensure!(
            parsed.query().is_none() && parsed.fragment().is_none(),
            "git URL query and fragment are not supported"
        );
        let path = parsed.path().trim_end_matches('/');
        let encoded = path.to_ascii_lowercase();
        anyhow::ensure!(
            !path.is_empty()
                && !["%2f", "%5c", "%2e"]
                    .iter()
                    .any(|part| encoded.contains(part)),
            "ambiguous git URL path"
        );
        let tail = path.rsplit('/').next().unwrap_or("").to_owned();
        let clone_url = parsed.to_string();
        let mut canonical = parsed.clone();
        canonical
            .set_username("")
            .map_err(|_| anyhow::anyhow!("invalid git URL authority"))?;
        canonical.set_path(path);
        (clone_url, canonical.to_string(), tail)
    };
    let repo = tail.strip_suffix(".git").unwrap_or(tail.as_str());
    anyhow::ensure!(
        !repo.is_empty()
            && repo != "."
            && repo != ".."
            && !repo.chars().any(|c| matches!(c, '/' | '\\' | ':'))
            && !repo.starts_with('.')
            && !repo.ends_with('.')
            && repo.len() <= 128,
        "unsafe or overlong repository name in git URL"
    );
    let canonical_origin = canonical_origin
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_owned();
    let slug: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "repo" } else { slug };
    use sha2::Digest;
    let digest = sha2::Sha256::digest(canonical_origin.as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(ParsedWorkspaceUrl {
        clone_url,
        canonical_origin,
        label: repo.to_string(),
        destination_name: format!("{slug}-{suffix}"),
    })
}

fn configured_origin(dest: &std::path::Path, remote_name: &str) -> Result<Option<String>> {
    let config = std::fs::read_to_string(dest.join(".git/config"))?;
    let remote_section = format!("remote \"{remote_name}\"");
    let mut in_remote = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_remote = line[1..line.len() - 1].trim() == remote_section;
        } else if in_remote
            && let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            return Ok(parse_workspace_url(value)
                .ok()
                .map(|parsed| parsed.canonical_origin));
        }
    }
    Ok(None)
}

fn configured_clone_remote_name(dest: &std::path::Path) -> Result<String> {
    let mut command = thegn_core::util::git_cmd(dest);
    command
        .arg("config")
        .arg("--get")
        .arg("clone.defaultRemoteName")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let (mut child, _group) = crate::platform::spawn_grouped(&mut command)
        .context("read configured clone remote name")?;
    let mut output = String::new();
    let read_result = if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output)
            .context("read configured clone remote name output")
    } else {
        Ok(0)
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "wait for the owned git config child"
    )]
    let status = child
        .wait()
        .context("wait for configured clone remote name")?;
    read_result?;
    if status.success() {
        let name = output.trim();
        anyhow::ensure!(
            !name.is_empty() && !name.contains(['[', ']', '\n', '\r']),
            "configured clone.defaultRemoteName is invalid"
        );
        Ok(name.to_string())
    } else if status.code() == Some(1) {
        Ok("origin".to_string())
    } else {
        anyhow::bail!("could not read clone.defaultRemoteName")
    }
}

fn existing_origin_matches(dest: &std::path::Path, expected: &str) -> bool {
    dest.join(".git").is_dir()
        && std::fs::symlink_metadata(dest.join(".git/HEAD"))
            .is_ok_and(|metadata| metadata.file_type().is_file())
        && configured_origin(dest, "origin")
            .ok()
            .flatten()
            .is_some_and(|origin| origin == expected)
}

fn has_expected_origin_without_head(dest: &std::path::Path, expected: &str) -> bool {
    configured_origin(dest, "origin")
        .ok()
        .flatten()
        .is_some_and(|origin| origin == expected)
        && !std::fs::symlink_metadata(dest.join(".git/HEAD"))
            .is_ok_and(|metadata| metadata.file_type().is_file())
}

fn workspace_repo_display(input: &str) -> String {
    parse_workspace_url(input)
        .map(|parsed| parsed.label)
        .unwrap_or_else(|_| "repository".into())
}

pub(crate) enum WorkspaceResolution {
    Repo(std::path::PathBuf),
    NotARepo(std::path::PathBuf),
}

/// Where a cloned-by-URL workspace lands (`[ui].workspaces_dir` / repo name);
/// `None` for local-path inputs.
pub(crate) fn workspace_clone_dest(
    input: &str,
    cfg: &thegn_core::config::Config,
) -> Option<std::path::PathBuf> {
    parse_workspace_url(input).ok().map(|parsed| {
        std::path::PathBuf::from(thegn_core::util::expand_tilde(&cfg.workspaces_dir))
            .join(parsed.destination_name)
    })
}

fn clone_workspace_repo_with_deadline(
    url: &str,
    dest: &std::path::Path,
    generation: u64,
    expected_origin: Option<&str>,
    limits: CloneLimits,
    mut cancelled: impl FnMut() -> bool,
    on_progress: impl FnMut(String) + Send + 'static,
) -> Result<()> {
    let mut staging = None;
    let mut parent_pin = None;
    let result = (|| {
        anyhow::ensure!(!cancelled(), "git clone cancelled");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let parent_path = dest.parent().unwrap_or(std::path::Path::new("."));
        let parent = std::fs::canonicalize(parent_path)
            .with_context(|| format!("canonicalize workspace parent {}", parent_path.display()))?;
        let pin = &*parent_pin.insert(thegn_core::fsperm::open_rooted_dir(&parent).with_context(
            || {
                format!(
                    "pin workspace parent {} (the parent directory must be owned by you)",
                    parent.display()
                )
            },
        )?);
        let dest_name = dest
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("workspace"))
            .to_str()
            .context("workspace destination name must be valid UTF-8")?;
        let name = dest_name;
        let reclaimed = reclaim_stale_clone_staging(
            &parent,
            name,
            std::process::id(),
            crate::platform::pid_alive,
        );
        if reclaimed > 0 {
            tracing::debug!(reclaimed, path = %parent.display(), "reclaimed stale workspace clone staging");
        }
        // Leftovers from a crashed run are reclaimed here before staging is checked.
        let staging_name = format!(".{name}.clone-{generation}-{}", std::process::id());
        let staging_path = parent.join(&staging_name);
        match std::fs::symlink_metadata(&staging_path) {
            Ok(_) => anyhow::bail!(
                "workspace clone staging path already exists: {}",
                staging_path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspect workspace clone staging path {}",
                        staging_path.display()
                    )
                });
            }
        }
        thegn_core::fsperm::ensure_private_dir(&staging_path).with_context(|| {
            format!(
                "create private workspace clone staging directory {}",
                staging_path.display()
            )
        })?;
        staging = Some(staging_path.clone());
        // Via the scrubbed git_cmd so a stray GIT_DIR can't redirect the
        // clone; the staging path is absolute, so the `-C` only sets the working dir.
        let cwd = &parent;
        // `--progress` forces git to emit its transfer counters even though stderr
        // is a pipe, not a TTY; we stream them to the picker's cloning panel.
        let mut command = thegn_core::util::git_cmd(cwd);
        command
            .arg("clone")
            .arg("--progress")
            .arg(url)
            .arg(&staging_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let (mut child, group) = crate::platform::spawn_grouped(&mut command)
            .with_context(|| format!("git clone into {}", staging_path.display()))?;
        let started = Instant::now();
        let last_output_ms = std::sync::Arc::new(AtomicU64::new(0));
        let mut reader = if let Some(stderr) = child.stderr.take() {
            let reader_last_output_ms = last_output_ms.clone();
            match std::thread::Builder::new()
                .name("thegn-clone-prg".into())
                .spawn(move || {
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
                    stream_progress_lines_with_activity(
                        std::io::BufReader::new(stderr),
                        on_progress,
                        || {
                            let millis = u64::try_from(started.elapsed().as_millis())
                                .unwrap_or(u64::MAX)
                                .saturating_add(1);
                            reader_last_output_ms.store(millis, Ordering::Relaxed);
                        },
                    );
                }) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    group.kill();
                    #[expect(clippy::disallowed_methods)]
                    if let Err(wait_error) = child.wait() {
                        return Err(wait_error).context("reap clone after reader spawn failure");
                    }
                    return Err(error).context("spawn workspace clone progress reader");
                }
            }
        } else {
            None
        };
        let mut term_sent_at = None;
        let mut stop_reason = None;
        let mut status = None;
        loop {
            let polled = match child.try_wait() {
                Ok(polled) => polled,
                Err(error) => {
                    group.kill();
                    #[expect(clippy::disallowed_methods)]
                    if let Err(wait_error) = child.wait() {
                        return Err(wait_error).context("reap clone after poll failure");
                    }
                    if let Some(reader) = reader.take() {
                        reader.join().map_err(|_| {
                            anyhow::anyhow!("workspace clone progress reader panicked")
                        })?;
                    }
                    return Err(error).context("poll git clone");
                }
            };
            if let Some(exit) = polled {
                status = Some(exit);
                // A helper may keep stderr open after git exits. Reap the whole
                // owned group so the progress reader reaches EOF as well.
                if !group.is_empty() {
                    group.terminate();
                }
                break;
            }
            let elapsed_ms = last_output_ms.load(Ordering::Relaxed);
            let last_output = (elapsed_ms != 0)
                .then(|| started + Duration::from_millis(elapsed_ms.saturating_sub(1)));
            let now = Instant::now();
            let is_cancelled = cancelled();
            let expired_reason = clone_stop_reason(now, started, limits, last_output, is_cancelled);
            match clone_step(
                now,
                started,
                limits,
                last_output,
                is_cancelled,
                term_sent_at,
            ) {
                CloneStep::Continue => std::thread::sleep(CLONE_POLL_INTERVAL),
                CloneStep::Terminate => {
                    stop_reason = expired_reason;
                    group.terminate();
                    term_sent_at = Some(Instant::now());
                }
                CloneStep::Kill => {
                    group.kill();
                    break;
                }
            }
        }
        if status.is_none() {
            // The child may have exited while the group was being killed.
            #[expect(clippy::disallowed_methods)]
            let exit = child.wait().context("wait for terminated git clone")?;
            status = Some(exit);
        }
        // Ensure no pipe-holding descendant survives a completed git process.
        let reap_deadline = Instant::now() + CLONE_TERMINATE_GRACE;
        while !group.is_empty() && Instant::now() < reap_deadline {
            std::thread::sleep(CLONE_POLL_INTERVAL);
        }
        if !group.is_empty() {
            group.kill();
        }
        let group_reap_deadline = Instant::now() + Duration::from_secs(2);
        while !group.is_empty() && Instant::now() < group_reap_deadline {
            std::thread::sleep(CLONE_POLL_INTERVAL);
        }
        anyhow::ensure!(group.is_empty(), "git clone process group did not exit");
        if let Some(reader) = reader.take() {
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("workspace clone progress reader panicked"))?;
        }
        let status = status.expect("clone loop always records or waits for child status");
        if let Some(reason) = stop_reason
            && (reason == CloneStopReason::Cancelled || !status.success())
        {
            anyhow::bail!("{}", reason.message());
        }
        if !status.success() {
            anyhow::bail!("git clone failed");
        }
        validate_staged_clone(&staging_path, expected_origin)?;
        staging = None;
        promote_staged_clone(pin, &staging_name, name)?;
        Ok(())
    })();
    if result.is_err()
        && let Some(staging) = staging
        && let Some(pin) = parent_pin.as_ref()
    {
        match pin.same_dir_as(pin.path()) {
            Ok(true) => remove_partial_clone_dest(&staging, false),
            Ok(false) => tracing::debug!(
                path = %staging.display(),
                "workspace clone parent was replaced; leaving staging for startup sweep"
            ),
            Err(error) => tracing::debug!(
                path = %staging.display(),
                %error,
                "could not verify workspace clone parent; leaving staging for startup sweep"
            ),
        }
    }
    result
}

fn validate_staged_clone(staging: &std::path::Path, expected_origin: Option<&str>) -> Result<()> {
    let head = std::fs::symlink_metadata(staging.join(".git/HEAD"))
        .with_context(|| format!("staged clone has no .git/HEAD: {}", staging.display()))?;
    anyhow::ensure!(
        head.file_type().is_file(),
        "staged clone .git/HEAD is not a regular file: {}",
        staging.display()
    );
    let mut command = thegn_core::util::git_cmd(staging);
    command
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let (mut child, _group) =
        crate::platform::spawn_grouped(&mut command).context("validate staged clone worktree")?;
    let mut output = String::new();
    let read_result = if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output)
            .context("read staged clone validation output")
    } else {
        Ok(0)
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "wait for the owned validation git child"
    )]
    let status = child.wait().context("wait for staged clone validation")?;
    read_result?;
    anyhow::ensure!(
        status.success() && output.trim() == "true",
        "staged clone is not a git work tree: {}",
        staging.display()
    );
    if let Some(expected) = expected_origin {
        let remote_name = configured_clone_remote_name(staging)?;
        anyhow::ensure!(
            configured_origin(staging, &remote_name)?.as_deref() == Some(expected),
            "staged clone origin does not match the requested origin: {}",
            staging.display()
        );
    }
    Ok(())
}

fn promote_staged_clone(
    pin: &thegn_core::fsperm::RootedDir,
    staging_name: &str,
    dest_name: &str,
) -> Result<()> {
    let staging = pin.path().join(staging_name);
    let dest = pin.path().join(dest_name);
    let result = (|| {
        anyhow::ensure!(
            pin.same_dir_as(pin.path())?,
            "workspace parent {} was replaced while cloning; not promoted",
            pin.path().display()
        );
        match std::fs::symlink_metadata(&dest) {
            Ok(_) => anyhow::bail!(
                "workspace destination appeared while cloning: {}; not replaced",
                dest.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect workspace destination {}", dest.display()));
            }
        }
        thegn_core::fsperm::restore_normal_directory_mode(&staging)
            .with_context(|| format!("restore workspace directory mode {}", staging.display()))?;
        match pin.rename_child_noreplace(staging_name, dest_name) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                anyhow::bail!(
                    "workspace destination appeared while cloning: {}; not replaced",
                    dest.display()
                );
            }
            Err(error) => {
                Err(error).with_context(|| format!("promote staged clone to {}", dest.display()))
            }
        }
    })();
    if result.is_err() {
        match pin.same_dir_as(pin.path()) {
            Ok(true) => remove_partial_clone_dest(&staging, false),
            Ok(false) => tracing::debug!(
                path = %staging.display(),
                "workspace clone parent was replaced; leaving staging for startup sweep"
            ),
            Err(error) => tracing::debug!(
                path = %staging.display(),
                %error,
                "could not verify workspace clone parent; leaving staging for startup sweep"
            ),
        }
    }
    result
}

/// Reclaim exact-name staging directories from dead clone processes, but only
/// when they are real directories owned by the current user. A live or reused
/// pid is kept because deleting another active clone is unsafe.
fn reclaim_stale_clone_staging(
    parent: &std::path::Path,
    name: &str,
    own_pid: u32,
    pid_alive: impl Fn(i64) -> bool,
) -> usize {
    reclaim_clone_staging(parent, Some(name), own_pid, pid_alive)
}

/// Reclaim dead-pid clone staging directories for every workspace name.
pub(crate) fn sweep_stale_clone_staging(
    workspaces_dir: &std::path::Path,
    own_pid: u32,
    pid_alive: impl Fn(i64) -> bool,
) -> usize {
    reclaim_clone_staging(workspaces_dir, None, own_pid, pid_alive)
}

fn reclaim_clone_staging(
    parent: &std::path::Path,
    name: Option<&str>,
    own_pid: u32,
    pid_alive: impl Fn(i64) -> bool,
) -> usize {
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, path = %parent.display(), "reclaim stale workspace clone staging");
            return 0;
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(%error, path = %parent.display(), "reclaim stale workspace clone staging");
                continue;
            }
        };
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(pid) = stale_staging_pid(file_name, name) else {
            continue;
        };
        if pid == own_pid || pid_alive(i64::from(pid)) {
            continue;
        }
        let path = entry.path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "reclaim stale workspace clone staging");
                continue;
            }
        };
        let file_type = metadata.file_type();
        if !file_type.is_dir()
            || file_type.is_symlink()
            || !thegn_core::fsperm::owned_by_current_user(&metadata)
        {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::debug!(%error, path = %path.display(), "reclaim stale workspace clone staging");
            }
        }
    }
    removed
}

fn stale_staging_pid(file_name: &str, name: Option<&str>) -> Option<u32> {
    let remainder = if let Some(name) = name {
        file_name.strip_prefix(&format!(".{name}.clone-"))?
    } else {
        let marker = file_name.rfind(".clone-")?;
        if !file_name.starts_with('.') || marker <= 1 {
            return None;
        }
        &file_name[marker + ".clone-".len()..]
    };
    let (generation, pid) = remainder.rsplit_once('-')?;
    if generation.is_empty()
        || pid.is_empty()
        || !generation.bytes().all(|byte| byte.is_ascii_digit())
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    if generation.parse::<u64>().is_err() {
        return None;
    }
    pid.parse::<u32>().ok()
}

fn remove_partial_clone_dest(dest: &std::path::Path, dest_existed: bool) {
    if dest_existed {
        return;
    }
    match std::fs::symlink_metadata(dest) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            if let Err(error) = std::fs::remove_dir_all(dest) {
                tracing::debug!(%error, path = %dest.display(), "remove partial workspace clone destination");
            }
        }
        Ok(_) | Err(_) => {}
    }
}

/// Read `git`'s progress stream, calling `on_line` with each finished segment.
/// git rewrites its progress counter in place with a carriage return, so we
/// break on `\r` *and* `\n` (trimming, dropping empties) rather than by line.
#[cfg(test)]
pub(crate) fn stream_progress_lines<R: std::io::BufRead>(reader: R, on_line: impl FnMut(String)) {
    stream_progress_lines_with_activity(reader, on_line, || {});
}

fn stream_progress_lines_with_activity<R: std::io::BufRead>(
    reader: R,
    mut on_line: impl FnMut(String),
    mut on_activity: impl FnMut(),
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut first_segment = true;
    const CLONE_BANNER: &[u8] = b"Cloning into ";
    let flush = |buf: &mut Vec<u8>, on_line: &mut dyn FnMut(String)| {
        let decoded = String::from_utf8_lossy(buf);
        let clean: String = decoded.chars().filter(|ch| !ch.is_control()).collect();
        let s = thegn_core::ci_log::redact(clean.trim());
        buf.clear();
        if !s.is_empty() {
            on_line(s);
        }
    };
    for byte in reader.bytes() {
        let Ok(b) = byte else { break };
        if b == b'\r' || b == b'\n' {
            if first_segment && !buf.is_empty() && !buf.starts_with(CLONE_BANNER) {
                on_activity();
            }
            flush(&mut buf, &mut on_line);
            first_segment = false;
        } else if buf.len() < MAX_PROGRESS_SEGMENT {
            buf.push(b);
            if !first_segment || (!CLONE_BANNER.starts_with(&buf) && !buf.starts_with(CLONE_BANNER))
            {
                on_activity();
            }
        }
    }
    flush(&mut buf, &mut on_line);
}

/// Result of a background workspace clone, delivered to the loop's
/// `workspace_clone_rx` drain arm.
pub(crate) struct WorkspaceCloneOutcome {
    pub(crate) generation: u64,
    pub(crate) url: String,
    pub(crate) dest: std::path::PathBuf,
    pub(crate) result: Result<()>,
}

/// What the background clone sends over `workspace_clone_rx`: streamed progress
/// lines while it runs, then a terminal `Done`.
pub(crate) enum CloneEvent {
    Progress {
        generation: u64,
        url: String,
        line: String,
    },
    Done(WorkspaceCloneOutcome),
}

/// What a manual new-workspace input resolves to. Pure classification — the
/// loop maps each variant onto the matching flow (off-loop clone, create+
/// switch, or a create-project confirm).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubmitPlan {
    /// A git URL whose clone dest isn't on disk yet → clone OFF the loop.
    Clone {
        url: String,
        dest: std::path::PathBuf,
    },
    /// An existing local path (or an already-cloned URL dest): repo →
    /// create+switch; non-repo dir → the git-init offer.
    Local(String),
    /// A path that doesn't exist but whose PARENT does — a new project one
    /// level below the present tree → confirm, then mkdir + git init.
    CreateNew {
        leaf: std::path::PathBuf,
    },
    Invalid(String),
}

/// Classify a typed new-workspace input (path / URL / new-project leaf).
pub(crate) fn plan_new_workspace_input(
    input: &str,
    cfg: &thegn_core::config::Config,
) -> SubmitPlan {
    let input = input.trim();
    if input.is_empty() {
        return SubmitPlan::Invalid("no project path or URL given".into());
    }
    if looks_like_git_url(input) {
        let parsed = match parse_workspace_url(input) {
            Ok(parsed) => parsed,
            Err(error) => return SubmitPlan::Invalid(error.to_string()),
        };
        let dest = workspace_clone_dest(input, cfg).expect("validated URL has a clone destination");
        if dest.exists() {
            if existing_origin_matches(&dest, &parsed.canonical_origin) {
                return SubmitPlan::Local(input.to_string());
            }
            if has_expected_origin_without_head(&dest, &parsed.canonical_origin) {
                return SubmitPlan::Invalid(format!(
                    "workspace destination is an incomplete clone (no HEAD): {}; remove it and retry",
                    dest.display()
                ));
            }
            return SubmitPlan::Invalid(format!(
                "workspace destination already exists with a different or unreadable origin: {}",
                dest.display()
            ));
        }
        let legacy = std::path::PathBuf::from(thegn_core::util::expand_tilde(&cfg.workspaces_dir))
            .join(&parsed.label);
        if legacy.exists() {
            if existing_origin_matches(&legacy, &parsed.canonical_origin) {
                return SubmitPlan::Local(input.to_string());
            }
            if has_expected_origin_without_head(&legacy, &parsed.canonical_origin) {
                return SubmitPlan::Invalid(format!(
                    "workspace destination is an incomplete clone (no HEAD): {}; remove it and retry",
                    legacy.display()
                ));
            }
            return SubmitPlan::Invalid(format!(
                "workspace destination already exists with a different or unreadable origin: {}",
                legacy.display()
            ));
        }
        return SubmitPlan::Clone {
            url: parsed.clone_url,
            dest,
        };
    }
    let expanded = thegn_core::util::expand_tilde(input);
    let path = std::path::PathBuf::from(expanded);
    let path = if path.is_absolute() {
        path
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        return SubmitPlan::Invalid("cannot resolve relative path".into());
    };
    if path.is_dir() {
        SubmitPlan::Local(input.to_string())
    } else if !path.exists() && path.parent().is_some_and(|p| p.is_dir()) {
        SubmitPlan::CreateNew { leaf: path }
    } else {
        SubmitPlan::Invalid(format!("path does not exist: {}", path.display()))
    }
}

/// mkdir the leaf (its parent is verified to exist — new projects only nest
/// one level below the present tree) + `git init`. Runs on an explicit user
/// confirm; ms-scale like the ConfirmInitGit arm.
pub(crate) fn init_new_project(leaf: &std::path::Path) -> Result<()> {
    anyhow::ensure!(
        leaf.parent().is_some_and(|p| p.is_dir()),
        "parent directory of {} does not exist",
        leaf.display()
    );
    std::fs::create_dir(leaf).with_context(|| format!("create {}", leaf.display()))?;
    // Accepted on-loop subprocess: `git init` on a local path is ms-scale and
    // runs on an explicit user confirm (same stance as the ConfirmInitGit arm).
    #[expect(clippy::disallowed_methods)]
    let status = thegn_core::util::git_cmd(leaf)
        .arg("init")
        // Null stdio: the compositor owns the alt-screen, so git's
        // "Initialized empty Git repository in …" on the inherited stdout would
        // scroll it and drift the render baseline. Only the exit code matters.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("git init {}", leaf.display()))?;
    anyhow::ensure!(status.success(), "git init failed for {}", leaf.display());
    Ok(())
}

/// Submit a clone to the single blocking supervisor; the loop completes the
/// create+switch in its `workspace_clone_rx` drain arm.
pub(crate) fn spawn_workspace_clone(
    generation: u64,
    url: String,
    dest: std::path::PathBuf,
    cfg: thegn_core::config::Config,
    tx: tokio::sync::mpsc::UnboundedSender<CloneEvent>,
    waker: termwiz::terminal::TerminalWaker,
) {
    let should_start = {
        let mut state = clone_supervisor_state()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(CloneJob {
            generation,
            url,
            dest,
            cfg,
            tx,
            waker,
        });
        if state.running {
            false
        } else {
            state.running = true;
            true
        }
    };
    if should_start {
        tokio::task::spawn_blocking(run_clone_supervisor);
    }
}

fn run_clone_supervisor() {
    loop {
        let job = {
            let mut state = clone_supervisor_state()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match state.pending.take() {
                Some(job) => job,
                None => {
                    state.running = false;
                    clone_supervisor_changed().notify_all();
                    return;
                }
            }
        };
        let generation = job.generation;
        let url = job.url.clone();
        let dest = job.dest.clone();
        let tx = job.tx.clone();
        let waker = job.waker.clone();
        if catch_clone_job(|| run_clone_job(job)).is_err() {
            send_clone_event_best_effort(
                &tx,
                CloneEvent::Done(WorkspaceCloneOutcome {
                    generation,
                    url,
                    dest,
                    result: Err(anyhow::anyhow!("workspace clone job panicked")),
                }),
            );
            wake_clone_loop_best_effort(&waker);
        }
    }
}

fn catch_clone_job(run: impl FnOnce()) -> std::thread::Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
}

fn run_clone_job(job: CloneJob) {
    let CloneJob {
        generation,
        url,
        dest,
        cfg,
        tx,
        waker,
    } = job;
    let progress_url = url.clone();
    let reader_url = progress_url.clone();
    let progress_tx = tx.clone();
    let progress_waker = waker.clone();
    let throttle = std::sync::Arc::new(std::sync::Mutex::new(ProgressThrottle::new(
        Duration::from_millis(100),
    )));
    let reader_throttle = throttle.clone();
    let reader_tx = progress_tx.clone();
    let reader_waker = progress_waker.clone();
    let result = parse_workspace_url(&url).and_then(|parsed| {
        clone_workspace_repo_with_deadline(
            &url,
            &dest,
            generation,
            Some(&parsed.canonical_origin),
            CloneLimits {
                total: CLONE_TOTAL_TIMEOUT,
                connect: CLONE_CONNECT_TIMEOUT,
                idle: CLONE_IDLE_TIMEOUT,
            },
            || !clone_event_is_current(generation, CLONE_GENERATION.load(Ordering::SeqCst)),
            move |line| {
                let pending = reader_throttle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .offer(line, Instant::now());
                if let Some(line) = pending {
                    send_clone_event_best_effort(
                        &reader_tx,
                        CloneEvent::Progress {
                            generation,
                            url: reader_url.clone(),
                            line,
                        },
                    );
                    wake_clone_loop_best_effort(&reader_waker);
                }
            },
        )
    });
    let pending = throttle
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .flush();
    if let Some(line) = pending {
        // best-effort: send the newest coalesced progress before terminal outcome
        send_clone_event_best_effort(
            &progress_tx,
            CloneEvent::Progress {
                generation,
                url: progress_url.clone(),
                line,
            },
        );
        wake_clone_loop_best_effort(&progress_waker);
    }
    if result.is_ok()
        && clone_event_is_current(generation, CLONE_GENERATION.load(Ordering::SeqCst))
        && let Err(error) = crate::git_worktree::initialize(&cfg, &dest, &dest, None)
    {
        send_clone_event_best_effort(
            &progress_tx,
            CloneEvent::Progress {
                generation,
                url: progress_url,
                line: thegn_core::ci_log::redact(&error.to_string()),
            },
        );
        wake_clone_loop_best_effort(&progress_waker);
    }
    send_clone_event_best_effort(
        &tx,
        CloneEvent::Done(WorkspaceCloneOutcome {
            generation,
            url,
            dest,
            result,
        }),
    );
    wake_clone_loop_best_effort(&waker);
}

pub(crate) fn create_workspace_from_input_with_config(
    input: &str,
    session: &mut crate::session::Session,
    db: &thegn_core::db::Db,
    cfg: &thegn_core::config::Config,
) -> Result<WorkspaceResolution> {
    let input = input.trim();
    anyhow::ensure!(!input.is_empty(), "no project path or URL given");

    let root = if let Ok(parsed) = parse_workspace_url(input) {
        let workspaces_dir =
            std::path::PathBuf::from(thegn_core::util::expand_tilde(&cfg.workspaces_dir));
        let dest = workspace_clone_dest(input, cfg).expect("parsed URL has a clone destination");
        let legacy = workspaces_dir.join(&parsed.label);
        let dest = if !dest.exists() && existing_origin_matches(&legacy, &parsed.canonical_origin) {
            legacy
        } else {
            dest
        };
        // URL inputs are cloned OFF the loop before this runs (NewWorkspace
        // handler → spawn_blocking → workspace_clone_rx drain); by the time
        // this is called the clone must already be on disk.
        anyhow::ensure!(
            dest.exists(),
            "clone of {input} not materialized at {} (the clone runs off-loop first)",
            dest.display()
        );
        std::fs::canonicalize(&dest).unwrap_or(dest)
    } else {
        let expanded = thegn_core::util::expand_tilde(input);
        let path = std::path::PathBuf::from(expanded);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        };
        anyhow::ensure!(path.is_dir(), "path does not exist: {}", path.display());
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        thegn_core::repo::main_worktree(&canonical).unwrap_or(canonical)
    };

    let root_s = root.to_string_lossy().into_owned();
    let name = parse_workspace_url(input)
        .map(|parsed| parsed.label)
        .unwrap_or_else(|_| {
            root.file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "workspace".into())
        });
    let kind = if thegn_core::repo::main_worktree(&root).is_some() {
        "repo"
    } else {
        return Ok(WorkspaceResolution::NotARepo(root));
    };
    db.put_workspace(&root_s, &name, kind)?;
    db.touch_repo(&root_s, &name)?;
    session.switch_to_workspace(&root_s, db)?;
    Ok(WorkspaceResolution::Repo(root))
}

/// Create + switch to a workspace for a LOCAL `input` (a path, or the dest of
/// a finished URL clone): park the outgoing workspace in the pool, resolve
/// and persist the new one, and sync chrome state. Never clones — URL inputs
/// go through the off-loop clone first (see `WorkspaceCloneOutcome`).
/// Returns whether a relayout is needed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn complete_workspace_create(
    input: &str,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    workspace_pool: &mut WorkspacePool,
    active_menu: &mut Option<MenuOverlay>,
    focus: &mut crate::focus::FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    drawer_runtime: &mut DrawerRuntime,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> bool {
    persist_session_layout(session, panes);
    let prev_id = session.id.clone();
    let snapshot = ResidentWorkspace {
        worktrees: session.worktrees.clone(),
        active: session.active,
    };
    match thegn_core::db::Db::open()
        .context("open thegn db")
        .and_then(|db| create_workspace_from_input_with_config(input, session, &db, cfg))
    {
        Ok(WorkspaceResolution::Repo(path)) => {
            workspace_pool.stash(prev_id, snapshot, panes);
            remap_cold_workspace_ids(session, panes);
            focus.zone = crate::focus::Zone::Center;
            refresh_tab_model(model, session, sb);
            model.status = format!("project created: {}", path.display());
            if let Some(dir) = crate::run::active_cwd(session) {
                drawer_runtime.reconcile(cfg, &dir, panes, center);
            }
            true
        }
        Ok(WorkspaceResolution::NotARepo(path)) => {
            *active_menu = Some(menu::init_git_menu(path.to_string_lossy().into_owned()));
            false
        }
        Err(e) => {
            model.status = format!("project create failed: {e}");
            false
        }
    }
}

/// Apply one `CloneEvent` from the off-loop clone to the picker: stream a
/// progress line into its cloning panel, or on `Done` complete+switch (closing
/// the picker) / park it in a visible error. Ignores results for a clone the
/// user has since cancelled or moved past. Returns whether the loop should
/// relayout + re-hydrate (a workspace was created).
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_clone_event(
    ev: CloneEvent,
    picker: &mut Option<crate::workspace_picker::WorkspacePicker>,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    workspace_pool: &mut WorkspacePool,
    active_menu: &mut Option<MenuOverlay>,
    focus: &mut crate::focus::FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    drawer_runtime: &mut DrawerRuntime,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> bool {
    match ev {
        CloneEvent::Progress {
            generation,
            url,
            line,
        } => {
            if clone_event_is_current(generation, CLONE_GENERATION.load(Ordering::SeqCst))
                && let Some(p) = picker.as_mut()
                && p.cloning_url() == Some(url.as_str())
            {
                p.set_clone_progress(line);
            }
            false
        }
        CloneEvent::Done(o) => {
            // Only honor a result the picker is still awaiting — a cancel (or a
            // move to another clone) abandons it rather than yanking the user
            // into a workspace they dismissed.
            let awaited =
                clone_event_is_current(o.generation, CLONE_GENERATION.load(Ordering::SeqCst))
                    && picker
                        .as_ref()
                        .is_some_and(|p| p.cloning_url() == Some(o.url.as_str()));
            if !awaited {
                return false;
            }
            match o.result {
                Ok(()) => {
                    *picker = None;
                    let dest = o.dest.to_string_lossy().into_owned();
                    complete_workspace_create(
                        &dest,
                        session,
                        panes,
                        workspace_pool,
                        active_menu,
                        focus,
                        model,
                        sb,
                        drawer_runtime,
                        cfg,
                        center,
                    )
                }
                Err(e) => {
                    if let Some(p) = picker.as_mut() {
                        p.set_error(format!(
                            "clone failed: {}",
                            thegn_core::ci_log::redact(&e.to_string())
                        ));
                    }
                    false
                }
            }
        }
    }
}

/// Map a [`crate::workspace_picker::PickerOutcome`] onto the shared workspace
/// flows: cancel, open a browsed repo, or classify+dispatch a manual input
/// (off-loop clone / local create+switch / new-project confirm / inline error).
/// Owns the picker's lifecycle so it stays on screen through a clone or a bad
/// input instead of vanishing. Returns whether the loop should relayout +
/// re-hydrate (a workspace was created).
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_picker_outcome(
    outcome: crate::workspace_picker::PickerOutcome,
    picker: &mut Option<crate::workspace_picker::WorkspacePicker>,
    clone_tx: &tokio::sync::mpsc::UnboundedSender<CloneEvent>,
    waker: &termwiz::terminal::TerminalWaker,
    session: &mut crate::session::Session,
    panes: &mut Panes,
    workspace_pool: &mut WorkspacePool,
    active_menu: &mut Option<MenuOverlay>,
    focus: &mut crate::focus::FocusState,
    model: &mut FrameModel,
    sb: &mut SidebarState,
    drawer_runtime: &mut DrawerRuntime,
    cfg: &thegn_core::config::Config,
    center: Rect,
) -> bool {
    use crate::workspace_picker::PickerOutcome;
    let create = |input: &str,
                  picker: &mut Option<crate::workspace_picker::WorkspacePicker>,
                  session: &mut crate::session::Session,
                  panes: &mut Panes,
                  workspace_pool: &mut WorkspacePool,
                  active_menu: &mut Option<MenuOverlay>,
                  focus: &mut crate::focus::FocusState,
                  model: &mut FrameModel,
                  sb: &mut SidebarState,
                  drawer_runtime: &mut DrawerRuntime| {
        *picker = None;
        complete_workspace_create(
            input,
            session,
            panes,
            workspace_pool,
            active_menu,
            focus,
            model,
            sb,
            drawer_runtime,
            cfg,
            center,
        )
    };
    match outcome {
        PickerOutcome::Pending => false,
        PickerOutcome::Cancel => {
            next_clone_generation();
            *picker = None;
            model.status = "project creation cancelled".into();
            false
        }
        PickerOutcome::OpenRepo(path) => {
            next_clone_generation();
            create(
                &path,
                picker,
                session,
                panes,
                workspace_pool,
                active_menu,
                focus,
                model,
                sb,
                drawer_runtime,
            )
        }
        PickerOutcome::Manual(input) => match plan_new_workspace_input(&input, cfg) {
            SubmitPlan::Clone { url, dest } => {
                // Keep the picker on screen in its cloning phase — apply_clone_event
                // streams progress and closes it (or shows an error) on done.
                model.status = format!("Cloning {}…", workspace_repo_display(&url));
                if let Some(p) = picker.as_mut() {
                    p.begin_clone(url.clone());
                }
                let generation = next_clone_generation();
                spawn_workspace_clone(
                    generation,
                    url,
                    dest,
                    cfg.clone(),
                    clone_tx.clone(),
                    waker.clone(),
                );
                false
            }
            SubmitPlan::Local(input) => {
                next_clone_generation();
                create(
                    &input,
                    picker,
                    session,
                    panes,
                    workspace_pool,
                    active_menu,
                    focus,
                    model,
                    sb,
                    drawer_runtime,
                )
            }
            SubmitPlan::CreateNew { leaf } => {
                next_clone_generation();
                *picker = None;
                *active_menu = Some(menu::create_project_menu(
                    leaf.to_string_lossy().into_owned(),
                ));
                false
            }
            SubmitPlan::Invalid(msg) => {
                next_clone_generation();
                // Stay open with the error inline so the input can be fixed.
                if let Some(p) = picker.as_mut() {
                    p.set_error(msg);
                }
                false
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tg-wscreate-{}-{}", std::process::id(), name))
    }

    #[test]
    fn progress_lines_split_on_cr_and_lf() {
        // git rewrites the transfer counter in place with '\r', then ends the
        // final line with '\n'. Each rewrite is a distinct segment; blanks and
        // whitespace are dropped/trimmed.
        let data = b"Cloning into 'x'...\nReceiving objects:  10% (1/10)\r\
Receiving objects: 100% (10/10)\rdone.\n";
        let mut lines = Vec::new();
        stream_progress_lines(std::io::Cursor::new(&data[..]), |l| lines.push(l));
        assert_eq!(
            lines,
            vec![
                "Cloning into 'x'...",
                "Receiving objects:  10% (1/10)",
                "Receiving objects: 100% (10/10)",
                "done.",
            ]
        );
    }

    #[test]
    fn clone_banner_does_not_count_as_remote_activity() {
        let activity_bytes = std::cell::Cell::new(0);
        stream_progress_lines_with_activity(
            std::io::Cursor::new(b"Cloning into 'x'...\nremote\n".as_slice()),
            |_| {},
            || activity_bytes.set(activity_bytes.get() + 1),
        );
        assert_eq!(activity_bytes.get(), "remote".len());
    }

    #[test]
    fn progress_lines_flush_trailing_without_terminator() {
        let mut lines = Vec::new();
        stream_progress_lines(std::io::Cursor::new(b"no newline".as_slice()), |l| {
            lines.push(l)
        });
        assert_eq!(lines, vec!["no newline"]);
    }

    #[test]
    fn progress_lines_cap_delimiter_free_segments() {
        let data = vec![b'x'; 1024 * 1024];
        let mut lines = Vec::new();
        stream_progress_lines(std::io::Cursor::new(data), |line| lines.push(line));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), MAX_PROGRESS_SEGMENT);
    }

    #[test]
    fn progress_lines_redact_credentials_and_strip_controls() {
        let mut lines = Vec::new();
        stream_progress_lines(
            std::io::Cursor::new(b"https://user:tok@host/x \x1b[31mred\x1b[0m\n".as_slice()),
            |line| lines.push(line),
        );
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("tok"));
        assert!(!lines[0].contains('\u{1b}'));
    }

    #[test]
    fn progress_throttle_coalesces_and_flushes_newest_line() {
        let start = Instant::now();
        let mut throttle = ProgressThrottle::new(Duration::from_millis(100));
        let mut emitted = Vec::new();
        for index in 0..10_000 {
            if let Some(line) = throttle.offer(
                format!("line-{index}"),
                start + Duration::from_micros(index * 100),
            ) {
                emitted.push(line);
            }
        }
        emitted.extend(throttle.flush());
        assert!(emitted.len() <= 11, "emitted {} events", emitted.len());
        assert_eq!(emitted.last().map(String::as_str), Some("line-9999"));
    }

    #[test]
    fn clone_generation_fences_old_events_and_each_begin_is_unique() {
        let _guard = CLONE_GENERATION_TEST_LOCK.lock().unwrap();
        let first = next_clone_generation();
        let second = next_clone_generation();
        assert_ne!(first, second);
        assert!(!clone_event_is_current(first, second));
        assert!(clone_event_is_current(second, second));
    }

    #[test]
    fn clone_shutdown_wait_returns_immediately_when_idle_and_is_bounded_when_running() {
        let idle = Mutex::new(CloneSupervisorState::default());
        let changed = Condvar::new();
        assert!(wait_for_clone_supervisor(
            &idle,
            &changed,
            Duration::from_secs(1)
        ));

        let running = Mutex::new(CloneSupervisorState {
            running: true,
            pending: None,
        });
        let started = Instant::now();
        assert!(!wait_for_clone_supervisor(
            &running,
            &changed,
            Duration::from_millis(20)
        ));
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn shutdown_clones_invalidates_the_current_generation() {
        let _guard = CLONE_GENERATION_TEST_LOCK.lock().unwrap();
        let old_generation = CLONE_GENERATION.load(Ordering::SeqCst);
        assert!(clone_event_is_current(
            old_generation,
            CLONE_GENERATION.load(Ordering::SeqCst)
        ));
        assert!(shutdown_clones(Duration::from_millis(20)));
        assert!(!clone_event_is_current(
            old_generation,
            CLONE_GENERATION.load(Ordering::SeqCst)
        ));
    }

    #[test]
    fn clone_supervisor_catches_job_panics() {
        assert!(catch_clone_job(|| panic!("injected clone job panic")).is_err());
    }

    #[test]
    fn clone_step_terminates_then_escalates_after_grace() {
        let started = Instant::now();
        let limits = CloneLimits {
            total: Duration::from_secs(5),
            connect: Duration::from_secs(5),
            idle: Duration::from_secs(5),
        };
        assert_eq!(
            clone_step(started, started, limits, None, true, None),
            CloneStep::Terminate
        );
        let sent = started + Duration::from_secs(1);
        assert_eq!(
            clone_step(
                sent + CLONE_TERMINATE_GRACE,
                started,
                limits,
                None,
                false,
                Some(sent)
            ),
            CloneStep::Kill
        );
        assert_eq!(
            clone_step(
                started + Duration::from_secs(5),
                started,
                limits,
                None,
                false,
                None
            ),
            CloneStep::Terminate
        );
    }

    #[test]
    fn clone_step_enforces_connect_and_idle_deadlines() {
        let started = Instant::now();
        let limits = CloneLimits {
            total: Duration::from_secs(60),
            connect: Duration::from_millis(300),
            idle: Duration::from_millis(500),
        };
        assert_eq!(
            clone_step(
                started + Duration::from_millis(300),
                started,
                limits,
                None,
                false,
                None
            ),
            CloneStep::Terminate
        );
        assert_eq!(
            clone_stop_reason(
                started + Duration::from_millis(300),
                started,
                limits,
                None,
                false
            ),
            Some(CloneStopReason::ConnectTimeout)
        );
        assert_eq!(
            clone_step(
                started + Duration::from_millis(600),
                started,
                limits,
                Some(started + Duration::from_millis(99)),
                false,
                None
            ),
            CloneStep::Terminate
        );
        assert_eq!(
            clone_stop_reason(
                started + Duration::from_millis(600),
                started,
                limits,
                Some(started + Duration::from_millis(99)),
                false
            ),
            Some(CloneStopReason::IdleTimeout)
        );
        assert_eq!(
            clone_step(
                started + Duration::from_millis(299),
                started,
                limits,
                Some(started + Duration::from_millis(200)),
                false,
                None
            ),
            CloneStep::Continue
        );
    }

    #[test]
    fn cancelled_clone_reports_cancelled_before_admission() {
        let temp = tempfile::tempdir().unwrap();
        let error = clone_workspace_repo_with_deadline(
            "https://example.invalid/repo.git",
            &temp.path().join("clone"),
            1,
            None,
            CloneLimits {
                total: Duration::from_secs(60),
                connect: Duration::from_secs(1),
                idle: Duration::from_secs(1),
            },
            || true,
            |_| {},
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "git clone cancelled");
    }

    #[test]
    fn partial_clone_destination_cleanup_only_removes_new_directories() {
        let temp = tempfile::tempdir().unwrap();
        let created = temp.path().join("created");
        std::fs::create_dir(&created).unwrap();
        std::fs::write(created.join("partial"), b"partial clone").unwrap();
        remove_partial_clone_dest(&created, false);
        assert!(!created.exists());

        let preexisting = temp.path().join("preexisting");
        std::fs::create_dir(&preexisting).unwrap();
        remove_partial_clone_dest(&preexisting, true);
        assert!(preexisting.is_dir());

        let regular_file = temp.path().join("file");
        std::fs::write(&regular_file, b"keep").unwrap();
        remove_partial_clone_dest(&regular_file, false);
        assert!(regular_file.is_file());
    }

    #[test]
    fn hung_clone_deadline_reaps_local_http_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let accept_thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => return Some(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept clone connection: {error}"),
                }
            }
        });
        let dest = temp.path().join("clone");
        let url = format!("http://{address}/x.git");
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let result = clone_workspace_repo_with_deadline(
                &url,
                &dest,
                2,
                None,
                CloneLimits {
                    total: Duration::from_secs(60),
                    connect: Duration::from_millis(300),
                    idle: Duration::from_secs(60),
                },
                || false,
                |_| {},
            );
            tx.send(result.map_err(|error| error.to_string())).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let elapsed = started.elapsed();
        worker.join().unwrap();
        let error = result.expect_err("a nonresponsive HTTP peer must time out");
        assert_eq!(error, "git clone timed out (no response from remote)");
        assert!(
            !error.contains("did not exit"),
            "the clone process group must be reaped, got: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "clone process group took {elapsed:?} to reap"
        );
        assert!(!temp.path().join("clone").exists());
        assert_no_clone_staging(temp.path());
        assert!(
            accept_thread.join().unwrap().is_some(),
            "git contacted local peer"
        );
    }

    #[test]
    fn hung_clone_total_deadline_reaps_local_http_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let accept_thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => return Some(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept clone connection: {error}"),
                }
            }
        });
        let dest = temp.path().join("clone");
        let url = format!("http://{address}/x.git");
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let result = clone_workspace_repo_with_deadline(
                &url,
                &dest,
                3,
                None,
                CloneLimits {
                    total: Duration::from_millis(300),
                    connect: Duration::from_secs(60),
                    idle: Duration::from_secs(60),
                },
                || false,
                |_| {},
            );
            tx.send(result.map_err(|error| error.to_string())).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let elapsed = started.elapsed();
        worker.join().unwrap();
        let error = result.expect_err("a nonresponsive HTTP peer must time out");
        assert_eq!(error, "git clone timed out");
        assert!(
            !error.contains("did not exit"),
            "the clone process group must be reaped, got: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "clone process group took {elapsed:?} to reap"
        );
        assert!(!temp.path().join("clone").exists());
        assert_no_clone_staging(temp.path());
        assert!(
            accept_thread.join().unwrap().is_some(),
            "git contacted local peer"
        );
    }

    #[test]
    fn plan_classifies_git_urls_as_clone() {
        let cfg = thegn_core::config::Config::default();
        match plan_new_workspace_input("https://github.com/acme/widget.git", &cfg) {
            SubmitPlan::Clone { url, dest } => {
                assert_eq!(url, "https://github.com/acme/widget.git");
                assert!(
                    dest.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("widget-")
                );
            }
            other => panic!("expected Clone, got {other:?}"),
        }
        // ssh remotes too
        assert!(matches!(
            plan_new_workspace_input("git@github.com:acme/widget.git", &cfg),
            SubmitPlan::Clone { .. }
        ));
        for url in [
            "ssh://blake@host/repo.git",
            "ssh://APKA123@git-codecommit.us-east-1.amazonaws.com/v1/repos/repo",
        ] {
            assert!(
                matches!(
                    plan_new_workspace_input(url, &cfg),
                    SubmitPlan::Clone { .. }
                ),
                "SSH usernames are part of clone transport identity: {url}"
            );
        }
    }

    #[test]
    fn destination_keys_distinguish_same_name_origins_and_reject_unsafe_urls() {
        let cfg = thegn_core::config::Config::default();
        let a = workspace_clone_dest("https://github.com/acme/repo.git", &cfg).unwrap();
        let b = workspace_clone_dest("https://github.com/other/repo.git", &cfg).unwrap();
        assert_ne!(a, b, "owner is part of the canonical origin digest");
        for invalid in [
            "https://github.com/acme/../repo.git",
            "https://github.com/acme/%2e%2e/repo.git",
            "https://user:secret@github.com/acme/repo.git",
            "https://github.com/acme/repo.git?token=secret",
            "https://github.com/acme/repo\\..\\escape.git",
        ] {
            assert!(
                matches!(
                    plan_new_workspace_input(invalid, &cfg),
                    SubmitPlan::Invalid(_)
                ),
                "{invalid}"
            );
        }
    }

    #[test]
    fn existing_url_destination_is_reused_only_when_origin_matches() {
        let mut cfg = thegn_core::config::Config::default();
        let root = tmp("existing-url-dest");
        std::fs::remove_dir_all(&root).unwrap_or(());
        std::fs::create_dir_all(&root).unwrap();
        cfg.workspaces_dir = root.to_string_lossy().into_owned();
        let url = "https://github.com/acme/repo.git";
        let dest = workspace_clone_dest(url, &cfg).unwrap();
        std::fs::create_dir_all(dest.join(".git")).unwrap();
        std::fs::write(
            dest.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/repo.git\n",
        )
        .unwrap();
        std::fs::write(dest.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(matches!(
            plan_new_workspace_input(url, &cfg),
            SubmitPlan::Local(ref path) if path == url
        ));

        std::fs::write(
            dest.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/other/repo.git\n",
        )
        .unwrap();
        assert!(matches!(
            plan_new_workspace_input(url, &cfg),
            SubmitPlan::Invalid(_)
        ));

        std::fs::remove_dir_all(&dest).unwrap();
        let legacy = root.join("repo");
        std::fs::create_dir_all(legacy.join(".git")).unwrap();
        std::fs::write(
            legacy.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/repo.git\n",
        )
        .unwrap();
        std::fs::write(legacy.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(matches!(
            plan_new_workspace_input(url, &cfg),
            SubmitPlan::Local(ref path) if path == url
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "run git commands in isolated temporary fixture repositories"
    )]
    fn fixture_git(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
        let mut command = thegn_core::util::git_cmd(cwd);
        command.args(args).env("GIT_CONFIG_GLOBAL", "/dev/null");
        command.output().unwrap()
    }

    fn committed_fixture_repo(root: &std::path::Path) -> std::path::PathBuf {
        let repo = root.join("source");
        std::fs::create_dir(&repo).unwrap();
        let output = fixture_git(&repo, &["init"]);
        assert!(
            output.status.success(),
            "git init: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = fixture_git(&repo, &["config", "user.name", "Test User"]);
        assert!(output.status.success());
        let output = fixture_git(&repo, &["config", "user.email", "test@example.invalid"]);
        assert!(output.status.success());
        std::fs::write(repo.join("tracked.txt"), b"fixture\n").unwrap();
        let output = fixture_git(&repo, &["add", "tracked.txt"]);
        assert!(output.status.success());
        let output = fixture_git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", "fixture"],
        );
        assert!(
            output.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        repo
    }

    fn assert_no_clone_staging(parent: &std::path::Path) {
        let entries = std::fs::read_dir(parent).unwrap();
        for entry in entries {
            let name = entry.unwrap().file_name();
            assert!(
                !name.to_string_lossy().contains(".clone-"),
                "staging entry remains: {}",
                name.to_string_lossy()
            );
        }
    }

    #[test]
    fn stale_staging_from_a_dead_pid_is_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let stale = temp.path().join(".ws.clone-7-4242");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("partial"), b"clone in progress").unwrap();

        assert_eq!(
            reclaim_stale_clone_staging(temp.path(), "ws", 1, |_| false),
            1
        );
        assert!(!stale.exists());
    }

    #[test]
    fn stale_staging_of_a_live_or_own_pid_is_kept() {
        let temp = tempfile::tempdir().unwrap();
        let live = temp.path().join(".ws.clone-7-4242");
        let own = temp.path().join(".ws.clone-8-1");
        for path in [&live, &own] {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("partial"), b"clone in progress").unwrap();
        }

        assert_eq!(
            reclaim_stale_clone_staging(temp.path(), "ws", 1, |pid| pid == 4242),
            0
        );
        assert!(live.join("partial").is_file());
        assert!(own.join("partial").is_file());
    }

    #[test]
    fn stale_staging_reclaim_ignores_other_names() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            ".other.clone-7-4242",
            ".ws.clone-x-4242",
            ".ws.clone-7-",
            ".ws.clone--4242",
            ".ws.clone-7-4242-extra",
            "ws",
        ] {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        let regular_file = temp.path().join(".ws.clone-9-4242");
        std::fs::write(&regular_file, b"not a directory").unwrap();

        assert_eq!(
            reclaim_stale_clone_staging(temp.path(), "ws", 1, |_| false),
            0
        );
        for name in [
            ".other.clone-7-4242",
            ".ws.clone-x-4242",
            ".ws.clone-7-",
            ".ws.clone--4242",
            ".ws.clone-7-4242-extra",
            "ws",
        ] {
            assert!(temp.path().join(name).is_dir(), "{name} should remain");
        }
        assert!(regular_file.is_file());
    }

    #[test]
    fn startup_sweep_reclaims_dead_staging_for_every_workspace_name() {
        let temp = tempfile::tempdir().unwrap();
        let stale = [".a.clone-7-4242", ".b-c.clone-8-4243", ".d.e.clone-9-4244"];
        for name in stale {
            let path = temp.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("partial"), b"interrupted clone").unwrap();
        }

        assert_eq!(
            sweep_stale_clone_staging(temp.path(), 1, |_| false),
            stale.len()
        );
        for name in stale {
            assert!(
                !temp.path().join(name).exists(),
                "{name} should be reclaimed"
            );
        }
    }

    #[test]
    fn startup_sweep_keeps_live_own_symlinked_and_non_matching_entries() {
        let temp = tempfile::tempdir().unwrap();
        let live = temp.path().join(".a.clone-7-4242");
        let own = temp.path().join(".a.clone-8-1");
        for path in [&live, &own] {
            std::fs::create_dir(path).unwrap();
            std::fs::write(path.join("partial"), b"clone in progress").unwrap();
        }
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("important"), b"keep").unwrap();
        let link = temp.path().join(".s.clone-7-4244");
        if crate::platform::symlink_file_for_test(&victim, &link).is_err() {
            return;
        }
        let non_matching_dirs = [
            ".clone-7-4245",
            "a.clone-7-4246",
            ".a.clone-x-4242",
            ".a.clone-7-",
            ".a.clone-7-4242-extra",
            "ws",
        ];
        for name in non_matching_dirs {
            std::fs::create_dir(temp.path().join(name)).unwrap();
        }
        let regular_file = temp.path().join(".f.clone-9-4247");
        std::fs::write(&regular_file, b"not a directory").unwrap();

        assert_eq!(
            sweep_stale_clone_staging(temp.path(), 1, |pid| pid == 4242),
            0
        );
        assert!(live.join("partial").is_file());
        assert!(own.join("partial").is_file());
        assert!(victim.join("important").is_file());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        for name in non_matching_dirs {
            assert!(temp.path().join(name).is_dir(), "{name} should remain");
        }
        assert!(regular_file.is_file());
    }

    #[test]
    fn stale_staging_reclaim_never_follows_a_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("important"), b"keep").unwrap();
        let link = temp.path().join(".ws.clone-7-4242");
        if crate::platform::symlink_file_for_test(&victim, &link).is_err() {
            return;
        }

        assert_eq!(
            reclaim_stale_clone_staging(temp.path(), "ws", 1, |_| false),
            0
        );
        assert!(victim.join("important").is_file());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn stale_staging_is_reclaimed_before_a_new_clone() {
        let temp = tempfile::tempdir().unwrap();
        let source = committed_fixture_repo(temp.path());
        let source_url = source.to_string_lossy().into_owned();
        let stale = temp.path().join(".workspace.clone-3-2147483647");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("partial"), b"interrupted clone").unwrap();
        let dest = temp.path().join("workspace");

        clone_workspace_repo_with_deadline(
            &source_url,
            &dest,
            44,
            None,
            CloneLimits {
                total: Duration::from_secs(30),
                connect: Duration::from_secs(5),
                idle: Duration::from_secs(5),
            },
            || false,
            |_| {},
        )
        .unwrap();

        assert!(dest.join("tracked.txt").is_file());
        assert_no_clone_staging(temp.path());
    }

    #[test]
    fn staged_clone_promotes_a_validated_clone_into_dest() {
        let temp = tempfile::tempdir().unwrap();
        let source = committed_fixture_repo(temp.path());
        let source_url = source.to_string_lossy().into_owned();
        let dest = temp.path().join("workspace");
        let normal_dir = temp.path().join("normal-dir");
        std::fs::create_dir(&normal_dir).unwrap();
        clone_workspace_repo_with_deadline(
            &source_url,
            &dest,
            41,
            None,
            CloneLimits {
                total: Duration::from_secs(30),
                connect: Duration::from_secs(5),
                idle: Duration::from_secs(5),
            },
            || false,
            |_| {},
        )
        .unwrap();
        assert!(dest.join(".git/HEAD").is_file());
        assert_eq!(
            thegn_core::fsperm::mode_bits(&dest).unwrap(),
            thegn_core::fsperm::mode_bits(&normal_dir).unwrap()
        );
        assert_no_clone_staging(temp.path());
    }

    #[test]
    fn staged_clone_failure_leaves_no_dest_and_no_staging() {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("workspace");
        let missing = temp.path().join("does-not-exist");
        let result = clone_workspace_repo_with_deadline(
            &missing.to_string_lossy(),
            &dest,
            42,
            None,
            CloneLimits {
                total: Duration::from_secs(30),
                connect: Duration::from_secs(5),
                idle: Duration::from_secs(5),
            },
            || false,
            |_| {},
        );
        assert!(result.is_err());
        assert!(!dest.exists());
        assert_no_clone_staging(temp.path());
    }

    #[test]
    fn staged_clone_refuses_when_dest_appears_before_promotion() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let dest = temp.path().join("dest");
        std::fs::create_dir(&staging).unwrap();
        std::fs::write(staging.join("from-stage"), b"staged").unwrap();
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("keep"), b"existing").unwrap();
        let pin = thegn_core::fsperm::open_rooted_dir(temp.path()).unwrap();
        let result = promote_staged_clone(&pin, "staging", "dest");
        assert!(result.is_err());
        assert_eq!(std::fs::read(dest.join("keep")).unwrap(), b"existing");
        assert!(!staging.exists());
    }

    #[test]
    fn staged_clone_rejects_a_non_utf8_destination_name() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        // SAFETY: the runtime guard above ensures these bytes use Unix OsStr encoding.
        let name = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(b"\xff") };
        let dest = temp.path().join(name);

        let result = clone_workspace_repo_with_deadline(
            "unused-source",
            &dest,
            43,
            None,
            CloneLimits {
                total: Duration::from_secs(30),
                connect: Duration::from_secs(5),
                idle: Duration::from_secs(5),
            },
            || false,
            |_| {},
        );

        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("destination name must be valid UTF-8")
        ));
        assert_no_clone_staging(temp.path());
    }

    #[test]
    fn staged_clone_refuses_promotion_when_the_parent_was_swapped() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("ws");
        let elsewhere = temp.path().join("elsewhere");
        let moved = temp.path().join("ws-moved");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::create_dir(parent.join(".x.clone-1-1")).unwrap();
        std::fs::write(parent.join(".x.clone-1-1/marker"), b"staged").unwrap();
        let pin = thegn_core::fsperm::open_rooted_dir(&parent).unwrap();

        std::fs::rename(&parent, &moved).unwrap();
        if let Err(error) = crate::platform::symlink_file_for_test(&elsewhere, &parent) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("create swapped workspace parent symlink: {error}");
        }

        let result = promote_staged_clone(&pin, ".x.clone-1-1", "x");
        assert!(matches!(
            result,
            Err(error) if error.to_string().contains("replaced while cloning")
        ));
        assert!(!elsewhere.join("x").exists());
        assert_eq!(
            std::fs::read(moved.join(".x.clone-1-1/marker")).unwrap(),
            b"staged"
        );
    }

    #[test]
    fn staged_clone_promotes_through_a_symlinked_workspaces_dir() {
        let temp = tempfile::tempdir().unwrap();
        let source = committed_fixture_repo(temp.path());
        let source_url = source.to_string_lossy().into_owned();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        std::fs::create_dir(&real).unwrap();
        if let Err(error) = crate::platform::symlink_file_for_test(&real, &link) {
            if matches!(
                error.kind(),
                std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
            ) {
                return;
            }
            panic!("create symlinked workspaces directory fixture: {error}");
        }
        let dest = link.join("workspace");

        clone_workspace_repo_with_deadline(
            &source_url,
            &dest,
            45,
            None,
            CloneLimits {
                total: Duration::from_secs(30),
                connect: Duration::from_secs(5),
                idle: Duration::from_secs(5),
            },
            || false,
            |_| {},
        )
        .unwrap();

        assert!(real.join("workspace/.git/HEAD").is_file());
    }

    #[test]
    fn staged_clone_validation_rejects_an_origin_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let source = committed_fixture_repo(temp.path());
        let source_url = source.to_string_lossy().into_owned();
        let staging = temp.path().join("staging");
        let staging_path = staging.to_string_lossy().into_owned();
        let output = fixture_git(
            temp.path(),
            &["clone", "--progress", &source_url, &staging_path],
        );
        assert!(
            output.status.success(),
            "git clone: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = fixture_git(&staging, &["config", "clone.defaultRemoteName", "origin"]);
        assert!(output.status.success());
        assert!(validate_staged_clone(&staging, None).is_ok());
        let other_origin = parse_workspace_url("https://github.com/other/repo.git")
            .unwrap()
            .canonical_origin;
        let acme_origin = parse_workspace_url("https://github.com/acme/repo.git")
            .unwrap()
            .canonical_origin;
        let output = fixture_git(
            &staging,
            &[
                "config",
                "remote.origin.url",
                "https://github.com/other/repo.git",
            ],
        );
        assert!(output.status.success());
        assert!(validate_staged_clone(&staging, Some(&other_origin)).is_ok());
        let error = validate_staged_clone(&staging, Some(&acme_origin)).unwrap_err();
        assert!(error.to_string().contains("origin does not match"));
        let output = fixture_git(&staging, &["config", "clone.defaultRemoteName", "upstream"]);
        assert!(output.status.success());
        let output = fixture_git(
            &staging,
            &[
                "config",
                "remote.upstream.url",
                "https://github.com/acme/repo.git",
            ],
        );
        assert!(output.status.success());
        assert!(validate_staged_clone(&staging, Some(&acme_origin)).is_ok());
    }

    #[test]
    fn staged_clone_incomplete_destination_is_not_reused() {
        let temp = tempfile::tempdir().unwrap();
        let cfg = thegn_core::config::Config {
            workspaces_dir: temp.path().to_string_lossy().into_owned(),
            ..thegn_core::config::Config::default()
        };
        let url = "https://github.com/acme/incomplete.git";
        let dest = workspace_clone_dest(url, &cfg).unwrap();
        std::fs::create_dir_all(dest.join(".git")).unwrap();
        std::fs::write(
            dest.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/incomplete.git\n",
        )
        .unwrap();
        match plan_new_workspace_input(url, &cfg) {
            SubmitPlan::Invalid(message) => assert!(message.contains("incomplete clone")),
            other => panic!("expected incomplete clone refusal, got {other:?}"),
        }
        std::fs::remove_dir_all(&dest).unwrap();
        let legacy = temp.path().join("incomplete");
        std::fs::create_dir_all(legacy.join(".git")).unwrap();
        std::fs::write(
            legacy.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://github.com/acme/incomplete.git\n",
        )
        .unwrap();
        match plan_new_workspace_input(url, &cfg) {
            SubmitPlan::Invalid(message) => assert!(message.contains("incomplete clone")),
            other => panic!("expected incomplete legacy clone refusal, got {other:?}"),
        }
    }

    #[test]
    fn plan_classifies_existing_dir_as_local() {
        let cfg = thegn_core::config::Config::default();
        let dir = tmp("local");
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.to_string_lossy().into_owned();
        assert_eq!(
            plan_new_workspace_input(&input, &cfg),
            SubmitPlan::Local(input.clone())
        );
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn plan_classifies_missing_leaf_with_existing_parent_as_create_new() {
        let cfg = thegn_core::config::Config::default();
        let parent = tmp("createnew");
        let _ = std::fs::remove_dir_all(&parent); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&parent).unwrap();
        let leaf = parent.join("fresh-project");
        match plan_new_workspace_input(&leaf.to_string_lossy(), &cfg) {
            SubmitPlan::CreateNew { leaf: got } => assert_eq!(got, leaf),
            other => panic!("expected CreateNew, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&parent); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }

    #[test]
    fn plan_rejects_missing_parent_and_empty_input() {
        let cfg = thegn_core::config::Config::default();
        // Two missing levels: new projects may only nest one level below the
        // present tree.
        let leaf = tmp("no-such-parent").join("deeper/leaf");
        assert!(matches!(
            plan_new_workspace_input(&leaf.to_string_lossy(), &cfg),
            SubmitPlan::Invalid(_)
        ));
        assert!(matches!(
            plan_new_workspace_input("   ", &cfg),
            SubmitPlan::Invalid(_)
        ));
    }

    #[test]
    fn init_new_project_creates_dir_and_git_repo() {
        let parent = tmp("initproj");
        let _ = std::fs::remove_dir_all(&parent); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&parent).unwrap();
        let leaf = parent.join("proj");
        init_new_project(&leaf).unwrap();
        assert!(leaf.join(".git").is_dir(), "git init produced a .git dir");
        // A leaf whose parent is missing is refused (and nothing is created).
        let bad = parent.join("missing/two-deep");
        assert!(init_new_project(&bad).is_err());
        assert!(!bad.exists());
        let _ = std::fs::remove_dir_all(&parent); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
