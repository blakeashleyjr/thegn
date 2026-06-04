//! Thin wrappers around `zellij action ...`. No-op gracefully outside a session.

use std::path::Path;
use std::process::Command;

pub fn in_zellij() -> bool {
    std::env::var_os("ZELLIJ").is_some() || std::env::var_os("ZELLIJ_SESSION_NAME").is_some()
}

fn action(args: &[&str]) -> bool {
    Command::new("zellij")
        .arg("action")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Open a new tiled pane running `cmd` at `cwd`.
pub fn new_pane(cwd: &Path, name: &str, direction: &str, cmd: &[&str]) -> bool {
    let mut a: Vec<String> = vec![
        "new-pane".into(),
        "--direction".into(),
        direction.into(),
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

pub fn close_pane() -> bool {
    action(&["close-pane"])
}

pub fn focus_previous_pane() -> bool {
    action(&["focus-previous-pane"])
}

pub fn go_to_tab_name(name: &str) -> bool {
    action(&["go-to-tab-name", name])
}
