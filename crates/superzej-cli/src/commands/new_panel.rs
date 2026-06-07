//! `superzej new-panel` — open a plain split pane (a "panel") in the focused
//! worktree. This is just a normal zellij pane (split/stack/move as usual); we
//! provide it mainly to scope the new pane's cwd to the worktree.
//!
//! `--in-place` (the Alt+N keybind: `Run ... { direction "Right" }`): this
//! process's own pane IS the panel — drop to a shell at the worktree root.
//! One pane per keypress, instead of a spawned panel plus a dead exited
//! command pane. Without it (the palette) a separate pane is spawned.

use crate::config::Config;
use crate::{msg, repo, util, zellij};
use anyhow::Result;
use std::path::PathBuf;

pub fn run(_cfg: &Config, dir: &str, in_place: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let worktree: PathBuf = std::env::var("SUPERZEJ_WORKTREE")
        .ok()
        .map(PathBuf::from)
        .or_else(|| repo::toplevel(&cwd))
        .unwrap_or(cwd);

    if !zellij::in_zellij() {
        msg::info(&format!(
            "(not in zellij) would open a panel at {}",
            worktree.display()
        ));
        return Ok(());
    }
    if in_place {
        zellij::rename_pane("panel");
        std::env::set_current_dir(&worktree)?;
        util::exec_shell();
    }
    zellij::new_pane_bare(&worktree, "panel", dir);
    Ok(())
}
