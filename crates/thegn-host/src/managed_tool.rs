//! Host-side acquisition for [`thegn_core::managed_tool`] specs.
//!
//! `thegn-core` decides *which* tier resolves a tool, *which* release asset
//! matches the platform, and *whether* an install is needed — but it carries no
//! HTTP client. This module performs the side effect: an `npm install` for
//! `Npm` sources, or a GitHub-release download + `chmod +x` for `GithubRelease`.
//! It runs off the event loop (the CLI path, or `spawn_blocking` when the
//! compositor provisions a tool) exactly as the managed pi install does — never
//! on the loop — and surfaces failures rather than degrading silently.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::{NamedTempFile, TempDir};
use thegn_core::managed_tool::{Arch, ManagedTool, Os, Source};
use thegn_core::{msg, util};

/// Ceiling for a managed-tool setup subprocess (`npm install`, `pi install`,
/// `cargo install`). Generous — a cold npm/cargo fetch can legitimately run for
/// minutes — but bounds an infinite hang: a stalled registry/network otherwise
/// wedges `thegn agent setup`, `debug setup`, and (via the sprite `managed_pi`
/// provisioning step) the sandbox-creation loading screen forever.
const SETUP_CMD_TIMEOUT: Duration = Duration::from_secs(600); // 10 min

/// Release downloads are deliberately unavailable until maintainers review
/// and ship a digest for each `(tool, version, asset)` tuple. Never fetch a
/// checksum beside the executable: it would have the same trust as the asset.
/// TODO(THE-182): add reviewed SHA-256 values for every managed release asset.
const TRUSTED_RELEASE_DIGESTS: &[(&str, &str, &str, &str)] = &[];

const DOWNLOAD_LIMIT: u64 = 128 * 1024 * 1024;
const EXTRACTED_FILE_LIMIT: u64 = 64 * 1024 * 1024;
const DOWNLOAD_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Output retained per stream for diagnostics: the first and last bytes only.
/// Everything in between is read (so the child never blocks on a full pipe)
/// and counted, but dropped.
const KEEP_HEAD: usize = 4096;
const KEEP_TAIL: usize = 4096;

/// Limits for one bounded subprocess run. Production uses [`Limits::setup`];
/// tests shrink the durations.
#[derive(Clone, Copy)]
struct Limits {
    /// One total deadline from spawn to the direct child's exit.
    timeout: Duration,
    /// Grace between the group SIGTERM and the group SIGKILL.
    term_grace: Duration,
    /// How long to wait for the readers to reach EOF once the whole process
    /// group has been killed.
    read_grace: Duration,
}

impl Limits {
    fn setup() -> Self {
        Self {
            timeout: SETUP_CMD_TIMEOUT,
            term_grace: Duration::from_secs(2),
            read_grace: Duration::from_secs(2),
        }
    }
}

/// Typed failure of a bounded subprocess run.
#[derive(Debug)]
enum RunError {
    Spawn(std::io::Error),
    Timeout {
        secs: u64,
        tail: String,
    },
    Nonzero {
        status: ExitStatus,
        tail: String,
    },
    Wait(std::io::Error),
    Read(std::io::Error),
    /// A process outside the owned group (it escaped via its own session)
    /// still holds an output pipe, so the reader could not reach EOF.
    PipeHeld {
        tail: String,
    },
    /// SIGINT/SIGTERM arrived (CLI): the group was stopped and reaped.
    Interrupted {
        tail: String,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "could not start: {e}"),
            Self::Timeout { secs, tail } => {
                write!(f, "timed out after {secs}s (process tree killed){tail}")
            }
            Self::Nonzero { status, tail } => write!(f, "exited with {status}{tail}"),
            Self::Wait(e) => write!(f, "wait/kill failed: {e}"),
            Self::Read(e) => write!(f, "output reader failed: {e}"),
            Self::Interrupted { tail } => write!(f, "interrupted (process tree killed){tail}"),
            Self::PipeHeld { tail } => write!(
                f,
                "an escaped process kept the output pipe open; output truncated{tail}"
            ),
        }
    }
}

impl std::error::Error for RunError {}

/// Bounded head+tail retention of one stream.
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl Capture {
    fn push(&mut self, chunk: &[u8]) {
        self.total += chunk.len() as u64;
        let room = KEEP_HEAD.saturating_sub(self.head.len());
        let (to_head, rest) = chunk.split_at(room.min(chunk.len()));
        self.head.extend_from_slice(to_head);
        // Only the last KEEP_TAIL bytes of `rest` can survive.
        let rest = &rest[rest.len().saturating_sub(KEEP_TAIL)..];
        self.tail.extend(rest);
        while self.tail.len() > KEEP_TAIL {
            self.tail.pop_front();
        }
    }

    fn retained(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// Redacted, lossy-UTF-8 rendering with an elision marker.
    fn render(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let kept = self.retained() as u64;
        let mut head = String::from_utf8_lossy(&self.head).into_owned();
        let mut tail = String::from_utf8_lossy(&tail).into_owned();
        if self.total <= kept {
            head.push_str(&tail);
            return thegn_core::ci_log::redact(head.trim());
        }
        // Cut both halves to line boundaries and redact them separately, so a
        // secret straddling the elision can never leave a readable fragment.
        if let Some(i) = head.rfind('\n') {
            head.truncate(i);
        }
        if let Some(i) = tail.find('\n') {
            tail.drain(..=i);
        }
        format!(
            "{}\n[... {} bytes elided ...]\n{}",
            thegn_core::ci_log::redact(head.trim()),
            self.total - kept,
            thegn_core::ci_log::redact(tail.trim())
        )
    }
}

struct Captured {
    stdout: Capture,
    stderr: Capture,
    /// A process that escaped the owned group (own session) still held an
    /// output pipe after a clean exit; the tree was killed but output may be
    /// truncated.
    pipe_held: bool,
}

fn tail_text(out: &Capture, err: &Capture) -> String {
    let body = [err.render(), out.render()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if body.is_empty() {
        body
    } else {
        format!(": {body}")
    }
}

type ReaderRx = mpsc::Receiver<std::io::Result<Capture>>;

/// Read `pipe` to EOF on its own thread, retaining only a bounded head/tail.
/// `tee` forwards the bytes live to this process's stderr for CLI presentation
/// (never stdout, which carries machine data such as `--json`) — capture is
/// identical either way, so output ownership never depends on the mode.
fn spawn_reader(
    name: &str,
    mut pipe: impl Read + Send + 'static,
    tee: bool,
) -> std::io::Result<ReaderRx> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
            let mut cap = Capture::default();
            let mut buf = [0u8; 8192];
            let result = loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        cap.push(&buf[..n]);
                        if tee {
                            let _ = std::io::stderr().write_all(&buf[..n]); // best-effort: live echo; capture is authoritative
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(result.map(|()| cap)); // best-effort: the receiver may have given up
        })?;
    Ok(rx)
}

