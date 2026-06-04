//! Thin wrappers around `zellij action ...`. No-op gracefully outside a session.

use std::path::Path;
use std::process::Command;

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
    Command::new("zellij")
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
    Command::new("zellij")
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
    let out = Command::new("zellij")
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
    Command::new("zellij")
        .args(["pipe", "--plugin", url, "--name", name, "--", payload])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
