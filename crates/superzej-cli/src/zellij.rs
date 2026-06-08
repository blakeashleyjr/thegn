//! Thin wrappers around `zellij action ...`. No-op gracefully outside a session.
//!
//! superzej drives its OWN pinned zellij, fully isolated from any system
//! `zellij`: a private binary (`SUPERZEJ_ZELLIJ_BIN`, wired by Nix to the
//! version-pinned `superzej-zellij`), a private socket/session namespace
//! (`ZELLIJ_SOCKET_DIR` = `~/.superzej/run`), and a private cache
//! (`XDG_CACHE_HOME` = `~/.superzej/cache`: plugin artifacts + permissions).
//! So superzej sessions never appear in a system `zellij list-sessions`, and
//! wiping its cache can't disturb a system zellij (and vice-versa). Every zellij
//! invocation goes through `command()` (one-shot) or `export_private_env()`
//! before exec (cold-start, so the whole session inherits it).

use crate::util;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The zellij binary superzej drives — a private, version-pinned build, NOT the
/// system `zellij`. `SUPERZEJ_ZELLIJ_BIN` overrides it (dev: the dev-shell
/// zellij; the Nix package wires it to the pinned `superzej-zellij`).
pub fn bin() -> String {
    std::env::var("SUPERZEJ_ZELLIJ_BIN").unwrap_or_else(|_| "zellij".into())
}