/// Run `cmd` to completion inside an owned process group / Job Object with one
/// total deadline. Both pipes are drained to EOF (the child never blocks on a
/// full pipe) while only a bounded head/tail is retained. The whole tree is
/// killed on timeout, wait failure, and after the direct child exits (so a
/// descendant holding a pipe cannot outlive the call). The direct child stays
/// unreaped (`waitid(WNOWAIT)`) until the group is killed, so the group id
/// cannot be reused under us. Every reader thread is joined before returning,
/// except when a process that escaped the group still holds a pipe
/// ([`RunError::PipeHeld`]).
// Off-loop only (CLI / spawn_blocking): the terminal `wait` follows a group
// SIGKILL, so it returns promptly.
#[expect(clippy::disallowed_methods)]
fn run_bounded(cmd: &mut Command, limits: Limits, tee: bool) -> Result<Captured, RunError> {
    const POLL: Duration = Duration::from_millis(25);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // CLI: the child lives in its own group, which the terminal's Ctrl-C no
    // longer reaches, so record SIGINT/SIGTERM and stop the group ourselves.
    let interrupt = tee.then(crate::platform::InterruptGuard::install);
    let (mut child, group) = crate::platform::spawn_grouped(cmd).map_err(RunError::Spawn)?;
    let deadline = Instant::now() + limits.timeout;
    let out_pipe = child.stdout.take().expect("stdout was piped");
    let err_pipe = child.stderr.take().expect("stderr was piped");
    let readers = spawn_reader("thegn-setup-stdout", out_pipe, tee)
        .and_then(|out| spawn_reader("thegn-setup-stderr", err_pipe, tee).map(|err| (out, err)));
    let (out_rx, err_rx) = match readers {
        Ok(pair) => pair,
        Err(e) => {
            // A reader that did start sees EOF once the group dies.
            group.kill();
            let _ = child.wait(); // best-effort: group was just killed; the spawn error is the outcome
            return Err(RunError::Read(e));
        }
    };

    let mut timed_out = false;
    let mut interrupted = false;
    let mut wait_err = None;
    loop {
        if interrupt.as_ref().is_some_and(|g| g.fired()) {
            interrupted = true;
            break;
        }
        match crate::platform::gate_child_exited(&mut child) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                wait_err = Some(e);
                break;
            }
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(POLL);
    }
    // If observing the leader failed (e.g. ECHILD) its pid may be reused, so
    // the group identity is no longer ours: never signal it. Kill the direct
    // child handle only.
    let mut owned = wait_err.is_none();
    if owned && (timed_out || interrupted) {
        group.terminate();
        let grace_end = Instant::now() + limits.term_grace;
        loop {
            match crate::platform::gate_child_exited(&mut child) {
                Ok(true) => break,
                Ok(false) if Instant::now() < grace_end => std::thread::sleep(POLL),
                Ok(false) => break,
                Err(e) => {
                    wait_err = Some(e);
                    owned = false;
                    break;
                }
            }
        }
    }
    if owned {
        // The leader is still unreaped here, so the group identity is ours:
        // kill every remaining member (compilers, package scripts, pipe holders).
        group.kill();
    } else {
        let _ = child.kill(); // best-effort: group ownership uncertain; the leader may be gone
    }
    let status = child.wait();
    drop(interrupt);

    let join = |rx: ReaderRx| match rx.recv_timeout(limits.read_grace) {
        Ok(Ok(cap)) => Ok(cap),
        Ok(Err(e)) => Err(RunError::Read(e)),
        Err(_) => Err(RunError::PipeHeld {
            tail: String::new(),
        }),
    };
    let (out, err) = (join(out_rx), join(err_rx));
    let (stdout, stderr, held) = match (out, err) {
        (Ok(o), Ok(e)) => (o, e, false),
        (Err(RunError::Read(e)), _) | (_, Err(RunError::Read(e))) => return Err(RunError::Read(e)),
        (o, e) => (o.unwrap_or_default(), e.unwrap_or_default(), true),
    };
    let tail = tail_text(&stdout, &stderr);
    if interrupted {
        return Err(RunError::Interrupted { tail });
    }
    if timed_out {
        return Err(RunError::Timeout {
            secs: limits.timeout.as_secs(),
            tail,
        });
    }
    if let Some(e) = wait_err {
        return Err(RunError::Wait(e));
    }
    let status = status.map_err(RunError::Wait)?;
    if !status.success() {
        return Err(if held {
            RunError::PipeHeld { tail }
        } else {
            RunError::Nonzero { status, tail }
        });
    }
    Ok(Captured {
        stdout,
        stderr,
        pipe_held: held,
    })
}

/// Run a setup subprocess. Setup commands are **non-interactive**: stdin is
/// `/dev/null` and git/ssh credential prompts are disabled, because the child
/// runs in its own process group (a background group that opens `/dev/tty`
/// would be stopped by SIGTTIN until the deadline). On the CLI, SIGINT/SIGTERM
/// are forwarded to that group. Because output is piped, tools that draw
/// progress bars on a TTY (npm, cargo) fall back to plain line output. Output is always captured (bounded, redacted) and
/// the whole process tree is owned and killed on deadline; on the CLI the
/// output is additionally echoed live, while under the TUI it is only logged so
/// npm progress never paints over the alt-screen frame. Shared by the pi setup
/// and generic tool installs. `fail` is the message when the child exits
/// non-zero. Bounded by [`SETUP_CMD_TIMEOUT`].
// CLI path or off-loop (sprite provisioning runs it from spawn_blocking); the
// blocking wait never happens on the event loop.
pub fn run_setup_cmd(mut cmd: Command, ctx: &str, fail: &str) -> Result<()> {
    let tui = msg::tui_active();
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env("GIT_ASKPASS", "true")
        .env("npm_config_progress", "false");
    match run_bounded(&mut cmd, Limits::setup(), !tui) {
        Ok(c) => {
            if c.pipe_held {
                tracing::warn!(
                    target: "thegn::provision",
                    cmd = ctx,
                    "setup succeeded but a process outside the owned group held an output pipe; captured output may be truncated"
                );
            }
            if tui && (c.stdout.total > 0 || c.stderr.total > 0) {
                tracing::debug!(
                    target: "thegn::provision",
                    cmd = ctx,
                    stdout = %c.stdout.render(),
                    stderr = %c.stderr.render(),
                    "managed-tool setup subprocess output (captured; not painted on the frame)"
                );
            }
            Ok(())
        }
        Err(RunError::Nonzero { tail, .. }) => Err(anyhow::anyhow!("{fail}{tail}")),
        Err(RunError::Spawn(e)) => Err(anyhow::Error::new(e).context(ctx.to_string())),
        Err(e) => Err(anyhow::anyhow!("{ctx}: {e}")),
    }
}

