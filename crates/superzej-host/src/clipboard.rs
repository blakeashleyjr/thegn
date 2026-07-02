//! Best-effort system-clipboard writes via the platform CLI tool
//! (`wl-copy` / `xclip` / `xsel` / `pbcopy` / `clip`). This complements the
//! OSC52 escape the host also emits on copy: OSC52 carries the selection to
//! the *outer* terminal (and works over SSH), while these tools hit the local
//! clipboard directly — covering terminals and desktops that don't honor
//! OSC52 (the common reason "it didn't actually copy"). The candidate
//! selection is pure and unit-tested; the spawn is fire-and-forget on a
//! detached thread so it never blocks the event loop.

use std::io::Write;
use std::process::{Command, Stdio};

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

/// Fire-and-forget copy: on a detached thread, try each candidate tool and
/// pipe `text` to the first that spawns. No-op when none are installed (the
/// OSC52 path the caller also emits still covers that case).
pub fn copy(text: &str) {
    let text = text.to_string();
    std::thread::spawn(move || {
        for argv in detect() {
            if pipe_to(&argv, &text) {
                break;
            }
        }
    });
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

/// Spawn one tool and write `text` to its stdin. Returns `true` if it spawned.
// off-loop: only called from copy()'s detached std::thread.
#[expect(clippy::disallowed_methods)]
fn pipe_to(argv: &[&str], text: &str) -> bool {
    let Some((cmd, args)) = argv.split_first() else {
        return false;
    };
    let child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
        // Drop closes stdin so the tool sees EOF and stores the content.
    }
    let _ = child.wait();
    true
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
    fn paste_candidates_mirror_copy_tools() {
        assert_eq!(paste_candidates("macos", false), vec![vec!["pbpaste"]]);
        let c = paste_candidates("linux", true);
        assert_eq!(c[0], vec!["wl-paste", "--no-newline"]);
        assert!(c.iter().any(|a| a[0] == "xclip" && a.contains(&"-o")));
        let x = paste_candidates("linux", false);
        assert_eq!(x[0], vec!["xclip", "-selection", "clipboard", "-o"]);
    }
}
