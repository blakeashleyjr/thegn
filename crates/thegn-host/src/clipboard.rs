//! Bounded asynchronous system-clipboard writes via the platform CLI tool
//! (`wl-copy` / `xclip` / `xsel` / `pbcopy` / `clip`). This complements the
//! OSC52 escape the host also emits on copy: OSC52 carries the selection to
//! the *outer* terminal (and works over SSH), while these tools hit the local
//! clipboard directly — covering terminals and desktops that don't honor
//! OSC52 (the common reason "it didn't actually copy"). The candidate
//! selection is pure and unit-tested. A single latest-wins worker owns helper
//! lifetimes and cancellation so a hung tool cannot accumulate resources.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Refuse rather than truncate: clipboard text can contain passwords and a
/// partial paste is more dangerous than an explicit failure.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Per-helper budget for process startup and closing stdin after the payload.
const HELPER_START_WRITE_BUDGET: Duration = Duration::from_secs(2);
/// Per-helper budget from spawn through observed exit.
const HELPER_EXIT_BUDGET: Duration = Duration::from_secs(5);
/// Whole-copy ceiling across all fallback candidates.
const OPERATION_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyError {
    NotStarted,
    PayloadTooLarge { bytes: usize },
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted => f.write_str("clipboard worker is unavailable"),
            Self::PayloadTooLarge { bytes } => write!(
                f,
                "clipboard text is {bytes} bytes; the maximum is {MAX_PAYLOAD_BYTES} bytes"
            ),
        }
    }
}

struct CopyJob {
    payload: Arc<[u8]>,
    cancelled: Arc<AtomicBool>,
}

struct WorkerState {
    pending: Option<CopyJob>,
    current: Option<Arc<AtomicBool>>,
    shutdown: bool,
}

struct ClipboardInner {
    state: Mutex<WorkerState>,
    wake: Condvar,
    outcome: Mutex<Option<CopyOutcome>>,
    waker: termwiz::terminal::TerminalWaker,
}

pub struct Clipboard {
    inner: Arc<ClipboardInner>,
    worker: Option<JoinHandle<()>>,
}

static ACTIVE: OnceLock<Mutex<Weak<ClipboardInner>>> = OnceLock::new();

fn active_slot() -> &'static Mutex<Weak<ClipboardInner>> {
    ACTIVE.get_or_init(|| Mutex::new(Weak::new()))
}

