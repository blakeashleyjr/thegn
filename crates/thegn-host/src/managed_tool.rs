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
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use thegn_core::managed_tool::{Arch, ManagedTool, Os, Source};
use thegn_core::{msg, util};

/// Ceiling for a managed-tool setup subprocess (`npm install`, `pi install`,
/// `cargo install`). Generous — a cold npm/cargo fetch can legitimately run for
/// minutes — but bounds an infinite hang: a stalled registry/network otherwise
/// wedges `thegn agent setup`, `debug setup`, and (via the sprite `managed_pi`
/// provisioning step) the sandbox-creation loading screen forever.
const SETUP_CMD_TIMEOUT: Duration = Duration::from_secs(600); // 10 min

/// Ceiling for the archive helper (`tar`) that unpacks a release asset.
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);

/// Output retained per stream for diagnostics: the first and last bytes only.
/// Everything in between is read (so the child never blocks on a full pipe)
/// and counted, but dropped.
const KEEP_HEAD: usize = 4096;
const KEEP_TAIL: usize = 4096;

/// Limits for one bounded subprocess run. Production uses [`Limits::setup`] /
/// [`Limits::extract`]; tests shrink the durations.
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
    fn extract() -> Self {
        Self {
            timeout: EXTRACT_TIMEOUT,
            ..Self::setup()
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
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        if self.total > kept {
            s.push_str(&format!("\n[... {} bytes elided ...]\n", self.total - kept));
        }
        s.push_str(&String::from_utf8_lossy(&tail));
        thegn_core::ci_log::redact(s.trim())
    }
}

