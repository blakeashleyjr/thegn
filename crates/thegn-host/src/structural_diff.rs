//! Structural (difftastic) diff plumbing, shared by the `thegn diff --structural`
//! CLI verb and the full-screen DiffView modal.
//!
//! This is a **read-only** surface: difft is invoked as git's external diff with
//! the sanitizers deliberately OFF (structural output never feeds `git apply` —
//! every *stageable* diff keeps `SANITIZED_DIFF`/`--no-ext-diff` elsewhere). The
//! tool is resolved through the managed-tool tiers (override → PATH → managed),
//! bounded by a wall-clock timeout plus difft's own byte/graph limits, and every
//! failure falls back to the internal viewer — never blocking, never erroring.
//!
//! difft emits SGR ANSI; the modal path parses it with the pure
//! [`thegn_core::ansi_cells`] parser (unknown escapes stripped) so the diff is
//! composed in truecolor and quantized once at the `wire.rs` chokepoint.

use anyhow::{Context, Result};
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use thegn_core::ansi_cells::{self, StyledLine};
use thegn_core::config::{Config, StructuralDiff};
use thegn_core::managed_tool::Resolution;
use thegn_core::remote::GitLoc;

const MAX_STRUCTURAL_STDOUT: usize = 8 * 1024 * 1024;
const MAX_STRUCTURAL_STDERR_TAIL: usize = 16 * 1024;
const READER_GRACE: Duration = Duration::from_millis(500);
const READ_CHUNK: usize = 8 * 1024;

/// Resolve difft through the three managed-tool tiers, returning a *usable*
/// binary path — `None` when nothing resolves (or the managed copy is not yet
/// installed), so the caller degrades to the internal viewer.
pub fn resolve_difft(cfg: &Config) -> Option<String> {
    let tool = thegn_core::difft::difft_tool();
    let over = cfg.managed_tools.get(&tool.name);
    match tool.resolve(over, thegn_core::util::which_path) {
        Resolution::Override { path, .. } | Resolution::OnPath { path } => Some(path),
        // Managed tier only counts when the pinned binary is actually present.
        Resolution::Managed { path, current } => current.then_some(path),
    }
}

/// The difft path a read-only diff surface should use under `mode`, or `None` to
/// render internally. `Off` never uses difft; `Auto`/`Difft` both require the
/// tool to resolve (the difference — whether to show a "falling back" notice on
/// a miss — is the caller's, keyed on the mode).
pub fn choose(cfg: &Config, mode: StructuralDiff) -> Option<String> {
    match mode {
        StructuralDiff::Off => None,
        StructuralDiff::Auto | StructuralDiff::Difft => resolve_difft(cfg),
    }
}

/// Bounds for a modal structural render.
#[derive(Debug, Clone, Copy)]
pub struct CaptureOpts {
    /// Content width difft lays out into (`DFT_WIDTH`).
    pub width: usize,
    /// Light terminal background (`DFT_BACKGROUND=light`) vs dark.
    pub light_bg: bool,
    /// Skip files whose diff exceeds this many bytes (`DFT_BYTE_LIMIT`).
    pub byte_limit: u64,
    /// Skip a file whose diff graph exceeds this many nodes (`DFT_GRAPH_LIMIT`).
    pub graph_limit: u64,
    /// Wall-clock ceiling for the whole difft run.
    pub timeout: Duration,
}

impl Default for CaptureOpts {
    fn default() -> Self {
        CaptureOpts {
            width: 120,
            light_bg: false,
            // difft's README is candid that it scales poorly on huge changes;
            // these mirror its own defaults' spirit and bound a runaway.
            byte_limit: 1_000_000,
            graph_limit: 3_000,
            timeout: Duration::from_secs(10),
        }
    }
}

/// The `DFT_*` environment difft reads (every flag has an env twin), for a run
/// routed through `git diff`'s external-diff hook.
fn dft_env(opts: &CaptureOpts) -> Vec<(String, String)> {
    vec![
        ("DFT_COLOR".into(), "always".into()),
        ("DFT_DISPLAY".into(), "inline".into()),
        ("DFT_WIDTH".into(), opts.width.max(20).to_string()),
        (
            "DFT_BACKGROUND".into(),
            if opts.light_bg { "light" } else { "dark" }.into(),
        ),
        ("DFT_BYTE_LIMIT".into(), opts.byte_limit.to_string()),
        ("DFT_GRAPH_LIMIT".into(), opts.graph_limit.to_string()),
    ]
}