impl Clipboard {
    pub fn start(waker: termwiz::terminal::TerminalWaker) -> Self {
        let inner = Arc::new(ClipboardInner {
            state: Mutex::new(WorkerState {
                pending: None,
                current: None,
                shutdown: false,
            }),
            wake: Condvar::new(),
            outcome: Mutex::new(None),
            waker,
        });
        *active_slot().lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&inner);
        let worker_inner = Arc::clone(&inner);
        let worker = std::thread::Builder::new()
            .name("thegn-clipboard".into())
            .spawn(move || worker_loop(worker_inner))
            .expect("clipboard worker thread must start");
        Self {
            inner,
            worker: Some(worker),
        }
    }

    pub fn shutdown(&mut self) {
        let should_join = {
            let mut state = self.inner.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.shutdown {
                false
            } else {
                state.shutdown = true;
                state.pending = None;
                if let Some(cancelled) = &state.current {
                    cancelled.store(true, Ordering::Release);
                }
                self.inner.wake.notify_one();
                true
            }
        };
        if should_join && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let mut active = active_slot().lock().unwrap_or_else(|e| e.into_inner());
        if active
            .upgrade()
            .is_some_and(|current| Arc::ptr_eq(&current, &self.inner))
        {
            *active = Weak::new();
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Queue the latest clipboard value. Replacing a pending or active value
/// cancels the older helper; cancellation is not reported as a failure.
pub fn copy(text: &str) -> Result<(), CopyError> {
    let bytes = text.as_bytes();
    if bytes.len() > MAX_PAYLOAD_BYTES {
        if let Some(inner) = active_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
        {
            record_outcome(&inner, CopyOutcome::Failed);
        }
        return Err(CopyError::PayloadTooLarge { bytes: bytes.len() });
    }
    let Some(inner) = active_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()
    else {
        return Err(CopyError::NotStarted);
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let job = CopyJob {
        payload: Arc::from(bytes),
        cancelled,
    };
    let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.shutdown {
        return Err(CopyError::NotStarted);
    }
    if let Some(cancelled) = &state.current {
        cancelled.store(true, Ordering::Release);
    }
    state.pending = Some(job);
    inner.wake.notify_one();
    Ok(())
}

/// Drain the latest completed result on the UI loop. Results contain no
/// clipboard text, helper argv, or error output.
pub fn poll_outcome() -> Option<CopyOutcome> {
    let inner = active_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .upgrade()?;
    inner
        .outcome
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

fn record_outcome(inner: &ClipboardInner, outcome: CopyOutcome) {
    *inner.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
    let _ = inner.waker.wake(); // best-effort: completion must not fail the copy path
}

fn worker_loop(inner: Arc<ClipboardInner>) {
    crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
    loop {
        let job = {
            let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pending.is_none() && !state.shutdown {
                state = inner.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            if state.shutdown {
                return;
            }
            let job = state.pending.take().expect("pending clipboard job");
            state.current = Some(Arc::clone(&job.cancelled));
            job
        };
        let succeeded = run_job(&job, Instant::now());
        let cancelled = job.cancelled.load(Ordering::Acquire);
        let mut state = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        state.current = None;
        if !cancelled && !state.shutdown {
            record_outcome(
                &inner,
                if succeeded {
                    CopyOutcome::Succeeded
                } else {
                    CopyOutcome::Failed
                },
            );
        }
    }
}

/// Ordered clipboard-tool argv candidates for `(os, wayland)`. Pure — the
/// caller resolves `os`/`wayland` from the environment. The first tool that
/// successfully spawns wins.
pub fn candidates(os: &str, wayland: bool) -> Vec<Vec<&'static str>> {
    match os {
        "macos" => vec![vec!["pbcopy"]],
        "windows" => vec![vec!["clip"]],
        // Linux/BSD: prefer the session's display-server tool, then fall back
        // to the other so a mislabelled session still copies.
        _ if wayland => vec![
            vec!["wl-copy"],
            vec!["xclip", "-selection", "clipboard"],
            vec!["xsel", "--clipboard", "--input"],
        ],
        _ => vec![
            vec!["xclip", "-selection", "clipboard"],
            vec!["xsel", "--clipboard", "--input"],
            vec!["wl-copy"],
        ],
    }
}

/// The candidate list for the live environment.
fn detect() -> Vec<Vec<&'static str>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    candidates(std::env::consts::OS, wayland)
}

/// Ordered clipboard-*read* argv candidates for `(os, wayland)` — the paste
/// counterpart of [`candidates`]. Pure; the first tool that produces output wins.
pub fn paste_candidates(os: &str, wayland: bool) -> Vec<Vec<&'static str>> {
    match os {
        "macos" => vec![vec!["pbpaste"]],
        // PowerShell's Get-Clipboard is the closest built-in on Windows.
        "windows" => vec![vec![
            "powershell",
            "-NoProfile",
            "-Command",
            "Get-Clipboard",
        ]],
        _ if wayland => vec![
            vec!["wl-paste", "--no-newline"],
            vec!["xclip", "-selection", "clipboard", "-o"],
            vec!["xsel", "--clipboard", "--output"],
        ],
        _ => vec![
            vec!["xclip", "-selection", "clipboard", "-o"],
            vec!["xsel", "--clipboard", "--output"],
            vec!["wl-paste", "--no-newline"],
        ],
    }
}

/// Read the system clipboard, trying each candidate tool until one produces
/// output. Returns `None` when no tool is installed or the clipboard is empty.
/// Synchronous (a short subprocess) — call off the hot path; used for the `"+`
/// register paste, a deliberate user action.
pub fn paste() -> Option<String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    for argv in paste_candidates(std::env::consts::OS, wayland) {
        if let Some(out) = read_from(&argv) {
            return Some(out);
        }
    }
    None
}

/// Ordered clipboard-**image**-read argv candidates for `(os, wayland)`, each of
/// which writes the clipboard image to **stdout as PNG** — the single interchange
/// format for the image-paste drop (THE-24). Pure; the first tool that produces
/// non-empty output wins. A tool that fails when the clipboard holds no image
/// (`wl-paste`/`xclip` exit non-zero for a missing type) simply advances the
/// chain, so no separate "list types" probe is needed — the read's own failure
/// is the signal.
///
/// The `image/png` MIME is requested explicitly so the clipboard tool converts
/// or refuses; thegn never parses or transcodes the bytes itself (no decoder
/// attack surface on untrusted clipboard content).
pub fn image_read_candidates(os: &str, wayland: bool) -> Vec<Vec<&'static str>> {
    match os {
        // pngpaste writes the clipboard image to stdout as PNG with `-`.
        "macos" => vec![vec!["pngpaste", "-"]],
        // Best-effort: emit the clipboard image as raw PNG bytes on stdout via
        // .NET. Untested on the shipping (Linux) alpha; degrades honestly when
        // absent. Kept as a candidate so a working box gets it for free.
        "windows" => vec![vec![
            "powershell",
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
             $i=[Windows.Forms.Clipboard]::GetImage(); \
             if($i){$m=New-Object IO.MemoryStream; \
             $i.Save($m,[Drawing.Imaging.ImageFormat]::Png); \
             $o=[Console]::OpenStandardOutput(); $b=$m.ToArray(); $o.Write($b,0,$b.Length)}",
        ]],
        // Prefer the session's display-server tool, then fall back to the other
        // so a mislabelled session still reads (mirrors `paste_candidates`).
        _ if wayland => vec![
            vec!["wl-paste", "-t", "image/png"],
            vec!["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
        ],
        _ => vec![
            vec!["xclip", "-selection", "clipboard", "-t", "image/png", "-o"],
            vec!["wl-paste", "-t", "image/png"],
        ],
    }
}

/// Read a clipboard **image** as PNG bytes, trying each candidate tool until one
/// produces non-empty output. `None` when no tool is installed or the clipboard
/// holds no image. Off-loop only (an image read plus the subsequent transfer is
/// not acceptable on the event loop) — called from the `paste_image` worker.
pub fn read_image() -> Option<Vec<u8>> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    for argv in image_read_candidates(std::env::consts::OS, wayland) {
        if let Some(bytes) = read_image_from(&argv) {
            return Some(bytes);
        }
    }
    None
}

