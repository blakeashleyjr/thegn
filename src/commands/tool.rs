//! `superzej tool <name>` — open a per-worktree tool (lazygit/yazi/editor/diff)
//! as a floating pane scoped to the focused worktree.

use crate::config::Config;
use crate::{msg, repo, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(cfg: &Config, name: &str, worktree: Option<String>, file: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree: PathBuf = worktree
        .map(PathBuf::from)
        .or_else(|| std::env::var("SUPERZEJ_WORKTREE").ok().map(PathBuf::from))
        .or_else(|| repo::toplevel(&cwd))
        .unwrap_or(cwd);

    // The editor is special: resolve the user's real editor and route GUI
    // editors to a detached launch instead of a terminal pane. An optional
    // `--file` opens that path instead of the worktree directory.
    if name == "editor" {
        open_editor(cfg, &worktree, file.as_deref());
        // The keybind path (no --file) runs in its own launcher pane that must
        // be closed. The plugin path (--file) is a host run_command with no pane
        // of its own, so closing here would kill the user's focused pane.
        if zellij::in_zellij() && file.is_none() {
            zellij::close_pane();
        }
        return Ok(());
    }

    let mut cmd = cfg
        .tool_command(name)
        .unwrap_or_else(|| msg::die(&format!("tool: unknown tool '{name}'")))
        .to_string();

    // 'diff' uses delta as pager when available for nicer output.
    if name == "diff" && util::have("delta") {
        cmd = "git -c core.pager=delta diff".to_string();
    }

    if zellij::in_zellij() {
        let sh = util::shell();
        zellij::new_float(&worktree, name, &[&sh, "-lc", &cmd]);
        // Close this launcher pane (spawned by the keybind's Run).
        zellij::close_pane();
    } else {
        msg::info(&format!(
            "(not in zellij) would run: {cmd}  [cwd={}]",
            Path::new(&worktree).display()
        ));
    }
    Ok(())
}

/// Launch the editor for `worktree`, opening `file` if given (else the worktree
/// directory). Honors an explicit `editor` tool command from config; otherwise
/// resolves `$VISUAL`/`$EDITOR` (the shipped default `${EDITOR:-vi} .` is POSIX
/// syntax that breaks under non-POSIX shells like fish, so it is treated as
/// "resolve from the environment"). GUI editors (vscode, zed, …) are spawned
/// detached so they don't sit in an empty terminal pane.
pub fn open_editor(cfg: &Config, worktree: &Path, file: Option<&str>) {
    let prog = editor_program(cfg);
    let target = file.unwrap_or(".");
    let cmd = format!("{prog} {}", sh_quote(target));
    if !zellij::in_zellij() {
        msg::info(&format!(
            "(not in zellij) would run: {cmd}  [cwd={}]",
            worktree.display()
        ));
        return;
    }
    if util::is_gui_editor(&prog) {
        util::spawn_detached(&cmd, worktree);
    } else {
        let sh = util::shell();
        zellij::new_float(worktree, "editor", &[&sh, "-lc", &cmd]);
    }
}

/// The editor program (with any flags, but no target): an explicit config
/// override, or the resolved `$VISUAL`/`$EDITOR`. A trailing ` .` in a configured
/// command is dropped so the caller can supply its own target (a file or `.`).
fn editor_program(cfg: &Config) -> String {
    let configured = cfg.tool_command("editor").unwrap_or_default().trim();
    if configured.is_empty() || configured.contains("${EDITOR") {
        util::editor()
    } else {
        configured
            .strip_suffix(" .")
            .unwrap_or(configured)
            .trim()
            .to_string()
    }
}

/// Single-quote a shell argument so paths with spaces/specials survive `-lc`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