/// Run `git diff <target>` through difft (as `GIT_EXTERNAL_DIFF`) and parse its
/// ANSI into styled lines for the modal. Off-loop (the caller spawns this on a
/// worker thread). Any failure is an `Err` the caller turns into a fall-back.
pub fn capture(
    loc: &GitLoc,
    target: &str,
    file: Option<&str>,
    difft: &str,
    opts: &CaptureOpts,
) -> Result<Vec<StyledLine>> {
    let mut args: Vec<&str> = vec!["diff", target];
    if let Some(f) = file {
        args.push("--");
        args.push(f);
    }
    // GIT_EXTERNAL_DIFF replaces git's own diff with difft; the sanitizers are
    // intentionally absent — this output is never staged.
    let mut env: Vec<(&str, &str)> =
        vec![("GIT_EXTERNAL_DIFF", difft), ("GIT_TERMINAL_PROMPT", "0")];
    let dft = dft_env(opts);
    for (k, v) in &dft {
        env.push((k.as_str(), v.as_str()));
    }
    let mut cmd = loc.git_command_env(&env, &args);
    let raw = run_capture(&mut cmd, opts.timeout).context("difft capture")?;
    Ok(ansi_cells::parse_ansi(&raw))
}

/// Stream `thegn diff --structural` to the terminal: `git diff` through difft,
/// inheriting stdio so difft's own colours render natively. CLI-only (no event
/// loop), so it inherits stdio and blocks on the child.
// CLI path: `thegn diff --structural` runs synchronously.
#[expect(clippy::disallowed_methods)]
pub fn run_cli(loc: &GitLoc, target: &str, file: Option<&str>, difft: &str) -> Result<()> {
    let mut args: Vec<&str> = vec!["diff", target];
    if let Some(f) = file {
        args.push("--");
        args.push(f);
    }
    let env: Vec<(&str, &str)> = vec![
        ("GIT_EXTERNAL_DIFF", difft),
        ("DFT_COLOR", "always"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ];
    let status = loc
        .git_command_env(&env, &args)
        .status()
        .context("failed to run git diff through difft")?;
    anyhow::ensure!(status.success(), "structural diff failed");
    Ok(())
}

/// Spawn `cmd` in a managed process group, with bounded output and runtime.
/// Reader threads report over channels so a descendant holding a pipe cannot
/// keep this worker blocked beyond the collection grace.
#[expect(clippy::disallowed_methods)]
fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let (mut child, group) = crate::platform::spawn_grouped(cmd).context("spawn difft")?;
    let Some(stdout_pipe) = child.stdout.take() else {
        group.kill();
        if let Err(error) = child.wait() {
            tracing::debug!(target: "thegn::diff", %error, "failed to reap difft child without stdout pipe");
        }
        anyhow::bail!("missing difft stdout pipe");
    };
    let Some(stderr_pipe) = child.stderr.take() else {
        group.kill();
        if let Err(error) = child.wait() {
            tracing::debug!(target: "thegn::diff", %error, "failed to reap difft child without stderr pipe");
        }
        anyhow::bail!("missing difft stderr pipe");
    };
    let (out_tx, out_rx) = mpsc::sync_channel(1);
    let (err_tx, err_rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        if let Err(error) = out_tx.send(drain_stdout(stdout_pipe)) {
            tracing::debug!(target: "thegn::diff", %error, "stdout reader result receiver closed");
        }
    });
    std::thread::spawn(move || {
        if let Err(error) = err_tx.send(drain_stderr(stderr_pipe)) {
            tracing::debug!(target: "thegn::diff", %error, "stderr reader result receiver closed");
        }
    });

    let status = loop {
        match child.try_wait() {
            Err(error) => {
                group.kill();
                if let Err(wait_error) = child.wait() {
                    tracing::debug!(target: "thegn::diff", %wait_error, "failed to reap difft after wait error");
                }
                return Err(error).context("wait difft");
            }
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                group.kill();
                if let Err(error) = child.wait() {
                    tracing::debug!(target: "thegn::diff", %error, "failed to reap timed-out difft child");
                }
                anyhow::bail!("difft timed out after {}s", timeout.as_secs());
            }
        }
    };

    // The leader has exited. Kill any external-diff descendants before waiting
    // for EOF so inherited pipe handles cannot extend the capture indefinitely.
    group.kill();
    let collect_deadline = Instant::now() + READER_GRACE;
    let stdout = receive_reader(&out_rx, collect_deadline, "stdout")??;
    let stderr = receive_reader(&err_rx, collect_deadline, "stderr")??;
    if stdout.1 {
        anyhow::bail!("structural diff output exceeded 8 MiB");
    }
    let stdout = String::from_utf8_lossy(&stdout.0).into_owned();
    if status.success() {
        return Ok(stdout);
    }
    anyhow::bail!(
        "difft exited {}: {}",
        status,
        String::from_utf8_lossy(&stderr).trim()
    );
}

