//! Explicit-action clipboard **image** paste (THE-24).
//!
//! When the user pastes and the clipboard holds an image (and, for the `"+`
//! register, no text), thegn reads that image **once**, size-gates it, drops it
//! as a generated-name PNG, and hands the file's path back to the event loop to
//! paste into the focused pane through the normal bracketed-paste hardening.
//!
//! - **Local pane** → a 0600 file in a 0700 dir under `$XDG_RUNTIME_DIR/thegn/
//!   paste` (fallback `$XDG_STATE_HOME/thegn/paste`); the absolute path is pasted.
//! - **Remote/provider pane** → the bytes stream over the worktree's **existing**
//!   `GitLoc` control channel (`sh_command` + stdin; no scp/sftp), landing in a
//!   confined drop dir in the user's own account on the remote; the **remote**
//!   path is pasted.
//!
//! Everything here runs OFF the event loop (a `spawn_blocking` worker): the
//! clipboard read, the size gate, the local write or ssh stream, and the sweep.
//! The outcome rides a channel + a `TerminalWaker` pulse; only the final paste
//! (pane input ⇒ `Panes` damage) and any status message (chrome ⇒ `Full`) touch
//! the loop. **The clipboard is read only inside this worker, only when the user
//! invoked a paste — never on a timer, at startup, on focus, or from a watcher.**
//! Image bytes are never logged (byte counts and outcomes only).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use termwiz::terminal::TerminalWaker;
use tokio::sync::mpsc as tokio_mpsc;
use tokio::task;

use thegn_core::config::ClipboardConfig;
use thegn_core::paste_drop;
use thegn_core::remote::GitLoc;

/// The result of one paste-image worker run, applied on the event loop.
#[derive(Debug)]
pub(crate) enum PasteImageOutcome {
    /// The image landed; paste `path` into pane `pane`. `remote` and `bytes` are
    /// for the (content-free) status line only.
    Pasted {
        pane: u32,
        path: String,
        remote: bool,
        bytes: usize,
    },
    /// The clipboard held no readable image (or no clipboard tool is installed).
    NoImage,
    /// The image exceeded `max_image_bytes`; nothing was written or sent.
    TooLarge { bytes: u64, cap: u64 },
    /// The write or transfer failed; `reason` is safe to show (no image content).
    Failed { reason: String },
}

/// Spawn the off-loop worker: read the clipboard image, gate it, drop it (local
/// or remote), sweep stale drops, and send a [`PasteImageOutcome`] + waker pulse.
///
/// `worktree` is the focused tab's worktree path (used to resolve the pane's
/// `GitLoc` **off-loop** — `GitLoc::for_worktree` opens the DB); `pane` is echoed
/// back so the loop pastes into the right pane even if focus moved.
pub(crate) fn spawn(
    worktree: PathBuf,
    pane: u32,
    cfg: ClipboardConfig,
    tx: tokio_mpsc::UnboundedSender<PasteImageOutcome>,
    waker: TerminalWaker,
) {
    task::spawn_blocking(move || {
        // User-visible (the pasted path lands in the pane) but not blocking — the
        // keystroke already returned. Declared per-task: tokio reuses blocking
        // threads across QoS classes.
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let outcome = run(&worktree, pane, &cfg);
        if tx.send(outcome).is_ok() {
            let _ = waker.wake(); // best-effort: waker pulse: an input nudge must never fail the calling path
        }
    });
}

/// The whole worker body, factored out so it is straight-line and testable in
/// parts. Never reads the clipboard unless called (i.e. only from [`spawn`]).
fn run(worktree: &Path, pane: u32, cfg: &ClipboardConfig) -> PasteImageOutcome {
    let Some(bytes) = crate::clipboard::read_image() else {
        return PasteImageOutcome::NoImage;
    };
    // Size gate FIRST — before a single byte is written locally or leaves the
    // machine. `bytes` is already in memory (the read is bounded by the OS pipe
    // for streaming tools; a pathological image is still caught here before any
    // write/stream), so this is the hard exfiltration bound.
    let len = bytes.len() as u64;
    if paste_drop::over_limit(len, cfg.max_image_bytes) {
        return PasteImageOutcome::TooLarge {
            bytes: len,
            cap: cfg.max_image_bytes,
        };
    }

    let name = drop_filename();
    // Resolving the location opens the DB — correctly off-loop here.
    let loc = GitLoc::for_worktree(worktree);
    let n = bytes.len();
    let result = if loc.is_remote() {
        remote_drop(&loc, cfg, &name, &bytes).map(|path| (path, true))
    } else {
        local_drop(cfg, &name, &bytes).map(|path| (path, false))
    };
    match result {
        Ok((path, remote)) => {
            tracing::debug!(target: "thegn::paste", bytes = n, remote, "clipboard image dropped");
            PasteImageOutcome::Pasted {
                pane,
                path,
                remote,
                bytes: n,
            }
        }
        Err(reason) => PasteImageOutcome::Failed { reason },
    }
}

