//! `superzej tool <name>` — open a per-worktree tool (lazygit/yazi/editor/diff)
//! as a floating pane scoped to the focused worktree.

use crate::config::Config;
use crate::{msg, repo, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(cfg: &Config, name: &str, worktree: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree: PathBuf = worktree
        .map(PathBuf::from)
        .or_else(|| std::env::var("SUPERZEJ_WORKTREE").ok().map(PathBuf::from))
        .or_else(|| repo::toplevel(&cwd))
        .unwrap_or(cwd);

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