/// Private IPC/socket dir — superzej sessions live in their own namespace,
/// invisible to (and unable to clobber) any system `zellij`. An already-set
/// `ZELLIJ_SOCKET_DIR` wins (so an in-session `superzej` inherits the running
/// server's namespace, and test harnesses can point at a throwaway sandbox).
pub fn socket_dir() -> PathBuf {
    std::env::var_os("ZELLIJ_SOCKET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| util::superzej_dir().join("run"))
}

/// Private cache dir (zellij plugin artifacts, `permissions.kdl`, session info),
/// keeping superzej's zellij fully separate from `~/.cache/zellij`.
pub fn cache_dir() -> PathBuf {
    util::superzej_dir().join("cache")
}

/// Apply superzej's private zellij environment to `cmd` (socket + cache dirs),
/// creating the dirs first.
fn private_env(cmd: &mut Command) {
    let (sock, cache) = (socket_dir(), cache_dir());
    let _ = std::fs::create_dir_all(&sock);
    let _ = std::fs::create_dir_all(&cache);
    cmd.env("ZELLIJ_SOCKET_DIR", &sock);
    cmd.env("XDG_CACHE_HOME", &cache);
}

/// A `Command` for the private zellij with the isolation env applied.
pub fn command() -> Command {
    let mut c = Command::new(bin());
    private_env(&mut c);
    c
}

/// Export the private zellij env into THIS process, so an exec'd zellij — and
/// the whole session it spawns (panes, plugin `run_command`, in-session
/// `superzej` calls) — inherits the private socket + cache. Cold-start/attach.
pub fn export_private_env() {
    let (sock, cache) = (socket_dir(), cache_dir());
    let _ = std::fs::create_dir_all(&sock);
    let _ = std::fs::create_dir_all(&cache);
    std::env::set_var("ZELLIJ_SOCKET_DIR", &sock);
    std::env::set_var("XDG_CACHE_HOME", &cache);
}

pub fn in_zellij() -> bool {
    std::env::var_os("ZELLIJ").is_some() || std::env::var_os("ZELLIJ_SESSION_NAME").is_some()
}

/// The single zellij session that hosts the whole superzej interface.
pub fn ui_session() -> String {
    std::env::var("SUPERZEJ_SESSION_NAME").unwrap_or_else(|_| "superzej".into())
}

/// Whether we're inside a superzej-*managed* zellij session.
pub fn in_superzej_session() -> bool {
    std::env::var_os("SUPERZEJ_SESSION").is_some()
}

fn action(args: &[&str]) -> bool {
    command()
        .arg("action")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Open a floating pane that closes when its command exits.
pub fn new_float(cwd: &Path, name: &str, cmd: &[&str]) -> bool {
    let mut a: Vec<String> = vec![
        "new-pane".into(),
        "--floating".into(),
        "--close-on-exit".into(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--name".into(),
        name.into(),
        "--".into(),
    ];
    a.extend(cmd.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    action(&refs)
}

/// Open the bottom file-manager drawer: a bottom-anchored, full-width (or
/// centered) floating pane that closes when its command exits. Pinned so it
/// stays above other floats. `height`/`width` come from `[drawer]` config.
pub fn new_drawer(cwd: &Path, name: &str, height: &str, width: &str, cmd: &[&str]) -> bool {
    let mut a = drawer_args(cwd, name, height, width);
    a.extend(cmd.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    action(&refs)
}

/// The `new-pane` args (through the trailing `--`) for a bottom-anchored drawer
/// of the given geometry — pure, so the geometry is unit-testable. `width`
/// "full" spans the tab; "center" is a narrower bottom band. `height` is a
/// percentage (or a row count, anchored near the bottom).
fn drawer_args(cwd: &Path, name: &str, height: &str, width: &str) -> Vec<String> {
    let (x, w) = match width {
        "center" => ("10%", "80%"),
        _ => ("0", "100%"),
    };
    let y = drawer_top(height);
    vec![
        "new-pane".into(),
        "--floating".into(),
        "--pinned".into(),
        "true".into(),
        "--close-on-exit".into(),
        "-x".into(),
        x.into(),
        "-y".into(),
        y,
        "--width".into(),
        w.into(),
        "--height".into(),
        height.to_string(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--name".into(),
        name.into(),
        "--".into(),
    ]
}

/// Top edge for a drawer of the given height: `100-H%` for a percentage height,
/// else a best-effort bottom anchor.
fn drawer_top(height: &str) -> String {
    match height
        .trim()
        .strip_suffix('%')
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        Some(h) => format!("{}%", 100u32.saturating_sub(h.min(100))),
        None => "60%".into(),
    }
}

/// Whether a pane named `name` exists in the FOCUSED tab (parsed from
/// `dump-layout`). Scoped to the focused tab so a drawer open in another
/// worktree's tab doesn't read as "present" here — each worktree's drawer is
/// independent.
pub fn pane_named_in_focused_tab(name: &str) -> bool {
    let Some(out) = command()
        .args(["action", "dump-layout"])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return false;
    };
    pane_named_in_focused_tab_in(&String::from_utf8_lossy(&out.stdout), name)
}

/// The pure parser behind `pane_named_in_focused_tab`: brace-depth tracking
/// isolates the `tab … focus=true { … }` block and looks for `name="<name>"`
/// inside it only.
fn pane_named_in_focused_tab_in(dump: &str, name: &str) -> bool {
    let needle = format!("name=\"{name}\"");
    let mut depth: i32 = 0;
    let mut tab_at: Option<i32> = None; // brace depth at which the focused tab opened
    for line in dump.lines() {
        // Inside the focused tab block, a matching pane name means present.
        // (The tab's own `name="…"` is a slug/branch, never the pane needle, and
        // is on the `focus=true` line which we only enter *after* this check.)
        if tab_at.is_some() && line.contains(&needle) {
            return true;
        }
        if tab_at.is_none() && line.trim_start().starts_with("tab ") && line.contains("focus=true")
        {
            tab_at = Some(depth);
        }
        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if let Some(start) = tab_at {
            if depth <= start {
                tab_at = None; // left the focused tab block
            }
        }
    }
    false
}

/// Open a new workspace tab (with a layout if given). Named layouts resolve
/// against superzej's private layout dir (passed explicitly via `--layout-dir`),
/// never the user's `~/.config/zellij/layouts`.
pub fn new_tab(name: &str, cwd: &Path, layout: Option<&str>) -> bool {
    let cwd = cwd.to_string_lossy();
    let ldir = crate::util::layout_dir().to_string_lossy().into_owned();
    let mut a = vec!["new-tab", "--name", name, "--cwd", &cwd];
    if let Some(l) = layout {
        a.push("--layout-dir");
        a.push(&ldir);
        a.push("--layout");
        a.push(l);
    }
    action(&a)
}

/// Open a plain tiled pane (default shell) at `cwd` — a "panel".
pub fn new_pane_bare(cwd: &Path, name: &str, direction: &str) -> bool {
    action(&[
        "new-pane",
        "--direction",
        direction,
        "--cwd",
        &cwd.to_string_lossy(),
        "--name",
        name,
    ])
}

/// Open a named tiled pane running `cmd` at `cwd`. Unlike drawers/floats, this
/// participates in the active layout and does not close automatically when the
/// command exits.
pub fn new_pane_cmd(cwd: &Path, name: &str, direction: &str, cmd: &[&str]) -> bool {
    let mut a = pane_cmd_args(cwd, name, direction);
    a.extend(cmd.iter().map(|s| s.to_string()));
    let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
    action(&refs)
}

/// The `new-pane` args (through trailing `--`) for a tiled command pane.
fn pane_cmd_args(cwd: &Path, name: &str, direction: &str) -> Vec<String> {
    vec![
        "new-pane".into(),
        "--direction".into(),
        direction.into(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--name".into(),
        name.into(),
        "--".into(),
    ]
}

pub fn close_pane() -> bool {
    action(&["close-pane"])
}

/// Close the active tab.
pub fn close_tab() -> bool {
    action(&["close-tab"])
}

pub fn go_to_tab_name(name: &str) -> bool {
    action(&["go-to-tab-name", name])
}

/// Names of all tabs in the current session.
pub fn tab_names() -> Vec<String> {
    command()
        .args(["action", "query-tab-names"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Rename the active tab.
pub fn rename_tab(name: &str) -> bool {
    action(&["rename-tab", name])
}

/// Rename the focused pane (the calling process's own pane, for `Run` panes).
pub fn rename_pane(name: &str) -> bool {
    action(&["rename-pane", name])
}

/// Name of the focused tab, parsed from `dump-layout` (the only query that
/// reports focus; `query-tab-names` doesn't). Fresh from the server, so it's
/// trustworthy even from plugin-spawned processes with stale-plugin parents.
pub fn focused_tab_name() -> Option<String> {
    let out = command()
        .args(["action", "dump-layout"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout).lines().find_map(|l| {
        let l = l.trim_start();
        if !(l.starts_with("tab ") && l.contains("focus=true")) {
            return None;
        }
        let rest = l.split_once("name=\"")?.1;
        Some(rest.split_once('"')?.0.to_string())
    })
}

/// Send a `zellij pipe` message to a plugin.
pub fn pipe_plugin(url: &str, name: &str, payload: &str) -> bool {
    command()
        .args(["pipe", "--plugin", url, "--name", name, "--", payload])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn drawer_top_from_percentage() {
        assert_eq!(drawer_top("35%"), "65%");
        assert_eq!(drawer_top("100%"), "0%");
        assert_eq!(drawer_top("0%"), "100%");
        assert_eq!(drawer_top(" 40% "), "60%"); // trimmed
    }

    #[test]
    fn drawer_top_clamps_and_falls_back() {
        assert_eq!(drawer_top("150%"), "0%"); // clamp >100 to bottom
        assert_eq!(drawer_top("20"), "60%"); // non-percentage -> best-effort anchor
        assert_eq!(drawer_top("auto"), "60%");
    }

    #[test]
    fn drawer_args_full_width_geometry() {
        let a = drawer_args(Path::new("/wt"), "superzej-files", "35%", "full");
        // anchored bottom-left, spanning the width, closing on exit, named + pinned.
        assert!(a.contains(&"--floating".to_string()));
        assert!(a.contains(&"--close-on-exit".to_string()));
        assert!(a.windows(2).any(|w| w[0] == "--pinned" && w[1] == "true"));
        assert!(a.windows(2).any(|w| w[0] == "-x" && w[1] == "0"));
        assert!(a.windows(2).any(|w| w[0] == "-y" && w[1] == "65%"));
        assert!(a.windows(2).any(|w| w[0] == "--width" && w[1] == "100%"));
        assert!(a.windows(2).any(|w| w[0] == "--height" && w[1] == "35%"));
        assert!(a.windows(2).any(|w| w[0] == "--cwd" && w[1] == "/wt"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--name" && w[1] == "superzej-files")
        );
        assert_eq!(a.last().unwrap(), "--"); // command appended after this
    }

    #[test]
    fn drawer_args_center_width_geometry() {
        let a = drawer_args(Path::new("/wt"), "superzej-files", "50%", "center");
        assert!(a.windows(2).any(|w| w[0] == "-x" && w[1] == "10%"));
        assert!(a.windows(2).any(|w| w[0] == "--width" && w[1] == "80%"));
        assert!(a.windows(2).any(|w| w[0] == "-y" && w[1] == "50%"));
    }

    #[test]
    fn pane_cmd_args_open_named_tiled_pane() {
        let a = pane_cmd_args(Path::new("/wt"), "\u{1f4cc} logs", "Right");
        assert_eq!(a[0], "new-pane");
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--direction" && w[1] == "Right")
        );
        assert!(a.windows(2).any(|w| w[0] == "--cwd" && w[1] == "/wt"));
        assert!(
            a.windows(2)
                .any(|w| w[0] == "--name" && w[1] == "\u{1f4cc} logs")
        );
        assert!(!a.contains(&"--floating".to_string()));
        assert!(!a.contains(&"--close-on-exit".to_string()));
        assert_eq!(a.last().unwrap(), "--");
    }

    // A trimmed-down but structurally faithful `dump-layout` with two tabs.
    fn dump(focused_has_drawer: bool, other_has_drawer: bool) -> String {
        let other = if other_has_drawer {
            "        pane name=\"superzej-files\" command=\"yazi\"\n"
        } else {
            "        pane\n"
        };
        let focused_float = if focused_has_drawer {
            "        floating_panes {\n            pane name=\"superzej-files\" command=\"yazi\"\n        }\n"
        } else {
            ""
        };
        format!(
            "layout {{\n    tab name=\"repo/home\" {{\n{other}    }}\n    \
             tab name=\"repo/feature\" focus=true {{\n        pane\n{focused_float}    }}\n}}\n"
        )
    }

    #[test]
    fn drawer_present_only_when_in_focused_tab() {
        // In the focused tab -> present.
        assert!(pane_named_in_focused_tab_in(
            &dump(true, false),
            "superzej-files"
        ));
        // Open only in another (background) worktree's tab -> NOT present here.
        assert!(!pane_named_in_focused_tab_in(
            &dump(false, true),
            "superzej-files"
        ));
        // Open in both -> present (the focused one counts).
        assert!(pane_named_in_focused_tab_in(
            &dump(true, true),
            "superzej-files"
        ));
        // Open in neither -> absent.
        assert!(!pane_named_in_focused_tab_in(
            &dump(false, false),
            "superzej-files"
        ));
    }

    #[test]
    fn drawer_present_handles_no_focused_tab_or_empty() {
        assert!(!pane_named_in_focused_tab_in("", "superzej-files"));
        assert!(!pane_named_in_focused_tab_in(
            "layout {\n    tab name=\"a\" {\n        pane name=\"superzej-files\"\n    }\n}",
            "superzej-files"
        ));
    }
}