/// Keep installer subprocesses away from credentials and control variables
/// inherited by thegn. The explicit HOME-backed locations keep package-manager
/// configuration and caches inside the private install workspace.
fn installer_env<I>(host: I, home: &Path) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut env = Vec::new();
    let mut host_home = None;
    let mut rustup_home_present = false;
    for (key, value) in host {
        let Some(key_text) = key.to_str() else {
            continue;
        };
        let keep = key_text.eq_ignore_ascii_case("PATH")
            || key_text.eq_ignore_ascii_case("LANG")
            || key_text
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("LC_"))
            || key_text.eq_ignore_ascii_case("TERM")
            || matches!(
                key_text.to_ascii_lowercase().as_str(),
                "http_proxy"
                    | "https_proxy"
                    | "no_proxy"
                    | "ssl_cert_file"
                    | "ssl_cert_dir"
                    | "nix_ssl_cert_file"
                    | "rustup_home"
                    | "rustup_toolchain"
                    | "systemroot"
                    | "pathext"
                    | "userprofile"
                    | "appdata"
                    | "localappdata"
                    | "comspec"
            );
        if key_text.eq_ignore_ascii_case("HOME") {
            host_home = Some(PathBuf::from(&value));
        }
        if key_text.eq_ignore_ascii_case("RUSTUP_HOME") {
            rustup_home_present = true;
        }
        if keep {
            env.push((key, value));
        }
    }
    if !rustup_home_present && let Some(real_home) = host_home {
        env.push((
            OsString::from("RUSTUP_HOME"),
            real_home.join(".rustup").into_os_string(),
        ));
    }
    env.extend([
        (OsString::from("HOME"), home.as_os_str().to_owned()),
        (
            OsString::from("CARGO_HOME"),
            home.join("cargo").into_os_string(),
        ),
        (
            OsString::from("npm_config_userconfig"),
            home.join("npmrc").into_os_string(),
        ),
        (
            OsString::from("npm_config_cache"),
            home.join("npm-cache").into_os_string(),
        ),
        (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
        (
            OsString::from("SSH_ASKPASS_REQUIRE"),
            OsString::from("never"),
        ),
        (OsString::from("GIT_ASKPASS"), OsString::from("true")),
        (
            OsString::from("npm_config_progress"),
            OsString::from("false"),
        ),
    ]);
    env
}

fn valid_tool_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value != "."
        && value != ".."
        && !matches!(value.as_bytes()[0], b'-' | b'.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_npm_package(value: &str) -> bool {
    if value.len() > 214 || value.is_empty() {
        return false;
    }
    let name = if let Some(scoped) = value.strip_prefix('@') {
        let Some((scope, package)) = scoped.split_once('/') else {
            return false;
        };
        if !valid_npm_component(scope) {
            return false;
        }
        package
    } else {
        value
    };
    valid_npm_component(name) && !name.contains('/')
}

fn valid_npm_component(value: &str) -> bool {
    !value.is_empty()
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'~' | b'-')
        })
}

fn valid_crate_name(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_exact_version(value: &str) -> bool {
    let (without_build, build_valid) = match value.split_once('+') {
        Some((before, build)) => (before, valid_semver_suffix(build)),
        None => (value, true),
    };
    if !build_valid {
        return false;
    }
    let (core, prerelease_valid) = match without_build.split_once('-') {
        Some((core, prerelease)) => (core, valid_semver_prerelease(prerelease)),
        None => (without_build, true),
    };
    prerelease_valid && core.split('.').count() == 3 && core.split('.').all(valid_semver_number)
}

fn valid_semver_number(value: &str) -> bool {
    !value.is_empty()
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_semver_suffix(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn valid_semver_prerelease(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || valid_semver_number(identifier))
        })
}

fn installer_workspace(managed: &Path, name: &str) -> Result<(TempDir, TempDir)> {
    let parent = managed
        .parent()
        .context("managed tool directory has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let home = tempfile::Builder::new()
        .prefix(".thegn-install-home-")
        .tempdir_in(parent)
        .with_context(|| format!("create private installer home in {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{name}.staging-"))
        .tempdir_in(parent)
        .with_context(|| format!("create staging directory in {}", parent.display()))?;
    Ok((home, staging))
}

fn promote_staging(staging: &Path, managed: &Path, bin_rel: &Path) -> Result<()> {
    promote_staging_with_rename(staging, managed, bin_rel, |from, to| {
        std::fs::rename(from, to)
    })
}

fn promote_staging_with_rename<F>(
    staging: &Path,
    managed: &Path,
    bin_rel: &Path,
    mut rename: F,
) -> Result<()>
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let expected = staging.join(bin_rel);
    std::fs::symlink_metadata(&expected).with_context(|| {
        format!(
            "installer did not produce expected binary {}",
            expected.display()
        )
    })?;
    let parent = managed
        .parent()
        .context("managed tool directory has no parent")?;
    let name = managed
        .file_name()
        .and_then(OsStr::to_str)
        .context("managed tool directory has no valid name")?;
    let old = tempfile::Builder::new()
        .prefix(&format!(".{name}.old-"))
        .tempdir_in(parent)
        .with_context(|| format!("reserve old install name in {}", parent.display()))?;
    let reserved_path = old.path().to_path_buf();
    std::fs::remove_dir(&reserved_path)
        .with_context(|| format!("release {}", reserved_path.display()))?;
    // Any later rename failure must leave the previous install untouched.
    let old_path = old.keep();
    let had_old = match std::fs::symlink_metadata(managed) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("inspect managed install {}", managed.display())));
        }
    };
    if had_old {
        rename(managed, &old_path)
            .with_context(|| format!("move previous install {}", managed.display()))?;
    }
    if let Err(error) = rename(staging, managed) {
        if had_old && let Err(restore_error) = rename(&old_path, managed) {
            return Err(anyhow::anyhow!(
                "promote staged install failed: {error}; restoring previous install failed: {restore_error}"
            ));
        }
        return Err(anyhow::Error::new(error).context("promote staged managed-tool install"));
    }
    if had_old && let Err(error) = std::fs::remove_dir_all(&old_path) {
        tracing::warn!(target: "thegn::provision", path = %old_path.display(), %error, "could not remove previous managed-tool install");
    }
    Ok(())
}

/// The managed tools thegn knows about, for `doctor` reporting and (later)
/// pre-provisioning.
pub fn known() -> Vec<ManagedTool> {
    vec![
        thegn_core::debug::bs_tool(),
        thegn_core::difft::difft_tool(),
    ]
}