/// The drop filename — the e2e freeze pins it (generated names are volatile);
/// otherwise `img-<utc-ms>-<6 rand>.png`.
fn drop_filename() -> String {
    if let Some(name) = crate::e2e_freeze::paste_image_name() {
        return name;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    paste_drop::generated_name(now_ms, &rand_token(6))
}

/// A short random alphanumeric token (CSPRNG) for the generated name — never
/// derived from clipboard metadata.
fn rand_token(len: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut raw = vec![0u8; len];
    // best-effort: a failed CSPRNG read just yields the zero-seed token; the
    // millisecond timestamp already disambiguates, so a collision is astronomically
    // unlikely and would only overwrite one same-ms drop.
    let _ = getrandom::fill(raw.as_mut_slice());
    raw.iter()
        .map(|b| ALPHABET[(*b as usize) % 36] as char)
        .collect()
}

/// The local drop directory: `$XDG_RUNTIME_DIR/thegn/paste` (preferred — tmpfs,
/// user-private) or `$XDG_STATE_HOME/thegn/paste`.
fn local_drop_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(thegn_core::util::xdg_state_home)
        .join("thegn")
        .join("paste")
}

/// Write the image to the local drop dir (0700 dir, 0600 file), sweep stale
/// drops, and return the absolute path. Errors are `String`s safe to surface.
fn local_drop(cfg: &ClipboardConfig, name: &str, bytes: &[u8]) -> Result<String, String> {
    write_drop_to_dir(&local_drop_dir(), cfg.keep_hours, name, bytes)
}

/// The dir-explicit core of [`local_drop`], so the write/perms/sweep are
/// unit-testable against a tmp dir without touching the process environment.
fn write_drop_to_dir(
    dir: &Path,
    keep_hours: u64,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    // Restrict the DIRECTORY first, then write inside it. `fsperm` tightens
    // perms *after* creation, so a file created first would sit at the umask
    // default for a moment; doing the dir first means that window is inside an
    // owner-only directory and nobody else can traverse in to see it.
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    thegn_core::fsperm::restrict_dir_to_owner(dir)
        .map_err(|e| format!("restrict {}: {e}", dir.display()))?;
    sweep_local(dir, keep_hours);
    let path = dir.join(name);
    write_drop_file(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Create the drop file **exclusively** (`create_new`, i.e. `O_EXCL`) and then
/// restrict it to the owner. The caller has already made the parent dir
/// owner-only, so the brief pre-`restrict` window is unreachable by anyone else;
/// `O_EXCL` additionally refuses to follow a pre-planted symlink or reuse an
/// existing inode. A same-name leftover (the e2e-frozen name, a retry) is
/// removed first — it can only be one of our own drops in our own dir.
fn write_drop_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    let mut f = match opts.open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(path)?;
            opts.open(path)?
        }
        other => other?,
    };
    f.write_all(bytes)?;
    thegn_core::fsperm::restrict_to_owner(path)
}

/// Stream the image over the worktree's existing control channel into the
/// confined remote drop dir, and return the **remote** absolute path (printed by
/// the remote script so `$HOME` is resolved without a second round-trip).
// off-loop: the whole worker runs in spawn_blocking, so the blocking
// `Child::wait` here is off the event loop (the disallowed-methods rule targets
// on-loop waits).
#[expect(clippy::disallowed_methods)]
fn remote_drop(
    loc: &GitLoc,
    cfg: &ClipboardConfig,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let script = paste_drop::remote_drop_script(&cfg.remote_dir, name, cfg.keep_hours);
    let mut cmd = loc.sh_command(&script);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("ssh spawn: {e}"))?;

    // Write the PNG on a scoped thread so a full pipe buffer (bytes > ~64 KiB)
    // can't deadlock against reading the remote's stdout.
    let mut stdin = child.stdin.take().ok_or("no stdin pipe")?;
    let payload = bytes.to_vec();
    let writer = std::thread::spawn(move || {
        // best-effort: a short write here is EPIPE — the remote died mid-stream
        // — and that already surfaces as a non-zero `child.wait()` status
        // below, which is what the caller reports. Failing here too would only
        // race the two error paths for a worse message.
        let _ = stdin.write_all(&payload); // best-effort: stdout write: EPIPE on a closed |head pipe is normal
        // Drop closes the pipe → the remote `cat` sees EOF and finishes.
    });

    let mut out = String::new();
    let mut stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let read_res = stdout.read_to_string(&mut out);
    let mut err = String::new();
    if let Some(mut se) = child.stderr.take() {
        let _ = se.read_to_string(&mut err); // best-effort: drain: bounded read of auxiliary output; failure loses the buffer, not the outcome
    }
    let _ = writer.join(); // best-effort: thread join: a panicked helper loses its output, not the caller
    let status = child.wait().map_err(|e| format!("ssh wait: {e}"))?;

    read_res.map_err(|e| format!("read remote path: {e}"))?;
    if !status.success() {
        let detail = err.trim();
        let detail = if detail.is_empty() {
            "remote command failed"
        } else {
            detail
        };
        return Err(detail.to_string());
    }
    let path = out.trim();
    if path.is_empty() {
        return Err("remote drop produced no path".to_string());
    }
    Ok(path.to_string())
}