fn receive_reader<T>(rx: &Receiver<T>, deadline: Instant, stream: &str) -> Result<T> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(result) => Ok(result),
        Err(RecvTimeoutError::Timeout) => {
            tracing::debug!(target: "thegn::diff", stream, "structural diff reader exceeded collection grace");
            anyhow::bail!("structural diff {stream} reader exceeded collection grace")
        }
        Err(RecvTimeoutError::Disconnected) => {
            anyhow::bail!("structural diff {stream} reader stopped")
        }
    }
}

fn drain_stdout(mut pipe: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut chunk = [0; READ_CHUNK];
    let mut exceeded = false;
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            return Ok((retained, exceeded));
        }
        let available = MAX_STRUCTURAL_STDOUT.saturating_sub(retained.len());
        let keep = read.min(available);
        retained.extend_from_slice(&chunk[..keep]);
        exceeded |= keep < read;
    }
}

fn drain_stderr(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut chunk = [0; READ_CHUNK];
    loop {
        let read = pipe.read(&mut chunk)?;
        if read == 0 {
            return Ok(tail);
        }
        let excess = tail
            .len()
            .saturating_add(read)
            .saturating_sub(MAX_STRUCTURAL_STDERR_TAIL);
        tail.drain(..excess);
        tail.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell_available() -> bool {
        crate::platform::gate_test_shell("true").is_some()
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.path().join("tracked.txt"), "before\n").unwrap();
        git(&dir, &["add", "tracked.txt"]);
        git(&dir, &["commit", "-q", "-m", "initial"]);
        std::fs::write(dir.path().join("tracked.txt"), "after\n").unwrap();
        dir
    }

    fn git(dir: &tempfile::TempDir, args: &[&str]) {
        let status = thegn_core::util::git_cmd(dir.path())
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Structural Diff Test")
            .env("GIT_AUTHOR_EMAIL", "structural-diff@example.invalid")
            .env("GIT_COMMITTER_NAME", "Structural Diff Test")
            .env("GIT_COMMITTER_EMAIL", "structural-diff@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn capture_with(difft: &str, timeout: Duration) -> Result<Vec<StyledLine>> {
        let dir = fixture();
        let opts = CaptureOpts {
            timeout,
            ..CaptureOpts::default()
        };
        capture(
            &GitLoc::Local(dir.path().to_path_buf()),
            "HEAD",
            None,
            difft,
            &opts,
        )
    }

    #[test]
    fn timeout_kills_external_diff_descendants() {
        if !shell_available() {
            return;
        }
        let dir = fixture();
        let marker = dir.path().join("survived");
        let snippet = format!("(sleep 1; touch '{}') & sleep 30 #", marker.display());
        let opts = CaptureOpts {
            timeout: Duration::from_millis(300),
            ..CaptureOpts::default()
        };
        let start = Instant::now();
        let result = capture(
            &GitLoc::Local(dir.path().to_path_buf()),
            "HEAD",
            None,
            &snippet,
            &opts,
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            !marker.exists(),
            "external-diff descendant survived timeout"
        );
    }

    #[test]
    fn descendant_held_pipe_does_not_block_capture() {
        if !shell_available() {
            return;
        }
        let start = Instant::now();
        let result = capture_with("echo ok; sleep 30 & #", Duration::from_secs(10));
        assert!(result.is_ok(), "capture failed: {result:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn oversized_stdout_is_rejected_after_bounded_drain() {
        if !shell_available() {
            return;
        }
        let result = capture_with("head -c 20000000 /dev/zero #", Duration::from_secs(10));
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("exceeded 8 MiB"), "{err:#}");
    }

    #[test]
    fn missing_difft_tool_is_an_error() {
        if !shell_available() {
            return;
        }
        let result = capture_with("/definitely/missing/thegn-difft", Duration::from_secs(5));
        assert!(result.is_err());
    }

    #[test]
    fn dft_environment_clamps_width_and_sets_background() {
        let opts = CaptureOpts {
            width: 5,
            light_bg: true,
            ..CaptureOpts::default()
        };
        let env = dft_env(&opts);
        assert!(env.contains(&("DFT_WIDTH".into(), "20".into())));
        assert!(env.contains(&("DFT_BACKGROUND".into(), "light".into())));
    }
}