/// Acquire a tool's binary into its managed dir — the raw fetch, without the
/// `needs_install` gate or version-marker write (callers own those, so the pi
/// setup can preserve its exact ordering). `Npm` shells out to `npm install
/// --prefix`; `GithubRelease` downloads the platform asset and marks it
/// executable.
pub fn acquire(tool: &ManagedTool) -> Result<()> {
    match &tool.source {
        Source::Npm { package } => {
            anyhow::ensure!(
                valid_tool_name(&tool.name),
                "invalid tool name: {:?}",
                tool.name
            );
            anyhow::ensure!(
                valid_npm_package(package),
                "invalid npm package: {package:?}"
            );
            anyhow::ensure!(
                valid_exact_version(&tool.version),
                "invalid version: {:?}",
                tool.version
            );
            let managed = tool.managed_dir();
            let bin_rel = tool
                .bin_path()
                .strip_prefix(&managed)
                .context("managed binary is outside its install directory")?
                .to_path_buf();
            let (home, staging) = installer_workspace(&managed, &tool.name)?;
            anyhow::ensure!(
                util::have("npm"),
                "npm not found — needed to install {package}@{}. \
                 Install Node/npm, or put the tool on PATH.",
                tool.version
            );
            let mut cmd = Command::new("npm");
            cmd.args(["install", "--prefix"])
                .arg(staging.path())
                .arg(format!("{package}@{}", tool.version));
            cmd.env_clear()
                .envs(installer_env(std::env::vars_os(), home.path()));
            run_setup_cmd(
                cmd,
                &format!("npm install {package}@{}", tool.version),
                &format!("npm install {package}@{} failed", tool.version),
            )?;
            promote_staging(staging.path(), &managed, &bin_rel)?;
            let _staging_path = staging.keep();
            drop(home);
            Ok(())
        }
        Source::Cargo { crate_name } => {
            anyhow::ensure!(
                valid_tool_name(&tool.name),
                "invalid tool name: {:?}",
                tool.name
            );
            anyhow::ensure!(
                valid_crate_name(crate_name),
                "invalid crate name: {crate_name:?}"
            );
            anyhow::ensure!(
                valid_exact_version(&tool.version),
                "invalid version: {:?}",
                tool.version
            );
            let managed = tool.managed_dir();
            let bin_rel = tool
                .bin_path()
                .strip_prefix(&managed)
                .context("managed binary is outside its install directory")?
                .to_path_buf();
            let (home, staging) = installer_workspace(&managed, &tool.name)?;
            anyhow::ensure!(
                util::have("cargo"),
                "cargo not found — needed to install {crate_name} {}. \
                 Install the Rust toolchain, or put the tool on PATH.",
                tool.version
            );
            let mut cmd = Command::new("cargo");
            cmd.args(["install", crate_name, "--version", &tool.version, "--root"])
                .arg(staging.path())
                .arg("--locked");
            cmd.env_clear()
                .envs(installer_env(std::env::vars_os(), home.path()));
            run_setup_cmd(
                cmd,
                &format!("cargo install {crate_name} --version {}", tool.version),
                &format!("cargo install {crate_name} {} failed", tool.version),
            )?;
            promote_staging(staging.path(), &managed, &bin_rel)?;
            let _staging_path = staging.keep();
            drop(home);
            Ok(())
        }
        Source::GithubRelease { repo, .. } => {
            let os = Os::current().context("unsupported OS for a managed download")?;
            let arch =
                Arch::current().context("unsupported architecture for a managed download")?;
            let asset = tool.asset_for(os, arch).with_context(|| {
                format!(
                    "{}: no release asset for this platform/architecture",
                    tool.name
                )
            })?;
            let url = format!(
                "https://github.com/{repo}/releases/download/{}/{asset}",
                tool.version
            );
            acquire_github_release(tool, asset, &url)
        }
    }
}

fn acquire_github_release(tool: &ManagedTool, asset: &str, url: &str) -> Result<()> {
    let bin = tool.bin_path();
    let parent = bin.parent().context("managed binary path has no parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let expected_digest = trusted_digest(&tool.name, &tool.version, asset).with_context(|| {
        format!(
            "no trusted SHA-256 pin for {} {} asset {asset}; refusing download",
            tool.name, tool.version
        )
    })?;
    let download = download_to(url, parent, expected_digest)?;
    if is_archive(asset) {
        let extracted = extract_binary(download.path(), asset, &tool.name, parent)?;
        make_executable(extracted.path())?;
        extracted
            .persist(&bin)
            .map_err(|e| e.error)
            .with_context(|| format!("atomically install {}", bin.display()))?;
    } else {
        make_executable(download.path())?;
        download
            .persist(&bin)
            .map_err(|e| e.error)
            .with_context(|| format!("atomically install {}", bin.display()))?;
    }
    Ok(())
}

/// Ensure a tool is installed and its version marker recorded: gate on
/// [`ManagedTool::needs_install`], [`acquire`], then mark. The generic one-call
/// path for tools without a bespoke setup (the pi setup drives [`acquire`]
/// directly to preserve its seed/register ordering; the debugger uses this).
pub fn install(tool: &ManagedTool, force: bool) -> Result<()> {
    if !tool.needs_install(force) {
        return Ok(());
    }
    acquire(tool)?;
    mark_installed(tool);
    Ok(())
}

/// Record the pinned version in the tool's marker file. Best-effort: the marker
/// is a cache (a missed write just triggers a reinstall next time), so its
/// failure must never fail the install.
pub fn mark_installed(tool: &ManagedTool) {
    if let Err(e) = std::fs::write(tool.version_marker(), &tool.version) {
        tracing::debug!(
            target: "thegn::provision",
            tool = %tool.name,
            error = %e,
            "best-effort: failed to write managed-tool version marker"
        );
    }
}

/// Whether a release asset filename is an archive we must extract rather than a
/// raw binary to write directly.
fn is_archive(asset: &str) -> bool {
    let a = asset.to_ascii_lowercase();
    a.ends_with(".tar.gz") || a.ends_with(".tgz") || a.ends_with(".zip")
}

/// Look up a shipped SHA-256 pin; a missing pin means the download is refused.
fn trusted_digest(tool: &str, version: &str, asset: &str) -> Option<&'static str> {
    TRUSTED_RELEASE_DIGESTS
        .iter()
        .find(|(name, pinned_version, pinned_asset, _)| {
            *name == tool && *pinned_version == version && *pinned_asset == asset
        })
        .map(|(_, _, _, digest)| *digest)
}