/// Spawn one image-read tool and capture its stdout as **raw bytes** (not
/// lossy-UTF-8 like [`read_from`], since PNG is binary). `None` if it can't
/// spawn, exits non-zero, or yields nothing.
// off-loop: only called from the paste_image worker (spawn_blocking), never the
// event loop — an image read + transfer is far past the ms budget a keypress has.
#[expect(clippy::disallowed_methods)]
fn read_image_from(argv: &[&str]) -> Option<Vec<u8>> {
    let (cmd, args) = argv.split_first()?;
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(out.stdout)
}

/// Spawn one read tool and capture its stdout as a `String`. `None` if it can't
/// spawn, exits non-zero, or yields no output.
// Accepted on-loop subprocess: a clipboard read is ms-scale and only runs on
// an explicit `"+` paste keypress. Revisit (spawn_blocking + channel) if a
// clipboard tool ever hangs in practice.
#[expect(clippy::disallowed_methods)]
fn read_from(argv: &[&str]) -> Option<String> {
    let (cmd, args) = argv.split_first()?;
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn run_job(job: &CopyJob, started: Instant) -> bool {
    let deadline = started + OPERATION_BUDGET;
    for argv in detect() {
        if job.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return false;
        }
        if pipe_to(&argv, Arc::clone(&job.payload), &job.cancelled, deadline) {
            return true;
        }
    }
    false
}