struct Captured {
    stdout: Capture,
    stderr: Capture,
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
/// `tee` forwards the bytes live to this process's stderr (`Some(true)`) or
/// stdout (`Some(false)`) for CLI presentation — capture is identical either
/// way, so output ownership never depends on the mode.
fn spawn_reader(
    name: &str,
    mut pipe: impl Read + Send + 'static,
    tee: Option<bool>,
) -> std::io::Result<ReaderRx> {
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut cap = Capture::default();
            let mut buf = [0u8; 8192];
            let result = loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => {
                        cap.push(&buf[..n]);
                        match tee {
                            Some(true) => {
                                let _ = std::io::stderr().write_all(&buf[..n]); // best-effort: live echo; capture is authoritative
                            }
                            Some(false) => {
                                let _ = std::io::stdout().write_all(&buf[..n]); // best-effort: live echo; capture is authoritative
                            }
                            None => {}
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
    let (mut child, group) = crate::platform::spawn_grouped(cmd).map_err(RunError::Spawn)?;
    let deadline = Instant::now() + limits.timeout;
    let out_pipe = child.stdout.take().expect("stdout was piped");
    let err_pipe = child.stderr.take().expect("stderr was piped");
    let readers =
        spawn_reader("thegn-setup-stdout", out_pipe, tee.then_some(false)).and_then(|out| {
            spawn_reader("thegn-setup-stderr", err_pipe, tee.then_some(true)).map(|err| (out, err))
        });
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
    let mut wait_err = None;
    loop {
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
    if timed_out || wait_err.is_some() {
        group.terminate();
        let grace_end = Instant::now() + limits.term_grace;
        while Instant::now() < grace_end
            && !matches!(crate::platform::gate_child_exited(&mut child), Ok(true))
        {
            std::thread::sleep(POLL);
        }
    }
    // The leader is still unreaped here, so the group identity is ours: kill
    // every remaining member (compilers, package scripts, pipe holders).
    group.kill();
    let status = child.wait();

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
    if held {
        return Err(RunError::PipeHeld { tail });
    }
    if !status.success() {
        return Err(RunError::Nonzero { status, tail });
    }
    Ok(Captured { stdout, stderr })
}

/// Run a setup subprocess. Output is always captured (bounded, redacted) and
/// the whole process tree is owned and killed on deadline; on the CLI the
/// output is additionally echoed live, while under the TUI it is only logged so
/// npm progress never paints over the alt-screen frame. Shared by the pi setup
/// and generic tool installs. `fail` is the message when the child exits
/// non-zero. Bounded by [`SETUP_CMD_TIMEOUT`].
// CLI path or off-loop (sprite provisioning runs it from spawn_blocking); the
// blocking wait never happens on the event loop.
pub fn run_setup_cmd(mut cmd: Command, ctx: &str, fail: &str) -> Result<()> {
    let tui = msg::tui_active();
    match run_bounded(&mut cmd, Limits::setup(), !tui) {
        Ok(c) => {
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
                util::have("npm"),
                "npm not found — needed to install {package}@{}. \
                 Install Node/npm, or put the tool on PATH.",
                tool.version
            );
            let mut cmd = Command::new("npm");
            cmd.args(["install", "--prefix"])
                .arg(tool.managed_dir())
                .arg(format!("{package}@{}", tool.version));
            run_setup_cmd(
                cmd,
                &format!("npm install {package}@{}", tool.version),
                &format!("npm install {package}@{} failed", tool.version),
            )
        }
        Source::Cargo { crate_name } => {
            anyhow::ensure!(
                util::have("cargo"),
                "cargo not found — needed to install {crate_name} {}. \
                 Install the Rust toolchain, or put the tool on PATH.",
                tool.version
            );
            let mut cmd = Command::new("cargo");
            cmd.args(["install", crate_name, "--version", &tool.version, "--root"])
                .arg(tool.managed_dir())
                .arg("--locked");
            run_setup_cmd(
                cmd,
                &format!("cargo install {crate_name} --version {}", tool.version),
                &format!("cargo install {crate_name} {} failed", tool.version),
            )
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
            let bin = tool.bin_path();
            if let Some(parent) = bin.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            if is_archive(asset) {
                // Tarball/zip release (e.g. difftastic): download to a temp file,
                // extract, and lift the wanted binary out to `bin`.
                let tmp = bin.with_file_name(format!(".{}.dl", tool.name));
                download_to(&url, &tmp)?;
                let r = extract_binary(&tmp, asset, &tool.name, &bin);
                let _ = std::fs::remove_file(&tmp); // best-effort: temp cleanup
                r?;
            } else {
                download_to(&url, &bin)?;
            }
            make_executable(&bin)?;
            Ok(())
        }
    }
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

/// Extract the binary named `want` (basename) out of a downloaded archive at
/// `archive` and place it at `dest`. Uses the OS `tar`/`unzip` — no archive
/// crate — into a scratch dir, then locates `want` (with a `.exe` tolerance on
/// Windows) and moves it. Off-loop (CLI / `spawn_blocking`), like the rest of
/// acquisition.
fn extract_binary(archive: &Path, asset: &str, want: &str, dest: &Path) -> Result<()> {
    let scratch = dest.with_file_name(format!(".{want}.x"));
    let _ = std::fs::remove_dir_all(&scratch); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    std::fs::create_dir_all(&scratch).with_context(|| format!("create {}", scratch.display()))?;
    let a = asset.to_ascii_lowercase();
    // `tar` on Windows 10+/macOS/Linux reads zips too; prefer it for one path.
    let flags = if a.ends_with(".zip") { "-xf" } else { "-xzf" };
    let mut cmd = Command::new("tar");
    cmd.arg(flags).arg(archive).arg("-C").arg(&scratch);
    // Owned process group + deadline + bounded output, like every setup child.
    let extracted = run_bounded(&mut cmd, Limits::extract(), false);
    let result = (|| -> Result<()> {
        extracted
            .map_err(|e| anyhow::anyhow!("extracting {asset} failed (is `tar` installed?): {e}"))?;
        let found = find_binary(&scratch, want)
            .with_context(|| format!("`{want}` not found inside {asset}"))?;
        // `rename` fails across filesystems; copy then remove is portable.
        std::fs::copy(&found, dest)
            .with_context(|| format!("install {} to {}", found.display(), dest.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&scratch); // best-effort: scratch cleanup
    result
}

/// Recursively find a file named `want` (or `want.exe`) under `dir`.
fn find_binary(dir: &Path, want: &str) -> Option<std::path::PathBuf> {
    let want_exe = format!("{want}.exe");
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(hit) = find_binary(&p, want) {
                return Some(hit);
            }
        } else if let Some(name) = p.file_name().and_then(|n| n.to_str())
            && (name == want || name == want_exe)
        {
            return Some(p);
        }
    }
    None
}

fn download_to(url: &str, dest: &Path) -> Result<()> {
    let resp = reqwest::blocking::get(url).with_context(|| format!("GET {url}"))?;
    anyhow::ensure!(
        resp.status().is_success(),
        "download {url} failed: HTTP {}",
        resp.status()
    );
    let bytes = resp
        .bytes()
        .with_context(|| format!("read body of {url}"))?;
    std::fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
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