/// Stream an asset into an owner-only temporary file, enforcing response and
/// body deadlines, a hard byte cap, and the shipped digest before returning it.
fn download_to(url: &str, dir: &Path, expected_digest: &str) -> Result<NamedTempFile> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            let trusted_host = matches!(
                url.host_str(),
                Some(
                    "github.com"
                        | "objects.githubusercontent.com"
                        | "release-assets.githubusercontent.com"
                )
            );
            if attempt.previous().len() < 5 && url.scheme() == "https" && trusted_host {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TOTAL_TIMEOUT)
        .build()
        .context("build bounded release HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "download {url} failed: HTTP {} (redirect policy permits only HTTPS GitHub release hosts)",
        response.status()
    );
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= DOWNLOAD_LIMIT,
            "download exceeds {DOWNLOAD_LIMIT} byte limit"
        );
    }
    let mut file = NamedTempFile::new_in(dir)
        .with_context(|| format!("create download in {}", dir.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let count = response
            .read(&mut buffer)
            .context("read release response")?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("download byte count overflow")?;
        anyhow::ensure!(
            total <= DOWNLOAD_LIMIT,
            "download exceeds {DOWNLOAD_LIMIT} byte limit"
        );
        hasher.update(&buffer[..count]);
        file.write_all(&buffer[..count])
            .context("write release temporary file")?;
    }
    let actual = format!("{:x}", hasher.finalize());
    anyhow::ensure!(actual == expected_digest, "SHA-256 mismatch for {url}");
    file.as_file_mut()
        .sync_all()
        .context("sync release temporary file")?;
    Ok(file)
}

/// Read a gzip tar archive without extracting paths to the filesystem. Only a
/// single regular member with the requested basename is materialized. Pax and
/// GNU long-name headers are rejected as special files; check this limitation
/// before adding pins for macOS-built tarballs.
fn extract_binary(archive: &Path, asset: &str, want: &str, dir: &Path) -> Result<NamedTempFile> {
    anyhow::ensure!(
        asset.to_ascii_lowercase().ends_with(".tar.gz")
            || asset.to_ascii_lowercase().ends_with(".tgz"),
        "ZIP release extraction is disabled until a reviewed ZIP parser is available"
    );
    let input =
        std::fs::File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut tar = flate2::read::GzDecoder::new(input);
    let mut target = None;
    let mut seen = std::collections::HashSet::new();
    let mut entries = 0_usize;
    let mut expanded = 0_u64;
    let mut terminated = false;
    loop {
        let mut header = [0_u8; 512];
        match tar.read(&mut header[..1])? {
            0 => break,
            1 => tar.read_exact(&mut header[1..])?,
            _ => unreachable!(),
        }
        if header.iter().all(|byte| *byte == 0) {
            terminated = true;
            break;
        }
        entries += 1;
        anyhow::ensure!(entries <= 4096, "archive has too many members");
        let stored_checksum = tar_octal(&header[148..156])?;
        let checksum: u64 = header
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    b' ' as u64
                } else {
                    *byte as u64
                }
            })
            .sum();
        anyhow::ensure!(stored_checksum == checksum, "invalid tar header checksum");
        let kind = header[156];
        let regular = kind == 0 || kind == b'0';
        let directory = kind == b'5';
        let path = tar_path(&header)?;
        anyhow::ensure!(safe_archive_path(&path), "unsafe archive path: {path}");
        let path_key = path.strip_suffix('/').unwrap_or(&path);
        anyhow::ensure!(
            seen.insert(path_key.to_string()),
            "duplicate archive member: {path}"
        );
        let size = tar_octal(&header[124..136])?;
        expanded = expanded
            .checked_add(size)
            .context("archive size overflow")?;
        anyhow::ensure!(
            expanded <= DOWNLOAD_LIMIT,
            "expanded archive exceeds {DOWNLOAD_LIMIT} byte limit"
        );
        anyhow::ensure!(
            regular || (directory && size == 0),
            "archive contains a link or special file: {path}"
        );
        anyhow::ensure!(
            size <= EXTRACTED_FILE_LIMIT,
            "archive member exceeds {EXTRACTED_FILE_LIMIT} byte limit"
        );
        let basename = Path::new(&path).file_name().and_then(|name| name.to_str());
        let is_wanted =
            regular && basename.is_some_and(|name| name == want || name == format!("{want}.exe"));
        if is_wanted {
            anyhow::ensure!(target.is_none(), "archive has duplicate executable targets");
            let mut output =
                NamedTempFile::new_in(dir).context("create extracted binary temporary file")?;
            copy_tar_payload(&mut tar, &mut output, size)?;
            target = Some(output);
        } else {
            skip_tar_payload(&mut tar, size)?;
        }
    }
    anyhow::ensure!(terminated, "tar archive has no end marker");
    let mut trailing = [0_u8; 32 * 1024];
    loop {
        let count = tar.read(&mut trailing).context("finish gzip tar stream")?;
        if count == 0 {
            break;
        }
        expanded = expanded
            .checked_add(count as u64)
            .context("archive size overflow")?;
        anyhow::ensure!(
            expanded <= DOWNLOAD_LIMIT,
            "expanded archive exceeds {DOWNLOAD_LIMIT} byte limit"
        );
        anyhow::ensure!(
            trailing[..count].iter().all(|byte| *byte == 0),
            "unexpected data after tar end marker"
        );
    }
    target.context(format!(
        "`{want}` not found as a regular member inside {asset}"
    ))
}

fn tar_octal(bytes: &[u8]) -> Result<u64> {
    let text = std::str::from_utf8(bytes).context("non-ASCII tar number")?;
    let text = text.trim_matches(|ch: char| ch == '\0' || ch == ' ');
    anyhow::ensure!(
        !text.is_empty() && text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)),
        "invalid tar number"
    );
    u64::from_str_radix(text, 8).context("tar number overflow")
}

fn tar_field(bytes: &[u8]) -> Result<&str> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).context("non-UTF-8 tar path")
}

fn tar_path(header: &[u8; 512]) -> Result<String> {
    let name = tar_field(&header[..100])?;
    let prefix = if &header[257..263] == b"ustar\0" {
        tar_field(&header[345..500])?
    } else {
        ""
    };
    Ok(if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    })
}

