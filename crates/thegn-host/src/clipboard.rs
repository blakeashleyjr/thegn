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
/// pipe `text` to the first that *succeeds*. A tool that spawns but exits
/// non-zero (e.g. `xclip` in a session that's actually Wayland, so it can't
/// reach X) doesn't stop the chain — the next candidate is tried. No-op when
/// none are installed (the OSC52 path the caller also emits still covers that).
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

/// Spawn one tool and write `text` to its stdin. Returns `true` only if the
/// tool *exited successfully* — a tool that spawns but fails (e.g. can't reach
/// the display server) returns `false` so the fallback chain keeps trying.
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
        let _ = stdin.write_all(text.as_bytes()); // best-effort: stdout write: EPIPE on a closed |head pipe is normal
        // Drop closes stdin so the tool sees EOF and stores the content.
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
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
        assert!(!pipe_to(&["false"], "x"), "nonzero exit must be a failure");
        assert!(pipe_to(&["true"], "x"), "zero exit is success");
        assert!(
            !pipe_to(&["definitely-not-a-real-binary-xyz"], "x"),
            "unspawnable tool is a failure"
        );
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
