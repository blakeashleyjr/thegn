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
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use thegn_core::ansi_cells::{self, StyledLine};
use thegn_core::config::{Config, StructuralDiff};
use thegn_core::managed_tool::Resolution;
use thegn_core::remote::GitLoc;

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

/// Spawn `cmd`, capturing stdout, bounded by `timeout` (the child is killed on
/// deadline). stderr is captured but only used in the error message. Off-loop
/// only — the blocking wait never touches the event loop.
// Off-loop (CLI / worker thread); the terminal `wait` after `kill` reaps a dead
// child promptly.
#[expect(clippy::disallowed_methods)]
fn run_capture(cmd: &mut Command, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
        .context("spawn difft")?;
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let out_h = std::thread::spawn(move || drain(stdout_pipe));
    let err_h = std::thread::spawn(move || drain(stderr_pipe));
    loop {
        if let Some(status) = child.try_wait().context("wait difft")? {
            let stdout = String::from_utf8_lossy(&out_h.join().unwrap_or_default()).into_owned();
            if status.success() {
                return Ok(stdout);
            }
            let stderr = String::from_utf8_lossy(&err_h.join().unwrap_or_default()).into_owned();
            anyhow::bail!("difft exited {}: {}", status, stderr.trim());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("difft timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn drain(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(mut p) = pipe {
        // best-effort: same shape as `host_discovery`'s drain — the bytes read
        // so far are returned, and a truncated payload cannot masquerade as
        // success because the caller parses it (JSON / exit status).
        let _ = p.read_to_end(&mut buf);
    }
    buf
}