/// Delete local drop files older than `keep_hours`, confined to the drop dir
/// (regular files matching the `img-*.png` stem only). Best-effort — a sweep
/// failure must never fail the paste.
fn sweep_local(dir: &Path, keep_hours: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Only files this feature wrote — never a stray user file.
        if !(name.starts_with("img-") && name.ends_with(".png")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let age_secs = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if paste_drop::sweep_eligible(age_secs, keep_hours) {
            let _ = std::fs::remove_file(&path); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }
    }
}

/// Turn an outcome into the loop's pending action: an optional (pane, path) to
/// paste, plus a status message (empty ⇒ no message). Pure so the mapping —
/// including the exact user-facing strings — is unit-tested without a pane.
pub(crate) fn resolve(outcome: PasteImageOutcome) -> (Option<(u32, String)>, String) {
    match outcome {
        PasteImageOutcome::Pasted {
            pane,
            path,
            remote,
            bytes,
        } => {
            let where_ = if remote { "remote" } else { "local" };
            let msg = format!(
                "Pasted image ({}, {where_})",
                paste_drop::human_bytes(bytes as u64)
            );
            (Some((pane, path)), msg)
        }
        PasteImageOutcome::NoImage => (
            None,
            "No image on the clipboard (need wl-paste / xclip / pngpaste)".to_string(),
        ),
        PasteImageOutcome::TooLarge { bytes, cap } => (
            None,
            format!(
                "Clipboard image {} exceeds the {} cap ([clipboard] max_image_bytes)",
                paste_drop::human_bytes(bytes),
                paste_drop::human_bytes(cap)
            ),
        ),
        PasteImageOutcome::Failed { reason } => (None, format!("Image paste failed: {reason}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rand_token_is_alnum_and_right_length() {
        let t = rand_token(6);
        assert_eq!(t.len(), 6);
        assert!(
            t.chars()
                .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn resolve_pasted_carries_pane_and_path() {
        let (paste, msg) = resolve(PasteImageOutcome::Pasted {
            pane: 7,
            path: "/d/img.png".into(),
            remote: true,
            bytes: 2048,
        });
        assert_eq!(paste, Some((7, "/d/img.png".to_string())));
        assert!(msg.contains("remote") && msg.contains("2 KiB"));
    }

    #[test]
    fn resolve_failures_paste_nothing_and_name_the_cause() {
        let (p, msg) = resolve(PasteImageOutcome::NoImage);
        assert!(p.is_none());
        assert!(msg.to_lowercase().contains("no image"));

        let (p, msg) = resolve(PasteImageOutcome::TooLarge {
            bytes: 11 * 1024 * 1024,
            cap: 10 * 1024 * 1024,
        });
        assert!(p.is_none(), "over-limit pastes nothing");
        assert!(msg.contains("11.0 MiB") && msg.contains("10.0 MiB"));

        let (p, msg) = resolve(PasteImageOutcome::Failed {
            reason: "host unreachable".into(),
        });
        assert!(p.is_none());
        assert!(msg.contains("host unreachable"));
    }

    #[test]
    fn local_drop_writes_0600_in_0700_dir_and_sweeps() {
        let dir = std::env::temp_dir().join(format!(
            "thegn-paste-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller

        let path = write_drop_to_dir(&dir, 24, "img-1-aa.png", b"\x89PNG-fake").expect("drop ok");
        let p = Path::new(&path);
        assert!(p.exists());
        assert_eq!(std::fs::read(p).unwrap(), b"\x89PNG-fake");
        // Read back through the fsperm seam so this stays platform-free (`None`
        // where mode bits don't exist; there the DACL is the restriction).
        if let Some(mode) = thegn_core::fsperm::mode_bits(p).unwrap() {
            assert_eq!(mode, 0o600, "file is 0600");
        }
        if let Some(mode) = thegn_core::fsperm::mode_bits(&dir).unwrap() {
            assert_eq!(mode, 0o700, "dir is 0700");
        }

        // A same-name leftover is replaced rather than failing the paste — the
        // e2e-frozen name repeats, and `create_new` would otherwise error.
        let again = write_drop_to_dir(&dir, 24, "img-1-aa.png", b"second").expect("re-drop ok");
        assert_eq!(std::fs::read(&again).unwrap(), b"second");

        // A stale drop is swept; a fresh one and a stray file survive.
        let stale = dir.join("img-old-bb.png");
        std::fs::write(&stale, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let stray = dir.join("keep.txt");
        std::fs::write(&stray, b"y").unwrap();
        sweep_local(&dir, 24);
        assert!(!stale.exists(), "stale img-*.png swept");
        assert!(stray.exists(), "non-drop file untouched");
        assert!(p.exists(), "fresh drop kept");

        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
