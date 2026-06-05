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

/// Open a new workspace tab (with a layout if given).
pub fn new_tab(name: &str, cwd: &Path, layout: Option<&str>) -> bool {
    let cwd = cwd.to_string_lossy();
    let mut a = vec!["new-tab", "--name", name, "--cwd", &cwd];
    if let Some(l) = layout {
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