fn safe_archive_path(path: &str) -> bool {
    let normalized = path.strip_suffix('/').unwrap_or(path);
    !normalized.is_empty()
        && !normalized.starts_with('/')
        && !normalized.contains('\\')
        && !normalized.contains(':')
        && normalized
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn copy_tar_payload(reader: &mut impl Read, output: &mut NamedTempFile, size: u64) -> Result<()> {
    let mut remaining = size;
    let mut buffer = [0_u8; 32 * 1024];
    while remaining > 0 {
        let chunk_len = remaining.min(buffer.len() as u64) as usize;
        let count = reader.read(&mut buffer[..chunk_len])?;
        anyhow::ensure!(count > 0, "truncated tar member");
        output
            .write_all(&buffer[..count])
            .context("write extracted binary")?;
        remaining -= count as u64;
    }
    skip_tar_padding(reader, size)
}

fn skip_tar_payload(reader: &mut impl Read, size: u64) -> Result<()> {
    let mut remaining = size;
    let mut buffer = [0_u8; 32 * 1024];
    while remaining > 0 {
        let chunk_len = remaining.min(buffer.len() as u64) as usize;
        let count = reader.read(&mut buffer[..chunk_len])?;
        anyhow::ensure!(count > 0, "truncated tar member");
        remaining -= count as u64;
    }
    skip_tar_padding(reader, size)
}

fn skip_tar_padding(reader: &mut impl Read, size: u64) -> Result<()> {
    let padding = (512 - size % 512) % 512;
    let mut bytes = [0_u8; 511];
    reader
        .read_exact(&mut bytes[..padding as usize])
        .context("truncated tar padding")
}

#[cfg(unix)]
pub(crate) fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use flate2::Compression;
    use std::net::TcpListener;

    fn tar_header(path: &str, size: usize, kind: u8) -> [u8; 512] {
        let mut header = [0_u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        let size_octal = format!("{size:011o}\0");
        header[124..136].copy_from_slice(size_octal.as_bytes());
        header[156] = kind;
        header[257..263].copy_from_slice(b"ustar\0");
        header[148..156].fill(b' ');
        let checksum: u64 = header.iter().map(|byte| *byte as u64).sum();
        let checksum_octal = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum_octal.as_bytes());
        header
    }

    fn tar_gz(entries: &[(&str, u8, &[u8])]) -> NamedTempFile {
        let mut archive = NamedTempFile::new().unwrap();
        {
            let mut gzip =
                flate2::write::GzEncoder::new(archive.as_file_mut(), Compression::fast());
            for (path, kind, payload) in entries {
                gzip.write_all(&tar_header(path, payload.len(), *kind))
                    .unwrap();
                gzip.write_all(payload).unwrap();
                let padding = (512 - payload.len() % 512) % 512;
                gzip.write_all(&[0; 512][..padding]).unwrap();
            }
            gzip.write_all(&[0; 1024]).unwrap();
            gzip.finish().unwrap();
        }
        archive
    }

    fn oversized_tar_gz() -> NamedTempFile {
        let mut archive = NamedTempFile::new().unwrap();
        {
            let mut gzip =
                flate2::write::GzEncoder::new(archive.as_file_mut(), Compression::fast());
            gzip.write_all(&tar_header(
                "difft",
                (EXTRACTED_FILE_LIMIT + 1) as usize,
                b'0',
            ))
            .unwrap();
            gzip.write_all(&[0; 1024]).unwrap();
            gzip.finish().unwrap();
        }
        archive
    }

    fn short_payload_tar_gz() -> NamedTempFile {
        let mut archive = NamedTempFile::new().unwrap();
        {
            let mut gzip =
                flate2::write::GzEncoder::new(archive.as_file_mut(), Compression::fast());
            gzip.write_all(&tar_header("difft", 6, b'0')).unwrap();
            gzip.write_all(b"bin").unwrap();
            gzip.finish().unwrap();
        }
        archive
    }

    #[test]
    fn missing_release_pin_refuses_asset_before_network_access() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManagedTool::github(
            "unreviewed-tool",
            "example/unreachable",
            "0.1.0",
            vec![thegn_core::managed_tool::AssetRule {
                os: Os::current().unwrap(),
                arch: Arch::current().unwrap(),
                asset: "unreviewed-tool.tar.gz".to_string(),
            }],
        )
        .with_layout(dir.path().to_path_buf(), "bin/unreviewed-tool", ".version");
        let error = acquire_github_release(
            &tool,
            "unreviewed-tool.tar.gz",
            "https://127.0.0.1:1/unreachable",
        )
        .unwrap_err();
        assert!(error.to_string().contains("no trusted SHA-256 pin"));
        assert!(!tool.bin_path().exists());
    }

    #[test]
    fn tar_extraction_accepts_only_one_expected_regular_member() {
        let archive = tar_gz(&[("difft-0.63.0/difft", b'0', b"binary")]);
        let dir = tempfile::tempdir().unwrap();
        let output = extract_binary(archive.path(), "difft.tar.gz", "difft", dir.path()).unwrap();
        assert_eq!(std::fs::read(output.path()).unwrap(), b"binary");
    }

    #[test]
    fn gnu_tar_atime_fields_are_not_treated_as_a_path_prefix() {
        let mut header = tar_header("difft", 0, b'0');
        header[257..265].copy_from_slice(b"ustar  \0");
        header[345..357].copy_from_slice(b"1780000000\0\0");
        assert_eq!(tar_path(&header).unwrap(), "difft");
    }

    #[test]
    fn tar_extraction_rejects_traversal_links_duplicates_oversize_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let traversal = tar_gz(&[("../difft", b'0', b"binary")]);
        assert!(extract_binary(traversal.path(), "a.tar.gz", "difft", dir.path()).is_err());
        let absolute = tar_gz(&[("/difft", b'0', b"binary")]);
        assert!(extract_binary(absolute.path(), "a.tar.gz", "difft", dir.path()).is_err());
        for kind in *b"12346" {
            let special = tar_gz(&[("difft", kind, b"target")]);
            assert!(extract_binary(special.path(), "a.tar.gz", "difft", dir.path()).is_err());
        }
        let duplicate = tar_gz(&[("a/difft", b'0', b"one"), ("b/difft", b'0', b"two")]);
        assert!(extract_binary(duplicate.path(), "a.tar.gz", "difft", dir.path()).is_err());
        let oversized = oversized_tar_gz();
        assert!(extract_binary(oversized.path(), "a.tar.gz", "difft", dir.path()).is_err());
        let truncated = tar_gz(&[("difft", b'0', b"binary")]);
        let mut bytes = std::fs::read(truncated.path()).unwrap();
        bytes.truncate(bytes.len() - 8);
        std::fs::write(truncated.path(), bytes).unwrap();
        assert!(extract_binary(truncated.path(), "a.tar.gz", "difft", dir.path()).is_err());
        let short_payload = short_payload_tar_gz();
        let error =
            extract_binary(short_payload.path(), "a.tar.gz", "difft", dir.path()).unwrap_err();
        assert!(error.to_string().contains("truncated tar member"));
    }

    #[test]
    fn download_refuses_redirects_and_wrong_digest() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                assert!(stream.read(&mut request).unwrap() > 0);
                if String::from_utf8_lossy(&request).contains("/redirect ") {
                    stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /body\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else if String::from_utf8_lossy(&request).contains("/interrupt ") {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nshort").unwrap();
                } else if String::from_utf8_lossy(&request).contains("/large ") {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 134217729\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ntest").unwrap();
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        assert!(
            download_to(
                &format!("http://{address}/redirect"),
                dir.path(),
                "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
            )
            .is_err()
        );
        assert!(
            download_to(
                &format!("http://{address}/body"),
                dir.path(),
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
        assert!(
            download_to(
                &format!("http://{address}/interrupt"),
                dir.path(),
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
        assert!(
            download_to(
                &format!("http://{address}/large"),
                dir.path(),
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        server.join().unwrap();
    }

    fn fast() -> Limits {
        Limits {
            timeout: Duration::from_millis(1500),
            term_grace: Duration::from_millis(300),
            read_grace: Duration::from_secs(2),
        }
    }

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn installer_env_drops_credentials_and_uses_synthetic_home() {
        let home = Path::new("/private/install-home");
        let host = vec![
            (OsString::from("HOME"), OsString::from("/real")),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
            (OsString::from("SSH_AUTH_SOCK"), OsString::from("/socket")),
            (
                OsString::from("ANTHROPIC_API_KEY"),
                OsString::from("secret"),
            ),
            (
                OsString::from("THEGN_CONTROL_SOCKET"),
                OsString::from("/socket"),
            ),
            (OsString::from("NPM_TOKEN"), OsString::from("secret")),
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("LANG"), OsString::from("C.UTF-8")),
            (OsString::from("sYsTeMrOoT"), OsString::from("C:\\Windows")),
            (
                OsString::from("pathext"),
                OsString::from(".COM;.EXE;.BAT;.CMD"),
            ),
            (
                OsString::from("aPpDaTa"),
                OsString::from("C:\\Users\\User\\AppData\\Roaming"),
            ),
            (
                OsString::from("USERPROFILE"),
                OsString::from("C:\\Users\\User"),
            ),
            (
                OsString::from("LOCALAPPDATA"),
                OsString::from("C:\\Users\\User\\AppData\\Local"),
            ),
            (
                OsString::from("ComSpec"),
                OsString::from("C:\\Windows\\cmd.exe"),
            ),
            (
                OsString::from("https_proxy"),
                OsString::from("https://proxy"),
            ),
        ];
        let env = installer_env(host, home);
        let keys = env
            .iter()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "PATH",
            "LANG",
            "https_proxy",
            "sYsTeMrOoT",
            "pathext",
            "aPpDaTa",
            "USERPROFILE",
            "LOCALAPPDATA",
            "ComSpec",
            "RUSTUP_HOME",
            "HOME",
            "CARGO_HOME",
            "npm_config_userconfig",
            "npm_config_cache",
            "GIT_TERMINAL_PROMPT",
            "SSH_ASKPASS_REQUIRE",
            "GIT_ASKPASS",
            "npm_config_progress",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys, expected);
        let values = env
            .into_iter()
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(values["HOME"].as_os_str(), home.as_os_str());
        assert_eq!(
            values["CARGO_HOME"].as_os_str(),
            home.join("cargo").as_os_str()
        );
        assert_eq!(
            values["npm_config_userconfig"].as_os_str(),
            home.join("npmrc").as_os_str()
        );
        assert_eq!(
            values["npm_config_cache"].as_os_str(),
            home.join("npm-cache").as_os_str()
        );
        assert_eq!(
            values["RUSTUP_HOME"].as_os_str(),
            Path::new("/real/.rustup").as_os_str()
        );
    }

    #[test]
    fn installer_child_sees_only_the_allowlist() {
        let home = Path::new("/synthetic-home");
        let host = vec![
            (OsString::from("HOME"), OsString::from("/real")),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
            (OsString::from("SSH_AUTH_SOCK"), OsString::from("/socket")),
            (
                OsString::from("THEGN_CONTROL_SOCKET"),
                OsString::from("/socket"),
            ),
            (OsString::from("PATH"), std::env::var_os("PATH").unwrap()),
        ];
        let mut command = sh("env");
        command.env_clear().envs(installer_env(host, home));
        let output = run_bounded(&mut command, fast(), false).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout.head);
        assert!(stdout.lines().any(|line| line == "HOME=/synthetic-home"));
        assert!(!stdout.lines().any(|line| line.starts_with("GH_TOKEN=")));
        assert!(
            !stdout
                .lines()
                .any(|line| line.starts_with("SSH_AUTH_SOCK="))
        );
        assert!(!stdout.lines().any(|line| line.starts_with("THEGN_")));
    }

    #[test]
    fn installer_grammar_refuses_bad_sources() {
        for (value, expected) in [
            ("@scope/srv", true),
            ("@2scope/4srv", true),
            ("left-pad", true),
            ("--registry=x", false),
            ("../x", false),
            ("a b", false),
            ("@scope/", false),
            ("UPPER", false),
        ] {
            assert_eq!(valid_npm_package(value), expected, "npm package {value:?}");
        }
        for (value, expected) in [
            ("bugstalker", true),
            ("Crate_2-name", true),
            ("--help", false),
            ("../x", false),
            ("a b", false),
            ("", false),
        ] {
            assert_eq!(valid_crate_name(value), expected, "crate name {value:?}");
        }
        for (value, expected) in [
            ("0.4.6", true),
            ("1.2.3-rc.1+b5", true),
            ("1.2.3-rc..1", false),
            ("1.2.3-01", false),
            ("1.2.3+build..5", false),
            ("1.2", false),
            ("^1.2.3", false),
            ("latest", false),
            ("01.2.3", false),
            ("1.2.3-", false),
            ("1.2.3+build+extra", false),
        ] {
            assert_eq!(valid_exact_version(value), expected, "version {value:?}");
        }
        for (value, expected) in [
            ("bugstalker", true),
            ("tool_1.test", true),
            ("..", false),
            ("a/b", false),
            ("-x", false),
            (".hidden", false),
            ("", false),
        ] {
            assert_eq!(valid_tool_name(value), expected, "tool name {value:?}");
        }
    }

    #[test]
    fn installer_refuses_before_spawn() {
        let path = tempfile::tempdir().unwrap();
        let path_text = path.path().to_str().unwrap();
        let _path = crate::testenv::EnvVarGuard::set(&[("PATH", path_text)]);
        let mut tool = ManagedTool::npm("debugger", "left-pad", "left-pad", "1.2.3");
        tool.version = "latest".to_string();
        let error = acquire(&tool).unwrap_err();
        assert!(error.to_string().contains("invalid version"));
    }

    #[test]
    fn installer_promote_keeps_previous_install_on_missing_binary() {
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("tool");
        let staging = tempfile::Builder::new()
            .prefix(".tool.staging-")
            .tempdir_in(root.path())
            .unwrap();
        std::fs::create_dir_all(managed.join("bin")).unwrap();
        std::fs::write(managed.join("bin/old"), b"previous").unwrap();
        std::fs::create_dir_all(staging.path().join("bin")).unwrap();
        assert!(promote_staging(staging.path(), &managed, Path::new("bin/x")).is_err());
        assert_eq!(std::fs::read(managed.join("bin/old")).unwrap(), b"previous");
        drop(staging);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn installer_promote_preserves_previous_install_when_restore_fails() {
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("tool");
        let staging = tempfile::Builder::new()
            .prefix(".tool.staging-")
            .tempdir_in(root.path())
            .unwrap();
        std::fs::create_dir_all(managed.join("bin")).unwrap();
        std::fs::write(managed.join("bin/old"), b"previous").unwrap();
        std::fs::create_dir_all(staging.path().join("bin")).unwrap();
        std::fs::write(staging.path().join("bin/x"), b"new").unwrap();
        let mut rename_calls = 0;

        let result = promote_staging_with_rename(
            staging.path(),
            &managed,
            Path::new("bin/x"),
            |from, to| {
                rename_calls += 1;
                if rename_calls > 1 {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected rename failure",
                    ))
                } else {
                    std::fs::rename(from, to)
                }
            },
        );
        assert!(result.is_err());
        let entries = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let preserved = entries
            .iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".tool.old-"))
            })
            .unwrap();
        assert_eq!(
            std::fs::read(preserved.join("bin/old")).unwrap(),
            b"previous"
        );
    }

    #[test]
    fn installer_promote_swaps_in_the_staged_install() {
        let root = tempfile::tempdir().unwrap();
        let managed = root.path().join("tool");
        let staging = tempfile::Builder::new()
            .prefix(".tool.staging-")
            .tempdir_in(root.path())
            .unwrap();
        std::fs::create_dir_all(managed.join("bin")).unwrap();
        std::fs::write(managed.join("bin/old"), b"previous").unwrap();
        std::fs::create_dir_all(staging.path().join("bin")).unwrap();
        std::fs::write(staging.path().join("bin/x"), b"new").unwrap();
        promote_staging(staging.path(), &managed, Path::new("bin/x")).unwrap();
        assert_eq!(std::fs::read(managed.join("bin/x")).unwrap(), b"new");
        assert!(!managed.join("bin/old").exists());
        assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tool.old-")
        }));
    }

    /// Alive means present and not a zombie awaiting an (init) reaper.
    fn alive(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s
                .rsplit(')')
                .next()
                .is_some_and(|r| !r.trim_start().starts_with('Z')),
            Err(_) => false,
        }
    }

    fn pid_of(c: &Capture) -> i32 {
        String::from_utf8_lossy(&c.head)
            .trim()
            .parse()
            .expect("pid on stdout")
    }

    fn settle_dead(pid: i32) -> bool {
        for _ in 0..100 {
            if !alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn flood_is_bounded_and_child_completes() {
        let r = run_bounded(
            &mut sh(
                "head -c 6000000 /dev/zero | tr '\\0' x; head -c 3000000 /dev/zero | tr '\\0' y >&2",
            ),
            fast(),
            false,
        )
        .expect("flooding child still succeeds");
        assert_eq!(r.stdout.total, 6_000_000);
        assert_eq!(r.stderr.total, 3_000_000);
        assert!(r.stdout.retained() <= KEEP_HEAD + KEEP_TAIL);
        assert!(r.stderr.retained() <= KEEP_HEAD + KEEP_TAIL);
        assert!(r.stdout.render().contains("bytes elided"));
    }

    #[test]
    fn pipe_holding_descendant_does_not_outlive_a_clean_exit() {
        let t = Instant::now();
        let r = run_bounded(&mut sh("sleep 60 & echo $!; exit 0"), fast(), false)
            .expect("direct child exited 0");
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "must not wait on the holder"
        );
        assert!(settle_dead(pid_of(&r.stdout)), "descendant must be killed");
    }

    #[test]
    fn timeout_kills_the_whole_tree() {
        let t = Instant::now();
        let e = run_bounded(&mut sh("(sleep 60 & echo $! ; wait) & wait"), fast(), false)
            .err()
            .expect("deadline");
        assert!(t.elapsed() < Duration::from_secs(8));
        let RunError::Timeout { tail, .. } = e else {
            panic!("expected Timeout, got {e}");
        };
        let pid: i32 = tail
            .trim_start_matches(':')
            .trim()
            .parse()
            .expect("pid in tail");
        assert!(settle_dead(pid), "grandchild must be killed");
    }

    #[test]
    fn nonzero_carries_redacted_bounded_tail() {
        // Built at runtime so no token-shaped literal sits in the source.
        let canary = format!("gh{}_{}", "p", "abcdefghijklmnopqrstuvwxyz0123456789");
        let mut cmd = sh("echo boom token=$CANARY >&2; exit 3");
        cmd.env("CANARY", &canary);
        let e = run_bounded(&mut cmd, fast(), false).err().expect("nonzero");
        let RunError::Nonzero { tail, .. } = e else {
            panic!("expected Nonzero");
        };
        assert!(tail.contains("boom"));
        assert!(!tail.contains(&canary));
    }

    #[test]
    fn render_cuts_to_lines_at_the_elision() {
        let mut c = Capture::default();
        let line = "0123456789abcdef0123456789abcdef\n";
        for _ in 0..1000 {
            c.push(line.as_bytes());
        }
        let r = c.render();
        assert!(r.contains("bytes elided"));
        let (a, b) = r.split_once("\n[... ").unwrap();
        assert!(a.lines().all(|l| l.len() == 32));
        let b = b.split_once("...]\n").unwrap().1;
        assert!(b.lines().all(|l| l.len() == 32));
    }

    #[test]
    fn missing_binary_is_a_typed_spawn_error() {
        let e = run_bounded(&mut Command::new("thegn-no-such-binary-360"), fast(), false)
            .err()
            .expect("spawn failure");
        assert!(matches!(e, RunError::Spawn(_)));
    }

    #[test]
    fn repeated_timeouts_do_not_leak_threads() {
        fn threads() -> usize {
            std::fs::read_dir("/proc/self/task")
                .map(|d| d.count())
                .unwrap_or(0)
        }
        let quick = Limits {
            timeout: Duration::from_millis(150),
            ..fast()
        };
        let before = threads();
        for _ in 0..5 {
            let _ = run_bounded(&mut sh("sleep 60"), quick, false);
        }
        // Other tests run concurrently; 5 leaked reader pairs would be +10.
        let mut ok = false;
        for _ in 0..100 {
            if threads() <= before + 6 {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(ok, "reader threads leaked");
    }
}