/// Spawn one helper and write the payload with bounded progress. Returns
/// `true` only if the tool exits successfully. The writer is one short-lived,
/// joined thread for the current helper only; killing the process group closes
/// its pipe and lets that thread settle.
#[expect(clippy::disallowed_methods)]
fn pipe_to(
    argv: &[&str],
    payload: Arc<[u8]>,
    cancelled: &AtomicBool,
    operation_deadline: Instant,
) -> bool {
    let Some((cmd, args)) = argv.split_first() else {
        return false;
    };
    let attempt_started = Instant::now();
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok((mut child, group)) = crate::platform::spawn_clipboard_helper(&mut command) else {
        return false;
    };
    let spawned = Instant::now();
    let write_deadline = (attempt_started + HELPER_START_WRITE_BUDGET).min(operation_deadline);
    let exit_deadline = (spawned + HELPER_EXIT_BUDGET).min(operation_deadline);
    let Some(stdin) = child.stdin.take() else {
        stop_attempt(child, group, None);
        return false;
    };
    let (write_tx, write_rx) = std::sync::mpsc::sync_channel(1);
    let writer = std::thread::spawn(move || {
        let mut stdin = stdin;
        let result = stdin.write_all(&payload);
        drop(stdin);
        let _ = write_tx.send(result);
    });

    loop {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= write_deadline {
            stop_attempt(child, group, Some(writer));
            return false;
        }
        match write_rx.try_recv() {
            Ok(Ok(())) => break,
            Ok(Err(_)) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                stop_attempt(child, group, Some(writer));
                return false;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    loop {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= exit_deadline {
            stop_attempt(child, group, Some(writer));
            return false;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // The leader remains unreaped here, so its group identity is
                // still owned while descendants are cleaned up.
                group.kill();
                let _ = child.wait();
                let _ = writer.join();
                return status.success();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                stop_attempt(child, group, Some(writer));
                return false;
            }
        }
    }
}

#[expect(clippy::disallowed_methods)]
fn stop_attempt(
    mut child: std::process::Child,
    group: crate::platform::GroupHandle,
    writer: Option<JoinHandle<()>>,
) {
    group.kill();
    let _ = child.kill();
    let _ = child.wait();
    if let Some(writer) = writer {
        let _ = writer.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_uses_pbcopy() {
        assert_eq!(candidates("macos", false), vec![vec!["pbcopy"]]);
        assert_eq!(candidates("macos", true), vec![vec!["pbcopy"]]);
    }

    #[test]
    fn windows_uses_clip() {
        assert_eq!(candidates("windows", false), vec![vec!["clip"]]);
    }

    #[test]
    fn wayland_prefers_wl_copy_then_x_tools() {
        let c = candidates("linux", true);
        assert_eq!(c[0], vec!["wl-copy"]);
        assert_eq!(c[1], vec!["xclip", "-selection", "clipboard"]);
        assert!(c.iter().any(|a| a[0] == "xsel"));
    }

    #[test]
    fn x11_prefers_xclip_then_falls_back_to_wl_copy() {
        let c = candidates("linux", false);
        assert_eq!(c[0], vec!["xclip", "-selection", "clipboard"]);
        assert_eq!(c.last().unwrap(), &vec!["wl-copy"]);
    }

    #[test]
    #[cfg(unix)]
    fn pipe_to_returns_false_on_nonzero_exit() {
        // A tool that spawns fine but exits non-zero (like `xclip` failing to
        // reach the display server) must NOT count as success, or it would
        // break the fallback chain. `/bin/false` models that; `/bin/true`
        // models a tool that actually stored the selection.
        let cancelled = AtomicBool::new(false);
        assert!(!pipe_to(
            &["false"],
            Arc::from(&b"x"[..]),
            &cancelled,
            Instant::now() + OPERATION_BUDGET,
        ));
        assert!(pipe_to(
            &["true"],
            Arc::from(&b"x"[..]),
            &cancelled,
            Instant::now() + OPERATION_BUDGET,
        ));
        assert!(
            !pipe_to(
                &["definitely-not-a-real-binary-xyz"],
                Arc::from(&b"x"[..]),
                &cancelled,
                Instant::now() + OPERATION_BUDGET,
            ),
            "unspawnable tool is a failure"
        );
    }

    #[test]
    fn oversized_payload_is_refused_without_truncation() {
        let error = CopyError::PayloadTooLarge {
            bytes: MAX_PAYLOAD_BYTES + 1,
        };
        assert_eq!(error, CopyError::PayloadTooLarge { bytes: 1_048_577 });
        assert!(error.to_string().contains("1048577"));
        assert!(error.to_string().contains("1048576"));
    }

    #[test]
    #[cfg(unix)]
    fn unread_stdin_is_cancelled_without_waiting_for_helper_exit() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_later = Arc::clone(&cancelled);
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            cancel_later.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = pipe_to(
            &["sh", "-c", "sleep 30"],
            Arc::from(vec![b'x'; MAX_PAYLOAD_BYTES]),
            &cancelled,
            started + OPERATION_BUDGET,
        );
        let _ = timer.join();
        assert!(!result);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn paste_candidates_mirror_copy_tools() {
        assert_eq!(paste_candidates("macos", false), vec![vec!["pbpaste"]]);
        let c = paste_candidates("linux", true);
        assert_eq!(c[0], vec!["wl-paste", "--no-newline"]);
        assert!(c.iter().any(|a| a[0] == "xclip" && a.contains(&"-o")));
        let x = paste_candidates("linux", false);
        assert_eq!(x[0], vec!["xclip", "-selection", "clipboard", "-o"]);
    }

    #[test]
    fn image_read_candidates_request_png_per_platform() {
        assert_eq!(
            image_read_candidates("macos", false),
            vec![vec!["pngpaste", "-"]]
        );
        // Wayland prefers wl-paste with the image/png target, then xclip.
        let w = image_read_candidates("linux", true);
        assert_eq!(w[0], vec!["wl-paste", "-t", "image/png"]);
        assert!(w[1].contains(&"image/png") && w[1][0] == "xclip");
        // X11 prefers xclip, then wl-paste.
        let x = image_read_candidates("linux", false);
        assert_eq!(
            x[0],
            vec!["xclip", "-selection", "clipboard", "-t", "image/png", "-o"]
        );
        assert_eq!(x.last().unwrap()[0], "wl-paste");
        // Every candidate names the PNG interchange type (the drop's format).
        for os in ["linux", "macos"] {
            for cand in image_read_candidates(os, false) {
                assert!(
                    cand.iter().any(|a| a.contains("png")),
                    "{os} candidate {cand:?} must request PNG"
                );
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn read_image_from_captures_raw_stdout_and_rejects_failures() {
        // A tool that emits bytes and exits zero yields those exact bytes …
        assert_eq!(
            read_image_from(&["printf", "\\211PNG"]).as_deref(),
            Some(&b"\x89PNG"[..])
        );
        // … a non-zero exit is skipped …
        assert_eq!(read_image_from(&["false"]), None);
        // … empty output is treated as "no image" …
        assert_eq!(read_image_from(&["true"]), None);
        // … and an unspawnable tool is a miss, not a panic.
        assert_eq!(read_image_from(&["definitely-not-a-real-binary-xyz"]), None);
    }
}
